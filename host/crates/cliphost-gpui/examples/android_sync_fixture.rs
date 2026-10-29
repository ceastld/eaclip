//! Android integration against the product GPUI NativeHost and its real Windows
//! clipboard dispatcher. The clipboard lives in a create-only, non-interactive
//! window station; this does not render a GPUI window or touch WinSta0.
//!
//! Debug only. Run with --data-root <absolute repository .tmp child> --seconds 600.
//! Android uses adb reverse for 49765 (fixture control) and 49766 (pinned TLS).
//! The product API is bound separately on 127.0.0.1:49767. POST /fixture/shutdown
//! stops the owned process; all three listeners are loopback only.

#[cfg(not(all(windows, debug_assertions)))]
fn main() {
    eprintln!("android_sync_fixture requires a Windows Debug build.");
    std::process::exit(2);
}

#[cfg(all(windows, debug_assertions))]
fn main() {
    let exit_code = match fixture::run() {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("android_sync_fixture: {error:#}");
            1
        }
    };
    // Core's native monitors are process scoped. Never restore WinSta0 while
    // those threads could still run: retain the private station until OS exit.
    std::process::exit(exit_code);
}

#[cfg(all(windows, debug_assertions))]
#[path = "../../cliphost-core/src/clipboard_private_station_test.rs"]
mod private_station;

#[cfg(all(windows, debug_assertions))]
mod fixture {
    use std::{
        fs,
        io::Cursor,
        net::{Ipv4Addr, TcpListener},
        path::{Component, Path, PathBuf},
        sync::Arc,
        time::{Duration, Instant},
    };

    use anyhow::{bail, ensure, Context, Result};
    use axum::{
        extract::{DefaultBodyLimit, State},
        http::StatusCode,
        routing::{get, post},
        Json, Router,
    };
    use base64::{engine::general_purpose::STANDARD, Engine};
    use cliphost_core::clipboard;
    use cliphost_gpui::service::{NativeHost, NativeHostOptions};
    use serde_json::{json, Value};
    use tokio::sync::{watch, Semaphore};

    use super::private_station::{self, PrivateStation};

    const CONTROL_PORT: u16 = 49765;
    const TLS_PORT: u16 = 49766;
    const API_PORT: u16 = 49767;
    const MAX_BODY: usize = 6 * 1024 * 1024;
    const MAX_IMAGE: usize = 4 * 1024 * 1024;

    struct Fixture {
        host: Arc<cliphost_core::HostState>,
        station: &'static PrivateStation,
        identity: Value,
        image_path: PathBuf,
        clipboard: Arc<Semaphore>,
        shutdown: watch::Sender<bool>,
    }

    type HttpResult = std::result::Result<Json<Value>, (StatusCode, Json<Value>)>;

    fn checked_root(value: &str) -> Result<PathBuf> {
        let root = PathBuf::from(value);
        ensure!(root.is_absolute(), "An absolute --data-root is required.");
        ensure!(
            !root
                .components()
                .any(|part| matches!(part, Component::ParentDir)),
            "Parent traversal is not permitted."
        );
        let allowed = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../..")
            .canonicalize()?
            .join(".tmp");
        fs::create_dir_all(&allowed)?;
        let allowed = allowed.canonicalize()?;
        // Resolve every existing ancestor before creating directories, so an
        // existing junction cannot redirect this fixture outside repository .tmp.
        let mut existing = root.as_path();
        let mut suffix = Vec::new();
        while !existing.exists() {
            suffix.push(
                existing
                    .file_name()
                    .context("Invalid fixture root")?
                    .to_owned(),
            );
            existing = existing.parent().context("Invalid fixture root")?;
        }
        let mut resolved = existing.canonicalize()?;
        for part in suffix.into_iter().rev() {
            resolved.push(part);
        }
        ensure!(
            resolved != allowed && resolved.starts_with(&allowed),
            "Fixture data must be in a child of this repository's .tmp directory."
        );
        ensure!(
            !resolved.exists() || fs::read_dir(&resolved)?.next().is_none(),
            "Use a new or empty fixture data root; never reuse an existing product database."
        );
        fs::create_dir_all(&resolved)?;
        Ok(resolved)
    }

    fn bind(port: u16) -> Result<TcpListener> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))?;
        listener.set_nonblocking(true)?;
        Ok(listener)
    }

    pub fn run() -> Result<()> {
        let args: Vec<_> = std::env::args().skip(1).collect();
        let mut root = None;
        let mut seconds = 600u64;
        let mut legacy_mobile = false;
        let mut cursor = 0;
        while cursor < args.len() {
            match args[cursor].as_str() {
                "--data-root" => {
                    cursor += 1;
                    root = Some(
                        args.get(cursor)
                            .context("--data-root requires a path")?
                            .clone(),
                    );
                }
                "--seconds" => {
                    cursor += 1;
                    seconds = args
                        .get(cursor)
                        .context("--seconds requires a number")?
                        .parse()?;
                    ensure!(
                        (1..=1800).contains(&seconds),
                        "--seconds must be between 1 and 1800."
                    );
                }
                "--legacy-mobile" => legacy_mobile = true,
                other => bail!("Unknown argument: {other}"),
            }
            cursor += 1;
        }
        let root = checked_root(&root.context("--data-root is required")?)?;
        let control = bind(CONTROL_PORT).context("Fixture control port is busy")?;
        let tls = bind(TLS_PORT).context("Fixture TLS port is busy")?;
        // This MUST happen before creating the native host, worker threads,
        // clipboard handles, or any user32 windows.
        let station = Box::leak(Box::new(
            PrivateStation::create().map_err(anyhow::Error::msg)?,
        ));
        station.verify_current().map_err(anyhow::Error::msg)?;
        std::env::remove_var("CLIPHOST_TAURI_HEADLESS");
        std::env::remove_var("CLIPHOST_GPUI_TEST_ROOT");
        clipboard::force_headless(false);
        // A brand-new private station starts with sequence zero, which the
        // product correctly treats as unavailable for guarded remote apply.
        // Initialize only this empty test clipboard before monitors start; this
        // fixture does not claim coverage of an uninitialized system clipboard.
        clipboard::write_text("").map_err(anyhow::Error::msg)?;
        station.verify_current().map_err(anyhow::Error::msg)?;
        let initial_clipboard_sequence = clipboard::sequence_number();
        ensure!(
            initial_clipboard_sequence != 0,
            "Private clipboard initialization failed"
        );
        let native = NativeHost::open_with_options(NativeHostOptions {
            data_root: Some(root.clone()),
            api_port: API_PORT,
            headless: false,
            background: true,
            isolated: true,
            verb_rpc: false,
            core_copy_acceptance: false,
        })
        .map_err(anyhow::Error::msg)?;
        let host = native.state();
        host.lan_config_update(&json!({"enabled":true,"port":TLS_PORT}))
            .map_err(anyhow::Error::msg)?;
        if legacy_mobile {
            host.sync_config_update(&json!({"enabled":true,"sendEnabled":true,"receiveEnabled":true}))
                .map_err(anyhow::Error::msg)?;
            let mut invitation = host.sync_invitation().map_err(anyhow::Error::msg)?;
            let url = format!("https://127.0.0.1:{TLS_PORT}/cliplink/sync/v1");
            invitation["httpsUrl"] = json!(url);
            invitation["httpsUrls"] = json!([url]);
            fs::write(root.join("invitation.json"), serde_json::to_vec_pretty(&invitation)?)?;
        }
        fs::write(root.join("lan-pairing.json"), serde_json::to_vec_pretty(&pairing_value(&host))?)?;
        let identity = json!({
            "fixture":"gpui-native-android-sync", "pid":std::process::id(),
            "renderer":"gpui", "host":"gpui-native-host",
            "hostImplementation":"cliphost_gpui::service::NativeHost",
            "privateStation":true,"privateWindowStation":station.name, "nativeClipboard":true,
            "headless":false, "isolated":true, "visibleUi":false,
            "initializedEmptyClipboard":true, "initialClipboardSequence":initial_clipboard_sequence,
            "dataRoot":root, "controlPort":CONTROL_PORT, "tlsPort":TLS_PORT,
            "apiPort":API_PORT, "loopbackOnly":true, "verbRpcEnabled":false,
            "desktopId":host.sync_device_id(),
            "legacyMobileEnabled":legacy_mobile,
        });
        let (stop_tx, mut stopped) = watch::channel(false);
        let state = Arc::new(Fixture {
            host: host.clone(),
            station,
            identity: identity.clone(),
            image_path: root.join("fixture-outgoing.png"),
            clipboard: Arc::new(Semaphore::new(1)),
            shutdown: stop_tx,
        });
        let runtime = native.runtime_handle().map_err(anyhow::Error::msg)?;
        let app = Router::new()
            .route("/fixture/current", get(current).post(write))
            .route("/fixture/identity", get(identify))
            .route("/fixture/pairing", get(pairing))
            .route("/fixture/device-sync", post(device_sync))
            .route("/fixture/forget", post(forget))
            .route("/fixture/shutdown", post(shutdown))
            .layer(DefaultBodyLimit::max(MAX_BODY))
            .with_state(state);
        let control_task = runtime.spawn(async move {
            let listener = tokio::net::TcpListener::from_std(control)?;
            axum::serve(listener, app).await
        });
        let tls_host = host.as_ref().clone();
        let udp_host = host.as_ref().clone();
        let udp_task = runtime.spawn(async move {
            cliphost_http::serve_lan_udp_loopback(cliphost_http::HttpState {
                host: udp_host, events: cliphost_http::event_bus(),
            }, TLS_PORT).await
        });
        let tls_task = runtime.spawn(async move {
            let listener = tokio::net::TcpListener::from_std(tls)?;
            cliphost_http::serve_lan_https_listener(
                cliphost_http::HttpState {
                    host: tls_host,
                    events: cliphost_http::event_bus(),
                },
                listener,
            )
            .await
        });
        let ready_deadline = Instant::now() + Duration::from_secs(10);
        while host.sync_status()["listenerRunning"] != true {
            ensure!(
                !control_task.is_finished() && !tls_task.is_finished() && !udp_task.is_finished(),
                "Fixture listener stopped before readiness."
            );
            ensure!(
                Instant::now() < ready_deadline,
                "Fixture TLS did not become ready."
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        fs::write(
            root.join("identity.json"),
            serde_json::to_vec_pretty(&identity)?,
        )?;
        println!("GPUI NativeHost private-clipboard fixture ready on loopback {CONTROL_PORT}/{TLS_PORT}; API {API_PORT}.");
        let deadline = Instant::now() + Duration::from_secs(seconds);
        while Instant::now() < deadline && !*stopped.borrow_and_update() {
            ensure!(
                !control_task.is_finished() && !tls_task.is_finished() && !udp_task.is_finished(),
                "Fixture listener exited unexpectedly."
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        // Let the HTTP shutdown acknowledgment leave before cancelling servers.
        std::thread::sleep(Duration::from_millis(150));
        host.sync_set_enabled(false).map_err(anyhow::Error::msg)?;
        host.device_sync_config_update(&json!({"enabled":false})).map_err(anyhow::Error::msg)?;
        udp_task.abort();
        tls_task.abort();
        control_task.abort();
        native.shutdown();
        fs::write(
            root.join("stopped.json"),
            serde_json::to_vec_pretty(
                &json!({"ok":true,"pid":std::process::id(),"privateWindowStationRetainedUntilExit":true}),
            )?,
        )?;
        Ok(())
    }

    fn access(state: &Fixture) -> Result<()> {
        private_station::attach(state.station.desktop).map_err(anyhow::Error::msg)?;
        state.station.verify_current().map_err(anyhow::Error::msg)
    }

    // NativeHost's blocking pool can already own user32 windows, so it cannot
    // safely change desktops. Each low-frequency fixture request gets a fresh
    // OS thread, attaches once, and is bounded by one shared permit. Cancelling
    // an HTTP request does not release that permit before its operation finishes.
    async fn clipboard_task(
        state: Arc<Fixture>,
        operation: impl FnOnce(Arc<Fixture>) -> Result<Value> + Send + 'static,
    ) -> Result<Value> {
        let permit = state
            .clipboard
            .clone()
            .try_acquire_owned()
            .context("A fixture clipboard operation is already running")?;
        let worker = std::thread::Builder::new()
            .name("android-fixture-clipboard".into())
            .spawn(move || access(&state).and_then(|()| operation(state)))?;
        // Joining also waits for thread-local owner/reader HWND destruction.
        // The Tokio pool only waits; all clipboard calls stay on the fresh OS
        // thread. Keep the permit until the join finishes even on HTTP timeout.
        let joined = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("Fixture clipboard worker panicked"))?
        });
        tokio::time::timeout(Duration::from_secs(10), joined)
            .await
            .context("Fixture clipboard operation timed out")?
            .context("Fixture clipboard join task exited")?
    }

    fn response(result: Result<Value>) -> HttpResult {
        result.map(Json).map_err(|error| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"error":error.to_string()})),
            )
        })
    }

    async fn identify(State(state): State<Arc<Fixture>>) -> Json<Value> {
        Json(state.identity.clone())
    }

    fn pairing_value(host: &cliphost_core::HostState) -> Value {
        json!({"announcement":host.lan_info(),"baseUrl":format!("https://127.0.0.1:{TLS_PORT}"),
            "pairingCode":host.lan_status()["pairingCode"]["code"]})
    }

    async fn pairing(State(state): State<Arc<Fixture>>) -> HttpResult {
        response(clipboard_task(state, |state| Ok(pairing_value(&state.host))).await)
    }

    async fn device_sync(State(state): State<Arc<Fixture>>, Json(params): Json<Value>) -> HttpResult {
        response(clipboard_task(state, move |state| {
            ensure!(params.as_object().is_some_and(|map| map.len() == 1) && params["enabled"].is_boolean(), "Specify only enabled:boolean.");
            let mut value = state.host.device_sync_config_update(&params).map_err(anyhow::Error::msg)?;
            value["ok"] = json!(true);
            Ok(value)
        }).await)
    }

    async fn forget(State(state): State<Arc<Fixture>>, Json(params): Json<Value>) -> HttpResult {
        response(clipboard_task(state, move |state| {
            ensure!(params.as_object().is_some_and(|map| map.len() == 1) && params["deviceId"].as_str().is_some_and(|id| id.len() <= 128), "Specify only deviceId:string.");
            state.host.lan_forget(&params).map_err(anyhow::Error::msg)
        }).await)
    }

    async fn shutdown(State(state): State<Arc<Fixture>>) -> Json<Value> {
        let _ = state.shutdown.send(true);
        Json(json!({"ok":true}))
    }

    async fn current(State(state): State<Arc<Fixture>>) -> HttpResult {
        response(clipboard_task(state, move |state| -> Result<Value> {
            let snapshot = clipboard::peek_snapshot();
            state.station.verify_current().map_err(anyhow::Error::msg)?;
            let Some(snapshot) = snapshot else { return Ok(json!({"unavailable":true})); };
            Ok(json!({"text":snapshot.text,"origin":snapshot.sync_origin,
                "pngBase64":snapshot.png.map(|bytes|STANDARD.encode(bytes)),"files":snapshot.files}))
        }).await)
    }

    // A screenshot/image copy has bitmap formats, not CF_HDROP. The product's
    // record-copy path intentionally includes a file fallback, which automatic
    // sync correctly rejects. Keep this synthetic source writer in the fixture.
    fn write_native_image(station: &PrivateStation, png: &[u8], dib: &[u8]) -> Result<()> {
        use windows_compat::{
            core::w,
            Win32::{
                Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND},
                System::{
                    DataExchange::{
                        CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW,
                        SetClipboardData,
                    },
                    Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE},
                },
                UI::WindowsAndMessaging::{
                    CreateWindowExW, DestroyWindow, HWND_MESSAGE, WINDOW_EX_STYLE, WINDOW_STYLE,
                },
            },
        };
        struct Memory(HGLOBAL);
        impl Memory {
            fn allocate(bytes: &[u8]) -> Result<Self> {
                unsafe {
                    let memory = Self(GlobalAlloc(GMEM_MOVEABLE, bytes.len())?);
                    let pointer = GlobalLock(memory.0);
                    ensure!(!pointer.is_null(), "Cannot lock fixture clipboard memory");
                    std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer.cast(), bytes.len());
                    let _ = GlobalUnlock(memory.0);
                    Ok(memory)
                }
            }
            fn publish(self, format: u32) -> Result<()> {
                unsafe {
                    SetClipboardData(format, HANDLE(self.0 .0))?;
                }
                std::mem::forget(self); // Windows owns successfully published memory.
                Ok(())
            }
        }
        impl Drop for Memory {
            fn drop(&mut self) {
                unsafe {
                    let _ = GlobalFree(self.0);
                }
            }
        }
        struct Owner(HWND);
        impl Drop for Owner {
            fn drop(&mut self) {
                unsafe {
                    let _ = DestroyWindow(self.0);
                }
            }
        }
        struct Close;
        impl Drop for Close {
            fn drop(&mut self) {
                unsafe {
                    let _ = CloseClipboard();
                }
            }
        }
        station.verify_current().map_err(anyhow::Error::msg)?;
        let png = Memory::allocate(png)?;
        let dib = Memory::allocate(dib)?;
        let png_format = unsafe { RegisterClipboardFormatW(w!("PNG")) };
        ensure!(
            png_format != 0,
            "Cannot register fixture PNG clipboard format"
        );
        let owner = Owner(unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                w!("Android fixture image source"),
                WINDOW_STYLE::default(),
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                None,
                None,
                None,
            )?
        });
        let mut opened = false;
        for _ in 0..10 {
            if unsafe { OpenClipboard(owner.0) }.is_ok() {
                opened = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        ensure!(opened, "Fixture clipboard is busy");
        let _close = Close;
        station.verify_current().map_err(anyhow::Error::msg)?;
        unsafe {
            EmptyClipboard()?;
        }
        dib.publish(8)?; // CF_DIB
        png.publish(png_format)?;
        station.verify_current().map_err(anyhow::Error::msg)?;
        Ok(())
    }

    async fn write(State(state): State<Arc<Fixture>>, Json(params): Json<Value>) -> HttpResult {
        response(
            clipboard_task(state, move |state| -> Result<Value> {
                ensure!(params.is_object(), "A JSON object is required.");
                ensure!(
                    params.get("text").is_some() ^ params.get("pngBase64").is_some(),
                    "Specify exactly one text or pngBase64 payload."
                );
                if let Some(text) = params.get("text") {
                    let text = text.as_str().context("text must be a string")?;
                    ensure!(
                        text.len() <= 1024 * 1024 && !text.contains('\0'),
                        "Invalid or oversized fixture text."
                    );
                    state.station.verify_current().map_err(anyhow::Error::msg)?;
                    clipboard::write_text(text).map_err(anyhow::Error::msg)?;
                } else {
                    let encoded = params["pngBase64"]
                        .as_str()
                        .context("pngBase64 must be a string")?;
                    let bytes = STANDARD.decode(encoded)?;
                    ensure!(
                        bytes.len() <= MAX_IMAGE && bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
                        "Fixture image must be PNG, at most 4 MiB."
                    );
                    let (width, height) = image::ImageReader::with_format(
                        Cursor::new(&bytes),
                        image::ImageFormat::Png,
                    )
                    .into_dimensions()?;
                    ensure!(
                        width > 0
                            && height > 0
                            && u64::from(width) * u64::from(height) <= 4_000_000,
                        "Fixture image dimensions exceed the limit."
                    );
                    // Complete validation before replacing either the source file
                    // or clipboard; the single fixture file bounds temporary bytes.
                    image::load_from_memory_with_format(&bytes, image::ImageFormat::Png)?;
                    fs::write(&state.image_path, &bytes)?;
                    let dib = clipboard::encode_dib_from_path(&state.image_path.to_string_lossy())
                        .map_err(anyhow::Error::msg)?;
                    write_native_image(state.station, &bytes, &dib)?;
                }
                state.station.verify_current().map_err(anyhow::Error::msg)?;
                Ok(json!({"ok":true,"sequence":clipboard::sequence_number()}))
            })
            .await,
        )
    }
}
