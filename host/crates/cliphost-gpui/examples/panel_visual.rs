//! Debug-only production PanelWindow renderer with disposable favorite records.
//! No NativeHost, API listener, clipboard capture, hooks, RPC or input injection.

#[cfg(not(debug_assertions))]
fn main() {
    eprintln!("panel_visual is available only in Debug builds.");
    std::process::exit(2);
}

#[cfg(debug_assertions)]
fn main() {
    if let Err(error) = fixture::run() {
        eprintln!("panel_visual: {error:#}");
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
    use cliphost_core::{
        extra_settings::GlobalAppConfig,
        i18n::{set_language, LanguageMode},
        AppPaths, HostState,
    };
    use cliphost_gpui::{
        panel::PanelWindow,
        platform,
        preview::{DesignTokens, TOKENS_JSON},
        DesignAssets,
    };
    use gpui::{Application, Timer};
    use serde_json::{json, Value};

    struct Options {
        dark: bool,
        width: f32,
        height: f32,
        exit_after_ms: u64,
        report: Option<PathBuf>,
    }

    impl Options {
        fn parse() -> Result<Option<Self>> {
            let mut options = Self {
                dark: false,
                width: 420.0,
                height: 520.0,
                exit_after_ms: 10_000,
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
                    "--width" | "--height" => {
                        let value: f32 = args.next().context("size requires a value")?.parse()?;
                        if !value.is_finite() || !(300.0..=1600.0).contains(&value) {
                            bail!("window dimensions must be between 300 and 1600 DIP");
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
                        if !(5_000..=60_000).contains(&options.exit_after_ms) {
                            bail!("--exit-after-ms must be between 5000 and 60000");
                        }
                    }
                    "--report" => {
                        options.report =
                            Some(args.next().context("--report requires a path")?.into());
                    }
                    "--help" | "-h" => {
                        println!("panel_visual --theme light|dark --width 420 --height 520 --exit-after-ms 10000 --report .tmp/panel-alignment-20261014/report.json");
                        println!("Disposable favorites; reports and data stay under this repository's .tmp/panel-alignment-20261014.");
                        return Ok(None);
                    }
                    _ => bail!("unknown argument: {arg}"),
                }
            }
            Ok(Some(options))
        }
    }

    fn isolated_root(repo: &Path) -> Result<(PathBuf, PathBuf)> {
        let evidence = repo.join(".tmp/panel-alignment-20261014");
        fs::create_dir_all(&evidence)?;
        let evidence = evidence.canonicalize()?;
        if !evidence.starts_with(repo) || evidence == repo {
            bail!("fixture evidence directory must resolve within the repository");
        }
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let root = evidence.join(format!("fixture-data-{}-{nonce}", std::process::id()));
        // Never reuse a database or sample from another fixture process.
        fs::create_dir(&root)?;
        Ok((evidence, root))
    }

    fn report_path(
        repo: &Path,
        evidence: &Path,
        root: &Path,
        given: Option<&Path>,
    ) -> Result<PathBuf> {
        let path = match given {
            Some(path) if path.is_absolute() => path.to_path_buf(),
            Some(path) => repo.join(path),
            None => root.join("diagnostics.json"),
        };
        let parent = path
            .parent()
            .context("report path has no parent")?
            .canonicalize()?;
        if !parent.starts_with(evidence) {
            bail!("report must stay under this repository's .tmp/panel-alignment-20261014");
        }
        let path = parent.join(path.file_name().context("report path has no file name")?);
        if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            bail!("report must be a regular local file");
        }
        Ok(path)
    }

    fn sample_favorites(host: &HostState, root: &Path) -> Result<String> {
        let folder = host
            .favor_folder_create(&json!({"title":"收藏夹"}))
            .map_err(|error| anyhow!(error))?;
        let parent = folder["id"]
            .as_str()
            .context("fixture root id")?
            .to_string();
        let server = host
            .favor_folder_create(&json!({"title":"服务器", "parentId":parent}))
            .map_err(|error| anyhow!(error))?;
        host.favor_add_text_items(&json!({"parentId":server["id"], "text":"演示服务器说明"}))
            .map_err(|error| anyhow!(error))?;
        for (title, content) in [
            ("项目管理员", "这里只是用于绘制验证的虚构说明。"),
            ("实验室使用说明", "设备使用前请先查看操作说明。"),
            ("常用终端命令", "cargo check -p cliphost-gpui"),
            ("工作区启动", "npm run dev"),
            (
                "开发环境配置参考与较长标题省略行为",
                "这是一段不会连接外部服务的虚构演示内容。",
            ),
            ("会议记录模板", "议题：\n决定：\n后续事项："),
            ("常用路径", "D:\\Projects\\Example\\README.md"),
            ("文本格式示例", "纯文本、链接与文件使用同一列表密度。"),
            ("每周检查清单", "检查构建与发布说明。"),
            ("快捷说明", "选择条目后可以使用已有的操作。"),
            ("归档说明", "此内容仅在一次性测试数据库中保存。"),
        ] {
            let created = host
                .favor_add_text_items(&json!({"parentId":parent, "text":content}))
                .map_err(|error| anyhow!(error))?;
            host.favor_update(&json!({"id":created["createdIds"][0], "title":title}))
                .map_err(|error| anyhow!(error))?;
        }
        host.favor_add_link(
            &json!({"parentId":parent, "title":"项目文档", "url":"https://example.invalid/docs"}),
        )
        .map_err(|error| anyhow!(error))?;
        let sample = root.join("示例说明.txt");
        fs::write(
            &sample,
            "This file belongs only to the disposable panel fixture.\n",
        )?;
        host.favor_add_paths(&json!({"parentId":parent, "paths":[sample]}))
            .map_err(|error| anyhow!(error))?;
        Ok(parent)
    }

    fn diagnostics(
        host: &HostState,
        window: &gpui::Window,
        options: &Options,
        root: &Path,
        parent: &str,
        started: Instant,
        self_exit: bool,
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
        let viewport = window.viewport_size();
        json!({
            "fixture":"panel-visual", "pid":std::process::id(), "hwnd":hwnd,
            "theme":if options.dark {"dark"} else {"light"}, "dataRoot":root,
            "width":options.width, "height":options.height,
            "viewport":{"width":f32::from(viewport.width),"height":f32::from(viewport.height)},
            "scaleFactor":window.scale_factor(), "elapsedMs":started.elapsed().as_millis(),
            "exitAfterMs":options.exit_after_ms, "selfExitRequested":self_exit,
            "debugOnly":true, "headless":cliphost_core::is_headless(),
            "nativeHost":false, "apiListener":false, "capture":false, "hooks":false,
            "rpc":false, "loginStartup":false, "focusRequested":false,
            "inputInjected":false, "isForeground":is_foreground,
            "panel":host.panel_native_snapshot(),
            "sampleCount":host.panel_page(&json!({"favorId":parent})).ok()
                .and_then(|page| page["items"].as_array().map(Vec::len)),
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
        let (evidence, root) = isolated_root(&repo)?;
        let report = report_path(&repo, &evidence, &root, options.report.as_deref())?;
        let paths = AppPaths::from_data_root(root.clone());
        fs::create_dir(&paths.configs)?;
        let config = GlobalAppConfig {
            ui_theme_mode: if options.dark { "dark" } else { "light" }.into(),
            ui_language: "zh-CN".into(),
            launch_at_startup: false,
            window_background_frosted_glass: false,
            ..GlobalAppConfig::default()
        };
        fs::write(
            paths.configs.join("GlobalConfig.json"),
            serde_json::to_vec_pretty(&config)?,
        )?;
        cliphost_core::force_headless(true);
        set_language(LanguageMode::Chinese);
        let host = Arc::new(HostState::open(paths, 29065).map_err(|error| anyhow!(error))?);
        let parent = sample_favorites(&host, &root)?;
        host.panel_show(&json!({"favorId":parent}))
            .map_err(|error| anyhow!(error))?;
        let tokens = DesignTokens::from_json(TOKENS_JSON).map_err(|error| anyhow!(error))?;
        let started = Instant::now();
        Application::new().with_assets(DesignAssets).run(move |cx| {
            let mut handle = None;
            PanelWindow::show(&mut handle, host.clone(), tokens, options.dark, cx)
                .expect("create isolated non-activating PanelWindow");
            let handle = handle.expect("panel window handle");
            handle
                .update(cx, |_, window, _| {
                    platform::set_geometry(
                        window,
                        None,
                        Some((f64::from(options.width), f64::from(options.height))),
                    )
                })
                .expect("access panel fixture")
                .expect("set fixture client size");
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            cx.spawn(async move |cx| loop {
                Timer::after(Duration::from_millis(50)).await;
                let _ = cx.update(|cx| PanelWindow::maintain(handle, &host, options.dark, cx));
                let self_exit = started.elapsed() >= Duration::from_millis(options.exit_after_ms);
                let value = handle.update(cx, |_, window, _| {
                    diagnostics(&host, window, &options, &root, &parent, started, self_exit)
                });
                let Ok(value) = value else {
                    break;
                };
                if let Err(error) = serde_json::to_vec_pretty(&value)
                    .map_err(|error| error.to_string())
                    .and_then(|bytes| fs::write(&report, bytes).map_err(|error| error.to_string()))
                {
                    eprintln!("fixture report failed: {error}");
                    cx.update(|cx| cx.quit()).ok();
                    break;
                }
                if self_exit {
                    cx.update(|cx| cx.quit()).ok();
                    break;
                }
            })
            .detach();
        });
        Ok(())
    }
}
