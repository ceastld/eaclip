#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use cliphost_core::i18n::{self, tr, LanguageMode};
use cliphost_gpui::{
    platform,
    preview::{DesignTokens, PreviewModel, FIXTURE_JSON, TOKENS_JSON},
    service::{NativeHost, NativeHostOptions},
    tray::TrayGuard,
    view::{ClipboardPreview, UiModel},
    DesignAssets,
};
use gpui::{prelude::*, px, size, Application, Bounds, Timer, WindowBounds, WindowOptions};
use serde_json::json;
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

mod crash_dialog;
mod core_copy_report;
mod native_crash;
mod startup_diagnostics;

struct PreviewLogger;
impl log::Log for PreviewLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Warn
    }
    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            let message = record.args().to_string();
            startup_diagnostics::warning(record.level(), record.target(), &message);
            eprintln!("{}: {}", record.level(), message);
        }
    }
    fn flush(&self) {}
}
static LOGGER: PreviewLogger = PreviewLogger;

#[cfg(debug_assertions)]
#[path = "qa_content_fixture.rs"]
mod qa_content_fixture;

#[derive(Clone)]
struct Options {
    dark: bool,
    theme_given: bool,
    theme_auto: bool,
    category: String,
    query: String,
    width: Option<f32>,
    height: Option<f32>,
    stress: bool,
    no_focus: bool,
    snapshot: bool,
    inspect: bool,
    exit_after_ms: Option<u64>,
    report: Option<PathBuf>,
    qa_content_fixture: Option<PathBuf>,
    qa_keyboard_input: bool,
    fixture: bool,
    sync_sources: bool,
    data_root: Option<PathBuf>,
    api_port: u16,
    api_port_given: bool,
    test_instance: bool,
    core_copy_acceptance: bool,
    headless: bool,
    background: bool,
    language: Option<LanguageMode>,
    /// Fixture-only list appearance slot (standard / simple) for design captures.
    appearance: Option<String>,
}

impl Options {
    fn parse() -> Result<Option<Self>, String> {
        Self::parse_from(std::env::args().skip(1))
    }

    fn parse_from(arguments: impl IntoIterator<Item = String>) -> Result<Option<Self>, String> {
        let mut options = Self {
            dark: true,
            theme_given: false,
            theme_auto: false,
            category: "all".into(),
            query: String::new(),
            width: None,
            height: None,
            stress: false,
            no_focus: false,
            snapshot: false,
            inspect: false,
            exit_after_ms: None,
            report: None,
            qa_content_fixture: None,
            qa_keyboard_input: false,
            fixture: false,
            sync_sources: false,
            data_root: None,
            // Resolve overrides only after the core restores a relaunch marker.
            api_port: cliphost_core::DEFAULT_API_PORT,
            api_port_given: false,
            test_instance: false,
            core_copy_acceptance: false,
            headless: false,
            background: false,
            language: None,
            appearance: None,
        };
        let mut args = arguments.into_iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--help" | "-h" => {
                    println!("ClipHost GPUI — native clipboard host\n\nDefault: real history/capture/paste, selected product data root, single instance.\n--background (start at sign-in without showing the clipboard)\n--fixture (isolated design preview, no host or capture)\n--theme dark|light --language system|zh-CN|en --appearance standard|simple (fixture only) --query TEXT --type all|text|image|file|top\n--width DIP --height DIP --no-focus\n--stress --snapshot --inspect (fixture only)\n--data-root PATH --api-port PORT\n--test-instance (requires explicit isolated data root, no shared RPC/LAN)\n--headless (isolated core testing only)\n--exit-after-ms N --report PATH (explicit QA diagnostics)");
                    return Ok(None);
                }
                "--theme" => {
                    options.theme_given = true;
                    options.dark = match args.next().as_deref() {
                        Some("dark") => true,
                        Some("light") => false,
                        _ => return Err("--theme requires dark or light".into()),
                    }
                }
                "--language" => {
                    let value = args
                        .next()
                        .ok_or("--language requires system, zh-CN or en")?;
                    options.language = Some(match value.as_str() {
                        "system" => LanguageMode::System,
                        "zh-CN" => LanguageMode::Chinese,
                        "en" => LanguageMode::English,
                        _ => return Err("--language requires system, zh-CN or en".into()),
                    });
                }
                "--type" => {
                    options.category = args.next().ok_or("--type requires a category")?;
                    if !matches!(
                        options.category.as_str(),
                        "all" | "text" | "image" | "file" | "top"
                    ) {
                        return Err("Unsupported preview category".into());
                    }
                }
                "--query" => options.query = args.next().ok_or("--query requires text")?,
                "--sync-sources" => options.sync_sources = true,
                "--appearance" => {
                    let value = args.next().ok_or("--appearance requires standard or simple")?;
                    if !matches!(value.as_str(), "standard" | "simple") {
                        return Err("--appearance requires standard or simple".into());
                    }
                    options.appearance = Some(value);
                }
                "--width" | "--height" => {
                    let value = args
                        .next()
                        .ok_or("Dimension is missing")?
                        .parse::<f32>()
                        .map_err(|_| "Invalid dimension")?;
                    let range = if arg == "--width" {
                        300.0..=1_000.0
                    } else {
                        250.0..=1_500.0
                    };
                    if !range.contains(&value) {
                        return Err("Preview dimension is outside the supported range".into());
                    }
                    if arg == "--width" {
                        options.width = Some(value);
                    } else {
                        options.height = Some(value);
                    }
                }
                "--stress" => options.stress = true,
                "--fixture" => options.fixture = true,
                "--data-root" => {
                    options.data_root =
                        Some(PathBuf::from(args.next().ok_or("Data root is missing")?))
                }
                "--api-port" => {
                    options.api_port_given = true;
                    options.api_port = args
                        .next()
                        .ok_or("API port is missing")?
                        .parse()
                        .map_err(|_| "Invalid API port")?
                }
                "--test-instance" => options.test_instance = true,
                "--qa-keyboard-input" => options.qa_keyboard_input = true,
                "--core-copy-acceptance" => {
                    if !cfg!(feature = "native-acceptance") {
                        return Err("--core-copy-acceptance requires the private native-acceptance build".into());
                    }
                    options.core_copy_acceptance = true;
                }
                "--headless" => options.headless = true,
                cliphost_core::startup::BACKGROUND_ARGUMENT => {
                    options.background = true;
                }
                "--no-focus" => options.no_focus = true,
                "--snapshot" => options.snapshot = true,
                "--inspect" => options.inspect = true,
                "--exit-after-ms" => {
                    options.exit_after_ms = Some(
                        args.next()
                            .ok_or("Exit timeout is missing")?
                            .parse::<u64>()
                            .map_err(|_| "Invalid exit timeout")?
                            .clamp(250, 600_000),
                    )
                }
                "--report" => {
                    options.report =
                        Some(PathBuf::from(args.next().ok_or("Report path is missing")?))
                }
                "--qa-content-fixture" => {
                    options.qa_content_fixture = Some(PathBuf::from(
                        args.next().ok_or("QA content fixture path is missing")?,
                    ));
                }
                // Private core restart/elevation arguments are forwarded intact.
                // wait_for_predecessor validates the payload and duplicate markers.
                arg if arg.starts_with("--cliphost-restart-parent=")
                    || arg.starts_with("--cliphost-restart-environment=")
                    || arg.eq_ignore_ascii_case(
                        cliphost_core::process_restart::DEMOTE_ATTEMPTED_ARG,
                    ) => {}
                _ => return Err(format!("Unknown option: {arg}")),
            }
        }
        if options.inspect || options.stress || options.snapshot {
            options.fixture = true;
        }
        if options.no_focus && !options.fixture {
            return Err(
                "Live mode uses the saved EnableNoFocusMode setting; --no-focus is for --fixture."
                    .into(),
            );
        }
        if options.appearance.is_some() && !options.fixture {
            return Err("--appearance is for --fixture; live mode uses the saved list appearance.".into());
        }
        if options.sync_sources && !options.fixture {
            return Err("--sync-sources is only for the isolated design fixture.".into());
        }
        if options.language.is_some() && !options.fixture {
            return Err(
                "--language is for --fixture; live mode uses the saved language setting.".into(),
            );
        }
        if (options.test_instance || options.headless) && options.data_root.is_none() {
            return Err("Isolated testing requires --data-root".into());
        }
        if options.qa_keyboard_input && (!cfg!(debug_assertions) || !options.test_instance
            || options.headless || options.fixture || options.core_copy_acceptance
            || !options.api_port_given || options.report.is_none()
            || matches!(options.api_port, 0 | 28865 | 28965 | 18765 | 18776 | 28766 | 29766)) {
            return Err("Keyboard QA requires a Debug native test instance, explicit isolated root, separate port and report.".into());
        }
        if options.core_copy_acceptance
            && (!options.test_instance || options.headless || options.fixture
                || !options.api_port_given
                || matches!(options.api_port, 0 | 28865 | 28965 | 18765 | 18776 | 28766 | 29766)) {
            return Err("Core-copy acceptance requires --test-instance, an explicit copied data root and separate API port; it cannot use headless or fixture mode.".into());
        }
        if options.qa_content_fixture.is_some()
            && (!cfg!(debug_assertions)
                || !options.test_instance
                || !options.headless
                || options.fixture
                || !options.api_port_given
                || options.report.is_none()
                || matches!(
                    options.api_port,
                    0 | 28865 | 28965 | 18765 | 18776 | 28766 | 29766
                ))
        {
            return Err("QA content fixtures require a Debug isolated headless host, explicit separate API port, data root and report.".into());
        }
        Ok(Some(options))
    }
}

fn write_report(path: &std::path::Path, report: &serde_json::Value) -> Result<(), String> {
    if core_copy_report::write_if_owned(path, report)? {
        return Ok(());
    }
    if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    std::fs::write(
        path,
        serde_json::to_vec_pretty(report).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())
}

fn run() -> Result<(), String> {
    let started = Instant::now();
    startup_diagnostics::phase("options.parse");
    let Some(mut options) = Options::parse()? else {
        return Ok(());
    };
    if options.core_copy_acceptance {
        if let Some(report) = options.report.as_deref() {
            core_copy_report::initialize(options.data_root.as_deref().unwrap(), report)?;
        }
    }
    if !options.fixture {
        // A replacement must restore its app overrides and wait for the exact
        // predecessor before singleton acquisition, path selection or elevation.
        startup_diagnostics::phase("restart.wait_predecessor");
        cliphost_core::process_restart::wait_for_predecessor()?;
    }
    if options.fixture {
        i18n::set_language(options.language.unwrap_or_default());
    }
    if !options.api_port_given {
        options.api_port = cliphost_core::resolve_listen_port();
    }
    startup_diagnostics::phase("design_assets.parse");
    let mut tokens = DesignTokens::from_json(TOKENS_JSON)?;
    tokens.window.width = options.width.unwrap_or(tokens.window.width);
    tokens.window.height = options.height.unwrap_or(tokens.window.height);
    let fixture_json = if options.sync_sources {
        let mut fixture: serde_json::Value = serde_json::from_str(FIXTURE_JSON).map_err(|error| error.to_string())?;
        fixture["items"][0]["sourceProcessName"] = json!("ClipHost Device Sync");
        fixture["items"][1]["sourceProcessName"] = json!("ClipHost Sync");
        serde_json::to_string(&fixture).map_err(|error| error.to_string())?
    } else { FIXTURE_JSON.to_owned() };
    let mut model = PreviewModel::from_json(&fixture_json)?;
    if options.stress {
        model.enable_stress()?;
    }
    if options.inspect {
        let page = model.query(&options.category, &options.query, 0, 1_000);
        let report = json!({"mode":"fixture", "theme":if options.dark {"dark"} else {"light"},
            "language":i18n::displayed_language().wire_name(),
            "category":options.category, "query":options.query, "fixtureCount":model.len(),
            "width":tokens.window.width,"height":tokens.window.height,
            "productionHostStarted":false, "page":page });
        if let Some(path) = options.report {
            write_report(&path, &report)?;
        }
        println!(
            "{}",
            serde_json::to_string(&report).map_err(|error| error.to_string())?
        );
        return Ok(());
    }
    #[cfg(debug_assertions)]
    let qa_fixture_ids = if let Some(fixture) = &options.qa_content_fixture {
        let ids = qa_content_fixture::seed(
            options
                .data_root
                .as_deref()
                .ok_or("QA data root is missing")?,
            fixture,
            options
                .report
                .as_deref()
                .ok_or("QA report path is missing")?,
        )?;
        println!(
            "QA content fixture imported {} isolated records.",
            ids.len()
        );
        Some(ids)
    } else {
        None
    };
    startup_diagnostics::phase("native_host.open");
    let service = if options.fixture {
        None
    } else {
        let native_options = NativeHostOptions {
            data_root: options.data_root.clone(),
            api_port: options.api_port,
            headless: options.headless,
            background: options.background,
            isolated: options.test_instance,
            verb_rpc: !options.test_instance && !options.headless,
            core_copy_acceptance: options.core_copy_acceptance,
        };
        let opened = if options.qa_keyboard_input {
            #[cfg(debug_assertions)]
            { NativeHost::open_keyboard_input_qa(native_options) }
            #[cfg(not(debug_assertions))]
            { Err("Keyboard QA requires a Debug build.".into()) }
        } else { NativeHost::open_with_options(native_options) };
        match opened {
            Ok(service) => Some(service),
            Err(error) if error.starts_with("EXISTING_INSTANCE:") => {
                if options.core_copy_acceptance {
                    return Err(format!("CORE_ACCEPTANCE_ALREADY_RUNNING: {error}"));
                }
                println!("{error}");
                return Ok(());
            }
            Err(error) => return Err(error),
        }
    };
    startup_diagnostics::phase("native_host.ready");
    #[cfg(debug_assertions)]
    let qa_isolation =
        if options.qa_keyboard_input {
            let service = service.as_ref().ok_or("Keyboard QA has no live service")?;
            Some(json!({
                "pid":std::process::id(),"debug":true,"isolated":service.isolated(),
                "headless":false,"keyboardInput":true,"captureEnabled":false,
                "globalHooksEnabled":true,"globalHotkeysEnabled":false,"verbRpcEnabled":false,
                "outboundEnabled":false,"dataRoot":service.state().data_root(),
                "apiPort":options.api_port,"reportSequence":0,
            }))
        } else if options.core_copy_acceptance {
            let service = service.as_ref().ok_or("Core-copy acceptance has no live service")?;
            Some(json!({
                "pid":std::process::id(),"debug":true,"isolated":service.isolated(),
                "headless":cliphost_core::is_headless(),"storageLayout":"core",
                "captureEnabled":false,"globalHooksEnabled":false,"verbRpcEnabled":false,
                "outboundEnabled":false,"receiverGuarded":true,
                "dataRoot":service.state().data_root(),"apiPort":options.api_port,
                "reportSequence":0,
            }))
        } else if let (Some(fixture), Some(service)) = (&options.qa_content_fixture, &service) {
            let fixture_bytes = std::fs::read(fixture).map_err(|error| error.to_string())?;
            let headless = cliphost_core::is_headless();
            let isolated = service.isolated();
            Some(json!({
                "pid":std::process::id(),"debug":true,"isolated":isolated,"headless":headless,
                "captureEnabled":!headless,"globalHooksEnabled":!headless,
                "verbRpcEnabled":!isolated && !headless,
                "dataRoot":service.state().data_root(),"apiPort":options.api_port,
                "fixturePath":fixture.canonicalize().map_err(|error|error.to_string())?,
                "fixtureSha256":cliphost_core::lan::sha256_hex(&fixture_bytes),
                "reportSequence":0,
            }))
        } else {
            None
        };
    let model = if let Some(host) = &service {
        UiModel::Live(host.clone())
    } else {
        UiModel::Fixture(model)
    };
    if let Some(service) = &service {
        let state = service.state();
        i18n::set_language(state.ui_language());
        if !options.theme_given {
            match state.config_get()["uiThemeMode"]
                .as_str()
                .unwrap_or("auto")
                .to_ascii_lowercase()
                .as_str()
            {
                "light" => options.dark = false,
                "dark" => options.dark = true,
                _ => options.theme_auto = true,
            }
        }
        let (width, height) = state.clipboard_window_size(false);
        if options.width.is_none() {
            tokens.window.width = width as f32;
        }
        if options.height.is_none() {
            tokens.window.height = height as f32;
        }
    }
    let running_service = service.clone();
    // Retain the tray worker even when its icon starts hidden: the worker
    // applies later visibility preferences without restarting the host.
    startup_diagnostics::phase("tray.start");
    let _tray = match &service {
        Some(service) if !service.isolated() => Some(TrayGuard::start(&service.state())?),
        _ => None,
    };
    startup_diagnostics::phase("gpui.application.create");
    Application::new().with_assets(DesignAssets).run(move |cx| {
        startup_diagnostics::phase("gpui.root_window.create");
        let card_tokens = tokens.clone();
        let bounds = Bounds::centered(
            None,
            size(px(tokens.window.width), px(tokens.window.height)),
            cx,
        );
        let no_focus = options.no_focus
            || running_service
                .as_ref()
                .is_some_and(|host| !host.state().should_activate_on_show());
        let view_options = options.clone();
        let handle = cx
            .open_window(
                WindowOptions {
                    titlebar: None,
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    focus: running_service.is_none() && !no_focus,
                    show: running_service.is_none(),
                    window_min_size: Some(size(px(300.0), px(250.0))),
                    ..Default::default()
                },
                move |window, cx| {
                    if let UiModel::Live(service) = &model {
                        let host = service.state();
                        window.on_window_should_close(cx, move |_, _| {
                            if host.close_behavior() == "ExitApplication" {
                                log::warn!(target: "cliphost_gpui::lifecycle", "Exit requested by main window close (ExitApplication)");
                                host.chrome_web_action(
                                    &json!({"kind":"mainHostCommand", "command":"exit"}),
                                );
                            } else {
                                host.window_hide();
                            }
                            false
                        });
                    }
                    let dark = if view_options.theme_auto {
                        matches!(
                            window.appearance(),
                            gpui::WindowAppearance::Dark | gpui::WindowAppearance::VibrantDark
                        )
                    } else {
                        view_options.dark
                    };
                    window.set_window_title(if matches!(model, UiModel::Live(_)) {
                        "ClipHost GPUI"
                    } else {
                        tr("ClipHost GPUI · 设计预览", "ClipHost GPUI · Design preview")
                    });
                    cx.new(|cx| {
                        startup_diagnostics::phase("gpui.preview.construct");
                        let mut view = ClipboardPreview::new(
                            tokens,
                            model,
                            dark,
                            view_options.category,
                            view_options.query,
                            no_focus,
                            view_options.snapshot,
                            started,
                            window,
                            cx,
                        );
                        startup_diagnostics::phase("gpui.preview.ready");
                        if view_options.report.is_some() {
                            view.enable_diagnostics();
                        }
                        if let Some(slot) = &view_options.appearance {
                            view.set_fixture_appearance(slot);
                        }
                        #[cfg(debug_assertions)]
                        if let (Some(ids), Some(path)) = (&qa_fixture_ids, &view_options.report) {
                            if let Err(error) = view.enable_content_qa(ids.clone(), path.clone()) {
                                eprintln!("QA harness initialization failed: {error}");
                                cx.quit();
                            }
                        }
                        view
                    })
                },
            )
            .expect("create isolated GPUI preview");
        handle
            .update(cx, |_, window, _| {
                if running_service.is_some() {
                    return;
                }
                if let Err(error) = platform::set_no_activate(window, no_focus) {
                    eprintln!("{error}");
                }
                if no_focus {
                    if let Err(error) = platform::show_no_activate(window) {
                        eprintln!("{error}");
                    }
                }
            })
            .expect("configure GPUI root HWND");
        startup_diagnostics::phase("gpui.root_window.ready");
        if let Some(service) = &running_service {
            #[cfg(windows)]
            cliphost_gpui::hover_card::install(service, card_tokens, cx);
            #[cfg(not(windows))]
            let _ = card_tokens;
            if !options.background {
                service.state().request_show();
            }
            let state = service.state();
            let mut displayed_language = i18n::displayed_language();
            cx.spawn(async move |cx| loop {
                Timer::after(Duration::from_secs(1)).await;
                i18n::set_language(state.ui_language());
                let next_language = i18n::displayed_language();
                if displayed_language != next_language {
                    displayed_language = next_language;
                    if cx.update(|cx| cx.refresh_windows()).is_err() {
                        break;
                    }
                }
            })
            .detach();
        }
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                log::warn!(target: "cliphost_gpui::lifecycle", "Exit requested after the last GPUI window closed");
                cx.quit();
            }
        })
        .detach();
        if options.exit_after_ms.is_some() || options.report.is_some() {
            #[cfg(debug_assertions)]
            let mut qa_report_sequence = 0_u64;
            cx.spawn(async move |cx| loop {
                Timer::after(Duration::from_millis(500)).await;
                #[cfg(debug_assertions)]
                let _ = handle.update(cx, |view, window, cx| {
                    view.poll_content_qa(window, cx);
                });
                let mut report = handle
                    .update(cx, |view, window, cx| view.report(window, cx))
                    .unwrap_or_else(|error| json!({"error":error.to_string()}));
                report["elapsedMs"] = json!(started.elapsed().as_secs_f64() * 1_000.0);
                #[cfg(debug_assertions)]
                if let Some(isolation) = &qa_isolation {
                    qa_report_sequence = qa_report_sequence.saturating_add(1);
                    report["qaIsolation"] = isolation.clone();
                    report["qaIsolation"]["reportSequence"] = json!(qa_report_sequence);
                }
                if let Some(path) = &options.report {
                    if let Err(error) = write_report(path, &report) {
                        eprintln!("QA report failed: {error}");
                    }
                }
                if options
                    .exit_after_ms
                    .is_some_and(|delay| started.elapsed() >= Duration::from_millis(delay))
                {
                    log::warn!(target: "cliphost_gpui::lifecycle", "Exit requested by explicit --exit-after-ms timer");
                    cx.update(|cx| cx.quit()).ok();
                    break;
                }
            })
            .detach();
        }
    });
    startup_diagnostics::phase("native_host.shutdown");
    if let Some(service) = service {
        service.shutdown();
    }
    Ok(())
}

fn main() {
    // The same verified package executable performs the staged replacement in
    // helper mode. Handle this before touching GPUI, product data or instances.
    if let Some(result) = cliphost_updater::maybe_apply_update_from_args() {
        if let Err(error) = result { eprintln!("ClipHost update: {error}"); std::process::exit(1); }
        return;
    }
    startup_diagnostics::initialize();
    if startup_diagnostics::interactive_failures_allowed() {
        if let Err(error) = native_crash::install(startup_diagnostics::active_log_path(), true) {
            startup_diagnostics::warning(
                log::Level::Warn,
                "cliphost_gpui::native_crash",
                &format!("Native exception log channel unavailable: {error}"),
            );
        }
    }
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(log::LevelFilter::Warn);
    if let Err(error) = run() {
        let _ = std::io::Write::write_fmt(
            &mut std::io::stderr(),
            format_args!("ClipHost GPUI: {error}\n"),
        );
        startup_diagnostics::run_failed(&error);
        std::process::exit(1);
    }
    startup_diagnostics::run_completed();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Options {
        Options::parse_from(args.iter().map(|arg| (*arg).to_string()))
            .unwrap()
            .unwrap()
    }

    #[test]
    fn keyboard_qa_requires_native_debug_and_explicit_isolation() {
        let valid = ["--qa-keyboard-input", "--test-instance", "--data-root", "data",
            "--api-port", "29065", "--report", "report.json"];
        let result = Options::parse_from(valid.iter().map(|s| (*s).to_owned()));
        assert_eq!(result.is_ok(), cfg!(debug_assertions));
        for removed in ["--test-instance", "--data-root", "--api-port", "--report"] {
            let mut args = valid.to_vec();
            let index = args.iter().position(|s| *s == removed).unwrap();
            let count = if removed == "--test-instance" { 1 } else { 2 };
            args.drain(index..index + count);
            assert!(Options::parse_from(args.into_iter().map(str::to_owned)).is_err());
        }
        for extra in ["--headless", "--fixture", "--core-copy-acceptance"] {
            let mut args = valid.to_vec(); args.push(extra);
            assert!(Options::parse_from(args.into_iter().map(str::to_owned)).is_err());
        }
        let mut args = valid.to_vec(); args[5] = "28965";
        assert!(Options::parse_from(args.into_iter().map(str::to_owned)).is_err());
    }

    #[cfg(debug_assertions)]
    #[test]
    fn qa_content_import_requires_all_isolation_flags() {
        let accepted = parse(&[
            "--qa-content-fixture",
            "fixture.json",
            "--test-instance",
            "--headless",
            "--data-root",
            "data",
            "--api-port",
            "29076",
            "--report",
            "live.json",
        ]);
        assert!(accepted.qa_content_fixture.is_some());
        for removed in [
            "--test-instance",
            "--headless",
            "--data-root",
            "--report",
            "--api-port",
        ] {
            let mut args = vec![
                "--qa-content-fixture",
                "fixture.json",
                "--test-instance",
                "--headless",
                "--data-root",
                "data",
                "--api-port",
                "29076",
                "--report",
                "live.json",
            ];
            let index = args.iter().position(|arg| *arg == removed).unwrap();
            let count = if matches!(removed, "--data-root" | "--report" | "--api-port") {
                2
            } else {
                1
            };
            args.drain(index..index + count);
            assert!(Options::parse_from(args.into_iter().map(str::to_owned)).is_err());
        }
    }

    #[test]
    fn qa_content_import_rejects_product_ports_and_design_mode() {
        for port in ["0", "28865", "28965", "18765", "18776", "28766", "29766"] {
            assert!(Options::parse_from(
                [
                    "--qa-content-fixture",
                    "fixture.json",
                    "--test-instance",
                    "--headless",
                    "--data-root",
                    "data",
                    "--api-port",
                    port,
                    "--report",
                    "live.json",
                ]
                .into_iter()
                .map(str::to_owned)
            )
            .is_err());
        }
        assert!(Options::parse_from(
            [
                "--qa-content-fixture",
                "fixture.json",
                "--test-instance",
                "--headless",
                "--data-root",
                "data",
                "--api-port",
                "29076",
                "--report",
                "live.json",
                "--fixture",
            ]
            .into_iter()
            .map(str::to_owned)
        )
        .is_err());
    }

    #[test]
    fn core_lifecycle_markers_do_not_replace_product_arguments() {
        let options = parse(&[
            "--cliphost-restart-parent=42:123",
            "--cliphost-restart-environment=e30",
            "--CLIPHOST-DEMOTE-ATTEMPTED",
            "--type",
            "text",
            "--query",
            "中文",
            "--api-port",
            "29965",
        ]);
        assert!(!options.fixture);
        assert_eq!(options.category, "text");
        assert_eq!(options.query, "中文");
        assert_eq!(options.api_port, 29965);
        assert!(options.api_port_given);
    }

    #[test]
    fn core_copy_acceptance_requires_explicit_native_isolation() {
        let valid = ["--core-copy-acceptance", "--test-instance", "--data-root", "copied-root", "--api-port", "29065"];
        assert_eq!(Options::parse_from(valid.into_iter().map(str::to_owned)).is_ok(), cfg!(feature = "native-acceptance"));
        for args in [
            vec!["--core-copy-acceptance"],
            vec!["--core-copy-acceptance", "--test-instance", "--data-root", "copied-root"],
            vec!["--core-copy-acceptance", "--test-instance", "--headless", "--data-root", "copied-root", "--api-port", "29065"],
            vec!["--core-copy-acceptance", "--test-instance", "--fixture", "--data-root", "copied-root", "--api-port", "29065"],
            vec!["--core-copy-acceptance", "--test-instance", "--data-root", "copied-root", "--api-port", "28965"],
        ] {
            assert!(Options::parse_from(args.into_iter().map(str::to_owned)).is_err());
        }
    }

    #[test]
    fn lifecycle_markers_preserve_fixture_and_isolated_flags() {
        assert!(parse(&["--fixture", "--cliphost-demote-attempted"]).fixture);
        let options = parse(&[
            "--headless",
            "--test-instance",
            "--data-root",
            "isolated-root",
            "--cliphost-restart-parent=42:123",
            "--cliphost-restart-environment=e30",
        ]);
        assert!(options.headless && options.test_instance);
        assert_eq!(options.data_root, Some(PathBuf::from("isolated-root")));
    }

    #[test]
    fn parser_defers_the_default_port_until_relaunch_environment_is_restored() {
        let options = parse(&[]);
        assert_eq!(options.api_port, cliphost_core::DEFAULT_API_PORT);
        assert!(!options.api_port_given);
        // A value that resembles a flag remains the search value, not a mode.
        assert!(!parse(&["--query", "--fixture"]).fixture);
    }

    #[test]
    fn background_mode_is_explicit_and_survives_lifecycle_markers() {
        assert!(!parse(&[]).background);
        let options = parse(&[
            cliphost_core::startup::BACKGROUND_ARGUMENT,
            "--cliphost-restart-parent=42:123",
            "--cliphost-demote-attempted",
        ]);
        assert!(options.background);
        assert!(!options.fixture && !options.headless);
        assert!(Options::parse_from(["--BACKGROUND".to_string()]).is_err());
        assert!(!parse(&["--query", "--background"]).background);
    }

    #[test]
    fn unrelated_private_looking_flags_are_still_rejected() {
        assert!(Options::parse_from(["--cliphost-unknown".to_string()]).is_err());
        assert!(Options::parse_from(["--cliphost-restart-parent".to_string()]).is_err());
        assert!(Options::parse_from(["--cliphost-restart-environment".to_string()]).is_err());
    }

    #[test]
    fn language_override_is_confined_to_isolated_design_fixtures() {
        assert_eq!(
            parse(&["--fixture", "--language", "en"]).language,
            Some(LanguageMode::English)
        );
        assert_eq!(
            parse(&["--fixture", "--language", "zh-CN"]).language,
            Some(LanguageMode::Chinese)
        );
        for args in [
            vec!["--language", "en"],
            vec!["--fixture", "--language", "fr"],
        ] {
            assert!(Options::parse_from(args.into_iter().map(str::to_owned)).is_err());
        }
    }
}
