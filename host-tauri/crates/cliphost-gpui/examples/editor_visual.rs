//! Debug-only rendering evidence for the production editor. Each invocation
//! owns a fresh repository .tmp data root and one non-activating editor HWND.
//! No production capture, hooks, RPC, OLE or input injection is initialized.

#[cfg(not(debug_assertions))]
fn main() {
    eprintln!("editor_visual requires a Debug build");
    std::process::exit(2);
}

#[cfg(debug_assertions)]
fn main() {
    if let Err(error) = fixture::run() {
        eprintln!("editor_visual: {error:#}");
        std::process::exit(1);
    }
}

#[cfg(debug_assertions)]
mod fixture {
    use std::{
        fs,
        path::{Component, Path, PathBuf},
        time::{Duration, Instant},
    };

    use anyhow::{anyhow, bail, Context as _, Result};
    use cliphost_core::{
        extra_settings::{FavorSettings, GlobalAppConfig},
        i18n::{set_language, LanguageMode},
        model::{ClipRecord, FavorRecord},
        AppPaths, AppStore, ClipKind,
    };
    use cliphost_gpui::{
        editor::{EditorTarget, EditorWindow},
        platform,
        preview::{DesignTokens, TOKENS_JSON},
        service::{NativeHost, NativeHostOptions},
        DesignAssets,
    };
    use gpui::{Application, Timer};
    use serde_json::{json, Value};

    const KINDS: &[&str] = &[
        "text",
        "html",
        "qq",
        "image",
        "files",
        "link",
        "command",
        "action",
        "steps",
        "folder",
        "unsupported",
    ];

    struct Options {
        dark: bool,
        width: f32,
        height: f32,
        kind: String,
        properties: bool,
        output: PathBuf,
        exit_ms: u64,
    }

    impl Options {
        fn parse() -> Result<Option<Self>> {
            let mut value = Self {
                dark: true,
                width: 480.0,
                height: 600.0,
                kind: "text".into(),
                properties: false,
                output: PathBuf::new(),
                exit_ms: 5_000,
            };
            let mut args = std::env::args().skip(1);
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--theme" => {
                        value.dark = match args.next().as_deref() {
                            Some("dark") => true,
                            Some("light") => false,
                            _ => bail!("--theme requires dark or light"),
                        }
                    }
                    "--width" => {
                        value.width = args.next().context("--width requires a value")?.parse()?
                    }
                    "--height" => {
                        value.height = args.next().context("--height requires a value")?.parse()?
                    }
                    "--kind" => value.kind = args.next().context("--kind requires a value")?,
                    "--page" => {
                        value.properties = match args.next().as_deref() {
                            Some("content") => false,
                            Some("properties") => true,
                            _ => bail!("--page requires content or properties"),
                        }
                    }
                    "--output" => {
                        value.output = args
                            .next()
                            .context("--output requires an absolute directory")?
                            .into()
                    }
                    "--exit-after-ms" => {
                        value.exit_ms = args
                            .next()
                            .context("--exit-after-ms requires a value")?
                            .parse()?
                    }
                    "--help" | "-h" => {
                        println!("editor_visual --theme dark|light --width 320|480 --height 600 --kind {} --page content|properties --output <absolute repository .tmp directory> --exit-after-ms 5000", KINDS.join("|"));
                        println!("Fresh isolated data only. A private ephemeral loopback listener is reported; no capture, hooks, RPC, OLE or input injection. A background own-HWND PrintWindow capture is saved as editor.png.");
                        return Ok(None);
                    }
                    _ => bail!("unknown option: {arg}"),
                }
            }
            if !value.width.is_finite()
                || !(320.0..=960.0).contains(&value.width)
                || !value.height.is_finite()
                || !(250.0..=960.0).contains(&value.height)
            {
                bail!("width must be 320..960 and height 250..960 DIP");
            }
            if !KINDS.contains(&value.kind.as_str()) {
                bail!("--kind requires {}", KINDS.join("|"));
            }
            if !(2_000..=60_000).contains(&value.exit_ms) {
                bail!("--exit-after-ms must be 2000..60000");
            }
            if !value.output.is_absolute() {
                bail!("--output requires an absolute path under this repository's .tmp");
            }
            Ok(Some(value))
        }
    }

    fn create_data(output: &Path) -> Result<(PathBuf, AppPaths)> {
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
        if !ancestor.starts_with(&scratch) {
            bail!("--output must remain under the canonical repository .tmp");
        }
        fs::create_dir_all(output)?;
        let output = output.canonicalize()?;
        if output == scratch || !output.starts_with(&scratch) {
            bail!("--output must be a child of repository .tmp");
        }
        // A repeated invocation cannot read or overwrite another run's data.
        let data = output.join("data");
        fs::create_dir(&data).context("output/data must be fresh")?;
        let paths = AppPaths::from_data_root(data);
        paths.ensure()?;
        Ok((output, paths))
    }

    fn history(id: &str, kind: ClipKind, title: &str, body: &str) -> ClipRecord {
        let mut row = ClipRecord::new_text(body, "editor-visual.exe");
        row.id = format!("editor-visual-{id}");
        row.kind = kind;
        row.title = Some(title.into());
        row.create_time = chrono::DateTime::from_timestamp(1_792_079_520, 0).unwrap();
        row.update_time = row.create_time;
        row
    }

    fn favorite(id: &str, kind: ClipKind, title: &str, body: &str) -> FavorRecord {
        let clip = history(id, kind, title, body);
        let mut row = FavorRecord::from_clip(&clip, None);
        row.id = format!("editor-visual-{id}");
        row.create_time = clip.create_time;
        row.update_time = clip.update_time;
        row.source_clip_id = None;
        row
    }

    fn seed(paths: &AppPaths, options: &Options) -> Result<EditorTarget> {
        let store = AppStore::open(&paths.db)?;
        let image = paths.image_library.join("editor-visual-reference.png");
        let png = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../ui-design/assets/clip-main-thumbnail.png");
        fs::copy(png, &image)?;
        let mut history_rows = vec![
            history("text", ClipKind::TextClipItem, "会议纪要 · 编辑窗口", "今天确认的三个细节：\n\n1. 编辑窗吸附在剪贴板右侧，和主窗口等高。\n2. 切换条目时继续编辑，不打断浏览。\n3. 属性收进独立区域，把空间留给内容。\n\n下次检查：窄窗、长文字与中文输入。"),
            history("html", ClipKind::HtmlClipItem, "剪贴板使用指南", "让重要的内容，随手可用\n把常用文字、链接和文件放在一起，减少重复输入，让注意力回到手头的工作。\n清楚的层级，恰当的留白。编辑内容时，仍然看得见上下文。\n保留加粗、斜体与段落\n列表与链接都应清晰可读\n查看示例文档"),
            history("qq", ClipKind::QQRichTextItem, "设计讨论 · 图文记录", "这张配色参考很适合新的编辑窗口。\n图片与文字放在一起，切换条目时也保留内容结构。"),
            history("image", ClipKind::ImageClipItem, "配色与布局参考.png", ""),
            history("files", ClipKind::FileClipItem, "编辑窗设计资料", ""),
            // The persisted kind enum is closed. A history command exercises
            // the same title-only/read-only-payload branch as unknown kinds,
            // without fabricating a new business kind or modifying host DTOs.
            history("unsupported", ClipKind::CmdClipItem, "历史命令 · 只读内容", "explorer.exe \"C:\\ClipFixture\\设计资料\""),
        ];
        history_rows[0].source_process_name = "notepad.exe".into();
        history_rows[1].source_process_name = "msedge.exe".into();
        history_rows[1].html_source_url = Some("https://example.com/clip/guide".into());
        history_rows[1].html_data = Some("<article><h2>让重要的内容，随手可用</h2><p>把常用文字、链接和文件放在一起，<strong>减少重复输入</strong>，让注意力回到手头的工作。</p><blockquote>清楚的层级，恰当的留白。编辑内容时，仍然看得见上下文。</blockquote><ul><li>保留 <strong>加粗</strong>、<em>斜体</em> 与段落</li><li>列表与链接都应清晰可读</li></ul><p><a href=\"https://example.com/clip\">查看示例文档 ↗</a></p></article>".into());
        history_rows[2].source_process_name = "QQ.exe".into();
        history_rows[2].qq_rich_text = Some(format!("<QQRichEditFormat><Info version=\"1001\"/><EditElement type=\"0\"><![CDATA[这张配色参考很适合新的编辑窗口。]]></EditElement><EditElement type=\"3\" filepath=\"{}\"/><EditElement type=\"0\"><![CDATA[图片与文字放在一起，切换条目时也保留内容结构。]]></EditElement></QQRichEditFormat>",image.display()));
        history_rows[3].source_process_name = "mspaint.exe".into();
        history_rows[3].image_path = Some(image.to_string_lossy().into_owned());
        history_rows[4].source_process_name = "explorer.exe".into();
        history_rows[4].file_paths = vec![
            "C:\\ClipFixture\\设计资料\\编辑窗口说明.md".into(),
            "C:\\ClipFixture\\设计资料\\配色参数.json".into(),
            "C:\\ClipFixture\\设计资料\\界面参考\\右侧吸附布局参考.png".into(),
        ];
        history_rows[5].source_process_name = "Quicker.exe".into();
        for row in &history_rows {
            store.insert_clip(row, true)?;
        }
        let mut favorites = vec![
            favorite(
                "link",
                ClipKind::LinkClipItem,
                "产品设计文档",
                "https://example.com/clip/design",
            ),
            favorite(
                "command",
                ClipKind::CmdClipItem,
                "打开示例资料目录",
                "explorer.exe \"C:\\ClipFixture\\设计资料\"",
            ),
            favorite(
                "action",
                ClipKind::ActionClipItem,
                "整理当前内容",
                "{\"Id\":\"editor-visual-action\",\"Title\":\"整理当前内容\"}",
            ),
            favorite(
                "steps",
                ClipKind::StepClipItem,
                "文本整理步骤",
                "[{\"stepRunnerKey\":\"sys:trimtext\",\"inputParams\":{\"text\":\"{输入文本}\"}}]",
            ),
            favorite("folder", ClipKind::FavoriteClipItem, "常用资料", ""),
        ];
        favorites[2].execute_param =
            "{\n  \"output\": \"markdown\",\n  \"keepLineBreaks\": true\n}".into();
        favorites[4].is_folder = true;
        favorites[4].body_text = None;
        favorites[4].children = vec![
            "editor-visual-link".into(),
            "editor-visual-command".into(),
            "editor-visual-action".into(),
        ];
        for row in &favorites {
            store.upsert_favor(row)?;
        }
        store.save_favors_atomic(&favorites)?;
        drop(store);
        fs::write(
            paths.configs.join("GlobalConfig.json"),
            serde_json::to_vec_pretty(&GlobalAppConfig {
                ui_theme_mode: if options.dark { "dark" } else { "light" }.into(),
                ui_language: "zh-CN".into(),
                launch_at_startup: false,
                run_as_administrator: false,
                window_background_frosted_glass: false,
                ..Default::default()
            })?,
        )?;
        fs::write(
            paths.configs.join("FavorConfig.json"),
            serde_json::to_vec_pretty(&FavorSettings {
                root_children: favorites.iter().map(|row| row.id.clone()).collect(),
                ..Default::default()
            })?,
        )?;
        let id = format!("editor-visual-{}", options.kind);
        Ok(
            if matches!(
                options.kind.as_str(),
                "link" | "command" | "action" | "steps" | "folder"
            ) {
                EditorTarget::Favorite(id)
            } else {
                EditorTarget::History(id)
            },
        )
    }

    #[cfg(windows)]
    fn foreground_hwnd() -> isize {
        unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow().0 as isize }
    }

    #[cfg(not(windows))]
    fn foreground_hwnd() -> isize {
        0
    }

    #[cfg(windows)]
    fn native_report(window: &gpui::Window, initial_foreground: isize) -> Result<Value> {
        use windows::Win32::{Foundation::RECT, UI::WindowsAndMessaging::*};
        let hwnd = platform::hwnd(window).map_err(|error| anyhow!(error))?;
        unsafe {
            let mut pid = 0;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid != std::process::id() {
                bail!("editor fixture HWND is not owned by this process");
            }
            let mut rect = RECT::default();
            let mut client = RECT::default();
            GetWindowRect(hwnd, &mut rect)?;
            GetClientRect(hwnd, &mut client)?;
            let extended = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            let foreground = GetForegroundWindow();
            Ok(
                json!({"hwnd":hwnd.0 as isize,"pid":pid,"visible":IsWindowVisible(hwnd).as_bool(),
                "foregroundHwnd":foreground.0 as isize,"initialForegroundHwnd":initial_foreground,
                "foregroundIsFixture":foreground==hwnd,"foregroundUnchanged":foreground.0 as isize==initial_foreground,
                "noActivate":extended & WS_EX_NOACTIVATE.0 as isize!=0,
                "topmost":extended & WS_EX_TOPMOST.0 as isize!=0,
                "rect":{"left":rect.left,"top":rect.top,"width":rect.right-rect.left,"height":rect.bottom-rect.top},
                "client":{"width":client.right-client.left,"height":client.bottom-client.top}}),
            )
        }
    }

    #[cfg(not(windows))]
    fn native_report(_: &gpui::Window, _: isize) -> Result<Value> {
        Ok(json!({"supported":false,"reason":"native capture requires Windows"}))
    }

    /// Called after WindowHandle::update has released its GPUI window borrow:
    /// PrintWindow can synchronously dispatch WM_PRINT to the fixture HWND.
    #[cfg(windows)]
    fn capture(hwnd_value: isize, output: &Path) -> Result<Value> {
        use windows::Win32::{
            Foundation::{HWND, RECT},
            Graphics::Gdi::*,
            UI::WindowsAndMessaging::GetWindowThreadProcessId,
        };
        // windows-rs places this user32 function under Storage::Xps. Keep the
        // disposable example independent of an otherwise unused Cargo feature.
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
            let width = rect.right - rect.left;
            let height = rect.bottom - rect.top;
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
            let printed = print_window(hwnd, dc, 3) != 0; // CLIENTONLY | RENDERFULLCONTENT
            SelectObject(dc, previous);
            let mut pixels = vec![0u8; (width as usize) * (height as usize) * 4];
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
            let nonuniform = pixels.chunks_exact(4).any(|p| p[..3] != first);
            for pixel in pixels.chunks_exact_mut(4) {
                pixel.swap(0, 2);
                pixel[3] = 255;
            }
            let path = output.join("editor.png");
            image::save_buffer(
                &path,
                &pixels,
                width as u32,
                height as u32,
                image::ColorType::Rgba8,
            )?;
            Ok(
                json!({"method":"PrintWindow(PW_CLIENTONLY|PW_RENDERFULLCONTENT)","path":path,
                "width":width,"height":height,"nonuniform":nonuniform,"inputInjected":false,"activationRequested":false}),
            )
        }
    }

    #[cfg(not(windows))]
    fn capture(_: isize, _: &Path) -> Result<Value> {
        Ok(json!({"supported":false,"reason":"PrintWindow requires Windows"}))
    }

    pub fn run() -> Result<()> {
        let Some(options) = Options::parse()? else {
            return Ok(());
        };
        let (output, paths) = create_data(&options.output)?;
        let target = seed(&paths, &options)?;
        cliphost_core::force_headless(true);
        set_language(LanguageMode::Chinese);
        let service = NativeHost::open_with_options(NativeHostOptions {
            data_root: Some(paths.data_root.clone()),
            api_port: 0,
            headless: true,
            background: false,
            isolated: true,
            verb_rpc: false,
            core_copy_acceptance: false,
        })
        .map_err(|error| anyhow!(error))?;
        let host = service.state();
        host.config_update(&json!({"enableNoFocusMode":true}))
            .map_err(|error| anyhow!(error))?;
        let tokens = DesignTokens::from_json(TOKENS_JSON).map_err(|error| anyhow!(error))?;
        let started = Instant::now();
        let root = output.clone();
        let port = host.api_port();
        let initial_foreground = foreground_hwnd();
        println!(
            "{}",
            json!({"fixture":"editor-visual","pid":std::process::id(),"output":output,
            "headless":true,"httpListener":{"address":"127.0.0.1","port":port,"privateEphemeral":true}})
        );
        Application::new().with_assets(DesignAssets).run(move |cx| {
            let mut handle=None;
            EditorWindow::show_target(&mut handle,host,tokens,options.dark,target,cx).expect("create isolated editor fixture");
            let window=handle.expect("editor fixture handle");
            window.update(cx,|view,window,cx| {
                view.set_visual_fixture_page(options.properties,cx);
                platform::set_no_activate(window,true).expect("retain no-activate editor fixture");
                platform::set_geometry(window,Some((60,80)),Some((f64::from(options.width),f64::from(options.height)))).expect("size own editor fixture");
                // PrintWindow can inspect a covered window. Keep fixture HWNDs
                // behind the user's work so a normal click cannot land on one.
                #[cfg(windows)]
                unsafe {
                    use windows::Win32::UI::WindowsAndMessaging::{SetWindowPos, HWND_BOTTOM, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE};
                    SetWindowPos(platform::hwnd(window).expect("own fixture HWND"), Some(HWND_BOTTOM),
                        0, 0, 0, 0, SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE).expect("lower own fixture without activation");
                }
            }).expect("configure own editor fixture");
            cx.on_window_closed(|cx| {if cx.windows().is_empty(){cx.quit();}}).detach();
            cx.spawn(async move |cx| {
                let mut capture_result=None;
                loop {
                    Timer::after(Duration::from_millis(250)).await;
                    let self_exit=started.elapsed()>=Duration::from_millis(options.exit_ms);
                    let value=window.update(cx,|view,window,cx| {
                        let pending=platform::has_pending(window).unwrap_or(true);
                        let viewport=window.viewport_size();
                        json!({"fixture":"editor-visual","pid":std::process::id(),"elapsedMs":started.elapsed().as_millis(),
                            "headless":true,"isolated":true,"capture":false,"globalHooks":false,"rpc":false,"ole":false,"inputInjected":false,
                            "dataRoot":paths.data_root,"output":root,"theme":if options.dark{"dark"}else{"light"},
                            "kind":options.kind,"page":if options.properties{"properties"}else{"content"},
                            "unsupportedFixtureUses":"history CmdClipItem read-only payload",
                            "requestedSize":{"width":options.width,"height":options.height},
                            "viewport":{"width":f32::from(viewport.width),"height":f32::from(viewport.height),"scaleFactor":window.scale_factor()},
                            "httpListener":{"address":"127.0.0.1","port":port,"privateEphemeral":true},
                            "nativePlacementPending":pending,"native":native_report(window,initial_foreground).unwrap_or_else(|error|json!({"error":error.to_string()})),
                            "editor":view.diagnostics(cx),"nativeError":platform::take_error(window).ok().flatten(),"selfExit":self_exit})
                    });
                    let Ok(mut value)=value else {break;};
                    if capture_result.is_none() && started.elapsed()>=Duration::from_millis(1_250)
                        && value["nativePlacementPending"].as_bool()==Some(false)
                        && value["editor"]["loading"].as_bool()==Some(false)
                    {
                        capture_result=Some(capture(value["native"]["hwnd"].as_i64().unwrap_or(0) as isize,&root)
                            .unwrap_or_else(|error|json!({"error":error.to_string()})));
                    }
                    value["screenshot"]=capture_result.clone().unwrap_or_else(||json!({"pending":true}));
                    if let Err(error)=serde_json::to_vec_pretty(&value).map_err(|error|anyhow!(error))
                        .and_then(|bytes|fs::write(root.join("report.json"),bytes).map_err(|error|anyhow!(error)))
                    {
                        eprintln!("fixture report failed: {error}");
                        let _=cx.update(|cx|cx.quit());
                        break;
                    }
                    if self_exit {let _=cx.update(|cx|cx.quit());break;}
                }
            }).detach();
        });
        service.shutdown();
        println!(
            "{}",
            json!({"completed":true,"output":output,"report":output.join("report.json"),"shutdown":true})
        );
        Ok(())
    }
}
