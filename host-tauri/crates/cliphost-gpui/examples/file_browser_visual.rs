//! Synthetic native File Browser window. Uses a fresh isolated core solely for
//! window ownership; directory contents and drive metadata stay in the fixture.

#[cfg(not(debug_assertions))]
fn main() {
    eprintln!("file_browser_visual is available only in Debug builds");
    std::process::exit(2);
}

#[cfg(debug_assertions)]
fn main() -> anyhow::Result<()> {
    use anyhow::{anyhow, bail};
    use cliphost_core::{host::HostState, paths::AppPaths};
    use cliphost_gpui::{
        file_browser::FileBrowserWindow,
        platform,
        preview::{DesignTokens, TOKENS_JSON},
        DesignAssets,
    };
    use gpui::{
        px, size, App, AppContext, Application, Bounds, Timer, WindowBounds, WindowOptions,
    };
    use serde_json::json;
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::Arc,
        time::{Duration, Instant},
    };

    let args = std::env::args().collect::<Vec<_>>();
    let argument = |name: &str| {
        args.windows(2)
            .find(|pair| pair[0] == name)
            .map(|pair| pair[1].clone())
    };
    let tokens = DesignTokens::from_json(TOKENS_JSON).map_err(|error| anyhow!(error))?;
    let p = tokens.file_browser_proposal;
    let dark = argument("--theme").as_deref() != Some("light");
    let interactive = args.iter().any(|value| value == "--interactive");
    let width = argument("--width")
        .and_then(|value| value.parse::<f32>().ok())
        .unwrap_or(p.width)
        .clamp(p.min_width, 1400.0);
    let height = argument("--height")
        .and_then(|value| value.parse::<f32>().ok())
        .unwrap_or(p.height)
        .clamp(p.min_height, 1100.0);
    let path = argument("--path").unwrap_or_default();
    let duration = argument("--exit-after-ms")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(12000)
        .clamp(1000, 180000);
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .canonicalize()?;
    let temporary = repo.join(".tmp");
    fs::create_dir_all(&temporary)?;
    let root = PathBuf::from(
        argument("--root")
            .ok_or_else(|| anyhow!("--root must name a new directory inside clip/.tmp"))?,
    );
    if !root.is_absolute() || root.exists() {
        bail!("--root must be an absolute, new, nonexisting directory");
    }
    let parent = root
        .parent()
        .ok_or_else(|| anyhow!("fixture root needs a parent"))?;
    if !parent.is_dir()
        || !parent
            .canonicalize()?
            .starts_with(temporary.canonicalize()?)
    {
        bail!("fixture root parent must exist inside clip/.tmp");
    }
    for directory in parent.ancestors().take_while(|path| *path != repo) {
        let metadata = fs::symlink_metadata(directory)?;
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if metadata.file_attributes() & 0x400 != 0 {
                bail!("fixture directories cannot contain reparse points");
            }
        }
        if metadata.file_type().is_symlink() {
            bail!("fixture directories cannot contain links");
        }
    }
    fs::create_dir(&root)?;
    fs::write(
        root.join(".file-browser-visual-fixture"),
        b"synthetic file browser; no real filesystem enumeration\n",
    )?;
    let output = root.join("report.json");
    cliphost_core::force_headless(true);
    cliphost_core::i18n::set_language(cliphost_core::i18n::LanguageMode::Chinese);
    let paths = AppPaths::from_data_root(root.join("data"));
    fs::create_dir_all(&paths.data_root)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let _runtime_guard = runtime.enter();
    let host =
        Arc::new(HostState::open_isolated_visual(paths, 29065).map_err(|error| anyhow!(error))?);
    let began = Instant::now();
    Application::new().with_assets(DesignAssets).run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(width), px(height)), cx);
        let build_host = host.clone();
        let handle = cx.open_window(WindowOptions {
            titlebar: None, focus: false, show: false,
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(p.min_width), px(p.min_height))),
            window_background: gpui::WindowBackgroundAppearance::Opaque,
            ..Default::default()
        }, move |window, cx| {
            window.set_window_title("Files · isolated native fixture");
            cx.new(|cx| FileBrowserWindow::new_fixture(build_host, tokens, dark, &path, window, cx))
        }).expect("open isolated file browser fixture");
        handle.update(cx, |view, window, _| -> Result<(), String> {
            view.initialize_native_window(window, Some((f64::from(width), f64::from(height))), !interactive)?;
            if interactive { platform::show(window, true, false) } else { platform::show_no_activate(window) }
        }).expect("configure file browser fixture").expect("show file browser fixture");
        cx.on_window_closed(|cx| { if cx.windows().is_empty() { cx.quit(); } }).detach();
        cx.spawn(async move |cx| loop {
            Timer::after(Duration::from_millis(120)).await;
            let done = began.elapsed() >= Duration::from_millis(duration);
            let Ok(mut report) = handle.update(cx, |view, window, cx| {
                let mut report = view.diagnostics(cx);
                #[cfg(windows)] { report["hwnd"] = json!(platform::hwnd(window).ok().map(|value| value.0 as isize)); }
                report["clientSize"] = json!({"width":f32::from(window.viewport_size().width),"height":f32::from(window.viewport_size().height)});
                report["scaleFactor"] = json!(window.scale_factor());
                report["nativeError"] = json!(platform::take_error(window).ok().flatten());
                report
            }) else { break; };
            report["pid"] = json!(std::process::id());
            report["elapsedMs"] = json!(began.elapsed().as_millis());
            report["selfExitRequested"] = json!(done);
            report["isolatedCore"] = json!(true);
            report["headlessEffects"] = json!(true);
            report["syntheticFilesystem"] = json!(true);
            report["apiListener"] = json!(false);
            report["inputInjected"] = json!(false);
            report["interactionEnabled"] = json!(interactive);
            let _ = fs::write(&output, serde_json::to_vec_pretty(&report).expect("file browser fixture report"));
            if done { let _ = cx.update(|cx| cx.quit()); break; }
        }).detach();
    });
    Ok(())
}
