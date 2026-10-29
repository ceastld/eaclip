use cliphost_core::paths::AppPaths;
use cliphost_core::host::ChromeTask;
use cliphost_core::{ChromeCommand, HostState};
use cliphost_http::{event_bus, serve_lan, HttpState};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{LogicalPosition, LogicalSize, Manager, WebviewUrl};
#[cfg(windows)]
mod panel_drop;
#[cfg(windows)]
mod panel_environment;
#[cfg(windows)]
use windows as windows_api;
#[cfg(windows)]
#[path = "../../shared/windows/src/clipboard_dispatch.rs"]
mod clipboard_dispatch;
#[cfg(windows)]
mod tool_window;
#[cfg(windows)]
mod window_effects;
mod window_corner_capture;
#[cfg(windows)]
mod window_shadow_pixels;
#[path = "../../shared/windows/src/hover_popup.rs"]
mod hover_popup;
mod context_menu_popup;
mod window_loading;
mod webview_recovery;
mod import_cli;
use window_loading::WindowLoading;

// Match the HTML boot surface before the renderer can report its theme. Both
// the HWND erase brush and WebView2's unpainted surface need an explicit color.
const BOOT_BACKGROUND: tauri::window::Color = tauri::window::Color(27, 27, 27, 255);

#[derive(Default)]
struct ExitState(AtomicBool);

#[derive(Clone, Default)]
struct MainThreadPoll(Arc<MainThreadPollState>);

#[derive(Default)]
struct MainThreadPollState {
    occupied: AtomicBool,
    retry: AtomicBool,
    wakeup: Arc<tokio::sync::Notify>,
}

struct MainThreadPollTicket(Arc<MainThreadPollState>);

impl Drop for MainThreadPollTicket {
    fn drop(&mut self) {
        self.0.occupied.store(false, Ordering::SeqCst);
        if self.0.retry.swap(false, Ordering::SeqCst) {
            self.0.wakeup.notify_one();
        }
    }
}

impl MainThreadPoll {
    fn new(wakeup: Arc<tokio::sync::Notify>) -> Self {
        Self(Arc::new(MainThreadPollState { wakeup, ..Default::default() }))
    }

    #[cfg(test)]
    fn schedule<E>(
        &self,
        enqueue: impl FnOnce(Box<dyn FnOnce() + Send>) -> Result<(), E>,
        work: impl FnOnce() + Send + 'static,
    ) -> Result<(), E> {
        let Some(ticket) = self.reserve() else { return Ok(()); };
        enqueue(Box::new(move || {
            // Keep the gate held while the UI executes, and release it if the
            // dispatcher rejects/drops the callback or the work unwinds.
            let _ticket = ticket;
            work();
        }))
    }

    /// Hold one permit across background snapshot collection and UI execution.
    /// A slow UI must not produce a new burst snapshot/dispatch every 50ms.
    fn reserve(&self) -> Option<MainThreadPollTicket> {
        if self.0.occupied.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
            self.0.retry.store(true, Ordering::SeqCst);
            // The running ticket may have released just before retry was set.
            // In either ordering one permit survives for the async dispatcher.
            if !self.0.occupied.load(Ordering::SeqCst) && self.0.retry.swap(false, Ordering::SeqCst) {
                self.0.wakeup.notify_one();
            }
            return None;
        }
        Some(MainThreadPollTicket(self.0.clone()))
    }
}

fn request_exit(app: &tauri::AppHandle) {
    if app.state::<ExitState>().0.swap(true, Ordering::SeqCst) { return; }
    let handle = app.clone();
    let host = app.try_state::<HostState>().map(|host| host.inner().clone());
    #[cfg(windows)]
    let clipboard = app.try_state::<std::sync::Arc<clipboard_dispatch::ClipboardDispatcher>>()
        .map(|writer| writer.inner().clone());
    tauri::async_runtime::spawn(async move {
        let work = tauri::async_runtime::spawn_blocking(move || {
            if let Some(host) = host { host.flush_for_shutdown(); }
            #[cfg(windows)]
            if let Some(writer) = clipboard { writer.shutdown(); }
        }).await;
        if let Err(error) = work {
            tracing::error!(%error, "Shutdown flush failed; retaining current process");
            handle.state::<ExitState>().0.store(false, Ordering::SeqCst);
            return;
        }
        handle.exit(0);
    });
}

/// Keep the bounded broadcast stream bounded when the owning UI is busy. One
/// delivery may wait for its UI acknowledgement; it never blocks a Tokio worker.
async fn dispatch_native_push(app: &tauri::AppHandle, work: impl FnOnce() + Send + 'static) {
    let (acknowledge, completed) = tokio::sync::oneshot::channel();
    if let Err(error) = app.run_on_main_thread(move || {
        work();
        let _ = acknowledge.send(());
    }) {
        tracing::debug!(%error, "Native host push dispatch unavailable");
    }
    // A rejected/dropped callback drops its sender and completes this await too.
    let _ = completed.await;
}

/// Await a native result without waiting synchronously for the UI message pump.
async fn dispatch_native_result<T: Send + 'static>(
    enqueue: impl FnOnce(Box<dyn FnOnce() + Send>) -> Result<(), String>,
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let (acknowledge, completed) = tokio::sync::oneshot::channel();
    enqueue(Box::new(move || {
        let _ = acknowledge.send(work());
    }))?;
    completed.await.map_err(|_| "Native UI callback was dropped before completion".to_string())?
}

#[cfg(windows)]
fn low_memory_notification_signaled() -> bool {
    use windows::Win32::{Foundation::{BOOL, CloseHandle}, System::Memory::{
        CreateMemoryResourceNotification, QueryMemoryResourceNotification,
        LowMemoryResourceNotification,
    }};
    unsafe {
        let Ok(notification) = CreateMemoryResourceNotification(LowMemoryResourceNotification)
            else { return false; };
        let mut signaled = BOOL(0);
        let queried = QueryMemoryResourceNotification(notification, &mut signaled);
        let _ = CloseHandle(notification);
        queried.is_ok() && signaled.as_bool()
    }
}

#[tauri::command]
fn set_host_background(window: tauri::WebviewWindow, rgb: [u8; 3]) -> Result<(), String> {
    // Transparent overlay windows must never acquire an opaque backing plate.
    if !matches!(window.label(), "clipboard" | "main" | "panel") {
        return Ok(());
    }
    window
        .set_background_color(Some(tauri::window::Color(rgb[0], rgb[1], rgb[2], 255)))
        .map_err(|err| err.to_string())
}

#[tauri::command]
fn refresh_host_corners(window: tauri::WebviewWindow) -> Result<(), String> {
    if !matches!(window.label(), "clipboard" | "main" | "panel") { return Ok(()); }
    let hwnd = window.hwnd().map_err(|error| error.to_string())?.0 as usize;
    window.run_on_main_thread(move || {
        window_effects::request_corner_refresh(windows::Win32::Foundation::HWND(hwnd as *mut _));
    }).map_err(|error| error.to_string())
}

#[tauri::command]
async fn get_native_titlebar_drag_support(window: tauri::WebviewWindow) -> Result<bool, String> {
    if !matches!(window.label(), "clipboard" | "main" | "panel") { return Ok(false); }
    #[cfg(windows)]
    {
        let (tx, rx) = tokio::sync::oneshot::channel();
        window.with_webview(move |view| {
            let result = (|| unsafe {
                use windows_core::Interface;
                use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Settings9;
                let settings = view.controller().CoreWebView2()?.Settings()?;
                // Wry enables this before the first navigation. Older runtimes
                // without Settings9 retain the existing explicit-drag fallback.
                let Ok(settings) = settings.cast::<ICoreWebView2Settings9>() else { return Ok(false); };
                let mut enabled = windows_core::BOOL::default();
                settings.IsNonClientRegionSupportEnabled(&mut enabled)?;
                windows_core::Result::Ok(enabled.as_bool())
            })().map_err(|error| error.to_string());
            let _ = tx.send(result);
        }).map_err(|error| error.to_string())?;
        rx.await.map_err(|_| "Native titlebar capability query was canceled".to_owned())?
    }
    #[cfg(not(windows))]
    { Ok(false) }
}

#[tauri::command]
async fn begin_host_window_drag(window: tauri::WebviewWindow, host: tauri::State<'_, HostState>,
    client_x: f64, client_y: f64, device_pixel_ratio: f64) -> Result<(), String> {
    if !matches!(window.label(), "clipboard" | "main" | "panel") {
        return Err("This host window does not support caption dragging".into());
    }
    let app = window.app_handle().clone();
    let callback_app = app.clone();
    let host = host.inner().clone();
    dispatch_native_result(
        |callback| app.run_on_main_thread(callback).map_err(|error| error.to_string()),
        move || {
            let Some(current) = callback_app.get_webview_window(window.label()) else {
                return Err("Caption drag owner was destroyed before native dispatch".into());
            };
            #[cfg(windows)]
            {
                // The injected caller identifies the window. A delayed press
                // must not move a replacement with the same window label.
                let hwnd = window.hwnd().map_err(|error| error.to_string())?;
                if current.hwnd().map_err(|error| error.to_string())?.0 != hwnd.0 {
                    return Err("Caption drag owner was replaced before native dispatch".into());
                }
                use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
                if cliphost_core::window_chrome::is_in_move_size(hwnd.0 as isize)
                    || unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) } >= 0 { return Ok(()); }
                let clipboard = window.label() == "clipboard";
                if clipboard { host.arm_caption_drag_interaction(false); }
                let result = cliphost_core::window_chrome::try_begin_caption_drag_at_client(
                    hwnd.0 as isize, client_x, client_y, device_pixel_ratio);
                if clipboard && !matches!(&result, Ok(true)) { host.end_window_move(); }
                result.map(|_| ())
            }
            #[cfg(not(windows))]
            {
                let _ = (current, host, client_x, client_y, device_pixel_ratio);
                window.start_dragging().map_err(|error| error.to_string())
            }
        },
    ).await
}

#[tauri::command]
async fn set_hover_preview(window: tauri::WebviewWindow, host: tauri::State<'_, HostState>,
    popup: tauri::State<'_, hover_popup::Service>, payload: hover_popup::Request) -> Result<(), String> {
    let app = window.app_handle().clone();
    let callback_app = app.clone();
    let (owner, scale) = dispatch_native_result(
        |callback| app.run_on_main_thread(callback).map_err(|error| error.to_string()),
        move || {
            if callback_app.get_webview_window(window.label()).is_none() {
                return Err("Hover owner was destroyed before native dispatch".into());
            }
            // Query the original handle on its owning thread. Do not retarget a
            // delayed request to a replacement window with the same label.
            #[cfg(windows)]
            let owner = window.hwnd().map_err(|error| error.to_string())?.0 as isize;
            #[cfg(not(windows))]
            let owner = 0;
            let scale = window.scale_factor().map_err(|error| error.to_string())?;
            Ok((owner, scale))
        },
    ).await?;
    popup.request(owner, scale, payload, host.inner().clone()).await
}

async fn dispatch_context_menu(
    window: tauri::WebviewWindow, host: HostState,
    action: impl FnOnce(&context_menu_popup::Service, &tauri::AppHandle, &HostState, tauri::WebviewWindow) -> Result<(), String> + Send + 'static,
) -> Result<(), String> {
    let app = window.app_handle().clone();
    let callback_app = app.clone();
    dispatch_native_result(|callback| app.run_on_main_thread(callback).map_err(|error| error.to_string()), move || {
        if callback_app.state::<ExitState>().0.load(Ordering::SeqCst) { return Err("Host is exiting".into()); }
        let service = callback_app.state::<context_menu_popup::Service>();
        action(&service, &callback_app, &host, window)
    }).await
}

#[tauri::command]
async fn context_menu_open(window: tauri::WebviewWindow, host: tauri::State<'_, HostState>, payload: context_menu_popup::OpenRequest) -> Result<(), String> {
    dispatch_context_menu(window, host.inner().clone(), move |service, app, host, source| service.open(app, host, source, payload)).await
}

#[tauri::command]
async fn context_menu_close(window: tauri::WebviewWindow, host: tauri::State<'_, HostState>, session_id: String) -> Result<(), String> {
    dispatch_context_menu(window, host.inner().clone(), move |service, app, host, source| {
        service.close_from(app, host, &source, &session_id); Ok(())
    }).await
}

#[tauri::command]
async fn context_menu_select(window: tauri::WebviewWindow, host: tauri::State<'_, HostState>, session_id: String, key: String) -> Result<(), String> {
    dispatch_context_menu(window, host.inner().clone(), move |service, app, host, popup| service.select(app, host, &popup, &session_id, &key)).await
}

#[tauri::command]
async fn context_menu_resize(window: tauri::WebviewWindow, host: tauri::State<'_, HostState>, session_id: String, width: f64, height: f64) -> Result<(), String> {
    dispatch_context_menu(window, host.inner().clone(), move |service, app, host, popup| {
        let result = service.resize(app, host, &popup, &session_id, width, height);
        if result.is_err() { service.fit_failed(app, host, &popup, &session_id); }
        result
    }).await
}

#[tauri::command]
async fn context_menu_ready(window: tauri::WebviewWindow, host: tauri::State<'_, HostState>) -> Result<(), String> {
    dispatch_context_menu(window, host.inner().clone(), |service, app, host, popup| service.ready(app, host, &popup)).await
}

fn boot_script(api_port: u16, initial_route: &str, window_label: &str) -> String {
    include_str!("boot.js")
        .replace("__CLIPHOST_BOOT_PORT__", &api_port.to_string())
        .replace(
            "__CLIPHOST_BOOT_WINDOW_LABEL__",
            &serde_json::to_string(window_label).expect("serialize host window label"),
        )
        .replace(
            "__CLIPHOST_BOOT_ROUTE__",
            &serde_json::to_string(initial_route).expect("serialize initial SPA route"),
        )
}

fn initial_route_for_label(label: &str) -> &'static str {
    match label {
        "main" => "/main/home",
        "panel" => "/panel",
        "quick-paste" => "/overlay/quick-paste",
        "copy-toast" => "/overlay/copy-toast",
        "super-menu" => "/overlay/super-menu",
        "context-menu" => "/overlay/context-menu",
        _ => "/clipboard",
    }
}

fn read_vite_dev_port_stamp() -> Option<u16> {
    let candidates = [
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../frontend/.intellitools-vite-dev.port"),
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../frontend/.intellitools-vite-dev.port"),
    ];
    for path in candidates {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            if let Ok(port) = trimmed.parse::<u16>() {
                if port > 0 {
                    return Some(port);
                }
            }
            break;
        }
    }
    None
}

fn tcp_localhost_open(port: u16) -> bool {
    use std::net::{Ipv4Addr, SocketAddr, TcpStream};
    use std::time::Duration;
    TcpStream::connect_timeout(
        &SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        Duration::from_millis(250),
    )
    .is_ok()
}

/// Match C# ClipHost DEBUG: load Vite when reachable so `cargo run` is not stuck on stale dist.
fn resolve_frontend_dev_base() -> Option<String> {
    if std::env::var("INTELLITOOLS_FRONTEND_DEV_PROBE")
        .map(|v| v.trim() == "0")
        .unwrap_or(false)
    {
        return None;
    }
    if let Ok(raw) = std::env::var("INTELLITOOLS_FRONTEND_DEV_URL") {
        let trimmed = raw.trim().trim_end_matches('/');
        if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
            return Some(trimmed.to_string());
        }
    }
    if !cfg!(debug_assertions) {
        return None;
    }
    let port = read_vite_dev_port_stamp().unwrap_or(5280);
    if tcp_localhost_open(port) {
        Some(format!("http://127.0.0.1:{port}"))
    } else {
        None
    }
}

/// Config windows use `create: false` so we can inject `boot_script` at document-start.
/// `window.eval` in `on_page_load` is too late: SPA modules pick native WebView2
/// `chrome.webview.postMessage` (nothing answers) and the clipboard list stays 总数: 0.
fn create_configured_window(app: &tauri::AppHandle, host: &HostState, label: &str) -> Result<(), Box<dyn std::error::Error>> {
    let loading = app.state::<WindowLoading>();
    let mut cfg = app.config().app.windows.iter().find(|cfg| cfg.label == label)
        .ok_or_else(|| format!("Missing window configuration: {label}"))?.clone();
    let started = std::time::Instant::now();
    // Window creation pumps native messages. Keep every surface hidden
    // until HWND registration, saved geometry and command runners exist.
    cfg.visible = false;
    if cfg.label == "clipboard" {
        // Start with the restored expanded size so the first geometry snapshot
        // cannot replace it with the default window dimensions.
        (cfg.width, cfg.height) = host.clipboard_window_size(false);
    }
    let route = initial_route_for_label(&cfg.label);
    if let Some(base) = loading.vite_base.as_deref() {
        let url = format!("{base}{route}");
        match url.parse() {
            Ok(parsed) => cfg.url = WebviewUrl::External(parsed),
            Err(err) => tracing::warn!("Vite SPA URL parse failed ({url}): {err}"),
        }
    }
    let script = boot_script(loading.api_port, route, &cfg.label);
    let mut builder = tauri::WebviewWindowBuilder::from_config(app, &cfg)?
        // WebView2 otherwise requests focus while building even a hidden window,
        // before the post-build no-activate guard can be installed.
        .focused(false)
        .skip_taskbar(true)
        .initialization_script(script);
    if !cfg.decorations {
        // The native shadow adds resize insets and a system outline. Root
        // surfaces use our own shadow/corners; transparent overlays draw theirs.
        builder = builder.shadow(false);
    }
    if matches!(cfg.label.as_str(), "clipboard" | "main" | "panel") {
        builder = builder.background_color(BOOT_BACKGROUND)
            .initialization_script(include_str!("background.js"));
    }
    if matches!(cfg.label.as_str(), "clipboard" | "panel") {
        builder = builder.initialization_script(include_str!("overlays.js"));
    }
    if cfg.label == "clipboard" {
        // Startup shows this window before the first ChromeCommand::Show.
        // Apply the same topmost policy before its owned shadows are created.
        builder = builder.always_on_top(true).focusable(!host.clipboard_no_activate());
    }
    if matches!(cfg.label.as_str(), "panel" | "quick-paste" | "copy-toast" | "super-menu") {
        builder = builder.focusable(false);
    }
    if cfg.label == "panel" {
        // Keep Chromium's HTML5 target so ordinary drags can be forwarded unchanged.
        builder = builder.disable_drag_drop_handler();
        #[cfg(windows)]
        match panel_environment::for_panel(app) {
            Ok(environment) => builder = builder.with_environment(environment),
            Err(error) => tracing::warn!(%error, "Panel native action drop unavailable"),
        }
    }
    #[cfg(windows)]
    { builder = builder.initialization_script(include_str!("hover-popup.js")); }
    #[cfg(windows)]
    if matches!(cfg.label.as_str(), "clipboard" | "panel" | "main" | "copy-toast") {
        builder = builder.initialization_script(include_str!("context-menu-popup.js"));
    }
    let window = builder.build()?;
    #[cfg(windows)]
    if !cfg.decorations {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::Graphics::Dwm::{
            DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE,
        };
        if let Ok(hwnd) = window.hwnd() {
            // Suppress the Win11 outline without removing native resize styles.
            // Older Windows versions do not support this attribute.
            let color = DWMWA_COLOR_NONE;
            if let Err(error) = unsafe {
                DwmSetWindowAttribute(HWND(hwnd.0), DWMWA_BORDER_COLOR,
                    &color as *const _ as *const _, std::mem::size_of_val(&color) as u32)
            } {
                tracing::debug!(%error, label, "Native window border suppression unavailable");
            }
        }
    }
    #[cfg(all(windows, feature = "native-acceptance"))]
    if std::env::var("CLIPHOST_NATIVE_CLOAK_WINDOWS").as_deref() == Ok("1") {
        // DWM only permits the owner process to cloak its own window. This
        // fixture mode exercises Show/Hide and movement without desktop UI.
        use windows::Win32::Foundation::{BOOL, HWND};
        use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_CLOAK};
        let enabled = BOOL(1);
        let hwnd = window.hwnd()?;
        unsafe {
            DwmSetWindowAttribute(HWND(hwnd.0), DWMWA_CLOAK,
                &enabled as *const _ as *const _, std::mem::size_of_val(&enabled) as u32)?;
        }
    }
    #[cfg(windows)]
    if cfg.label == "panel" {
        panel_drop::watch(&window, &app.state::<HostState>());
    }
    tracing::info!(label = %cfg.label, elapsed_ms = started.elapsed().as_millis(), "Host WebView created");
    if matches!(cfg.label.as_str(), "clipboard" | "main" | "panel") {
        if let Ok(hwnd) = window.hwnd() {
            cliphost_core::window_chrome::install_html_titlebar_passthrough(hwnd.0 as isize);
            if cfg.label == "clipboard" {
                let drag_host = host.clone();
                cliphost_core::window_chrome::set_caption_drag_observer(hwnd.0 as isize, move |active| {
                    if active { drag_host.arm_caption_drag_interaction(false); }
                    else { drag_host.end_window_move(); }
                });
            }
        }
    }
    #[cfg(windows)]
    if let Ok(hwnd) = window.hwnd() {
        // Install after the caption hook, before geometry restoration or show.
        tool_window::configure(windows::Win32::Foundation::HWND(hwnd.0), label);
        if matches!(label, "clipboard" | "main" | "panel") {
            if let Err(error) = window_effects::install(windows::Win32::Foundation::HWND(hwnd.0)) {
                tracing::warn!(%error, label, "Custom window appearance unavailable");
            }
        }
    }
    if let Ok(hwnd) = window.hwnd() {
        let handle = hwnd.0 as isize;
        match label {
            "clipboard" => host.set_host_hwnd(handle),
            "main" => host.set_main_hwnd(handle),
            "panel" => host.set_panel_hwnd(handle),
            "quick-paste" => host.set_qp_hwnd(handle),
            "super-menu" => host.set_super_menu_hwnd(handle),
            _ => {}
        }
        #[cfg(windows)]
        if let Some(popup) = app.try_state::<hover_popup::Service>() {
            if let Err(error) = popup.bind_owner(handle) {
                tracing::warn!(%error, label, "Hover owner registration failed");
            }
        }
    }
    restore_persisted_window(&window, host);
    if let Err(error) = app.state::<webview_recovery::Service>()
        .watch(&window, host.chrome_command_notifier()) {
        tracing::warn!(%error, label, "WebView failure watcher registration failed");
    }
    Ok(())
}

fn ensure_window_created(app: &tauri::AppHandle, host: &HostState, label: &str) -> Result<(), Box<dyn std::error::Error>> {
    let loading = app.state::<WindowLoading>();
    // A completed construction is not evidence that the native window still
    // exists. Leave in-progress builders alone while they pump messages.
    if loading.is_built(label) && app.get_webview_window(label).is_none() {
        loading.remove(label);
    }
    if loading.begin_create(label) {
        // Never hold the load-state mutex while WebView2 pumps native messages.
        if let Err(error) = create_configured_window(app, host, label) {
            loading.remove(label);
            return Err(error);
        }
        loading.built(label);
    }
    Ok(())
}

fn init_tracing(logs_dir: &std::path::Path) {
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::{fmt, EnvFilter};

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let log_path = cliphost_core::log_file::current_log_file_path(logs_dir);
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(file) => {
            tracing_subscriber::registry()
                .with(filter)
                .with(fmt::layer().with_writer(std::io::stderr))
                .with(fmt::layer().with_ansi(false).with_writer(std::sync::Mutex::new(file)))
                .init();
        }
        Err(err) => {
            tracing_subscriber::fmt().with_env_filter(filter).init();
            tracing::warn!("rolling log file unavailable ({log_path:?}): {err}");
        }
    }
}

fn handle_secondary_launch(host: &HostState, args: &[String]) {
    if !cliphost_core::startup::is_background_launch(args) {
        // Use the normal transition to unfold and restore no-focus handling.
        host.request_show();
    }
}

pub fn run() {
    if let Some(code) = import_cli::run_if_requested() {
        std::process::exit(code);
    }
    let run_started = std::time::Instant::now();
    let background_launch = cliphost_core::startup::is_background_launch(std::env::args().skip(1));
    if let Err(error) = cliphost_core::process_restart::wait_for_predecessor() {
        eprintln!("ClipHost restart aborted: {error}");
        std::process::exit(1);
    }
    if std::env::var("CLIPHOST_TAURI_ALLOW_DIALOGS").is_err() {
        std::env::set_var("CLIPHOST_TAURI_ALLOW_DIALOGS", "1");
    }
    let paths = AppPaths::resolve().expect("resolve Tauri data root");
    #[cfg(feature = "native-acceptance")]
    let fixture_identifier = cliphost_core::native_acceptance::initialize(&paths)
        .expect("validate native acceptance isolation");
    // C# `App.OnStartup` → `EnsureElevatedIfConfigured` before the loopback bind.
    #[cfg(not(feature = "native-acceptance"))]
    cliphost_core::elevation::ensure_elevated_if_configured(&paths.configs.join("GlobalConfig.json"));
    #[allow(unused_mut)]
    let mut context = tauri::generate_context!();
    #[cfg(feature = "native-acceptance")]
    {
        let config = context.config_mut();
        config.identifier = fixture_identifier;
        if std::env::var("CLIPHOST_NATIVE_START_HIDDEN").as_deref() == Ok("1") {
            for window in &mut config.app.windows {
                window.visible = false;
            }
        }
    }

    tauri::Builder::default()
        .manage(ExitState::default())
        .manage(context_menu_popup::Service::default())
        .invoke_handler(tauri::generate_handler![set_host_background, refresh_host_corners, get_native_titlebar_drag_support, begin_host_window_drag, set_hover_preview,
            context_menu_open, context_menu_close, context_menu_select, context_menu_resize, context_menu_ready])
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            if let Some(host) = app.try_state::<HostState>() {
                handle_secondary_launch(&host, &args);
            }
        }))
        .setup(move |app| {
            // Plugin setup runs first. A secondary launch must exit before it
            // opens the database, starts clipboard/hooks or claims RPC ports.
            let setup_started = std::time::Instant::now();
            paths.ensure()?;
            init_tracing(&paths.logs);
            tracing::info!(elapsed_ms = run_started.elapsed().as_millis(), "Host Tauri setup entered");
            let preferred_port = cliphost_core::resolve_listen_port();
            let (port, http_listener) = cliphost_http::bind_loopback_with_fallback(preferred_port)?;
            let host = HostState::open(paths, port).map_err(std::io::Error::other)?;
            if background_launch {
                // Match the native visibility before hooks, tray or RPC can toggle it.
                host.dismiss_native_window("clipboard");
            }
            host.set_window_runtime_ready(false);
            app.manage(host.clone());
            #[cfg(windows)]
            app.manage(hover_popup::Service::new().map_err(std::io::Error::other)?);
            let pipe_started = std::time::Instant::now();
            if let Err(err) = host.start_verb_rpc() {
                tracing::warn!("isolated verb RPC listen skipped: {err}");
            }
            tracing::info!(elapsed_ms = pipe_started.elapsed().as_millis(), "Host verb RPC initialized");
            let http_state = HttpState { host: host.clone(), events: event_bus() };
            let serve_state = http_state.clone();
            // Native windows share one event-bus subscription. Browser clients may
            // still use SSE; preloaded WebViews never exhaust loopback connections.
            let mut pushes = http_state.events.subscribe();
            let push_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    match pushes.recv().await {
                        Ok(body) => {
                            let envelope = serde_json::from_str::<serde_json::Value>(&body).ok();
                            let event_name = envelope.as_ref()
                                .filter(|value| value.get("type").and_then(|v| v.as_str()) == Some("event"))
                                .and_then(|value| value.get("eventName").and_then(|v| v.as_str()));
                            // Overlay pages consume their legacy state callback,
                            // not generic bridge events. Startup bursts alone
                            // leave later selection/armed-action changes stale.
                            if event_name == Some("quickPasteState") {
                                if let Some(envelope) = envelope.as_ref() {
                                    if let Some(detail) = envelope.get("data").cloned() {
                                        let handle = push_handle.clone();
                                        dispatch_native_push(&push_handle, move || {
                                            if let Some(window) = handle.get_webview_window("quick-paste") {
                                                inject_quick_paste(&window, &detail);
                                            }
                                        }).await;
                                    }
                                    continue;
                                }
                            }
                            let script = format!(
                                "if(window.__clipHostDeliverInbound){{window.__clipHostDeliverInbound({body});}}else{{(window.__clipHostPushQueue=window.__clipHostPushQueue||[]).push({body});}}"
                            );
                            let appearance = event_name == Some("clipConfigChanged");
                            let handle = push_handle.clone();
                            dispatch_native_push(&push_handle, move || {
                                for window in handle.webview_windows().values() {
                                    // Toast content has its own bounded delivery queue.
                                    // Unrelated history/menu updates need not wake it.
                                    if window.label() == "copy-toast" && !appearance {
                                        continue;
                                    }
                                    let _ = window.eval(&script);
                                }
                            }).await;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                            tracing::warn!("native host push receiver lagged by {count} events");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
            tauri::async_runtime::spawn(async move {
                if let Err(err) = cliphost_http::serve_listener(serve_state, http_listener).await {
                    tracing::error!("loopback HTTP failed: {err}");
                }
            });
            let lan_state = http_state.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(err) = serve_lan(lan_state).await {
                    tracing::warn!("ClipLink LAN listener failed: {err}");
                }
            });
            let vite_base = resolve_frontend_dev_base();
            if let Some(base) = vite_base.as_deref() {
                tracing::info!("ClipHost Tauri loading Vite SPA at {base}");
            }
            app.manage(WindowLoading::new(port, vite_base));
            app.manage(webview_recovery::Service::default());
            ensure_window_created(app.handle(), &host, "clipboard")?;
            {
                let handle = app.handle().clone();
                host.set_ole_drag_ui_runner(move |payload| {
                    let (tx, rx) = std::sync::mpsc::sync_channel(1);
                    handle
                        .run_on_main_thread(move || {
                            let result = cliphost_core::ole_drag::begin_ole_drag(&payload);
                            let _ = tx.send(result);
                        })
                        .map_err(|err| err.to_string())?;
                    rx.recv().unwrap_or_else(|err| Err(err.to_string()))
                });
            }
            #[cfg(windows)]
            {
                let writer = std::sync::Arc::new(
                    clipboard_dispatch::ClipboardDispatcher::start().map_err(std::io::Error::other)?
                );
                let clipboard_writer = writer.clone();
                host.set_clipboard_ui_runner(move |job| clipboard_writer.run(job));
                app.manage(writer);
            }
            #[cfg(not(windows))]
            {
                let handle = app.handle().clone();
                host.set_clipboard_ui_runner(move |job| {
                    let (tx, rx) = std::sync::mpsc::sync_channel(1);
                    handle
                        .run_on_main_thread(move || {
                            let _ = tx.send(job());
                        })
                        .map_err(|err| err.to_string())?;
                    rx.recv().unwrap_or_else(|err| Err(err.to_string()))
                });
            }
            install_tray(app, &host)?;
            // Input/clipboard waits must not delay hide or toast timers. Await
            // each keyboard worker so queued chords retain their original order.
            let keyboard_host = host.clone();
            let keyboard_wakeup = keyboard_host.keyboard_work_notifier();
            tauri::async_runtime::spawn(async move {
                // Keep modifier-release/timeout backup polling, but queued
                // shortcuts and wheel moves do not wait for the next tick.
                let period = std::time::Duration::from_millis(50);
                let mut maintenance = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
                maintenance.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tokio::select! {
                        _ = keyboard_wakeup.notified() => {},
                        _ = maintenance.tick() => {},
                    }
                    let worker_host = keyboard_host.clone();
                    if let Err(error) = tauri::async_runtime::spawn_blocking(move || {
                        // The predicate also takes HostInner. Never park a Tokio
                        // worker on it while memory pressure slows host work.
                        if worker_host.needs_keyboard_work_poll() {
                            worker_host.poll_keyboard_work();
                        }
                    }).await {
                        tracing::error!(%error, "Host keyboard polling failed");
                    }
                }
            });
            let policy_host = host.clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    let worker_host = policy_host.clone();
                    if let Err(error) = tauri::async_runtime::spawn_blocking(move || {
                        if worker_host.needs_fast_hide_poll() {
                            worker_host.poll_hide();
                        }
                    }).await {
                        tracing::error!(%error, "Host policy polling failed");
                    }
                }
            });
            let hide_host = host.clone();
            let handle = app.handle().clone();
            let flush_state = http_state.clone();
            let chrome_wakeup = hide_host.chrome_command_notifier();
            let main_thread_poll = MainThreadPoll::new(chrome_wakeup.clone());
            tauri::async_runtime::spawn(async move {
                let period = std::time::Duration::from_millis(50);
                let mut maintenance = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
                maintenance.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    let maintenance_tick = tokio::select! {
                        _ = chrome_wakeup.notified() => false,
                        _ = maintenance.tick() => true,
                    };
                    let Some(ticket) = main_thread_poll.reserve() else { continue; };
                    let (main_update, qp_burst) = if maintenance_tick {
                        let worker_host = hide_host.clone();
                        let worker_state = flush_state.clone();
                        match tauri::async_runtime::spawn_blocking(move || {
                            let main = worker_host.take_main_host_connection_update();
                            let qp = worker_host.take_qp_state_burst();
                            cliphost_http::flush_host_events(&worker_state);
                            (main, qp)
                        }).await {
                            Ok(snapshot) => snapshot,
                            Err(error) => {
                                tracing::error!(%error, "Host window maintenance snapshot failed");
                                continue;
                            }
                        }
                    } else { (None, None) };
                    let command_app = handle.clone();
                    let command_host = hide_host.clone();
                    let _ = handle.run_on_main_thread(move || {
                        let _ticket = ticket;
                        recover_failed_webviews(&command_app, &command_host);
                        if let Some(no_activate) = command_host.take_clipboard_no_activate_update() {
                            if let Some(window) = command_app.get_webview_window("clipboard") {
                                let _ = window.set_focusable(!no_activate);
                            }
                        }
                        // Leave commands in HostState until the UI can execute them,
                        // so hide/reopen can still cancel pending input and lifecycle work.
                        while let Some(cmd) = command_host.take_chrome_task() {
                            apply_chrome(&command_app, &command_host, cmd);
                        }
                        command_app.state::<context_menu_popup::Service>().validate_owner(&command_app, &command_host);
                        let ready = command_app.state::<WindowLoading>().take_ready();
                        for task in ready {
                            apply_chrome(&command_app, &command_host, task);
                        }
                        if let Some(detail) = main_update {
                            push_host_window_state(&command_app, "main", &detail);
                        }
                        if let Some(detail) = qp_burst {
                            if let Some(window) = command_app.get_webview_window("quick-paste") {
                                inject_quick_paste(&window, &detail);
                                if let Ok(hwnd) = window.hwnd() {
                                    let _ = cliphost_core::window_menu::set_overlay_pass_through(
                                        hwnd.0 as isize,
                                        command_host.qp_overlay_pass_through(),
                                    );
                                }
                            }
                        }
                    });
                }
            });
            let maintenance_host = host.clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    let worker_host = maintenance_host.clone();
                    // Database waits must not delay the window command loop.
                    // Await this worker so maintenance jobs never overlap.
                    if let Err(error) = tauri::async_runtime::spawn_blocking(move || {
                        worker_host.commit_pending_clear();
                        worker_host.poll_history_cleanup();
                    }).await {
                        tracing::error!(%error, "History maintenance worker failed");
                    }
                }
            });
            let window_state_host = host.clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                    let worker_host = window_state_host.clone();
                    if let Err(error) = tauri::async_runtime::spawn_blocking(move || {
                        worker_host.poll_window_state_persistence();
                    }).await {
                        tracing::warn!(%error, "Window state persistence worker failed");
                    }
                }
            });
            tracing::info!(elapsed_ms = setup_started.elapsed().as_millis(), "Host window command processing ready");
            host.set_window_runtime_ready(true);
            for cfg in &app.config().app.windows {
                if cfg.visible && !background_launch {
                    if let Some(window) = app.get_webview_window(&cfg.label) {
                        window.show()?;
                        if cfg.label == "clipboard" && host.should_activate_on_show() {
                            window.set_focus()?;
                        }
                    }
                }
            }
            Ok(())
        })
        .on_page_load(move |webview, payload| {
            if payload.event() == tauri::webview::PageLoadEvent::Started {
                if let Some(host) = webview.app_handle().try_state::<HostState>() {
                    webview.app_handle().state::<context_menu_popup::Service>()
                        .document_started(webview.app_handle(), &host, webview.label());
                }
            }
            if let Some(loading) = webview.app_handle().try_state::<WindowLoading>() {
                loading.page_ready(webview.label(), payload.event() == tauri::webview::PageLoadEvent::Finished);
            }
            if payload.event() == tauri::webview::PageLoadEvent::Finished {
                tracing::info!(label = %webview.label(), "Host page loaded");
                if webview.label() == "clipboard" {
                    let handle = webview.app_handle().clone();
                    let preload_handle = handle.clone();
                    // Queue after the initial page is loaded: the primary view
                    // stays responsive at startup and WebView2 construction
                    // finishes before the first ordinary shortcut invocation.
                    tauri::async_runtime::spawn(async move {
                        // Let the first clipboard frame paint before building
                        // another WebView, even if page-load finished inside
                        // WebView2's construction message pump.
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        let _ = handle.run_on_main_thread(move || {
                            if preload_handle.state::<ExitState>().0.load(Ordering::SeqCst) { return; }
                            if let Some(host) = preload_handle.try_state::<HostState>() {
                                if host.quick_paste_enabled() {
                                    #[cfg(windows)]
                                    if low_memory_notification_signaled() {
                                        tracing::info!("Quick paste preload skipped during system memory pressure");
                                        return;
                                    }
                                    if let Err(error) = ensure_window_created(&preload_handle, &host, "quick-paste") {
                                        // Preloading is opportunistic. A user-triggered
                                        // show still retries and reports actual errors.
                                        tracing::warn!(%error, "Quick paste preload unavailable");
                                    }
                                }
                            }
                        });
                    });
                    #[cfg(windows)]
                    if webview.app_handle().state::<context_menu_popup::Service>().schedule_preload() {
                        let handle = webview.app_handle().clone();
                        // Give the primary page and the earlier quick-paste warmup
                        // their first frame before constructing another renderer.
                        tauri::async_runtime::spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_millis(750)).await;
                            let preload_handle = handle.clone();
                            if let Err(error) = handle.run_on_main_thread(move || {
                                if preload_handle.state::<ExitState>().0.load(Ordering::SeqCst) { return; }
                                let loading = preload_handle.state::<WindowLoading>();
                                let host = preload_handle.state::<HostState>();
                                if !loading.is_built("clipboard") || !host.hide_snapshot().web_view_ready {
                                    tracing::info!("Native context menu preload skipped while primary page is loading");
                                    return;
                                }
                                if low_memory_notification_signaled() {
                                    tracing::info!("Native context menu preload skipped during system memory pressure");
                                    return;
                                }
                                if let Err(error) = preload_handle.state::<context_menu_popup::Service>().preload(&preload_handle) {
                                    tracing::warn!(%error, "Native context menu preload unavailable");
                                }
                            }) {
                                tracing::warn!(%error, "Native context menu preload dispatch unavailable");
                            }
                        });
                    }
                }
            }
            if let Some(host) = webview.app_handle().try_state::<HostState>() {
                update_page_input_readiness(&host, webview.label(), payload.event());
                #[cfg(windows)]
                if webview.label() == "panel" && payload.event() == tauri::webview::PageLoadEvent::Finished {
                    if let Some(window) = webview.app_handle().get_webview_window("panel") {
                        panel_drop::refresh(&window, &host);
                    }
                }
            }
            if matches!(webview.label(), "clipboard" | "main" | "panel") {
                if let Ok(hwnd) = webview.window().hwnd() {
                    cliphost_core::window_chrome::install_html_titlebar_passthrough(hwnd.0 as isize);
                }
            }
            if payload.event() == tauri::webview::PageLoadEvent::Finished {
                if let Some(host) = webview.app_handle().try_state::<HostState>() {
                    let detail = match webview.label() {
                        "main" => host.main_window_state_dto(),
                        "panel" => host.panel_window_state_dto(),
                        _ => host.window_state_dto(),
                    };
                    push_host_window_state(webview.app_handle(), webview.label(), &detail);
                }
            }
        })
        .on_window_event(|window, event| {
            if matches!(event, tauri::WindowEvent::Destroyed) {
                if let Some(host) = window.try_state::<HostState>() {
                    if matches!(window.label(), "clipboard" | "main" | "panel") {
                        host.retire_window_presentation(window.label());
                    }
                    let service = window.state::<context_menu_popup::Service>();
                    if window.label() == context_menu_popup::LABEL { service.renderer_destroyed(window.app_handle(), &host); }
                    else { service.close_owner(window.app_handle(), &host, window.label()); }
                }
                if let Some(loading) = window.try_state::<WindowLoading>() {
                    loading.remove(window.label());
                }
            }
            #[cfg(windows)]
            if window.label() == "panel" && matches!(event, tauri::WindowEvent::Destroyed) {
                panel_drop::clear();
            }
            if matches!(
                event,
                tauri::WindowEvent::Moved(_) | tauri::WindowEvent::Resized(_)
            ) {
                if let Some(host) = window.try_state::<HostState>() {
                    window.state::<context_menu_popup::Service>().close_owner(window.app_handle(), &host, window.label());
                    hide_hover_popup(window.app_handle(), window.label());
                    // Builder and saved-state restore can dispatch geometry
                    // messages before startup properties have been applied.
                    if window.try_state::<WindowLoading>().is_some_and(|loading| loading.is_built(window.label())) {
                        persist_window_rect(window, &host);
                    }
                }
            }
            if let tauri::WindowEvent::Focused(focused) = event {
                if window.label() == "clipboard" {
                    if let Some(host) = window.try_state::<HostState>() {
                        if *focused {
                            host.notify_activated();
                        } else {
                            host.notify_deactivated();
                        }
                    }
                }
            }
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                close_context_menu_for_owner(window.app_handle(), window.label());
                hide_hover_popup(window.app_handle(), window.label());
                match window.label() {
                    "clipboard" | "quick-paste" | "copy-toast" | "super-menu" | "panel" => {
                        api.prevent_close();
                        if let Some(host) = window.try_state::<HostState>() {
                            host.dismiss_native_window(window.label());
                        }
                        let _ = window.hide();
                    }
                    "main" => {
                        api.prevent_close();
                        let exit = window
                            .try_state::<HostState>()
                            .is_some_and(|host| host.close_behavior() == "ExitApplication");
                        if exit {
                            request_exit(window.app_handle());
                        } else {
                            if let Some(host) = window.try_state::<HostState>() {
                                host.dismiss_native_window(window.label());
                            }
                            let _ = window.hide();
                        }
                    }
                    _ => {}
                }
            }
        })
        .build(context)
        .expect("build ClipHost Tauri")
        .run(|app, event| {
            // Rebuilding a failed browser can temporarily remove every window.
            // A tray host exits only through its explicit shutdown path.
            if let tauri::RunEvent::ExitRequested { api, .. } = &event {
                if !app.state::<ExitState>().0.load(Ordering::SeqCst) {
                    api.prevent_exit();
                }
            }
            #[cfg(windows)]
            if matches!(event, tauri::RunEvent::Exit) {
                if let Some(writer) = app.try_state::<std::sync::Arc<clipboard_dispatch::ClipboardDispatcher>>() {
                    writer.shutdown();
                }
                panel_drop::shutdown();
                if let Some(popup) = app.try_state::<hover_popup::Service>() { popup.shutdown(); }
                tracing::info!(process_id = std::process::id(), "Host native exit cleanup complete");
            }
        });
}

fn persist_window_rect(window: &tauri::Window, host: &HostState) {
    let Ok(pos) = window.outer_position() else {
        return;
    };
    // `set_size` restores the client size. Even undecorated Windows windows
    // have invisible shadow/resize margins: saving outer_size here adds those
    // margins again on every fold/unfold (and main-window restore).
    let Ok(size) = window.inner_size() else {
        return;
    };
    let scale = window.scale_factor().unwrap_or(1.0).max(0.1);
    host.remember_physical_host_window_rect(
        window.label(),
        pos.x,
        pos.y,
        size.width,
        size.height,
        scale,
    );
}

fn restore_persisted_window(window: &tauri::WebviewWindow, host: &HostState) {
    if !matches!(window.label(), "clipboard" | "panel" | "main") { return; }
    let scale = window.scale_factor().unwrap_or(1.0).max(0.1);
    let client_size = window.inner_size().ok().map(|size| {
        (f64::from(size.width) / scale, f64::from(size.height) / scale)
    });
    if let Some((pos, size)) = host.restored_host_window_rect_with_client_size(window.label(), client_size) {
        if let Some(pos) = pos {
            use cliphost_core::window_state::WindowPosition;
            match pos {
                WindowPosition::Logical(x, y) => {
                    let _ = window.set_position(LogicalPosition::new(x as f64, y as f64));
                }
                WindowPosition::Physical(x, y) => {
                    let _ = window.set_position(tauri::PhysicalPosition::new(x, y));
                }
            }
        }
        if let Some((w, h)) = size {
            let _ = window.set_size(LogicalSize::new(w as f64, h as f64));
        }
    }
    // Property-only changes need a current snapshot even on the first run,
    // before the user has moved or resized this native window.
    persist_window_rect(&window.as_ref().window(), host);
}

fn install_tray(app: &mut tauri::App, host: &HostState) -> Result<(), Box<dyn std::error::Error>> {
    let show = MenuItem::with_id(app, "show", "打开主窗口", true, None::<&str>)?;
    let clipboard = MenuItem::with_id(app, "clipboard", "剪贴板", true, None::<&str>)?;
    let favorites = MenuItem::with_id(app, "favorites", "收藏", true, None::<&str>)?;
    let settings = MenuItem::with_id(app, "settings", "设置", true, None::<&str>)?;
    let help = MenuItem::with_id(app, "help", "帮助", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &clipboard, &favorites, &settings, &help, &quit])?;
    // A tray does not inherit the executable/window icon automatically.
    let icon = app.default_window_icon().cloned().ok_or("Missing embedded ClipHost icon")?;
    let tray = TrayIconBuilder::with_id("main")
        .icon(icon)
        .menu(&menu)
        .show_menu_on_left_click(false)
        .tooltip("ClipHost Tauri")
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => navigate(app, "/main/home"),
            "clipboard" => navigate(app, "/clipboard"),
            "favorites" => navigate(app, "/favorites"),
            "settings" => navigate(app, "/main/settings"),
            "help" => navigate(app, "/main/docs"),
            "quit" => {
                request_exit(app);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let tauri::tray::TrayIconEvent::Click {
                button: tauri::tray::MouseButton::Left,
                button_state: tauri::tray::MouseButtonState::Up,
                ..
            } = event
            {
                navigate(tray.app_handle(), "/clipboard");
            }
        })
        .build(app)?;
    if !host.show_tray_icon() {
        let _ = tray.set_visible(false);
    }
    Ok(())
}

fn spa_assign_script(route: &str) -> String {
    format!(
        "try{{(window.__clipHostAssignRoute||function(r){{if(typeof window.__clipHostNavigate===\"function\"){{window.__clipHostNavigate(r);return;}}var dest=new URL(r,location.href);if(dest.origin!==location.origin)return;var next=dest.pathname+dest.search+dest.hash;if(location.pathname+location.search+location.hash===next)return;location.assign(next);}})({route:?})}}catch(e){{}}"
    )
}

fn navigate(app: &tauri::AppHandle, route: &str) {
    if let Some(host) = app.try_state::<HostState>() {
        host.show_web_route(route);
    }
}

fn sync_tray_visible(app: &tauri::AppHandle, host: &HostState) {
    if let Some(tray) = app.tray_by_id("main") {
        let _ = tray.set_visible(host.show_tray_icon());
    }
}

fn show_labeled_window(app: &tauri::AppHandle, host: &HostState, label: &str, activate: bool, task: ChromeTask) {
    if let Some(window) = app.get_webview_window(label) {
        if let Some(route) = host.take_pending_nav(label) {
            let _ = window.eval(&spa_assign_script(&route));
        }
        log_presentation_result(label, "show", window.show());
        let explicit = host.chrome_task_explicit_presentation(task);
        if activate && explicit {
            log_presentation_result(label, "focus", window.set_focus());
        }
        #[cfg(windows)]
        if explicit {
            match window.hwnd() {
                Ok(hwnd) => tool_window::present_explicit(windows::Win32::Foundation::HWND(hwnd.0), label,
                    tool_window::ShowLayer::Normal, activate),
                Err(_) => tracing::warn!(label, stage = "explicit-show-hwnd", error = true, "Host window presentation"),
            }
        }
    }
}

fn log_presentation_result(label: &str, stage: &'static str, result: tauri::Result<()>) {
    // Avoid provider error text; presentation diagnostics contain no page data.
    if result.is_err() {
        tracing::warn!(label, stage, error = true, "Host window presentation");
    }
}

fn hide_labeled_window(app: &tauri::AppHandle, label: &str) {
    close_context_menu_for_owner(app, label);
    hide_hover_popup(app, label);
    if let Some(window) = app.get_webview_window(label) {
        let _ = window.hide();
    }
}

fn hide_hover_popup(app: &tauri::AppHandle, label: &str) {
    #[cfg(windows)]
    if let (Some(popup), Some(window)) = (app.try_state::<hover_popup::Service>(), app.get_webview_window(label)) {
        if let Ok(hwnd) = window.hwnd() { popup.hide_owner(hwnd.0 as isize); }
    }
}

fn hide_clipboard_window(app: &tauri::AppHandle, host: &HostState) {
    close_context_menu_for_owner(app, "clipboard");
    hide_hover_popup(app, "clipboard");
    if let Some(window) = app.get_webview_window("clipboard") {
        if !host.should_activate_on_show() {
            // Retained DOM input focus fires focusin again on the next show and
            // would immediately request an editable handoff in a fresh no-focus session.
            let _ = window.eval("try{const el=document.activeElement;if(el&&typeof el.blur==='function')el.blur();}catch(e){}");
        }
        let _ = window.hide();
    }
}

fn apply_overlay_window(
    window: &tauri::WebviewWindow,
    place: Option<(i32, i32, i32, i32)>,
    pass_through: bool,
    on_hwnd: impl FnOnce(isize),
) {
    if let Some((x, y, w, h)) = place {
        let position = tauri::PhysicalPosition::new(x, y);
        if window.outer_position().ok() != Some(position) {
            let _ = window.set_position(position);
        }
        let size = tauri::PhysicalSize::new(w as u32, h as u32);
        if window.inner_size().ok() != Some(size) {
            let _ = window.set_size(size);
        }
    }
    if let Ok(hwnd) = window.hwnd() {
        let handle = hwnd.0 as isize;
        on_hwnd(handle);
        let _ = cliphost_core::window_menu::apply_overlay_no_activate(handle);
        let _ = cliphost_core::window_menu::set_overlay_pass_through(handle, pass_through);
    }
}

fn reveal_overlay_window(
    window: &tauri::WebviewWindow,
    place: Option<(i32, i32, i32, i32)>,
    pass_through: bool,
    on_hwnd: impl FnOnce(isize),
) {
    // Native no-activate placement can show the HWND before Tao records it.
    let was_visible = window.is_visible().unwrap_or(false);
    apply_overlay_window(window, place, pass_through, on_hwnd);
    if !was_visible {
        let _ = window.show();
    }
    if let Ok(hwnd) = window.hwnd() {
        let handle = hwnd.0 as isize;
        let _ = cliphost_core::window_menu::apply_overlay_no_activate(handle);
        let _ = cliphost_core::window_menu::set_overlay_pass_through(handle, pass_through);
    }
}

fn hide_overlay_window(window: &tauri::WebviewWindow) {
    close_context_menu_for_owner(window.app_handle(), window.label());
    if let Ok(hwnd) = window.hwnd() {
        let handle = hwnd.0 as isize;
        let (x, y, w, h) = cliphost_core::window_menu::overlay_warmup_bounds(handle);
        let _ = window.set_position(tauri::PhysicalPosition::new(x, y));
        let _ = window.set_size(tauri::PhysicalSize::new(w, h));
        let _ = cliphost_core::window_menu::apply_overlay_hidden(handle);
    }
}

fn close_context_menu_for_owner(app: &tauri::AppHandle, label: &str) {
    if let (Some(service), Some(host)) = (app.try_state::<context_menu_popup::Service>(), app.try_state::<HostState>()) {
        service.close_owner(app, &host, label);
    }
}

/// C# `DispatchHandleEscapeToWebViewAsync`: SPA `handleEscape` false → `Hide()`.
fn clipboard_handle_escape_script(token: &str, input_token: &str) -> String {
    include_str!("escape.js")
        .replace("__CLIPHOST_ESCAPE_TOKEN__", &serde_json::to_string(token).unwrap())
        .replace("__CLIPHOST_ESCAPE_INPUT_TOKEN__", &serde_json::to_string(input_token).unwrap())
}

fn eval_labeled(app: &tauri::AppHandle, label: &str, script: &str) {
    if let Some(window) = app.get_webview_window(label) {
        let _ = window.eval(script);
    }
}

fn open_path_with_shell(path: &str) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", path])
            .spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    }
}

fn animate_clipboard_edge(
    app: &tauri::AppHandle,
    host: &HostState,
    window: &tauri::WebviewWindow,
    target: (f64, f64),
    tucked: bool,
) {
    let Some(generation) = host.begin_clipboard_animation(tucked) else {
        return;
    };
    let from = window.outer_position().ok()
        .map(|pos| (pos.x as f64, pos.y as f64))
        .unwrap_or(target);
    let frames = cliphost_core::edge_hide::animation_frames(from, target);
    let last = frames.len() - 1;
    let step = std::time::Duration::from_millis(
        cliphost_core::edge_hide::ANIMATION_MS
            / cliphost_core::edge_hide::ANIMATION_STEPS.max(1) as u64,
    );
    let app = app.clone();
    let host = host.clone();
    let window = window.clone();
    tauri::async_runtime::spawn(async move {
        for (index, (x, y)) in frames.into_iter().enumerate() {
            let frame_host = host.clone();
            let frame_window = window.clone();
            let current = dispatch_native_result(
                |callback| app.run_on_main_thread(callback).map_err(|error| error.to_string()),
                move || {
                    if !frame_host.clipboard_animation_is_current(generation, tucked) {
                        return Ok(false);
                    }
                    // Keep the shadow attached during motion. Only the last valid
                    // tuck frame hides it; canceled frames cannot change its state.
                    if index == 0 {
                        clear_window_shadow_tuck(&frame_window);
                    }
                    let moved = frame_window.set_position(
                        tauri::PhysicalPosition::new(x as i32, y as i32),
                    ).is_ok();
                    #[cfg(windows)]
                    if moved && tucked && index == last {
                        if let Ok(hwnd) = frame_window.hwnd() {
                            window_effects::set_tucked(windows::Win32::Foundation::HWND(hwnd.0), true);
                        }
                    }
                    Ok(moved)
                },
            ).await;
            if !matches!(current, Ok(true)) {
                break;
            }
            // Move the existing HWND without showing or activating it. Ordinary
            // Show/Hide remains immediate and invalidates queued edge frames.
            if index != last {
                tokio::time::sleep(step).await;
            }
        }
    });
}

fn place_clipboard_window_immediately(
    host: &HostState,
    window: &tauri::WebviewWindow,
    target: (f64, f64),
    tucked: bool,
) {
    let Some(generation) = host.begin_clipboard_animation(tucked) else {
        return;
    };
    // Explicit Show restores placement immediately; hover-driven RestoreEdge
    // and EdgeHide use their own slide animation.
    if host.clipboard_animation_is_current(generation, tucked) {
        #[cfg(windows)]
        if tucked {
            if let Ok(hwnd) = window.hwnd() {
                window_effects::set_tucked(windows::Win32::Foundation::HWND(hwnd.0), true);
            }
        }
        let _ = window.set_position(tauri::PhysicalPosition::new(target.0 as i32, target.1 as i32));
        #[cfg(windows)]
        if !tucked {
            if let Ok(hwnd) = window.hwnd() {
                window_effects::set_tucked(windows::Win32::Foundation::HWND(hwnd.0), false);
            }
        }
    }
}

fn clear_window_shadow_tuck(window: &tauri::WebviewWindow) {
    #[cfg(windows)]
    if let Ok(hwnd) = window.hwnd() {
        window_effects::set_tucked(windows::Win32::Foundation::HWND(hwnd.0), false);
    }
    #[cfg(not(windows))]
    let _ = window;
}

fn update_page_input_readiness(host: &HostState, label: &str, event: tauri::webview::PageLoadEvent) {
    let ready = event == tauri::webview::PageLoadEvent::Finished;
    match label {
        "clipboard" => host.set_clipboard_web_view_ready(ready),
        "panel" => host.set_panel_web_view_ready(ready),
        _ => {}
    }
}

/// WebView2 failure callbacks enqueue recovery. Release the old controller and
/// rebuild/reload only here, after its COM callback has returned to the UI loop.
fn recover_failed_webviews(app: &tauri::AppHandle, host: &HostState) {
    let service = app.state::<webview_recovery::Service>();
    for (label, task) in app.state::<WindowLoading>().take_stalled_shows() {
        if host.prepare_chrome_task(task) && service.recover_stalled_show(&label) {
            tracing::warn!(%label, "Requested WebView load stalled; rebuilding its controller");
        }
    }
    for request in service.take_pending() {
        if !service.is_current(&request) { continue; }
        let label = request.label.as_str();
        host.begin_webview_recovery(label);
        let loading = app.state::<WindowLoading>();
        loading.page_ready(label, false);
        let result = match request.action {
            webview_recovery::Action::Reload => match app.get_webview_window(label) {
                Some(window) => reload_failed_webview(&window),
                None => ensure_window_created(app, host, label).map_err(|error| error.to_string()),
            },
            webview_recovery::Action::Recreate => {
                if let Some(window) = app.get_webview_window(label) {
                    hide_hover_popup(app, label);
                    match label {
                        "clipboard" => host.set_host_hwnd(0),
                        "panel" => host.set_panel_hwnd(0),
                        "main" => host.set_main_hwnd(0),
                        "quick-paste" => host.set_qp_hwnd(0),
                        "super-menu" => host.set_super_menu_hwnd(0),
                        _ => {}
                    }
                    // destroy is an event-loop message, not a synchronous close.
                    // Wait for Destroyed before reusing the same Tauri label.
                    let _ = window.hide();
                    if let Err(error) = window.destroy() {
                        tracing::warn!(%error, label, "Failed WebView window destruction failed");
                    }
                    service.failed(&request);
                    continue;
                }
                loading.remove(label);
                ensure_window_created(app, host, label).map_err(|error| error.to_string())
            }
        };
        match result {
            Ok(()) => {
                service.succeeded(&request);
                host.retry_webview_show(label);
                tracing::info!(label, action = ?request.action, "WebView recovery started");
            }
            Err(error) => {
                service.failed(&request);
                tracing::warn!(%error, label, action = ?request.action, "WebView recovery will retry");
            }
        }
    }
}

fn reload_failed_webview(window: &tauri::WebviewWindow) -> Result<(), String> {
    #[cfg(windows)]
    {
        // Tauri reload() acknowledges dispatch rather than the underlying HRESULT.
        // with_webview executes inline on this owning thread; never block it.
        let (tx, rx) = std::sync::mpsc::channel();
        window.with_webview(move |view| {
            let result = unsafe { view.controller().CoreWebView2().and_then(|core| core.Reload()) }
                .map_err(|error| error.to_string());
            let _ = tx.send(result);
        }).map_err(|error| error.to_string())?;
        rx.try_recv().map_err(|_| "WebView reload must run on its UI thread".to_owned())?
    }
    #[cfg(not(windows))]
    { window.reload().map_err(|error| error.to_string()) }
}

fn apply_chrome(app: &tauri::AppHandle, host: &HostState, task: ChromeTask) {
    // Recheck ownership and current intent on the owning UI thread.
    let cmd = task.command();
    if !host.prepare_chrome_task(task) {
        return;
    }
    let lazy_label = match cmd {
        ChromeCommand::Show => Some("clipboard"),
        ChromeCommand::ShowMain => Some("main"),
        ChromeCommand::ShowPanel => Some("panel"),
        ChromeCommand::ShowQuickPaste => Some("quick-paste"),
        ChromeCommand::ShowToast => Some("copy-toast"),
        ChromeCommand::ShowSuperMenu => Some("super-menu"),
        _ => None,
    };
    if let Some(label) = lazy_label {
        let recovery = app.state::<webview_recovery::Service>();
        recovery.recover_on_show(label);
        if app.state::<WindowLoading>().stalled_for_show(label)
            && recovery.recover_stalled_show(label) {
            tracing::warn!(label, "Explicit Show found a stalled WebView load; rebuilding its controller");
        }
        if recovery.pending_action(label).is_some() {
            // Keep routes/content in HostState until the failed controller has
            // been replaced. Recovery will requeue only a still-requested show.
            return;
        }
        if let Err(error) = ensure_window_created(app, host, label) {
            tracing::error!(%error, label, "Host window creation failed");
            if label != "copy-toast" {
                host.show_host_toast(&format!("窗口打开失败：{error}"), "error", Some("ClipHost"), None, None);
            }
            return;
        }
        if app.state::<WindowLoading>().defer_show(label, task) { return; }
        // Creation pumps messages; cancel/expiry may have occurred during it.
        if !host.prepare_chrome_task(task) { return; }
    }
    if let Some(window) = app.get_webview_window("clipboard") {
        // Tao rebuilds extended styles from its own flags on show/resize/fold.
        // Keep that cache aligned; changing WS_EX_NOACTIVATE alone is overwritten.
        let _ = window.set_focusable(!host.clipboard_no_activate());
    }
    match cmd {
        ChromeCommand::Hide => {
            hide_clipboard_window(app, host);
        }
        ChromeCommand::EdgeHide => {
            if let Some(window) = app.get_webview_window("clipboard") {
                if let Some(target) = host.edge_tuck_position() {
                    animate_clipboard_edge(app, host, &window, target, true);
                } else {
                    // Moving away from an edge can invalidate a queued tuck.
                    // The policy owns full Hide; positioning must not invent it.
                    host.cancel_edge_tuck();
                }
            }
        }
        ChromeCommand::RestoreEdge => {
            if let (Some(window), Some(target)) = (
                app.get_webview_window("clipboard"), host.edge_show_restore_position()
            ) {
                animate_clipboard_edge(app, host, &window, target, false);
            }
        }
        ChromeCommand::Show => {
            if let Some(window) = app.get_webview_window("clipboard") {
                // C# `AdjustWindowStateToNormal` then `RefreshHostTopmost` (Topmost always).
                log_presentation_result("clipboard", "unminimize", window.unminimize());
                log_presentation_result("clipboard", "topmost-policy", window.set_always_on_top(true));
                let collapsed = host.hide_snapshot().is_collapsed;
                let tucked = host.chrome_command_lifecycle_allowed(ChromeCommand::EdgeHide);
                let _ = window.set_resizable(!collapsed);
                let (width, height) = host.clipboard_window_size(collapsed);
                let size = LogicalSize::new(width, height);
                let _ = window.set_size(size);
                if tucked {
                    if let Some((x, y)) = host.edge_tuck_position() {
                        let _ = window.set_position(tauri::PhysicalPosition::new(x as i32, y as i32));
                    }
                } else if let Some((x, y)) = host.place_main_window_logical(width, height) {
                    let _ = window.set_position(tauri::PhysicalPosition::new(x, y));
                } else if let Some((x, y)) = host.edge_show_restore_position() {
                    place_clipboard_window_immediately(host, &window, (x, y), false);
                }
                if let Some(route) = host.take_pending_nav("clipboard") {
                    let _ = window.eval(&spa_assign_script(&route));
                }
                log_presentation_result("clipboard", "show", window.show());
                if tucked {
                    #[cfg(windows)]
                    if let Ok(hwnd) = window.hwnd() {
                        window_effects::set_tucked(windows::Win32::Foundation::HWND(hwnd.0), true);
                    }
                } else {
                    clear_window_shadow_tuck(&window);
                }
                let explicit = host.chrome_task_explicit_presentation(task);
                let activate = host.should_activate_on_show();
                if explicit && activate {
                    log_presentation_result("clipboard", "focus", window.set_focus());
                }
                // Owned popups can change native Z order independently of
                // Tao's cached flag. Restore the required clipboard layer.
                if let Ok(hwnd) = window.hwnd() {
                    let native = windows::Win32::Foundation::HWND(hwnd.0);
                    if explicit {
                        tool_window::present_explicit(native, "clipboard", tool_window::ShowLayer::Topmost, activate);
                    } else {
                        tool_window::ensure_topmost(native);
                    }
                } else if explicit {
                    tracing::warn!(label = "clipboard", stage = "explicit-show-hwnd", error = true, "Host window presentation");
                }
            }
            push_window_state(app, host);
        }
        ChromeCommand::Fold => {
            if let Some(window) = app.get_webview_window("clipboard") {
                let (width, height) = host.clipboard_window_size(true);
                let _ = window.set_resizable(false);
                let _ = window.set_size(LogicalSize::new(width, height));
                let _ = window.show();
                clear_window_shadow_tuck(&window);
            }
            push_window_state(app, host);
        }
        ChromeCommand::Unfold => {
            if let Some(window) = app.get_webview_window("clipboard") {
                let (width, height) = host.clipboard_window_size(false);
                let _ = window.set_resizable(true);
                let _ = window.set_size(LogicalSize::new(width, height));
                let _ = window.show();
                clear_window_shadow_tuck(&window);
            }
            push_window_state(app, host);
        }
        ChromeCommand::BeginCaptionDrag(label) => {
            if let Some(window) = app.get_webview_window(label) {
                if let Ok(hwnd) = window.hwnd() {
                    // ReleaseCapture must run on the window's owning UI thread.
                    let hwnd = hwnd.0 as isize;
                    let _ = app.run_on_main_thread(move || {
                        cliphost_core::window_chrome::begin_caption_drag(hwnd);
                    });
                }
            }
        }
        ChromeCommand::PushWindowState => {
            push_window_state(app, host);
            push_main_host_state(app, host);
            push_host_window_state(app, "panel", &host.panel_window_state_dto());
            sync_tray_visible(app, host);
        }
        ChromeCommand::ShowQuickPaste => {
            if let Some(window) = app.get_webview_window("quick-paste") {
                // Queue the current session before exposing the retained WebView.
                inject_quick_paste(&window, &host.qp_state_detail());
                reveal_overlay_window(
                    &window,
                    host.qp_overlay_place(),
                    host.qp_overlay_pass_through(),
                    |hwnd| {
                        host.set_qp_hwnd(hwnd);
                    },
                );
            }
        }
        ChromeCommand::HideQuickPaste => {
            if let Some(window) = app.get_webview_window("quick-paste") {
                // Hide before clearing content and retain the bounded surface;
                // collapsing to warmup size forces a WebView resize on reopen.
                let _ = window.hide();
                if let Ok(hwnd) = window.hwnd() {
                    let _ = cliphost_core::window_menu::apply_overlay_hidden(hwnd.0 as isize);
                }
                inject_quick_paste(&window, &host.qp_state_detail());
            }
        }
        ChromeCommand::ShowToast => {
            if let Some(window) = app.get_webview_window("copy-toast") {
                reveal_overlay_window(
                    &window,
                    host.toast_overlay_place(),
                    host.toast_overlay_pass_through(),
                    |_| {},
                );
                if let Some(detail) = host.take_toast_push() {
                    inject_copy_toast(&window, &detail);
                }
            }
        }
        ChromeCommand::HideToast => {
            if let Some(window) = app.get_webview_window("copy-toast") {
                hide_overlay_window(&window);
            }
        }
        ChromeCommand::OpenQuickEdit => {
            if let Some(route) = host.take_toast_route() {
                navigate(app, &route);
            }
        }
        ChromeCommand::ShowSuperMenu => {
            if let Some(window) = app.get_webview_window("super-menu") {
                reveal_overlay_window(
                    &window,
                    host.super_menu_overlay_place(),
                    host.super_menu_overlay_pass_through(),
                    |hwnd| {
                        host.set_super_menu_hwnd(hwnd);
                    },
                );
                inject_super_menu(&window, &host.super_menu_state_detail());
            }
        }
        ChromeCommand::HideSuperMenu => {
            if let Some(window) = app.get_webview_window("super-menu") {
                inject_super_menu(&window, &host.super_menu_state_detail());
                hide_overlay_window(&window);
            }
        }
        ChromeCommand::DismissContextMenu(id) => {
            app.state::<context_menu_popup::Service>().dismiss(app, host, id, "dismiss", None);
        }
        ChromeCommand::ShowMain => {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                if let Some((w, h)) = host.saved_host_window_size("main") {
                    let _ = window.set_size(LogicalSize::new(w as f64, h as f64));
                } else {
                    let _ = window.set_size(LogicalSize::new(760.0, 580.0));
                }
            }
            show_labeled_window(app, host, "main", true, task);
            if let Some(window) = app.get_webview_window("main") {
                if let Ok(hwnd) = window.hwnd() {
                    host.set_main_hwnd(hwnd.0 as isize);
                }
            }
            push_main_host_state(app, host);
        }
        ChromeCommand::HideMain => {
            host.mark_main_hidden();
            hide_labeled_window(app, "main");
        }
        ChromeCommand::ShowPanel => {
            if let Some(window) = app.get_webview_window("panel") {
                // Editing made this window focusable; each new panel session
                // starts in the C# panel's no-activate mode again.
                let _ = window.set_focusable(false);
                let _ = window.set_size(LogicalSize::new(420.0, 520.0));
                if let Some((x, y)) = host.place_panel_window_logical(420.0, 520.0) {
                    let _ = window.set_position(tauri::PhysicalPosition::new(x, y));
                }
                let _ = window.set_always_on_top(true);
                if let Ok(hwnd) = window.hwnd() {
                    host.set_panel_hwnd(hwnd.0 as isize);
                    let _ = cliphost_core::window_menu::apply_overlay_no_activate(hwnd.0 as isize);
                }
            }
            // C# `PanelHostWindowWebView.UseNoActivateWhileVisible` — do not steal focus.
            show_labeled_window(app, host, "panel", false, task);
            push_host_window_state(app, "panel", &host.panel_window_state_dto());
        }
        ChromeCommand::HidePanel => {
            hide_labeled_window(app, "panel");
        }
        ChromeCommand::ShowCeaBox => {
            host.ceabox_hide();
            host.show_host_toast("CeaBox 需要 GPUI 原生宿主，当前使用 Tauri 回退。", "error", Some("CeaBox"), None, None);
        }
        ChromeCommand::HideCeaBox => {}
        ChromeCommand::ShowReopen => {
            host.reopen_hide();
            host.show_host_toast("重开快捷窗需要 GPUI 原生宿主，当前使用 Tauri 回退。", "error", Some("重开"), None, None);
        }
        ChromeCommand::HideReopen => {}
        ChromeCommand::ShowFileBrowser => {
            host.file_browser_hide();
            host.show_host_toast("文件窗口需要 GPUI 原生宿主，当前使用 Tauri 回退。", "error", Some("文件"), None, None);
        }
        ChromeCommand::HideFileBrowser => {}
        ChromeCommand::PanelBack => {
            eval_labeled(app, "panel", "try{history.back();}catch(e){}");
        }
        ChromeCommand::PanelRefresh => {
            eval_labeled(
                app,
                "panel",
                "try{window.dispatchEvent(new CustomEvent('panel-host-refresh'));}catch(e){}",
            );
        }
        ChromeCommand::PanelSettings => {
            eval_labeled(
                app,
                "panel",
                "try{window.dispatchEvent(new CustomEvent('panel-host-settings'));}catch(e){}",
            );
        }
        ChromeCommand::OpenLog => {
            if let Ok(body) = host.latest_log() {
                if let Some(path) = body.get("path").and_then(|v| v.as_str()) {
                    open_path_with_shell(path);
                }
            }
        }
        ChromeCommand::Exit => {
            request_exit(app);
        }
        ChromeCommand::RouteDigit { digit, repeat } => {
            eval_labeled(app, "clipboard", &include_str!("clipboard-digit.js")
                .replace("__CLIPHOST_DIGIT__", &digit.to_string())
                .replace("__CLIPHOST_DIGIT_REPEAT__", if repeat { "true" } else { "false" }));
        }
        ChromeCommand::ClipboardNavKey { key, shift } => {
            let key_json = serde_json::to_string(&key).unwrap_or_else(|_| "\"\"".into());
            eval_labeled(app, "clipboard", &include_str!("clipboard-navigation.js")
                .replace("__CLIPHOST_NAV_KEY__", &key_json)
                .replace("__CLIPHOST_NAV_SHIFT__", if shift { "true" } else { "false" }));
        }
        ChromeCommand::ClipboardSwitchTab { reverse } => {
            let flag = if reverse { "true" } else { "false" };
            eval_labeled(
                app,
                "clipboard",
                &format!(
                    "(function(){{try{{var api=window.__clipHostPrimaryTabs;if(api&&typeof api.switchTab==='function'){{api.switchTab({flag});}}}}catch(e){{}}}})();"
                ),
            );
        }
        ChromeCommand::ClipboardSearchBackspace => {
            eval_labeled(
                app,
                "clipboard",
                "(function(){try{var api=window.__clipHostFakeFocusSearch;if(api&&typeof api.backspaceSearch==='function'){api.backspaceSearch();}}catch(e){}})();",
            );
        }
        ChromeCommand::ClipboardKeyboardFocus => {
            if host.clipboard_keyboard_focus_allowed() {
                if let Some(window) = app.get_webview_window("clipboard") {
                    let _ = window.set_focus();
                    let webview: &tauri::Webview = window.as_ref();
                    let _ = webview.set_focus();
                }
            }
        }
        ChromeCommand::HostKeyboardFocus(label) => {
            if host.host_keyboard_focus_allowed(label) {
                if let Some(window) = app.get_webview_window(label) {
                    let _ = window.set_focusable(true);
                    if let Ok(hwnd) = window.hwnd() {
                        let _ = cliphost_core::window_menu::set_no_activate(hwnd.0 as i64, false);
                    }
                    let _ = window.set_focus();
                    let webview: &tauri::Webview = window.as_ref();
                    let _ = webview.set_focus();
                }
            }
        }
        ChromeCommand::ClipboardActivateSearch => {
            if !host.clipboard_keyboard_focus_allowed() {
                return;
            }
            eval_labeled(
                app,
                "clipboard",
                "(function(){try{var api=window.__clipHostFakeFocusSearch;if(api&&typeof api.focusSearchIme==='function'){api.focusSearchIme();}else if(api&&typeof api.focusSearch==='function'){api.focusSearch();}}catch(e){}})();",
            );
        }
        ChromeCommand::ClipboardAppendSearch(ch) => {
            let ch_json = serde_json::to_string(&ch.to_string()).unwrap_or_else(|_| "\"\"".into());
            eval_labeled(
                app,
                "clipboard",
                &format!(
                    "(function(){{try{{var api=window.__clipHostFakeFocusSearch;if(api&&typeof api.appendSearchChar==='function'){{api.appendSearchChar({ch_json});}}}}catch(e){{}}}})();"
                ),
            );
        }
        ChromeCommand::ClipboardHandleEscape => {
            if let Some(input_token) = task.escape_input_token() {
                eval_labeled(app, "clipboard", &clipboard_handle_escape_script(&task.escape_session_token(), &input_token));
            }
        }
        ChromeCommand::PanelNavKey(key) => {
            let key_json = serde_json::to_string(&key).unwrap_or_else(|_| "\"\"".into());
            eval_labeled(
                app,
                "panel",
                &include_str!("panel-navigation.js").replace("__CLIPHOST_PANEL_KEY__", &key_json),
            );
        }
    }
}

fn push_window_state(app: &tauri::AppHandle, host: &HostState) {
    push_host_window_state(app, "clipboard", &host.window_state_dto());
}

fn push_main_host_state(app: &tauri::AppHandle, host: &HostState) {
    push_host_window_state(app, "main", &host.main_host_state_dto());
}

fn push_host_window_state(app: &tauri::AppHandle, label: &str, detail: &serde_json::Value) {
    if let Some(window) = app.get_webview_window(label) {
        let json = detail.to_string();
        let script = format!(
            r#"(() => {{
  const patch = {json};
  const prev = window.__clipHostWindowState;
  const detail = prev ? Object.assign({{}}, prev, patch) : patch;
  window.__clipHostWindowState = detail;
  try {{ window.dispatchEvent(new CustomEvent('clip-host-window-state', {{ detail }})); }} catch {{}}
}})();"#
        );
        let _ = window.eval(&script);
    }
}

fn inject_quick_paste(window: &tauri::WebviewWindow, detail: &serde_json::Value) {
    let json = detail.to_string();
    let script = format!(
        r#"(() => {{
  const detail = {json};
  const deliver = window.__clipHostQuickPasteDeliver;
  if (typeof deliver === 'function') {{ try {{ deliver(detail); }} catch {{}} return; }}
  else {{
    const q = window.__clipHostQuickPasteQueue || (window.__clipHostQuickPasteQueue = []);
    q.push(detail);
  }}
  try {{ window.dispatchEvent(new CustomEvent('cea-quick-paste-state', {{ detail }})); }} catch {{}}
}})();"#
    );
    let _ = window.eval(&script);
}

fn inject_copy_toast(window: &tauri::WebviewWindow, detail: &serde_json::Value) {
    let json = detail.to_string();
    let script = format!(
        r#"(() => {{
  const detail = {json};
  const deliver = window.__clipHostToastDeliver;
  if (typeof deliver === 'function') {{ try {{ deliver(detail); }} catch {{}} return; }}
  const q = window.__clipHostToastQueue || (window.__clipHostToastQueue = []);
  q.push(detail);
}})();"#
    );
    let _ = window.eval(&script);
}

fn inject_super_menu(window: &tauri::WebviewWindow, detail: &serde_json::Value) {
    let json = detail.to_string();
    let script = format!(
        r#"(() => {{
  const detail = {json};
  const deliver = window.__clipHostSuperMenuDeliver;
  if (typeof deliver === 'function') {{ try {{ deliver(detail); }} catch {{}} }}
  else {{
    const q = window.__clipHostSuperMenuQueue || (window.__clipHostSuperMenuQueue = []);
    q.push(detail);
  }}
  try {{ window.dispatchEvent(new CustomEvent('cea-super-menu-state', {{ detail }})); }} catch {{}}
}})();"#
    );
    let _ = window.eval(&script);
}

#[cfg(test)]
mod tests {
    use super::{boot_script, clipboard_handle_escape_script};

    #[test]
    fn native_result_wait_releases_executor_and_reports_dispatch_failures() {
        use std::collections::VecDeque;
        use std::sync::{Arc, Mutex};

        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let queued = Arc::new(Mutex::new(VecDeque::<Box<dyn FnOnce() + Send>>::new()));
        runtime.block_on(async {
            let queue = queued.clone();
            let waiting = tokio::spawn(super::dispatch_native_result(move |job| {
                queue.lock().unwrap().push_back(job);
                Ok(())
            }, || Ok(41)));
            tokio::task::yield_now().await;
            assert!(!waiting.is_finished(), "native work ran before the UI callback");
            assert_eq!(queued.lock().unwrap().len(), 1);
            // Even a runtime with a single worker can serve unrelated work while
            // a busy UI retains the callback, without a synchronous getter wait.
            assert_eq!(tokio::spawn(async { 42 }).await.unwrap(), 42);
            queued.lock().unwrap().pop_front().unwrap()();
            assert_eq!(waiting.await.unwrap(), Ok(41));

            let rejected = super::dispatch_native_result::<()>(|_| Err("dispatch rejected".into()),
                || panic!("rejected native callback ran")).await;
            assert_eq!(rejected, Err("dispatch rejected".into()));
            let dropped = super::dispatch_native_result::<()>(|_| Ok(()),
                || panic!("discarded native callback ran")).await;
            assert!(dropped.unwrap_err().contains("dropped before completion"));
            let destroyed = super::dispatch_native_result::<()>(|job| { job(); Ok(()) },
                || Err("owner destroyed".into())).await;
            assert_eq!(destroyed, Err("owner destroyed".into()));
        });
    }

    #[test]
    fn main_thread_poll_wakes_coalesced_work_after_ui_release() {
        use std::collections::VecDeque;
        use std::future::Future;
        use std::sync::{Arc, Mutex};
        use std::task::{Context, Poll, Waker};

        let wakeup = Arc::new(tokio::sync::Notify::new());
        let poll = super::MainThreadPoll::new(wakeup.clone());
        let queued = Arc::new(Mutex::new(VecDeque::<Box<dyn FnOnce() + Send>>::new()));
        let mut context = Context::from_waker(Waker::noop());
        let mut waiting = Box::pin(wakeup.notified());
        assert_eq!(waiting.as_mut().poll(&mut context), Poll::Pending);
        let queue = queued.clone();
        let running_poll = poll.clone();
        poll.schedule(move |job| {
            queue.lock().unwrap().push_back(job);
            Ok::<_, ()>(())
        }, move || {
            // A wake can arrive after the UI drained commands but before its
            // ticket releases. It must request a retry, without executing here.
            running_poll.schedule::<()>(|_| panic!("UI work reentered"), || {}).unwrap();
        }).unwrap();
        poll.schedule::<()>(|_| panic!("blocked UI accumulated callbacks"), || {}).unwrap();
        assert_eq!(queued.lock().unwrap().len(), 1);
        assert_eq!(waiting.as_mut().poll(&mut context), Poll::Pending);

        let job = queued.lock().unwrap().pop_front().unwrap();
        job();
        assert_eq!(waiting.as_mut().poll(&mut context), Poll::Ready(()), "coalesced wake waited for maintenance");
        let queue = queued.clone();
        poll.schedule(move |job| {
            queue.lock().unwrap().push_back(job);
            Ok::<_, ()>(())
        }, || {}).unwrap();
        assert_eq!(queued.lock().unwrap().len(), 1, "wake could not post work immediately after UI release");
    }

    #[test]
    fn main_thread_poll_coalesces_waiting_and_running_work() {
        use std::collections::VecDeque;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};

        let poll = super::MainThreadPoll::default();
        let queued = Arc::new(Mutex::new(VecDeque::<Box<dyn FnOnce() + Send>>::new()));
        let latest = Arc::new(AtomicUsize::new(0));
        let observed = Arc::new(AtomicUsize::new(0));
        for tick in 1..=250 {
            latest.store(tick, Ordering::SeqCst);
            let (queue, state, result, running_poll) =
                (queued.clone(), latest.clone(), observed.clone(), poll.clone());
            poll.schedule(move |job| {
                queue.lock().unwrap().push_back(job);
                Ok::<_, ()>(())
            }, move || {
                result.store(state.load(Ordering::SeqCst), Ordering::SeqCst);
                // Reentrant ticks while work runs must not enqueue another poll.
                running_poll.schedule::<()>(|_| panic!("poll queued while executing"), || {}).unwrap();
            }).unwrap();
        }
        assert_eq!(queued.lock().unwrap().len(), 1, "a blocked UI accumulated poll callbacks");
        assert_eq!(observed.load(Ordering::SeqCst), 0, "UI work ran on the polling worker");
        let job = queued.lock().unwrap().pop_front().unwrap();
        job();
        assert_eq!(observed.load(Ordering::SeqCst), 250, "UI work did not see the latest state");

        // A rejected dispatch and a queued callback discarded during shutdown must
        // both release the gate, so later ticks remain eligible to execute.
        assert!(poll.schedule(|_| Err::<(), _>("dispatcher closed"), || {}).is_err());
        let queue = queued.clone();
        poll.schedule(move |job| {
            queue.lock().unwrap().push_back(job);
            Ok::<_, ()>(())
        }, || panic!("discarded callback executed")).unwrap();
        queued.lock().unwrap().clear();
        let result = observed.clone();
        poll.schedule(|job| { job(); Ok::<_, ()>(()) }, move || {
            result.store(251, Ordering::SeqCst);
        }).unwrap();
        assert_eq!(observed.load(Ordering::SeqCst), 251);
    }

    #[test]
    fn background_launch_stays_hidden_until_an_explicit_show() {
        use cliphost_core::{ChromeCommand, HostState, paths::AppPaths};
        cliphost_core::clipboard::force_headless(true);
        let root = tempfile::tempdir().unwrap();
        let host = HostState::open(AppPaths::from_data_root(root.path().to_path_buf()), 28965).unwrap();
        host.dismiss_native_window("clipboard");
        super::update_page_input_readiness(&host, "clipboard", tauri::webview::PageLoadEvent::Finished);
        assert!(!host.hide_snapshot().visible);
        while let Some(task) = host.take_chrome_task() {
            assert_ne!(task.command(), ChromeCommand::Show);
        }

        let background = vec!["cliphost-app.exe".into(), "--background".into()];
        super::handle_secondary_launch(&host, &background);
        assert!(!host.hide_snapshot().visible);
        assert!(host.take_chrome_task().is_none());

        host.execute_verb("clip toggle").unwrap();
        assert!(host.hide_snapshot().visible, "first toggle must show the background host");
        while host.take_chrome_task().is_some() {}
        super::handle_secondary_launch(&host, &background);
        assert!(host.hide_snapshot().visible, "a background duplicate must preserve an open window");
        assert!(host.take_chrome_task().is_none());

        host.dismiss_native_window("clipboard");
        while host.take_chrome_task().is_some() {}
        super::handle_secondary_launch(&host, &["cliphost-app.exe".into()]);
        assert!(host.hide_snapshot().visible);
        assert_eq!(host.take_chrome_task().unwrap().command(), ChromeCommand::Show);
    }

    #[test]
    fn page_reload_rejects_dequeued_input_after_the_new_document_is_ready() {
        use cliphost_core::{ChromeCommand, HostState, paths::AppPaths};
        use tauri::webview::PageLoadEvent::{Started, Finished};
        cliphost_core::clipboard::force_headless(true);
        for label in ["clipboard", "panel"] {
            let root = tempfile::tempdir().unwrap();
            let host = HostState::open(AppPaths::from_data_root(root.path().to_path_buf()), 28965).unwrap();
            host.favor_folder_create(&serde_json::json!({"title": "Reload fixture"})).unwrap();
            host.panel_show(&serde_json::json!({})).unwrap();
            super::update_page_input_readiness(&host, "clipboard", Finished);
            super::update_page_input_readiness(&host, "panel", Finished);
            while host.take_chrome_task().is_some() {}
            let request = serde_json::json!({
                "kind": "clipboardHostChrome", "sourceHostWindow": label,
                "requestHostKeyboardFocus": true,
            });
            host.chrome_web_action(&request);
            let old = host.take_chrome_task().unwrap();
            assert_eq!(old.command(), if label == "clipboard" { ChromeCommand::ClipboardKeyboardFocus }
                else { ChromeCommand::HostKeyboardFocus("panel") });
            host.show_web_route("/settings/clip");
            let independent = host.take_chrome_task().unwrap();
            assert_eq!(independent.command(), ChromeCommand::ShowMain);
            assert!(host.chrome_task_input_allowed(old));
            super::update_page_input_readiness(&host, label, Started);
            super::update_page_input_readiness(&host, label, Finished);
            assert!(!host.prepare_chrome_task(old), "old {label} input reached a new document");
            assert!(host.prepare_chrome_task(independent), "reload invalidated a lifecycle command");
            if label == "clipboard" { host.request_show(); }
            else { host.panel_show(&serde_json::json!({})).unwrap(); }
            while host.take_chrome_task().is_some() {}
            host.chrome_web_action(&request);
            let fresh = host.take_chrome_task().unwrap();
            super::update_page_input_readiness(&host, label, Finished);
            assert!(host.prepare_chrome_task(fresh), "duplicate readiness cancelled valid new input");
        }
    }

    #[test]
    fn page_load_start_suspends_input_only_for_its_own_window() {
        use cliphost_core::{host::{ChromeCommand, HostState}, paths::AppPaths};
        use tauri::webview::PageLoadEvent::{Started, Finished};
        cliphost_core::clipboard::force_headless(true);
        for label in ["clipboard", "panel"] {
            let root = tempfile::tempdir().unwrap();
            let host = HostState::open(AppPaths::from_data_root(root.path().to_path_buf()), 28965).unwrap();
            host.favor_folder_create(&serde_json::json!({"title": "Reload fixture"})).unwrap();
            host.panel_show(&serde_json::json!({})).unwrap();
            super::update_page_input_readiness(&host, "clipboard", Finished);
            super::update_page_input_readiness(&host, "panel", Finished);
            let (own, other) = if label == "clipboard" {
                (ChromeCommand::ClipboardActivateSearch, ChromeCommand::PanelNavKey("Enter"))
            } else {
                (ChromeCommand::PanelNavKey("Enter"), ChromeCommand::RouteDigit { digit: 1, repeat: false })
            };
            assert!(host.chrome_command_input_allowed(own));
            assert!(host.chrome_command_input_allowed(other));
            super::update_page_input_readiness(&host, label, Started);
            assert!(!host.chrome_command_input_allowed(own), "{label} loading still accepted keys");
            assert!(host.chrome_command_input_allowed(other));
            assert!(host.chrome_command_input_allowed(ChromeCommand::ShowMain));
            super::update_page_input_readiness(&host, label, Finished);
            assert!(host.chrome_command_input_allowed(own));
            assert!(host.chrome_command_input_allowed(other));
        }
    }

    #[test]
    fn boot_route_is_json_escaped() {
        let route = "/clipboard?query=\"text\"";
        let js = boot_script(28965, route, "panel");
        assert!(js.contains("const sourceWindowLabel = \"panel\";"));
        assert!(js.contains(&format!("const initialRoute = {};", serde_json::to_string(route).unwrap())));
        assert!(js.contains("const apiPort = 28965;"));
        assert!(!js.contains("__CLIPHOST_BOOT_"));
    }

    #[test]
    fn escape_reports_unhandled_to_host() {
        let escape = clipboard_handle_escape_script("session-fixture", "input-fixture");
        assert!(escape.contains("handleEscape"));
        assert!(escape.contains("escapeUnhandled"));
    }
}
