//! Native Reopen rendering and interaction against a newly created isolated
//! database. Never starts a listener, captures, hooks, or production discovery.

#[cfg(not(debug_assertions))]
fn main() {
    eprintln!("reopen_visual is available only in Debug builds");
    std::process::exit(2);
}

#[cfg(debug_assertions)]
fn main() -> anyhow::Result<()> {
    use std::{fs, path::{Path, PathBuf}, sync::Arc, time::{Duration, Instant}};
    use anyhow::{anyhow, bail};
    use cliphost_core::{host::HostState, paths::AppPaths, store::AppStore};
    use cliphost_gpui::{platform, preview::{DesignTokens, TOKENS_JSON}, reopen::ReopenWindow, DesignAssets};
    use gpui::{px, size, App, AppContext, Application, Bounds, Timer, WindowBounds, WindowOptions};
    use serde_json::json;

    let args: Vec<String> = std::env::args().collect();
    let argument = |name: &str| args.windows(2).find(|pair| pair[0] == name).map(|pair| pair[1].clone());
    let quick = argument("--mode").as_deref() == Some("quick");
    let interactive = args.iter().any(|value| value == "--interactive");
    let settings_open = args.iter().any(|value| value == "--settings-open");
    let dark = argument("--theme").as_deref() != Some("light");
    let width = argument("--width").and_then(|value| value.parse::<f32>().ok()).unwrap_or(if quick { 420.0 } else { 900.0 }).clamp(320.0, 1200.0);
    let height = argument("--height").and_then(|value| value.parse::<f32>().ok()).unwrap_or(if quick { 560.0 } else { 640.0 }).clamp(380.0, 1000.0);
    let duration = argument("--exit-after-ms").and_then(|value| value.parse::<u64>().ok()).unwrap_or(8000).clamp(1000, 180000);
    let query = argument("--query").unwrap_or_default();
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..").canonicalize()?;
    let temporary = repo.join(".tmp");
    fs::create_dir_all(&temporary)?;
    let requested = argument("--root").ok_or_else(|| anyhow!("--root must name a new directory inside this repository's .tmp"))?;
    let root = PathBuf::from(requested);
    if !root.is_absolute() || root.exists() { bail!("--root must be an absolute, new, nonexisting directory"); }
    let parent = root.parent().ok_or_else(|| anyhow!("fixture root needs a parent"))?;
    if !parent.is_dir() || !parent.canonicalize()?.starts_with(temporary.canonicalize()?) { bail!("fixture root parent must exist inside clip/.tmp"); }
    for directory in parent.ancestors().take_while(|path| *path != repo) {
        let metadata = fs::symlink_metadata(directory)?;
        #[cfg(windows)] {
            use std::os::windows::fs::MetadataExt;
            if metadata.file_attributes() & 0x400 != 0 { bail!("fixture directories cannot contain reparse points"); }
        }
        if metadata.file_type().is_symlink() { bail!("fixture directories cannot contain links"); }
    }
    fs::create_dir(&root)?;
    fs::write(root.join(".reopen-visual-fixture"), b"isolated synthetic Reopen fixture\n")?;
    let output = argument("--report").map(PathBuf::from).unwrap_or_else(|| root.join("report.json"));
    let output_parent = output.parent().ok_or_else(|| anyhow!("report needs a parent"))?;
    if !output_parent.is_dir() || !output_parent.canonicalize()?.starts_with(root.canonicalize()?) || output.exists() {
        bail!("report must be a new file within the new fixture root");
    }

    cliphost_core::force_headless(true);
    cliphost_core::i18n::set_language(cliphost_core::i18n::LanguageMode::Chinese);
    let paths = AppPaths::from_data_root(root.join("data"));
    fs::create_dir_all(&paths.data_root)?;
    let seed = [
        r"C:\Example\工作\产品方案", r"C:\Example\下载", r"D:\Example\项目\设计资料",
        r"C:\Example\工作\报表", r"E:\Example\归档\2025\报表", r"\\example-server\团队\共享资料",
        r"D:\Example\项目归档\2026\秋季产品与内容改版\第三轮评审\最终交付与说明文档",
        r"C:\Example\图片\截图", r"D:\Example\归档\旧项目", r"D:\Example\内容\素材",
        r"D:\Example\工作\会议纪要", r"E:\Example\归档\草稿", r"E:\Example\旧项目\参考文档",
    ];
    {
        let store = AppStore::open(&paths.db)?;
        for path in seed.iter().rev() { store.upsert_explorer_record(path)?; }
    }
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let _runtime_guard = runtime.enter();
    let host = Arc::new(HostState::open_isolated_visual(paths, 29065).map_err(|error| anyhow!(error))?);
    let tokens = DesignTokens::from_json(TOKENS_JSON).map_err(|error| anyhow!(error))?;
    let began = Instant::now();
    Application::new().with_assets(DesignAssets).run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(width), px(height)), cx);
        let build_host = host.clone();
        let handle = cx.open_window(WindowOptions {
            titlebar: None, focus: false, show: false,
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_background: gpui::WindowBackgroundAppearance::Opaque,
            ..Default::default()
        }, move |window, cx| {
            window.set_window_title("Reopen · isolated native fixture");
            cx.new(|cx| ReopenWindow::new_fixture(build_host, tokens, dark, quick, &query, settings_open, window, cx))
        }).expect("open isolated Reopen fixture");
        handle.update(cx, |view, window, _| -> Result<(), String> {
            view.initialize_native_window(window, Some((f64::from(width), f64::from(height))), !interactive)?;
            if interactive { platform::show(window, true, false) } else { platform::show_no_activate(window) }
        }).expect("configure fixture").expect("show fixture");
        cx.on_window_closed(|cx| { if cx.windows().is_empty() { cx.quit(); } }).detach();
        cx.spawn(async move |cx| loop {
            Timer::after(Duration::from_millis(120)).await;
            let done = began.elapsed() >= Duration::from_millis(duration);
            let Ok(mut report) = handle.update(cx, |view, window, cx| {
                let mut report = view.diagnostics(cx);
                #[cfg(windows)] {
                    let hwnd = platform::hwnd(window).ok().map(|value| value.0 as isize);
                    report["hwnd"] = json!(hwnd);
                    report["isForeground"] = json!(hwnd.is_some_and(|hwnd| unsafe {
                        windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow().0 as isize == hwnd
                    }));
                }
                report["clientSize"] = json!({"width":f32::from(window.viewport_size().width),"height":f32::from(window.viewport_size().height)});
                report["scaleFactor"] = json!(window.scale_factor());
                report["nativeError"] = json!(platform::take_error(window).ok().flatten());
                report
            }) else { break; };
            report["pid"] = json!(std::process::id());
            report["selfExitRequested"] = json!(done);
            report["elapsedMs"] = json!(began.elapsed().as_millis());
            report["syntheticSeedCount"] = json!(seed.len());
            report["isolatedCore"] = json!(true);
            report["headlessEffects"] = json!(true);
            report["apiListener"] = json!(false);
            report["userDataRead"] = json!(false);
            report["inputInjected"] = json!(false);
            report["interactionEnabled"] = json!(interactive);
            let _ = fs::write(&output, serde_json::to_vec_pretty(&report).expect("report JSON"));
            if done { let _ = cx.update(|cx| cx.quit()); break; }
        }).detach();
    });
    Ok(())
}
