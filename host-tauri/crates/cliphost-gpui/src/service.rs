//! Native product host. Reuses core business, HTTP/Bridge and Verb RPC without
//! initializing Tauri, Vite, a browser renderer or another clipboard database.

use crate::{
    filters::ListRequest,
    instance::{InstanceGuard, identity},
    preview::{PreviewPage, PreviewRow, PreviewThumbnail},
};
use cliphost_core::{HostState, dto::ClipboardItemDto, paths::AppPaths};
use cliphost_http::{HttpState, event_bus, flush_host_events};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    net::{Ipv4Addr, TcpListener},
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    runtime::{Builder, Runtime},
    sync::broadcast,
    task::JoinHandle,
};

#[cfg(windows)]
use windows_compat as windows_api;
#[cfg(windows)]
#[path = "../../../src-tauri/src/clipboard_dispatch.rs"]
mod clipboard_dispatch;

#[derive(Debug, Clone)]
pub struct NativeHostOptions {
    pub data_root: Option<PathBuf>,
    pub api_port: u16,
    pub headless: bool,
    pub background: bool,
    pub isolated: bool,
    pub verb_rpc: bool,
    /// Private desktop acceptance build only; never enables normal v2 startup.
    pub core_copy_acceptance: bool,
}

impl Default for NativeHostOptions {
    fn default() -> Self {
        Self {
            data_root: None,
            api_port: cliphost_core::resolve_listen_port(),
            headless: cliphost_core::is_headless(),
            background: false,
            isolated: false,
            verb_rpc: true,
            core_copy_acceptance: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct HostEvent {
    pub name: String,
    pub data: Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListResponse {
    items: Vec<ClipboardItemDto>,
    total: usize,
    skip: usize,
    take: usize,
    has_more: bool,
    next_skip: usize,
}

pub struct NativeHost {
    host: Arc<HostState>,
    session: String,
    events: broadcast::Sender<HostEvent>,
    runtime: Mutex<Option<Runtime>>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    thumbnails: Mutex<HashMap<PathBuf, PreviewThumbnail>>,
    stopped: AtomicBool,
    isolated: bool,
    #[cfg(windows)]
    clipboard: Option<Arc<clipboard_dispatch::ClipboardDispatcher>>,
    #[cfg(windows)]
    hover: Mutex<Option<Arc<crate::hover::HoverService>>>,
    // Retain the same-product claim until this NativeHost is actually dropped.
    _instance: InstanceGuard,
}

impl NativeHost {
    pub fn open(explicit_root: Option<PathBuf>) -> Result<Arc<Self>, String> {
        Self::open_with_options(NativeHostOptions {
            data_root: explicit_root,
            ..Default::default()
        })
    }

    pub fn open_with_options(options: NativeHostOptions) -> Result<Arc<Self>, String> {
        Self::open_with_options_internal(options, false)
    }

    #[cfg(debug_assertions)]
    pub fn open_keyboard_input_qa(options: NativeHostOptions) -> Result<Arc<Self>, String> {
        if !options.isolated || options.headless || options.core_copy_acceptance || options.verb_rpc {
            return Err("Keyboard QA requires a native isolated instance without shared RPC.".into());
        }
        HostState::validate_keyboard_input_qa(&AppPaths::from_data_root(
            options.data_root.clone().ok_or("Keyboard QA requires an explicit data root.")?
        ), options.api_port)?;
        Self::open_with_options_internal(options, true)
    }

    fn open_with_options_internal(mut options: NativeHostOptions, keyboard_qa: bool) -> Result<Arc<Self>, String> {
        validate_core_copy_options(&options)?;
        #[cfg(feature = "native-acceptance")]
        if options.core_copy_acceptance {
            // Inspect raw path components before legacy canonicalization can
            // follow a junction, including every existing nested candidate path.
            let paths = AppPaths::from_data_root(options.data_root.clone().unwrap());
            cliphost_core::native_acceptance::validate_core_copy_tree(&paths)?;
        }
        #[cfg(all(feature = "native-acceptance", not(test)))]
        if !options.core_copy_acceptance {
            return Err("This acceptance executable requires --core-copy-acceptance; normal product startup is disabled.".into());
        }
        options.isolated |= options.headless;
        if options.isolated {
            // Core acceptance already passed the fixed workspace .tmp tree
            // preflight above. Do not inspect the legacy QA parent override:
            // an inherited environment value must not probe an unrelated root.
            if !options.core_copy_acceptance && !keyboard_qa {
                options.data_root = Some(validate_isolated_root(
                    options
                        .data_root
                        .as_deref()
                        .ok_or("An isolated/headless native host requires an explicit data root.")?,
                )?);
            }
            options.verb_rpc = false;
        } else if cliphost_core::is_headless() {
            return Err(
                "A headless process must use explicit isolated native-host options.".into(),
            );
        }

        validate_listener_port(options.api_port, options.isolated)?;
        #[cfg(feature = "native-acceptance")]
        if options.core_copy_acceptance {
            let paths = AppPaths::from_data_root(options.data_root.clone().unwrap());
            let declared_port = std::env::var(cliphost_core::paths::API_PORT_ENV)
                .ok().and_then(|value| value.parse::<u16>().ok());
            if declared_port != Some(options.api_port) {
                return Err("Core-copy acceptance requires matching explicit CLI and environment API ports.".into());
            }
            // This validates the nonce, receiver PID and fixed repository .tmp
            // boundary before singleton, logging, storage or desktop services.
            cliphost_core::native_acceptance::initialize(&paths)?;
        }
        let id = identity(if options.isolated {
            options.data_root.as_deref()
        } else {
            None
        });
        let compatibility_window = !options.headless;
        // A duplicate must leave before data selection can remember its choice.
        // The initial claim is released before a privilege replacement launches;
        // a continuing process reacquires it before binding/opening any services.
        let initial_claim = InstanceGuard::acquire_for_launch(
            id.clone(),
            compatibility_window,
            options.background,
        )?;
        let (paths, (instance, activations)) = bootstrap_instance(
            options.isolated,
            initial_claim,
            || {
                prepare_startup_paths(&options, || {
                    AppPaths::resolve().map_err(|error| error.to_string())
                })
            },
            |global_config| {
                if std::env::var("CLIPHOST_TAURI_ALLOW_DIALOGS").is_err() {
                    std::env::set_var("CLIPHOST_TAURI_ALLOW_DIALOGS", "1");
                }
                cliphost_core::elevation::ensure_elevated_if_configured(global_config);
            },
            || InstanceGuard::acquire_for_launch(id, compatibility_window, options.background),
        )?;
        let listener = bind_listener(options.api_port, options.isolated)?;
        let port = listener
            .local_addr()
            .map_err(|error| error.to_string())?
            .port();
        if options.isolated {
            std::env::set_var("CLIPHOST_GPUI_ISOLATED", "1");
        }
        std::env::set_var("CLIPHOST_UI_RENDERER", "gpui");
        if options.headless {
            cliphost_core::force_headless(true);
        }
        paths.ensure().map_err(|error| error.to_string())?;
        init_tracing(&paths.logs);
        // Headless is a process mode, not a temporary guard: delayed core work
        // must continue skipping OS clipboard, registry and input operations.
        let runtime = Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("cliphost-native")
            .enable_all()
            .build()
            .map_err(|error| error.to_string())?;
        let host = {
            let _runtime = runtime.enter();
            let state = if options.core_copy_acceptance {
                #[cfg(feature = "native-acceptance")]
                { HostState::open_isolated_core_acceptance(paths, port)? }
                #[cfg(not(feature = "native-acceptance"))]
                { return Err("Core-copy acceptance is absent from normal builds.".into()); }
            } else if keyboard_qa {
                #[cfg(debug_assertions)]
                { HostState::open_keyboard_input_qa(paths, port)? }
                #[cfg(not(debug_assertions))]
                { return Err("Keyboard QA requires a Debug build.".into()); }
            } else {
                HostState::open(paths, port)?
            };
            Arc::new(state)
        };
        host.set_window_runtime_ready(false);
        if options.background {
            // The core and HWND must agree before shortcuts, RPC or HTTP can
            // observe this instance. The first explicit toggle will then show it.
            host.window_hide();
        }
        let session = host
            .create_session()
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or("Core did not create a native UI session.")?
            .to_string();

        #[cfg(windows)]
        let clipboard = if options.headless || keyboard_qa {
            None
        } else {
            let dispatcher = Arc::new(clipboard_dispatch::ClipboardDispatcher::start()?);
            let runner = dispatcher.clone();
            host.set_clipboard_ui_runner(move |job| runner.run(job));
            Some(dispatcher)
        };
        if options.verb_rpc {
            host.start_verb_rpc()
                .map_err(|error| format!("Native Verb RPC startup failed: {error}"))?;
        }

        let http = HttpState {
            host: host.as_ref().clone(),
            events: event_bus(),
        };
        let (events, _) = broadcast::channel(256);
        let mut pushes = http.events.subscribe();
        let native_events = events.clone();
        let event_task = runtime.spawn(async move {
            loop {
                match pushes.recv().await {
                    Ok(body) => {
                        if let Ok(envelope) = serde_json::from_str::<Value>(&body) {
                            if let Some(name) = envelope.get("eventName").and_then(Value::as_str) {
                                let _ = native_events.send(HostEvent {
                                    name: name.into(),
                                    data: envelope.get("data").cloned().unwrap_or(Value::Null),
                                });
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(count)) => {
                        let _ = native_events.send(HostEvent {
                            name: "nativeRefreshRequired".into(),
                            data: json!({ "lagged": count }),
                        });
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        let serve_http = http.clone();
        let server_events = events.clone();
        let server = runtime.spawn(async move {
            if let Err(error) = cliphost_http::serve_listener(serve_http, listener).await {
                let _ = server_events.send(HostEvent {
                    name: "nativeServiceError".into(),
                    data: json!({ "service": "http", "error": error.to_string() }),
                });
            }
        });
        let event_state = http.clone();
        let event_wakeup = host.push_event_notifier();
        let event_flush = runtime.spawn(async move {
            let wakeup = event_wakeup;
            let mut activations = activations;
            let mut activations_open = true;
            // Flush startup events too. notify_one retains a permit if the
            // producer runs before this task starts waiting.
            wakeup.notify_one();
            loop {
                let mut show = false;
                tokio::select! {
                    _ = wakeup.notified() => {}
                    activation = activations.recv(), if activations_open => {
                        show = activation.is_some();
                        activations_open = show;
                    }
                }
                // Coalesce repeated launches, keeping the async receive live
                // if more arrive while the blocking flush owns the host lock.
                for _ in 0..64 {
                    match activations.try_recv() {
                        Ok(()) => show = true,
                        Err(_) => break,
                    }
                }
                let state = event_state.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    if show {
                        state.host.request_show();
                    }
                    flush_host_events(&state);
                })
                .await;
            }
        });
        let keyboard_host = host.clone();
        let keyboard = runtime.spawn(async move {
            let wakeup = keyboard_host.keyboard_work_notifier();
            loop {
                // A hidden idle host has no timer. Active input retains the
                // release/timeout fallback, and each producer wakes us at once.
                match keyboard_host.keyboard_work_delay() {
                    Some(delay) => {
                        tokio::select! { _ = wakeup.notified() => {}, _ = tokio::time::sleep(delay) => {} }
                    }
                    None => wakeup.notified().await,
                }
                // A session may retire before this task or its worker runs.
                if !keyboard_host.needs_keyboard_work_poll() {
                    continue;
                }
                let state = keyboard_host.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    if state.needs_keyboard_work_poll() {
                        state.poll_keyboard_work();
                    }
                })
                .await;
            }
        });
        let maintenance_host = host.clone();
        let maintenance = runtime.spawn(async move {
            let wakeup = maintenance_host.maintenance_notifier();
            loop {
                match maintenance_host.maintenance_delay() {
                    Some(delay) => {
                        tokio::select! {
                            _ = wakeup.notified() => continue,
                            _ = tokio::time::sleep(delay) => {}
                        }
                    }
                    None => {
                        wakeup.notified().await;
                        continue;
                    }
                }
                let state = maintenance_host.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    state.poll_window_state_persistence();
                    state.commit_pending_clear();
                    state.poll_history_cleanup();
                })
                .await;
            }
        });
        let mut tasks = vec![event_task, server, event_flush, keyboard, maintenance];
        if !cfg!(test)
            && !options.isolated
            && !options.headless
            && !keyboard_qa
            && !options.core_copy_acceptance
            && !cliphost_core::NATIVE_ACCEPTANCE_BUILD
            && std::env::var("CLIPHOST_GPUI_ISOLATED").as_deref() != Ok("1")
        {
            tasks.push(runtime.spawn(crate::settings::run_automatic_update_checks(host.clone())));
        }
        if !options.isolated {
            tasks.push(runtime.spawn(async move {
                if let Err(error) = cliphost_http::serve_lan(http).await {
                    tracing::warn!(%error, "Native host configured LAN listener stopped");
                }
            }));
        }

        Ok(Arc::new(Self {
            host,
            session,
            events,
            runtime: Mutex::new(Some(runtime)),
            tasks: Mutex::new(tasks),
            thumbnails: Mutex::new(HashMap::new()),
            stopped: AtomicBool::new(false),
            isolated: options.isolated,
            #[cfg(windows)]
            hover: Mutex::new(None),
            #[cfg(windows)]
            clipboard,
            _instance: instance,
        }))
    }

    pub fn state(&self) -> Arc<HostState> {
        self.host.clone()
    }
    pub fn host(&self) -> Arc<HostState> {
        self.state()
    }
    pub fn isolated(&self) -> bool {
        self.isolated
    }
    pub fn subscribe(&self) -> broadcast::Receiver<HostEvent> {
        self.events.subscribe()
    }

    /// Share the product runtime with native popup/drag adapters, without
    /// starting a second business host or collector.
    pub fn runtime_handle(&self) -> Result<tokio::runtime::Handle, String> {
        self.runtime
            .lock()
            .map_err(|_| "Native runtime is unavailable.")?
            .as_ref()
            .map(|runtime| runtime.handle().clone())
            .ok_or_else(|| "Native runtime has stopped.".into())
    }

    #[cfg(windows)]
    pub fn hover_service(&self) -> Result<Arc<crate::hover::HoverService>, String> {
        if self.stopped.load(Ordering::Acquire) || cliphost_core::is_headless() {
            return Err("Native hover requires a running desktop host.".into());
        }
        let mut slot = self
            .hover
            .lock()
            .map_err(|_| "Native hover is unavailable.")?;
        if let Some(service) = slot.as_ref() {
            return Ok(service.clone());
        }
        let service = Arc::new(crate::hover::HoverService::new(
            (*self.host).clone(),
            self.runtime_handle()?,
        )?);
        *slot = Some(service.clone());
        Ok(service)
    }

    /// Call on a background executor. Uses the actual core paging/ranking/DTO
    /// contract, including its maximum 500-row page; no frontend HTTP round trip.
    pub fn search(
        &self,
        category: &str,
        query: &str,
        skip: usize,
        take: usize,
    ) -> Result<PreviewPage, String> {
        self.search_request(&ListRequest::category(category), query, skip, take)
    }

    /// Same as [`Self::search`] with the selected tab's filter item and the
    /// search-box filters (sort, usage, creation time).
    pub fn search_request(
        &self,
        request: &ListRequest,
        query: &str,
        skip: usize,
        take: usize,
    ) -> Result<PreviewPage, String> {
        if self.stopped.load(Ordering::Acquire) {
            return Err("Native host is shutting down.".into());
        }
        let value = self
            .host
            .search_items(&request.params(query, skip, take, &self.session))?;
        let page: ListResponse =
            serde_json::from_value(value).map_err(|error| error.to_string())?;
        let rows = page
            .items
            .into_iter()
            .map(|item| {
                let thumbnail = if item.kind == "ImageClipItem" {
                    self.thumbnail(&item.id)
                } else {
                    None
                };
                let preview_text = if query.trim().is_empty() || item.highlighted_text.is_empty() {
                    item.display_text.clone()
                } else {
                    item.highlighted_text.clone()
                };
                PreviewRow {
                    item,
                    preview_text,
                    thumbnail,
                }
            })
            .collect();
        Ok(PreviewPage {
            rows,
            total: page.total,
            skip: page.skip,
            take: page.take,
            has_more: page.has_more,
            next_skip: page.next_skip,
        })
    }

    fn thumbnail(&self, id: &str) -> Option<PreviewThumbnail> {
        let path = self.host.hover_image_file(id, false).ok()?;
        // Probe a bounded header only, after rejecting offline/link paths.
        // Full decode belongs to the visible-row background loader.
        let source = crate::history_image_resources::probe_source(&path).ok()?;
        if let Some(cached) = self
            .thumbnails
            .lock()
            .unwrap()
            .get(&path)
            .filter(|entry| entry.source_version == source.source_version)
        {
            return Some(cached.clone());
        }
        let thumbnail = PreviewThumbnail {
            asset: path.to_string_lossy().into_owned(),
            width: source.width,
            height: source.height,
            label: path.file_name()?.to_string_lossy().into_owned(),
            source_version: source.source_version,
        };
        let mut cache = self.thumbnails.lock().unwrap();
        if cache.len() >= 256 {
            cache.clear();
        }
        cache.insert(path, thumbnail.clone());
        Some(thumbnail)
    }

    /// Run on a worker, then exit the owning native process. Core's existing OS
    /// monitor/hooks are process scoped; this is not an in-process restart API.
    pub fn shutdown(&self) {
        if self.stopped.swap(true, Ordering::AcqRel) {
            return;
        }
        self.host.set_window_runtime_ready(false);
        #[cfg(windows)]
        if let Some(hover) = self.hover.lock().unwrap().take() {
            hover.shutdown();
        }
        self.host.stop_verb_rpc();
        for task in self.tasks.lock().unwrap().drain(..) {
            task.abort();
        }
        #[cfg(windows)]
        if let Some(writer) = &self.clipboard {
            writer.shutdown();
        }
        let _ = self
            .host
            .delete_session(&json!({ "sessionId": self.session }));
        self.host.flush_for_shutdown();
        if let Some(runtime) = self.runtime.lock().unwrap().take() {
            if tokio::runtime::Handle::try_current().is_ok() {
                runtime.shutdown_background();
            } else {
                runtime.shutdown_timeout(Duration::from_secs(2));
            }
        }
    }
}

impl Drop for NativeHost {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn init_tracing(logs: &Path) {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let path = cliphost_core::log_file::current_log_file_path(logs);
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(file) => {
            let subscriber = tracing_subscriber::registry()
                .with(filter)
                .with(fmt::layer().with_writer(std::io::stderr))
                .with(fmt::layer().with_ansi(false).with_writer(Mutex::new(file)));
            let _ = tracing::subscriber::set_global_default(subscriber);
            tracing::info!("GPUI native host service starting without WebView");
        }
        Err(error) => {
            let subscriber = tracing_subscriber::fmt().with_env_filter(filter).finish();
            let _ = tracing::subscriber::set_global_default(subscriber);
            tracing::warn!(%error, ?path, "Native host log file could not be opened");
        }
    }
}

fn validate_core_copy_options(options: &NativeHostOptions) -> Result<(), String> {
    if !options.core_copy_acceptance { return Ok(()); }
    if !cfg!(feature = "native-acceptance") {
        return Err("Core-copy acceptance requires the private native-acceptance build.".into());
    }
    if !options.isolated || options.headless || options.data_root.is_none()
        || options.verb_rpc || matches!(options.api_port, 0 | 28865 | 28965 | 18765 | 18776 | 28766 | 29766) {
        return Err("Core-copy acceptance requires a native isolated instance, explicit copied root and separate API port, with shared RPC disabled.".into());
    }
    Ok(())
}

fn prepare_startup_paths(
    options: &NativeHostOptions,
    resolve_default: impl FnOnce() -> Result<AppPaths, String>,
) -> Result<AppPaths, String> {
    let paths = match options.data_root.as_ref() {
        Some(root) => {
            if !root.is_absolute() {
                return Err("The native host data root must be absolute.".into());
            }
            AppPaths::from_data_root(root.clone())
        }
        None if options.isolated || options.headless => {
            return Err("An isolated/headless native host requires an explicit data root.".into());
        }
        None => resolve_default()?,
    };
    Ok(paths)
}

fn bootstrap_instance<T>(
    isolated: bool,
    initial_claim: T,
    prepare_paths: impl FnOnce() -> Result<AppPaths, String>,
    elevate: impl FnOnce(&Path),
    reacquire: impl FnOnce() -> Result<T, String>,
) -> Result<(AppPaths, T), String> {
    let paths = prepare_paths()?;
    if isolated {
        return Ok((paths, initial_claim));
    }
    // ensure_elevated can launch and exit. The child's single-instance probe
    // must not find this bootstrap process's mutex/compatibility window.
    drop(initial_claim);
    elevate(&paths.configs.join("GlobalConfig.json"));
    Ok((paths, reacquire()?))
}

fn validate_listener_port(port: u16, isolated: bool) -> Result<(), String> {
    if port == 0 && !isolated {
        return Err("An ephemeral API port is only allowed in isolated tests.".into());
    }
    if [28865, 18765, 18776, 28766].contains(&port) {
        return Err(format!("Native host cannot use reserved port {port}."));
    }
    Ok(())
}

fn bind_listener(port: u16, isolated: bool) -> Result<TcpListener, String> {
    validate_listener_port(port, isolated)?;
    if port == 0 {
        let listener =
            TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|error| error.to_string())?;
        listener
            .set_nonblocking(true)
            .map_err(|error| error.to_string())?;
        return Ok(listener);
    }
    if isolated {
        let listener =
            TcpListener::bind((Ipv4Addr::LOCALHOST, port)).map_err(|error| error.to_string())?;
        listener
            .set_nonblocking(true)
            .map_err(|error| error.to_string())?;
        Ok(listener)
    } else {
        cliphost_http::bind_loopback_with_fallback(port)
            .map(|(_, listener)| listener)
            .map_err(|error| error.to_string())
    }
}

fn normalized_target(path: &Path) -> Result<PathBuf, String> {
    let full = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| error.to_string())?
            .join(path)
    };
    if full
        .components()
        .any(|part| matches!(part, Component::ParentDir))
    {
        return Err("An isolated test path cannot contain parent traversal.".into());
    }
    let mut existing = full.as_path();
    let mut suffix = Vec::new();
    while !existing.exists() {
        suffix.push(
            existing
                .file_name()
                .ok_or("Cannot resolve isolated test path.")?
                .to_os_string(),
        );
        existing = existing
            .parent()
            .ok_or("Cannot resolve isolated test path.")?;
    }
    let mut normalized = existing.canonicalize().map_err(|error| error.to_string())?;
    for part in suffix.into_iter().rev() {
        normalized.push(part);
    }
    Ok(normalized)
}

fn validate_isolated_root(root: &Path) -> Result<PathBuf, String> {
    let allowed = std::env::var_os("CLIPHOST_GPUI_TEST_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../..")
                .join(".tmp")
        });
    // The compile-time workspace reference is normalized before applying the
    // strict no-parent-traversal rule to user-provided destinations.
    let allowed = if allowed.exists() {
        allowed.canonicalize().map_err(|error| error.to_string())?
    } else {
        return Err("The isolated test parent directory must already exist.".into());
    };
    let root = normalized_target(root)?;
    if root == allowed || !root.starts_with(&allowed) {
        return Err(format!(
            "Isolated data must be in a child directory of {}.",
            allowed.display()
        ));
    }
    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cliphost_core::{
        model::{ClipKind, ClipRecord},
        store::AppStore,
    };
    use std::io::{Read, Write};

    const CHILD_ENV: &str = "CLIPHOST_GPUI_SERVICE_TEST_CHILD";

    fn test_parent() -> PathBuf {
        let parent = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../..")
            .canonicalize()
            .unwrap()
            .join(".tmp");
        std::fs::create_dir_all(&parent).unwrap();
        parent
            .canonicalize()
            .expect("repository .tmp for isolated tests")
    }

    fn options(root: &Path) -> NativeHostOptions {
        NativeHostOptions {
            data_root: Some(root.into()),
            api_port: 0,
            headless: true,
            background: false,
            isolated: true,
            verb_rpc: false,
            core_copy_acceptance: false,
        }
    }

    #[test]
    fn core_copy_acceptance_never_falls_back_to_normal_startup() {
        let mut candidate = options(Path::new("copied-root"));
        candidate.core_copy_acceptance = true;
        assert!(validate_core_copy_options(&candidate).is_err());
        candidate.headless = false;
        candidate.api_port = 29065;
        assert_eq!(validate_core_copy_options(&candidate).is_ok(), cfg!(feature = "native-acceptance"));
        for invalid_port in [0, 28865, 28965, 18765, 18776, 28766, 29766] {
            candidate.api_port = invalid_port;
            assert!(validate_core_copy_options(&candidate).is_err());
        }
        candidate.api_port = 29065;
        candidate.isolated = false;
        assert!(validate_core_copy_options(&candidate).is_err());
        candidate.isolated = true;
        candidate.verb_rpc = true;
        assert!(validate_core_copy_options(&candidate).is_err());
        candidate.verb_rpc = false;
        candidate.data_root = None;
        assert!(validate_core_copy_options(&candidate).is_err());
    }

    fn request(port: u16, method: &str, path: &str, body: &str) -> String {
        let mut stream = std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        write!(stream, "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }

    #[test]
    fn isolated_core_http_events_roundtrip() {
        // Headless/core isolation flags are process scoped. Run the substantive
        // fixture in a child test process so other tests inherit no mode/env,
        // registry guards, clipboard state, monitor threads or event listeners.
        if std::env::var(CHILD_ENV).as_deref() != Ok("1") {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "service::tests::isolated_core_http_events_roundtrip",
                    "--nocapture",
                ])
                .env(CHILD_ENV, "1")
                .env("CLIPHOST_TAURI_HEADLESS", "1")
                .env("CLIPHOST_GPUI_TEST_ROOT", test_parent())
                .status()
                .unwrap();
            assert!(status.success(), "Isolated service child must pass");
            return;
        }
        let root = tempfile::Builder::new()
            .prefix("gpui-service-")
            .tempdir_in(test_parent())
            .unwrap();
        let paths = AppPaths::from_data_root(root.path().into());
        paths.ensure().unwrap();
        let store = AppStore::open(&paths.db).unwrap();
        let text = ClipRecord::new_text("真实存储中的中文服务测试", "fixture");
        let text_id = text.id.clone();
        store.insert_clip(&text, true).unwrap();
        let image_path = paths.image_library.join("native-fixture.png");
        std::fs::write(
            &image_path,
            include_bytes!("../../../../ui-design/assets/clip-main-thumbnail.png"),
        )
        .unwrap();
        let mut image = ClipRecord::new_text("native-image", "fixture");
        image.kind = ClipKind::ImageClipItem;
        image.image_path = Some(image_path.to_string_lossy().into_owned());
        store.insert_clip(&image, true).unwrap();
        drop(store);

        let mut host_options = options(root.path());
        host_options.background = true;
        let native = NativeHost::open_with_options(host_options).unwrap();
        let state = native.state();
        assert!(native.isolated());
        assert!(
            !state.hide_snapshot().visible,
            "Sign-in startup must leave core visibility hidden"
        );
        assert!(!state.hide_snapshot().no_focus_session_active);
        state.request_toggle(false);
        assert!(
            state.hide_snapshot().visible,
            "The first explicit toggle must show the hidden host"
        );
        state.window_hide();
        assert_eq!(state.app_info()["clipHost"]["runtime"], "gpui+rust");
        let page = native.search("text", "中文", 0, 100).unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.rows[0].item.id, text_id);
        assert!(page.rows[0].preview_text.contains("中文"));
        let images = native.search("image", "", 0, 100).unwrap();
        let thumbnail = images.rows[0].thumbnail.as_ref().unwrap();
        assert!(Path::new(&thumbnail.asset).is_absolute());
        assert_eq!((thumbnail.width, thumbnail.height), (320, 180));

        let health = request(state.api_port(), "GET", "/api/health", "");
        assert!(health.starts_with("HTTP/1.1 200"));
        assert!(health.contains(cliphost_core::paths::HEALTH_SERVICE_ID));
        let ping = request(
            state.api_port(),
            "POST",
            "/api/rpc",
            r#"{"id":"native-test","method":"host.ping"}"#,
        );
        assert!(ping.contains(cliphost_core::paths::HEALTH_SERVICE_ID));

        let mut events = native.subscribe();
        state.toggle_pin(&json!({ "id": text_id })).unwrap();
        let event = native
            .runtime
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        let event = events.recv().await.unwrap();
                        if event.name == "clipChanged" {
                            break event;
                        }
                    }
                })
                .await
                .unwrap()
            });
        assert_eq!(event.name, "clipChanged");
        let pinned = native.search("top", "", 0, 100).unwrap();
        assert_eq!(pinned.rows[0].item.id, text_id);
        assert!(pinned.rows[0].item.is_top);
        state.delete_item(&json!({ "id": text_id })).unwrap();
        assert_eq!(native.search("text", "", 0, 100).unwrap().total, 0);
        let clear_id = state
            .add_text(&json!({"text":"deferred clear maintenance"}))
            .unwrap()["createdIds"][0]
            .as_str()
            .unwrap()
            .to_string();
        let verification_store = AppStore::open(&paths.db).unwrap();
        let pending = state.clear_all(true).unwrap();
        assert!(
            verification_store.get_clip(&clear_id).is_ok(),
            "clear remains undoable before the five-second deadline"
        );
        state
            .clear_cancel(&json!({"clearId":pending["clearId"]}))
            .unwrap();
        assert!(verification_store.get_clip(&clear_id).is_ok());
        state.clear_all(true).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        while !state.clear_pending().is_null() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            state.clear_pending().is_null(),
            "the background GPUI host must commit the elapsed undo snapshot"
        );
        assert!(
            verification_store.get_clip(&clear_id).is_err(),
            "the maintenance loop must perform the actual core transaction"
        );
        native.shutdown();
        assert!(native.search("all", "", 0, 100).is_err());
        let stored = AppStore::open(&paths.db).unwrap();
        assert!(
            stored.get_clip(&text_id).is_err(),
            "Delete must persist in the existing core store"
        );
    }

    #[test]
    fn duplicate_is_rejected_before_database_or_capture_startup() {
        let root = tempfile::Builder::new()
            .prefix("gpui-claim-")
            .tempdir_in(test_parent())
            .unwrap();
        let target = root.path().join("not-created");
        let resolved = validate_isolated_root(&target).unwrap();
        let (_owner, _) = InstanceGuard::acquire(identity(Some(&resolved)), false).unwrap();
        let error = NativeHost::open_with_options(options(&target))
            .err()
            .unwrap();
        assert!(crate::instance::is_existing_instance(&error));
        assert!(
            !target.exists(),
            "Duplicate startup must not create data or SQLite"
        );
    }

    #[test]
    fn isolated_paths_and_listeners_stay_within_test_boundary() {
        assert!(validate_isolated_root(&test_parent().join("../production")).is_err());
        assert!(validate_isolated_root(&test_parent()).is_err());
        assert!(
            NativeHost::open_with_options(NativeHostOptions {
                headless: true,
                background: false,
                isolated: true,
                data_root: None,
                api_port: 0,
                verb_rpc: false,
                core_copy_acceptance: false,
            })
            .is_err()
        );
        for port in [28865, 18765, 18776, 28766] {
            assert!(bind_listener(port, true).is_err());
        }
        let listener = bind_listener(0, true).unwrap();
        assert!(listener.local_addr().unwrap().ip().is_loopback());
        assert!(bind_listener(0, false).is_err());
    }

    struct BootstrapClaim<'a> {
        events: &'a std::cell::RefCell<Vec<&'static str>>,
        drop_event: &'static str,
    }

    impl Drop for BootstrapClaim<'_> {
        fn drop(&mut self) {
            self.events.borrow_mut().push(self.drop_event);
        }
    }

    #[test]
    fn normal_bootstrap_releases_initial_claim_before_elevation_and_reclaims_afterward() {
        let events = std::cell::RefCell::new(Vec::new());
        let root = test_parent().join("bootstrap-order");
        let initial = BootstrapClaim {
            events: &events,
            drop_event: "release-initial",
        };
        let (paths, claim) = bootstrap_instance(
            false,
            initial,
            || {
                events.borrow_mut().push("paths");
                Ok(AppPaths::from_data_root(root.clone()))
            },
            |global_config| {
                assert_eq!(*events.borrow(), ["paths", "release-initial"]);
                assert_eq!(global_config, root.join("Configs/GlobalConfig.json"));
                events.borrow_mut().push("elevation");
            },
            || {
                events.borrow_mut().push("reacquire");
                Ok(BootstrapClaim {
                    events: &events,
                    drop_event: "release-final",
                })
            },
        )
        .unwrap();
        assert_eq!(paths.data_root, root);
        assert_eq!(
            *events.borrow(),
            ["paths", "release-initial", "elevation", "reacquire"]
        );
        drop(claim);
        assert_eq!(events.borrow().last(), Some(&"release-final"));
    }

    #[test]
    fn isolated_bootstrap_never_resolves_production_or_changes_privileges() {
        let events = std::cell::RefCell::new(Vec::new());
        let root = test_parent().join("bootstrap-isolated");
        let initial = BootstrapClaim {
            events: &events,
            drop_event: "release-initial",
        };
        let (paths, claim) = bootstrap_instance(
            true,
            initial,
            || prepare_startup_paths(&options(&root), || panic!("production resolver called")),
            |_| panic!("isolated bootstrap attempted elevation"),
            || panic!("isolated bootstrap relinquished its claim"),
        )
        .unwrap();
        assert_eq!(paths.data_root, root);
        assert!(events.borrow().is_empty());
        drop(claim);
        assert_eq!(*events.borrow(), ["release-initial"]);
    }

    #[test]
    fn failed_reacquisition_stops_bootstrap_before_data_is_created() {
        let events = std::cell::RefCell::new(Vec::new());
        let parent = tempfile::Builder::new()
            .prefix("gpui-bootstrap-")
            .tempdir_in(test_parent())
            .unwrap();
        let target = parent.path().join("not-created");
        let initial = BootstrapClaim {
            events: &events,
            drop_event: "release-initial",
        };
        let result = bootstrap_instance(
            false,
            initial,
            || Ok(AppPaths::from_data_root(target.clone())),
            |_| {
                assert_eq!(*events.borrow(), ["release-initial"]);
            },
            || Err(crate::instance::EXISTING_INSTANCE_PREFIX.into()),
        );
        assert!(crate::instance::is_existing_instance(
            &result.err().unwrap()
        ));
        assert!(!target.exists());
    }

    #[test]
    fn startup_path_selection_rejects_missing_isolated_and_relative_roots() {
        for options in [
            NativeHostOptions {
                data_root: None,
                headless: true,
                ..Default::default()
            },
            NativeHostOptions {
                data_root: Some(PathBuf::from("relative-root")),
                ..Default::default()
            },
        ] {
            assert!(
                prepare_startup_paths(&options, || panic!("invalid root fell back to production"))
                    .is_err()
            );
        }
    }
}
