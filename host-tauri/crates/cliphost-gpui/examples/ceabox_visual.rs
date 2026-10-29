//! Pure CeaBox rendering fixture: no HostState, user data, capture or hooks.

#[cfg(not(debug_assertions))]
fn main() {
    eprintln!("ceabox_visual is available only in Debug builds");
    std::process::exit(2);
}

#[cfg(debug_assertions)]
fn main() -> anyhow::Result<()> {
    use std::{fs, path::Path, time::{Duration, Instant}};
    use anyhow::{anyhow, bail};
    use cliphost_core::ceabox::CeaBoxSource;
    use cliphost_gpui::{ceabox::CeaBoxWindow, platform,
        preview::{DesignTokens, TOKENS_JSON}, DesignAssets};
    use gpui::{px, size, App, AppContext, Application, Bounds, Timer, WindowBounds, WindowOptions};

    let args: Vec<String> = std::env::args().collect();
    // Explicit opt-in for native mouse QA; default screenshots never activate.
    let interactive = args.iter().any(|value| value == "--interactive");
    let argument = |name: &str| args.windows(2).find(|pair| pair[0] == name).map(|pair| pair[1].clone());
    let dark = argument("--theme").as_deref() != Some("light");
    let width = argument("--width").and_then(|value| value.parse::<f32>().ok()).unwrap_or(680.0).clamp(360.0, 1200.0);
    let height = argument("--height").and_then(|value| value.parse::<f32>().ok()).unwrap_or(510.0).clamp(330.0, 1000.0);
    let query = argument("--query").unwrap_or_else(|| "项目".into());
    let source = CeaBoxSource::parse(argument("--source").as_deref().unwrap_or("all"));
    let duration = argument("--exit-after-ms").and_then(|value| value.parse::<u64>().ok()).unwrap_or(6000).clamp(1000, 180000);
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..").canonicalize()?;
    let evidence = repo.join(".tmp/ceabox/native");
    fs::create_dir_all(&evidence)?;
    let output = argument("--report").map(std::path::PathBuf::from).unwrap_or_else(|| evidence.join("window-report.json"));
    let parent = output.parent().ok_or_else(|| anyhow!("report needs a parent"))?;
    fs::create_dir_all(parent)?;
    if !parent.canonicalize()?.starts_with(repo.join(".tmp").canonicalize()?) {
        bail!("fixture report must stay within this repository's .tmp directory");
    }
    if fs::symlink_metadata(&output).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        bail!("fixture report cannot be a link");
    }
    let tokens = DesignTokens::from_json(TOKENS_JSON).map_err(|error| anyhow!(error))?;
    let began = Instant::now();
    Application::new().with_assets(DesignAssets).run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(width), px(height)), cx);
        let handle = cx.open_window(WindowOptions {
            titlebar: None, focus: false, show: false,
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_background: gpui::WindowBackgroundAppearance::Opaque,
            ..Default::default()
        }, move |window, cx| {
            window.set_window_title("CeaBox · isolated design fixture");
            cx.new(|cx| CeaBoxWindow::new_fixture(tokens, dark, &query, source, window, cx))
        }).expect("open pure CeaBox fixture");
        handle.update(cx, |view, window, _| -> Result<(), String> {
            view.initialize_native_window(window, Some((f64::from(width), f64::from(height))), !interactive)?;
            if interactive {
                // Match production's topmost band so physical mouse QA cannot
                // land on another window over the fixture. Still no activation.
                platform::show(window, true, false)
            } else {
                platform::show_no_activate(window)
            }
        }).expect("configure fixture").expect("show without activation");
        cx.on_window_closed(|cx| { if cx.windows().is_empty() { cx.quit(); } }).detach();
        cx.spawn(async move |cx| loop {
            Timer::after(Duration::from_millis(120)).await;
            let done = began.elapsed() >= Duration::from_millis(duration);
            let Ok(mut report) = handle.update(cx, |view, window, cx| {
                let mut report = view.report(window, cx);
                #[cfg(windows)] {
                    let hwnd = platform::hwnd(window).ok().map(|value| value.0 as isize);
                    report["hwnd"] = serde_json::json!(hwnd);
                    report["isForeground"] = serde_json::json!(hwnd.is_some_and(|hwnd| unsafe {
                        windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow().0 as isize == hwnd
                    }));
                }
                report["scaleFactor"] = serde_json::json!(window.scale_factor());
                report["nativeError"] = serde_json::json!(platform::take_error(window).ok().flatten());
                report
            }) else { break; };
            report["pid"] = serde_json::json!(std::process::id());
            report["selfExitRequested"] = serde_json::json!(done);
            report["elapsedMs"] = serde_json::json!(began.elapsed().as_millis());
            report["hostState"] = serde_json::json!(false);
            report["apiListener"] = serde_json::json!(false);
            report["userDataRead"] = serde_json::json!(false);
            report["inputInjected"] = serde_json::json!(false);
            report["interactionEnabled"] = serde_json::json!(interactive);
            let _ = fs::write(&output, serde_json::to_vec_pretty(&report).expect("report JSON"));
            if done { let _ = cx.update(|cx| cx.quit()); break; }
        }).detach();
    });
    Ok(())
}
