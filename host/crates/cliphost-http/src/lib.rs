use std::net::SocketAddr;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, on, post, MethodFilter};
use axum::{Json, Router};
use cliphost_bridge::{handle_request, HostPushEnvelope};
use cliphost_core::HostState;
use serde::Deserialize;
use serde_json::Value;
use futures_util::StreamExt;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;

mod lan_tls;
mod loopback_access;
#[cfg(all(test, feature = "webdav-sync-dev", debug_assertions))]
mod webdav_dev_tests;
#[cfg(feature = "private-sync")]
mod mobile_sync;
#[cfg(feature = "private-sync")]
mod device_sync_pull;
#[cfg(feature = "private-sync")]
mod relay_client;
#[cfg(all(test, feature = "private-sync"))]
mod mobile_sync_tests;
#[cfg(test)]
mod device_sync_integration_tests;
#[cfg(all(test, feature = "private-sync"))]
mod device_sync_pull_tests;
#[cfg(all(test, windows))]
mod lan_interop_tests;

pub use lan_tls::{serve_lan_https, serve_lan_https_listener};
#[cfg(all(debug_assertions, feature = "private-sync"))]
pub use relay_client::start_fixture as start_relay_fixture;

const ICON_CACHE_CONTROL: &str = "private, max-age=86400";
const PRIVATE_NETWORK_HEADER: HeaderName =
    HeaderName::from_static("access-control-allow-private-network");
const CORS_ALLOW_METHODS: HeaderValue =
    HeaderValue::from_static("GET, POST, DELETE, HEAD, OPTIONS");
const CORS_ALLOW_HEADERS: HeaderValue =
    HeaderValue::from_static("Content-Type, X-Clip-Web-Session");
const JSON_CONTENT_TYPE: HeaderValue =
    HeaderValue::from_static("application/json; charset=utf-8");

#[derive(Clone)]
pub struct HttpState {
    pub host: HostState,
    pub events: broadcast::Sender<String>,
}

pub fn event_bus() -> broadcast::Sender<String> {
    let (tx, _rx) = broadcast::channel(256);
    tx
}

pub fn router(state: HttpState) -> Router {
    #[cfg(all(feature = "webdav-sync-dev", debug_assertions))]
    state.host.start_webdav_dev_worker();
    #[cfg(feature = "private-sync")]
    relay_client::start(state.host.clone());
    let router = Router::new()
        .route("/api/rpc", post(rpc))
        .route("/api/events", get(events))
        .route("/api/health", get(health))
        .route("/api/health/app-info", get(app_info))
        .route("/api/health/data-paths", get(data_paths))
        .route("/api/health/latest-log", get(latest_log))
        .route("/api/health/restart", post(restart))
        .route("/api/health/cloud-sync", get(cloud_sync))
        .route("/api/health/quicker", get(quicker))
        .route("/api/clip/pin-overlay/status", get(pin_overlay_status))
        .route("/api/clip/path-existence", get(path_existence))
        .route("/api/clip/process-filter-presets", get(process_filter_presets_get).post(process_filter_presets_save))
        .route("/api/clip/process-filter-presets/import", post(process_filter_presets_import))
        .route("/api/clip/items/{id}/image", get(clip_image).head(clip_image))
        .route("/api/clip/favorites/{id}/image", get(favor_image).head(favor_image))
        .route("/api/clip/local-image", get(local_image).head(local_image))
        .route("/api/clip/file-icon", get(file_icon))
        // C# `HandleWindowBackgroundImage` is GET only; axum `get()` also matches HEAD.
        .route(
            "/api/clip/ui/window-background-image",
            on(MethodFilter::GET, window_background),
        )
        .route("/api/clip/ui/pick-folder", post(pick_folder))
        .route("/api/clip/ui/pick-files", post(pick_files))
        .route("/api/clip/ui/pick-directories", post(pick_directories))
        .route("/api/clip/collect/drafts/{draftId}", get(draft_get).delete(draft_discard))
        .route("/api/clip/collect/drafts/{draftId}/commit", post(draft_commit))
        .route("/api/host/verb", post(verb))
        .route("/api/clip/ole-drag", post(ole_drag))
        .route("/api/clip/host-window", post(host_window))
        .route("/api/clip/quick-paste", post(quick_paste))
        .route("/api/clip/toast", post(clip_toast))
        .route("/api/clip/super-menu", post(super_menu))
        .route("/api/clip/chrome", post(clip_chrome))
        .route("/api/icons/fa-vector", get(fa_vector))
        .route("/api/menu/sessions", post(menu_create))
        .route("/api/menu/sessions/{session_id}", delete(menu_close))
        .route(
            "/api/menu/sessions/{session_id}/nodes/{node_id}/children",
            post(menu_children),
        )
        .route(
            "/api/menu/sessions/{session_id}/nodes/{node_id}/context-menu",
            post(menu_context),
        )
        .route(
            "/api/menu/sessions/{session_id}/nodes/{node_id}/invoke",
            post(menu_invoke),
        )
        .route(
            "/api/menu/sessions/{session_id}/nodes/{node_id}/value",
            post(menu_set_value),
        )
        .route("/api/menu/window-thumbnail", get(menu_thumbnail))
        .route(
            "/api/file-menu/custom-paths",
            get(file_menu_paths).put(file_menu_put),
        )
        .route("/api/file-menu/custom-paths/add", post(file_menu_add))
        .route("/api/file-menu/custom-paths/remove", post(file_menu_remove))
        .route("/api/file-menu/custom-paths/move", post(file_menu_move))
        .route(
            "/api/file-menu/custom-paths/pick-folders",
            post(file_menu_pick),
        )
        .route("/api/clip/ui/pick-image-file", post(pick_image_file))
        .route(
            "/api/clip/ui/stage-window-background-image",
            post(stage_background),
        );
    let router = attach_private_sync_routes(router);
    let router = attach_webdav_dev_routes(router);
    attach_csharp_debug_routes(router)
        .fallback(unknown_loopback)
        .with_state(state)
        .layer(tower_http::catch_panic::CatchPanicLayer::custom(
            csharp_loopback_panic_response,
        ))
        .layer(middleware::from_fn(loopback_browser_boundary))
}

fn attach_webdav_dev_routes(router: Router<HttpState>) -> Router<HttpState> {
    #[cfg(all(feature = "webdav-sync-dev", debug_assertions))]
    { return router.route("/api/dev/webdav", get(webdav_dev_status).post(webdav_dev_command)); }
    #[cfg(not(all(feature = "webdav-sync-dev", debug_assertions)))]
    { router }
}

#[cfg(all(feature = "webdav-sync-dev", debug_assertions))]
async fn webdav_dev_status(State(state): State<HttpState>) -> Json<Value> {
    Json(state.host.webdav_dev_status())
}

#[cfg(all(feature = "webdav-sync-dev", debug_assertions))]
async fn webdav_dev_command(State(state): State<HttpState>, body: Bytes) -> Response {
    let request=match parse_loopback_json_or_empty(&body) {
        Ok(Some(value))=>value,
        _=>return (StatusCode::BAD_REQUEST,Json(serde_json::json!({"error":"WebDAV dev 操作需要 JSON 请求。"}))).into_response(),
    };
    blocking_host_work(move || {
        match state.host.webdav_dev_command(&request) {
            Ok(value)=>{flush_host_events(&state);Json(value).into_response()},
            Err(error)=>{flush_host_events(&state);(StatusCode::BAD_REQUEST,Json(serde_json::json!({"error":error}))).into_response()},
        }
    }).await
}

/// Private transport handlers do not exist in the normal public build.
fn attach_private_sync_routes(router: Router<HttpState>) -> Router<HttpState> {
    #[cfg(feature = "private-sync")]
    { return router
        .route("/api/clip/mobile-sync", get(mobile_sync_status).post(mobile_sync_config))
        .route("/api/clip/mobile-sync/invitation", get(mobile_sync_invitation))
        .route("/api/clip/mobile-sync/clients/{deviceId}", delete(mobile_sync_forget)); }
    #[cfg(not(feature = "private-sync"))]
    { router }
}

/// C# `ClipHostLoopbackRequestHandler` `#if DEBUG` `/api/debug/*` routes only.
fn attach_csharp_debug_routes(router: Router<HttpState>) -> Router<HttpState> {
    #[cfg(debug_assertions)]
    {
        router
            .route("/api/debug/window-chrome/open", post(debug_chrome_open))
            .route("/api/debug/quick-paste/status", get(qp_status))
            .route("/api/debug/quick-paste/preview", post(qp_preview))
            .route("/api/debug/quick-paste/end", post(qp_end))
            .route("/api/debug/toast", post(debug_toast))
            .route("/api/debug/toast/collapse", post(debug_toast_collapse))
    }
    #[cfg(not(debug_assertions))]
    {
        router
    }
}

fn apply_loopback_cors(response: &mut Response, origin: Option<&HeaderValue>) {
    let headers = response.headers_mut();
    headers.append(header::VARY, HeaderValue::from_static("Origin"));
    if let Some(origin) = origin {
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
        headers.insert(header::ACCESS_CONTROL_ALLOW_METHODS, CORS_ALLOW_METHODS.clone());
        headers.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, CORS_ALLOW_HEADERS.clone());
        headers.insert(PRIVATE_NETWORK_HEADER, HeaderValue::from_static("true"));
    }
}

fn apply_csharp_json_charset(response: &mut Response) {
    let Some(current) = response.headers().get(header::CONTENT_TYPE) else {
        return;
    };
    let Ok(value) = current.to_str() else {
        return;
    };
    if value == "application/json" {
        response
            .headers_mut()
            .insert(header::CONTENT_TYPE, JSON_CONTENT_TYPE.clone());
    }
}

async fn loopback_browser_boundary(req: Request, next: Next) -> Response {
    // Check before dispatch, including simple requests that do not preflight.
    // CORS response headers alone cannot prevent a hostile site's RPC writes.
    let origin = match loopback_access::check_request(&req, cfg!(debug_assertions)) {
        Ok(origin) => origin,
        Err(()) => {
            let mut response = (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({"error":"Untrusted loopback request."})),
            ).into_response();
            apply_loopback_cors(&mut response, None);
            return response;
        }
    };
    if req.method() == Method::OPTIONS {
        let mut response = StatusCode::NO_CONTENT.into_response();
        apply_loopback_cors(&mut response, origin.as_ref());
        return response;
    }
    let path = req.uri().path().to_string();
    let mut response = next.run(req).await;
    // C# loopback has no 405: unmatched method falls through TryHandleAsync.
    // `/api/menu/*` unmatched → empty 404 (`ClipHostLoopbackHandlerResult.NotFound`);
    // other paths → 404 `{ error: "Not found" }`.
    if response.status() == StatusCode::METHOD_NOT_ALLOWED {
        response = csharp_unmatched_not_found(&path);
    } else if is_axum_json_extractor_rejection(&response) {
        // C# `HostJson.Deserialize` throw → `ProcessRequestAsync` 500 `{ error: ex.Message }`.
        // axum `JsonSyntaxError` is 400 text/plain; `JsonDataError` is 422 text/plain.
        response = csharp_json_parse_failure(response).await;
    }
    apply_csharp_json_charset(&mut response);
    apply_loopback_cors(&mut response, origin.as_ref());
    response
}

fn is_axum_json_extractor_rejection(response: &Response) -> bool {
    let status = response.status();
    if status != StatusCode::BAD_REQUEST && status != StatusCode::UNPROCESSABLE_ENTITY {
        return false;
    }
    !response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.to_ascii_lowercase().starts_with("application/json"))
}

fn empty_not_found() -> Response {
    StatusCode::NOT_FOUND.into_response()
}

fn is_csharp_menu_empty_not_found(path: &str) -> bool {
    const PREFIX: &str = "/api/menu/";
    path.len() >= PREFIX.len() && path[..PREFIX.len()].eq_ignore_ascii_case(PREFIX)
}

fn csharp_unmatched_not_found(path: &str) -> Response {
    if is_csharp_menu_empty_not_found(path) {
        return empty_not_found();
    }
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": "Not found" })),
    )
        .into_response()
}

/// C# unmatched loopback: menu prefix is empty 404; otherwise JSON `{ error: "Not found" }`.
async fn unknown_loopback(uri: axum::http::Uri) -> Response {
    csharp_unmatched_not_found(uri.path())
}

/// C# `ClipHostLoopbackHttpServer` outer `catch (Exception ex)` → 500 `{ error: ex.Message }`.
fn csharp_loopback_panic_response(err: Box<dyn std::any::Any + Send>) -> Response {
    let message = panic_payload_message(err);
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": message })),
    )
        .into_response()
}

fn panic_payload_message(err: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = err.downcast_ref::<String>() {
        s.clone()
    } else if let Some(s) = err.downcast_ref::<&str>() {
        (*s).to_string()
    } else {
        "Internal Server Error".into()
    }
}

async fn csharp_json_parse_failure(response: Response) -> Response {
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap_or_default();
    let message = String::from_utf8_lossy(&bytes).trim().to_string();
    let message = if message.is_empty() {
        "Invalid JSON.".to_string()
    } else {
        message
    };
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": message })),
    )
        .into_response()
}

/// C# `HandleHostVerb` / `DeserializeJsonOrDefault`: blank body → none; invalid JSON bubbles to 500.
fn parse_loopback_json_or_empty(bytes: &Bytes) -> Result<Option<Value>, String> {
    let text = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
    if text.trim().is_empty() {
        Ok(None)
    } else {
        serde_json::from_str(text).map(Some).map_err(|e| e.to_string())
    }
}

fn loopback_json_body(bytes: &Bytes) -> Result<Value, Response> {
    match parse_loopback_json_or_empty(bytes) {
        Ok(Some(value)) => Ok(value),
        Ok(None) => Ok(Value::Null),
        Err(err) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": err })),
        )
            .into_response()),
    }
}

pub fn bind_loopback(port: u16) -> Result<std::net::TcpListener, std::io::Error> {
    let listener = std::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port)))?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

/// Bind the preferred port, or the next free loopback port, without dropping the listener.
pub fn bind_loopback_with_fallback(
    start: u16,
) -> Result<(u16, std::net::TcpListener), std::io::Error> {
    let forbidden = [
        cliphost_core::paths::QUICKER_MCP_PORT,
        cliphost_core::paths::CSHARP_DEFAULT_API_PORT,
        cliphost_core::paths::GPUI_IPC_PORT,
    ];
    let mut last_err: Option<std::io::Error> = None;
    for port in start..start.saturating_add(64) {
        if forbidden.contains(&port) {
            continue;
        }
        match bind_loopback(port) {
            Ok(listener) => return Ok((port, listener)),
            Err(err) => last_err = Some(err),
        }
    }
    Err(last_err.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            "no ClipHost Tauri loopback port available",
        )
    }))
}

pub async fn serve_listener(
    state: HttpState,
    listener: std::net::TcpListener,
) -> Result<(), std::io::Error> {
    let listener = tokio::net::TcpListener::from_std(listener)?;
    tracing::info!(
        "ClipHost Tauri loopback listening on {}",
        listener.local_addr()?
    );
    axum::serve(listener, router(state)).await
}

pub async fn serve(state: HttpState, port: u16) -> Result<(), std::io::Error> {
    let (_bound, listener) = bind_loopback_with_fallback(port)?;
    serve_listener(state, listener).await
}

async fn rpc(State(state): State<HttpState>, body: String) -> Response {
    // C# `HandleClipConfigUpdate` / `HandleWindowHide` return `{ ok: true }` and do not
    // broadcast `clipChanged`; only `flush_host_events` from DynamicData-style host pushes.
    blocking_host_work(move || {
        let response = handle_request(&state.host, &body);
        flush_host_events(&state);
        Json(serde_json::to_value(response).unwrap_or(serde_json::json!({"ok": false}))).into_response()
    }).await
}

#[cfg(feature = "private-sync")]
async fn mobile_sync_status(State(state): State<HttpState>) -> Response {
    blocking_host_work(move || Json(state.host.sync_status()).into_response()).await
}

async fn process_filter_presets_get(State(state): State<HttpState>) -> Response {
    blocking_host_work(move || match state.host.process_filter_presets_get() {
        Ok(value) => Json(value).into_response(),
        Err(error) => (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error":error}))).into_response(),
    }).await
}
async fn process_filter_presets_save(State(state): State<HttpState>, Json(params): Json<Value>) -> Response {
    blocking_host_work(move || {
        let result = state.host.process_filter_presets_save(&params);
        flush_host_events(&state);
        match result {
            Ok(value) => Json(value).into_response(),
            Err(error) => (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error":error}))).into_response(),
        }
    }).await
}
async fn process_filter_presets_import(State(state): State<HttpState>, Json(params): Json<Value>) -> Response {
    blocking_host_work(move || {
        let result = state.host.process_filter_presets_import(&params);
        flush_host_events(&state);
        match result {
            Ok(value) => Json(value).into_response(),
            Err(error) => (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error":error}))).into_response(),
        }
    }).await
}
#[cfg(feature = "private-sync")]
async fn mobile_sync_config(State(state): State<HttpState>, Json(params): Json<Value>) -> Response {
    blocking_host_work(move || {
        let result = state.host.sync_config_update(&params);
        flush_host_events(&state);
        match result { Ok(value) => Json(value).into_response(), Err(error) => (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error":error}))).into_response() }
    }).await
}
#[cfg(feature = "private-sync")]
async fn mobile_sync_invitation(State(state): State<HttpState>) -> Response {
    blocking_host_work(move || match state.host.sync_invitation() {
        Ok(value) => Json(value).into_response(), Err(error) => (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error":error}))).into_response()
    }).await
}
#[cfg(feature = "private-sync")]
async fn mobile_sync_forget(State(state): State<HttpState>, Path(device): Path<String>) -> Response {
    blocking_host_work(move || {
        let result = state.host.sync_forget_client(&device);
        flush_host_events(&state);
        match result { Ok(()) => Json(serde_json::json!({"ok":true})).into_response(), Err(error) => (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error":error}))).into_response() }
    }).await
}

async fn events(State(state): State<HttpState>) -> Response {
    let rx = state.events.subscribe();
    let events = BroadcastStream::new(rx).filter_map(|msg| async move {
        match msg {
            Ok(body) => Some(Ok::<Event, std::convert::Infallible>(Event::default().data(body))),
            Err(_) => None,
        }
    });
    let stream = futures_util::stream::once(std::future::ready(Ok::<Event, std::convert::Infallible>(
        Event::default().comment("connected"),
    )))
    .chain(events);
    let mut response = (
        [(header::CACHE_CONTROL, "no-cache")],
        Sse::new(stream).keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(25))
                .text("ping"),
        ),
    )
        .into_response();
    // C# `HandleEventsRequestAsync` Content-Type `text/event-stream; charset=utf-8`.
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream; charset=utf-8"),
    );
    response
}

async fn health(State(state): State<HttpState>) -> Json<Value> {
    Json(state.host.health())
}

async fn app_info(State(state): State<HttpState>) -> Response {
    blocking_host_work(move || {
        let info = state.host.app_info();
        #[cfg(feature = "private-sync")]
        let mut info = info;
        #[cfg(feature = "private-sync")]
        { info["relaySync"] = state.host.relay_status(); }
        Json(info).into_response()
    }).await
}

async fn data_paths(State(state): State<HttpState>) -> Response {
    blocking_host_work(move || Json(state.host.data_paths()).into_response()).await
}

async fn latest_log(State(state): State<HttpState>) -> Response {
    blocking_host_work(move || match state.host.latest_log() {
        Ok(body) => Json(body).into_response(),
        Err(err) => (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": err }))).into_response(),
    }).await
}

async fn restart(State(state): State<HttpState>, body: Option<Json<Value>>) -> Response {
    let prefer_medium = body
        .as_ref()
        .and_then(|Json(v)| {
            v.get("preferMediumIntegrityReplacement")
                .or_else(|| v.get("PreferMediumIntegrityReplacement"))
                .and_then(|x| x.as_bool())
        })
        .unwrap_or(false);
    blocking_host_work(move || Json(state.host.restart(prefer_medium)).into_response()).await
}

#[derive(Deserialize, Default)]
struct CloudSyncQuery {
    #[serde(default, alias = "includeManifest")]
    include_manifest: Option<String>,
}

async fn cloud_sync(State(state): State<HttpState>, Query(query): Query<CloudSyncQuery>) -> Json<Value> {
    let include = query
        .include_manifest
        .as_deref()
        .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
    // A plugin can remain connected while its cloud request waits. Keep the
    // request executor free for window actions, cancellation and progress.
    Json(tokio::task::spawn_blocking(move || state.host.health_cloud_sync_probe(include))
        .await.unwrap_or_else(|error| serde_json::json!({"ok":false,"error":error.to_string()})))
}

async fn pin_overlay_status(State(state): State<HttpState>) -> Response {
    blocking_host_work(move || Json(state.host.pin_overlay_status()).into_response()).await
}

#[derive(Deserialize)]
struct PathExistenceQuery {
    path: String,
}

async fn path_existence(State(state): State<HttpState>, Query(query): Query<PathExistenceQuery>) -> Response {
    blocking_host_work(move || Json(
        state
            .host
            .path_exists(&serde_json::json!({ "path": query.path }))
            .unwrap_or_else(|e| serde_json::json!({ "ok": false, "exists": false, "error": e })),
    ).into_response()).await
}

async fn quicker(State(state): State<HttpState>) -> Response {
    blocking_host_work(move || Json(state.host.quicker_status()).into_response()).await
}

async fn clip_image(method: Method, State(state): State<HttpState>, Path(id): Path<String>) -> Response {
    blocking_host_work(move || match state.host.image_bytes(&id) {
        Ok((bytes, mime)) => binary_or_head(method, &mime, bytes),
        Err(_) => empty_not_found(),
    }).await
}

async fn favor_image(method: Method, State(state): State<HttpState>, Path(id): Path<String>) -> Response {
    blocking_host_work(move || match state.host.favor_image_bytes(&id) {
        Ok((bytes, mime)) => binary_or_head(method, &mime, bytes),
        Err(_) => empty_not_found(),
    }).await
}

#[derive(Deserialize)]
struct LocalImageQuery {
    path: String,
}

#[derive(Deserialize, Default)]
struct FileIconQuery {
    path: Option<String>,
    proc: Option<String>,
    spec: Option<String>,
}

#[derive(Deserialize)]
struct ThumbQuery {
    hwnd: i64,
    #[serde(default)]
    w: Option<i32>,
}

async fn local_image(
    method: Method,
    State(state): State<HttpState>,
    Query(query): Query<LocalImageQuery>,
    headers: HeaderMap,
) -> Response {
    blocking_host_work(move || match state.host.local_image_bytes(&query.path) {
        Some((bytes, mime)) => {
            binary_or_head_ranged(method, &mime, bytes, headers.get(header::RANGE))
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }).await
}

fn binary_or_head(method: Method, mime: &str, bytes: Vec<u8>) -> Response {
    if method == Method::HEAD {
        (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, mime.to_string()),
                (header::CONTENT_LENGTH, bytes.len().to_string()),
            ],
        )
            .into_response()
    } else {
        (StatusCode::OK, [(header::CONTENT_TYPE, mime.to_string())], bytes).into_response()
    }
}

/// C# `PhysicalFileResult.EnableRangeProcessing` for local-image / window-background only.
fn binary_or_head_ranged(
    method: Method,
    mime: &str,
    bytes: Vec<u8>,
    range: Option<&HeaderValue>,
) -> Response {
    let total = bytes.len() as u64;
    match parse_bytes_range(range, total) {
        ByteRange::Full => {
            let headers = [
                (header::CONTENT_TYPE, mime.to_string()),
                (header::ACCEPT_RANGES, "bytes".into()),
                (header::CONTENT_LENGTH, total.to_string()),
            ];
            if method == Method::HEAD {
                (StatusCode::OK, headers).into_response()
            } else {
                (StatusCode::OK, headers, bytes).into_response()
            }
        }
        ByteRange::Partial {
            start,
            end_inclusive,
        } => {
            let from = start as usize;
            let to = (end_inclusive as usize).saturating_add(1).min(bytes.len());
            let slice = bytes[from..to].to_vec();
            let headers = [
                (header::CONTENT_TYPE, mime.to_string()),
                (header::ACCEPT_RANGES, "bytes".into()),
                (
                    header::CONTENT_RANGE,
                    format!("bytes {start}-{end_inclusive}/{total}"),
                ),
                (header::CONTENT_LENGTH, slice.len().to_string()),
            ];
            if method == Method::HEAD {
                (StatusCode::PARTIAL_CONTENT, headers).into_response()
            } else {
                (StatusCode::PARTIAL_CONTENT, headers, slice).into_response()
            }
        }
        ByteRange::Unsatisfiable => (
            StatusCode::RANGE_NOT_SATISFIABLE,
            [
                (header::ACCEPT_RANGES, "bytes".into()),
                (header::CONTENT_RANGE, format!("bytes */{total}")),
            ],
        )
            .into_response(),
    }
}

enum ByteRange {
    Full,
    Partial { start: u64, end_inclusive: u64 },
    Unsatisfiable,
}

fn parse_bytes_range(range: Option<&HeaderValue>, total: u64) -> ByteRange {
    let Some(raw) = range.and_then(|v| v.to_str().ok()) else {
        return ByteRange::Full;
    };
    let Some(spec) = raw.trim().strip_prefix("bytes=") else {
        return ByteRange::Full;
    };
    let spec = spec.split(',').next().unwrap_or("").trim();
    if spec.is_empty() {
        return ByteRange::Unsatisfiable;
    }
    if let Some(suffix) = spec.strip_prefix('-') {
        let Ok(n) = suffix.parse::<u64>() else {
            return ByteRange::Unsatisfiable;
        };
        if n == 0 || total == 0 {
            return ByteRange::Unsatisfiable;
        }
        let n = n.min(total);
        return ByteRange::Partial {
            start: total - n,
            end_inclusive: total - 1,
        };
    }
    let mut parts = spec.splitn(2, '-');
    let Some(start_raw) = parts.next() else {
        return ByteRange::Unsatisfiable;
    };
    let Ok(start) = start_raw.parse::<u64>() else {
        return ByteRange::Unsatisfiable;
    };
    if total == 0 || start >= total {
        return ByteRange::Unsatisfiable;
    }
    let end_inclusive = match parts.next() {
        Some("") | None => total - 1,
        Some(end_raw) => {
            let Ok(end) = end_raw.parse::<u64>() else {
                return ByteRange::Unsatisfiable;
            };
            if end < start {
                return ByteRange::Unsatisfiable;
            }
            end.min(total - 1)
        }
    };
    ByteRange::Partial {
        start,
        end_inclusive,
    }
}

async fn file_icon(State(state): State<HttpState>, Query(query): Query<FileIconQuery>) -> Response {
    blocking_host_work(move || match state.host.file_icon_png(
        query.path.as_deref(),
        query.proc.as_deref(),
        query.spec.as_deref(),
    ) {
        Ok(bytes) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "image/png"),
                (header::CACHE_CONTROL, ICON_CACHE_CONTROL),
            ],
            bytes,
        )
            .into_response(),
        Err(err) if err == "path, proc, or spec is required." || err == "spec is not a shell/file icon path." => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": err })),
        )
            .into_response(),
        Err(_) => empty_not_found(),
    }).await
}

async fn window_background(
    method: Method,
    State(state): State<HttpState>,
    headers: HeaderMap,
) -> Response {
    blocking_host_work(move || match state.host.window_background_bytes() {
        Some((bytes, mime)) => {
            binary_or_head_ranged(method, &mime, bytes, headers.get(header::RANGE))
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }).await
}

async fn pick_folder(State(state): State<HttpState>, body: Bytes) -> Response {
    let params = match loopback_json_body(&body) {
        Ok(value) => value,
        Err(response) => return response,
    };
    blocking_host_work(move || Json(state.host.pick_folder(&params)
        .unwrap_or_else(|e| serde_json::json!({ "error": e }))).into_response()).await
}

async fn pick_files(State(state): State<HttpState>, body: Bytes) -> Response {
    let params = match loopback_json_body(&body) {
        Ok(value) => value,
        Err(response) => return response,
    };
    blocking_host_work(move || Json(state.host.pick_files(&params)
        .unwrap_or_else(|e| serde_json::json!({ "error": e }))).into_response()).await
}

async fn pick_directories(State(state): State<HttpState>, body: Bytes) -> Response {
    let params = match loopback_json_body(&body) {
        Ok(value) => value,
        Err(response) => return response,
    };
    blocking_host_work(move || Json(state.host.pick_directories(&params)
        .unwrap_or_else(|e| serde_json::json!({ "error": e }))).into_response()).await
}

async fn draft_get(State(state): State<HttpState>, Path(draft_id): Path<String>) -> Response {
    blocking_host_work(move || match state.host.collect_get(&serde_json::json!({ "draftId": draft_id })) {
        Ok(body) => Json(body).into_response(),
        Err(_) => empty_not_found(),
    }).await
}

async fn draft_discard(State(state): State<HttpState>, Path(draft_id): Path<String>) -> Response {
    blocking_host_work(move || {
        let _ = state.host.collect_discard(&serde_json::json!({ "draftId": draft_id }));
        StatusCode::NO_CONTENT.into_response()
    }).await
}

async fn draft_commit(
    State(state): State<HttpState>,
    Path(draft_id): Path<String>,
    body: Bytes,
) -> Response {
    let mut params = match loopback_json_body(&body) {
        Ok(value) => value,
        Err(response) => return response,
    };
    if let Value::Object(map) = &mut params {
        map.insert("draftId".into(), serde_json::json!(draft_id));
    } else {
        params = serde_json::json!({ "draftId": draft_id });
    }
    blocking_host_work(move || match state.host.collect_commit(&params) {
        Ok(_) => {
            publish(
                &state.events,
                HostPushEnvelope::event("favoritesChanged", serde_json::json!({ "ts": chrono::Utc::now() })),
            );
            StatusCode::NO_CONTENT.into_response()
        }
        Err(err) if err == "Draft not found or expired." => empty_not_found(),
        Err(err) => (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": err }))).into_response(),
    }).await
}

async fn ole_drag(State(state): State<HttpState>, Json(body): Json<Value>) -> Response {
    match tokio::task::spawn_blocking(move || state.host.begin_ole_drag(&body)).await {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(err)) => (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": err }))).into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": err.to_string() })),
        )
            .into_response(),
    }
}

async fn host_window(State(state): State<HttpState>, Json(body): Json<Value>) -> Response {
    // Short state transitions can still wait behind another state owner. Never
    // let repeated window requests occupy every async worker during that wait.
    blocking_host_work(move || host_window_response(&state.host, &body)).await
}

fn host_window_response(state: &HostState, body: &Value) -> Response {
    let action = body
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let value = body.get("value").and_then(|v| v.as_bool()).unwrap_or(false);
    let host = body
        .get("host")
        .and_then(|v| v.as_str())
        .unwrap_or("clipboard");
    if action == "escapeUnhandled" && host.eq_ignore_ascii_case("clipboard") {
        if body.get("escapeInputToken").is_some_and(|value| !value.is_string()) {
            return Json(state.window_state_dto()).into_response();
        }
        return Json(state.clipboard_escape_unhandled(
            body.get("escapeToken").and_then(Value::as_str),
            body.get("escapeInputToken").and_then(Value::as_str),
        )).into_response();
    }
    Json(state.apply_window_action_for(action, value, host)).into_response()
}

async fn quick_paste(State(state): State<HttpState>, Json(body): Json<Value>) -> Response {
    blocking_host_work(move || {
        let result = state.host.qp_web_action(&body);
        flush_host_events(&state);
        Json(result).into_response()
    }).await
}

async fn clip_toast(State(state): State<HttpState>, Json(body): Json<Value>) -> Response {
    blocking_host_work(move || {
        let result = state.host.toast_web_action(&body);
        flush_host_events(&state);
        Json(result).into_response()
    }).await
}

async fn super_menu(State(state): State<HttpState>, Json(body): Json<Value>) -> Response {
    blocking_host_work(move || {
        let result = state.host.super_menu_web_action(&body);
        flush_host_events(&state);
        Json(result).into_response()
    }).await
}

async fn clip_chrome(State(state): State<HttpState>, Json(body): Json<Value>) -> Response {
    blocking_host_work(move || {
        let result = state.host.chrome_web_action(&body);
        flush_host_events(&state);
        Json(result).into_response()
    }).await
}

async fn verb(State(state): State<HttpState>, body: Bytes) -> Response {
    let body = match parse_loopback_json_or_empty(&body) {
        Ok(Some(value)) => value,
        Ok(None) => Value::Null,
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": err })),
            )
                .into_response();
        }
    };
    let line = body
        .get("line")
        .or_else(|| body.get("Line"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if line.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "line is required" })),
        )
            .into_response();
    }
    let ctx = cliphost_core::verb::VerbRunContext::from_json(
        body.get("context")
            .or_else(|| body.get("Context"))
            .or_else(|| body.get("runContext"))
            .or_else(|| body.get("RunContext")),
    );
    let line = line.to_string();
    blocking_host_work(move || {
        match state.host.execute_verb_with_context(&line, ctx.as_ref()) {
            Ok(_) => {
                flush_host_events(&state);
                // C# `HandleHostVerb` success is always `{ ok: true }` (verb extras stay on the executor).
                Json(serde_json::json!({ "ok": true })).into_response()
            }
            Err(err) => {
                flush_host_events(&state);
                (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": err }))).into_response()
            }
        }
    }).await
}

#[derive(Deserialize)]
struct FaVectorQuery {
    spec: Option<String>,
}

async fn fa_vector(Query(query): Query<FaVectorQuery>) -> Response {
    let spec = query.spec.unwrap_or_default();
    if spec.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "Missing spec" })),
        )
            .into_response();
    }
    match crate::fa_lookup(&spec) {
        Some(vector) => (
            [(header::CACHE_CONTROL, ICON_CACHE_CONTROL)],
            Json(vector),
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

fn fa_lookup(spec: &str) -> Option<Value> {
    cliphost_core::fa_vector::try_get_vector(spec)
}

fn is_csharp_menu_verb_action(action: &str) -> bool {
    // C# `Enum.TryParse<MenuVerbAction>(..., ignoreCase: true)` does not trim.
    matches!(
        action.to_ascii_lowercase().as_str(),
        "window" | "text" | "bookmarks" | "file" | "favor" | "super" | "reopen" | "clipboard"
    )
}

async fn menu_create(State(state): State<HttpState>, body: Bytes) -> Response {
    let body = match loopback_json_body(&body) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let action = body
        .get("action")
        .or_else(|| body.get("Action"))
        .and_then(|v| v.as_str());
    let action = action.unwrap_or("super");
    if !is_csharp_menu_verb_action(action) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "message": format!("Unknown action: {action}") })),
        )
            .into_response();
    }
    let files = body
        .get("files")
        .or_else(|| body.get("Files"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let source = json_window_handle(&body, &["sourceWindowHandle", "SourceWindowHandle"]);
    let target = json_window_handle(&body, &["targetWindowHandle", "TargetWindowHandle"]);
    let action = action.to_string();
    let files = files.to_string();
    blocking_host_work(move || Json(state.host.menu_create_session_ex(&action, &files, source, target)).into_response()).await
}

// Shell/COM and filesystem calls must not occupy Tokio's request executor.
async fn blocking_host_work(work: impl FnOnce() -> Response + Send + 'static) -> Response {
    match tokio::task::spawn_blocking(work).await {
        Ok(response) => response,
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({
            "message": error.to_string(),
        }))).into_response(),
    }
}

async fn menu_close(State(state): State<HttpState>, Path(session_id): Path<String>) -> Response {
    // C# `CloseMenuSessionAsync` always returns 204, even when the session is already gone.
    blocking_host_work(move || {
        let _ = state.host.menu_close_session(&session_id);
        StatusCode::NO_CONTENT.into_response()
    }).await
}

#[derive(Deserialize)]
struct MenuNodePath {
    session_id: String,
    node_id: String,
}

fn json_window_handle(params: &Value, keys: &[&str]) -> i64 {
    for key in keys {
        if let Some(value) = params.get(*key) {
            if let Some(n) = value.as_i64() {
                return n;
            }
            if let Some(n) = value.as_u64() {
                return n as i64;
            }
            if let Some(s) = value.as_str() {
                if let Ok(n) = s.parse::<i64>() {
                    return n;
                }
            }
        }
    }
    0
}

async fn menu_children(
    State(state): State<HttpState>,
    Path(path): Path<MenuNodePath>,
    body: Option<Json<Value>>,
) -> Response {
    let body = body.map(|Json(v)| v).unwrap_or(Value::Null);
    menu_nodes_response(state, path, "children", body).await
}

async fn menu_context(
    State(state): State<HttpState>,
    Path(path): Path<MenuNodePath>,
    body: Option<Json<Value>>,
) -> Response {
    let body = body.map(|Json(v)| v).unwrap_or(Value::Null);
    menu_nodes_response(state, path, "context-menu", body).await
}

async fn menu_nodes_response(state: HttpState, path: MenuNodePath, collection: &'static str, body: Value) -> Response {
    blocking_host_work(move || {
        state.host.menu_apply_handles(&path.session_id, &body);
        let cursor = body
            .get("cursor")
            .or_else(|| body.get("Cursor"))
            .and_then(|v| v.as_str());
        match state
            .host
            .menu_nodes_ex(&path.session_id, &path.node_id, collection, cursor)
        {
            Ok(body) => Json(body).into_response(),
            Err(err) => (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "message": err })),
            )
                .into_response(),
        }
    }).await
}

async fn menu_invoke(
    State(state): State<HttpState>,
    Path(path): Path<MenuNodePath>,
    body: Option<Json<Value>>,
) -> Response {
    let body = body.map(|Json(v)| v).unwrap_or(Value::Null);
    blocking_host_work(move || {
        match state.host.menu_invoke(&path.session_id, &path.node_id, &body) {
            Ok(body) => Json(body).into_response(),
            Err(err) => (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "message": err })),
            )
                .into_response(),
        }
    }).await
}

async fn menu_set_value(
    State(state): State<HttpState>,
    Path(path): Path<MenuNodePath>,
    body: Bytes,
) -> Response {
    let body = match parse_loopback_json_or_empty(&body) {
        Ok(None) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "message": "value is required" })),
            )
                .into_response();
        }
        Ok(Some(value)) => value,
        Err(err) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": err })),
            )
                .into_response();
        }
    };
    blocking_host_work(move || {
        state.host.menu_apply_handles(&path.session_id, &body);
        let value = body
            .get("value")
            .or_else(|| body.get("Value"))
            .and_then(|v| v.as_f64().or_else(|| v.as_i64().map(|n| n as f64)))
            .or_else(|| body.is_object().then_some(0.0));
        match state.host.menu_set_value_ex(&path.session_id, &path.node_id, value) {
            Ok(body) => Json(body).into_response(),
            Err(err) => (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "message": err })),
            )
                .into_response(),
        }
    }).await
}

async fn menu_thumbnail(State(state): State<HttpState>, Query(query): Query<ThumbQuery>) -> Response {
    if query.hwnd == 0 {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "message": "hwnd is required" })),
        )
            .into_response();
    }
    blocking_host_work(move || match state
        .host
        .window_thumbnail(query.hwnd as isize, query.w.unwrap_or(360))
    {
        Some(bytes) => (StatusCode::OK, [(header::CONTENT_TYPE, "image/jpeg")], bytes).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }).await
}

async fn file_menu_paths(State(state): State<HttpState>) -> Response {
    blocking_host_work(move || Json(state.host.file_menu_snapshot()).into_response()).await
}

/// C# `ParsePathBody` / `ParseLinesBody`: invalid JSON is ignored, not 500.
fn parse_file_menu_json(bytes: &Bytes) -> Option<Value> {
    let text = std::str::from_utf8(bytes).ok()?;
    if text.trim().is_empty() {
        return None;
    }
    serde_json::from_str(text).ok()
}

fn file_menu_json_status(value: &Value) -> StatusCode {
    if value.get("ok").and_then(|v| v.as_bool()) == Some(true) {
        StatusCode::OK
    } else {
        StatusCode::BAD_REQUEST
    }
}

fn file_menu_lines(body: Option<&Value>) -> Vec<String> {
    let Some(body) = body else {
        return Vec::new();
    };
    let items = body
        .get("lines")
        .or_else(|| body.get("paths"))
        .and_then(|v| v.as_array());
    let Some(items) = items else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

async fn file_menu_put(State(state): State<HttpState>, body: Bytes) -> Response {
    let lines = file_menu_lines(parse_file_menu_json(&body).as_ref());
    blocking_host_work(move || match state.host.file_menu_replace(lines) {
        Ok(value) => Json(value).into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": err })),
        )
            .into_response(),
    }).await
}

async fn file_menu_add(State(state): State<HttpState>, body: Bytes) -> Response {
    let parsed = parse_file_menu_json(&body);
    let path = parsed
        .as_ref()
        .and_then(|v| v.get("path").and_then(|p| p.as_str()))
        .unwrap_or("").to_string();
    blocking_host_work(move || match state.host.file_menu_add(&path) {
        Ok(value) => (file_menu_json_status(&value), Json(value)).into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": err })),
        )
            .into_response(),
    }).await
}

async fn file_menu_remove(State(state): State<HttpState>, body: Bytes) -> Response {
    let parsed = parse_file_menu_json(&body);
    let path = parsed
        .as_ref()
        .and_then(|v| v.get("path").and_then(|p| p.as_str()))
        .unwrap_or("").to_string();
    blocking_host_work(move || match state.host.file_menu_remove(&path) {
        Ok(value) => (file_menu_json_status(&value), Json(value)).into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": err })),
        )
            .into_response(),
    }).await
}

async fn file_menu_move(State(state): State<HttpState>, body: Bytes) -> Response {
    let parsed = parse_file_menu_json(&body);
    let path = parsed
        .as_ref()
        .and_then(|v| v.get("path").and_then(|p| p.as_str()))
        .unwrap_or("").to_string();
    let delta = parsed
        .as_ref()
        .and_then(|v| v.get("delta").and_then(|d| d.as_i64()))
        .unwrap_or(0) as i32;
    if path.trim().is_empty() || delta == 0 {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "message": "path and delta required" })),
        )
            .into_response();
    }
    blocking_host_work(move || match state.host.file_menu_move(&path, delta) {
        Ok(value) => (file_menu_json_status(&value), Json(value)).into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": err })),
        )
            .into_response(),
    }).await
}

async fn file_menu_pick(State(state): State<HttpState>) -> Response {
    blocking_host_work(move || file_menu_finish_pick(&state.host,
        state.host.pick_directories(&serde_json::json!({})))).await
}

fn file_menu_finish_pick(host: &HostState, picked: Result<Value, String>) -> Response {
    let result = picked.and_then(|picked| {
        let paths: Vec<String> = picked.get("paths").and_then(|v| v.as_array())
            .into_iter().flatten().filter_map(|v| v.as_str())
            .filter(|p| !p.is_empty()).map(str::to_string).collect();
        host.file_menu_add_directories(&paths)
    });
    match result {
        Ok(value) => Json(value).into_response(),
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error":err}))).into_response(),
    }
}

async fn pick_image_file(State(state): State<HttpState>, body: Bytes) -> Response {
    let params = match loopback_json_body(&body) {
        Ok(value) => value,
        Err(response) => return response,
    };
    blocking_host_work(move || match state.host.pick_image_file(&params) {
        Ok(value) => Json(value).into_response(),
        Err(err) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": err })),
        )
            .into_response(),
    }).await
}

async fn stage_background(
    State(state): State<HttpState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    blocking_host_work(move || {
        let content_type = headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if let Some((bytes, name)) = parse_multipart_file(body.as_ref(), content_type) {
            match state.host.stage_window_background_bytes(&bytes, &name) {
                Ok(value) => Json(value).into_response(),
                Err(err) => (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({ "error": err })),
                )
                    .into_response(),
            }
        } else if let Ok(json) = serde_json::from_slice::<Value>(body.as_ref()) {
            let path = json.get("path").and_then(|v| v.as_str()).unwrap_or("");
            match state.host.stage_window_background_path(path) {
                Ok(value) => Json(value).into_response(),
                Err(err) => (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({ "error": err })),
                )
                    .into_response(),
            }
        } else {
            Json(serde_json::json!({ "path": Value::Null, "cancelled": true })).into_response()
        }
    }).await
}

fn parse_multipart_file(body: &[u8], content_type: &str) -> Option<(Vec<u8>, String)> {
    let boundary = content_type
        .split(';')
        .map(str::trim)
        .find_map(|part| part.strip_prefix("boundary=").or_else(|| part.strip_prefix("Boundary=")))?
        .trim()
        .trim_matches('"');
    if boundary.is_empty() {
        return None;
    }
    let marker = b"Content-Disposition: form-data; name=\"file\"";
    let idx = find_bytes(body, marker, 0)?;
    let mut file_name = String::new();
    if let Some(name_idx) = find_bytes(body, b"filename=\"", idx) {
        let start = name_idx + b"filename=\"".len();
        if let Some(rel) = body[start..].iter().position(|b| *b == b'"') {
            file_name = String::from_utf8_lossy(&body[start..start + rel]).into_owned();
        }
    }
    let header_end = find_bytes(body, b"\r\n\r\n", idx)?;
    let data_start = header_end + 4;
    let footer = format!("\r\n--{boundary}").into_bytes();
    let data_end = find_bytes(body, &footer, data_start).unwrap_or(body.len());
    Some((body[data_start..data_end].to_vec(), file_name))
}

fn find_bytes(haystack: &[u8], needle: &[u8], start: usize) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack[start..]
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|pos| start + pos)
}

#[cfg(debug_assertions)]
async fn debug_chrome_open(State(state): State<HttpState>) -> Json<Value> {
    // C# `HandleWindowChromeDebugOpen` → `{ ok: hwnd != 0, hwnd }`.
    state.host.request_show();
    let hwnd = state.host.host_hwnd() as i64;
    Json(serde_json::json!({ "ok": hwnd != 0, "hwnd": hwnd }))
}

#[cfg(debug_assertions)]
async fn debug_toast_collapse(State(state): State<HttpState>) -> Json<Value> {
    publish(
        &state.events,
        HostPushEnvelope::event("toastCollapse", serde_json::json!({ "ok": true })),
    );
    Json(serde_json::json!({ "ok": true, "collapsed": true }))
}

// ClipLink HTTP lives on LAN TLS (`lan_tls.rs`). C# `ClipHostLoopbackHttpServer` has no `/cliplink`.
#[cfg_attr(not(test), allow(dead_code))]
async fn cliplink_info(State(state): State<HttpState>) -> Json<Value> {
    Json(state.host.lan_info())
}

#[cfg_attr(not(test), allow(dead_code))]
async fn cliplink_pair(State(state): State<HttpState>, Json(body): Json<Value>) -> Response {
    match state.host.lan_accept_inbound_pair(&body) {
        Ok(body) => {
            publish(
                &state.events,
                HostPushEnvelope::event("lan.peers.changed", serde_json::json!({ "paired": true })),
            );
            Json(body).into_response()
        }
        Err(err) => {
            let status = cliphost_core::lan::pair_http_status(&err);
            let code = if status == 403 {
                StatusCode::FORBIDDEN
            } else {
                StatusCode::BAD_REQUEST
            };
            (code, Json(cliphost_core::lan::problem_json(status, &err))).into_response()
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
async fn cliplink_prepare(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    match authenticate(&state, &headers).and_then(|device| {
        state
            .host
            .lan_prepare_inbound(&device, &body)
            .map_err(|e| (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "detail": e }))))
    }) {
        Ok(body) => Json(body).into_response(),
        Err(err) => err.into_response(),
    }
}

#[cfg_attr(not(test), allow(dead_code))]
async fn cliplink_text(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    match authenticate(&state, &headers).and_then(|device| {
        state
            .host
            .lan_receive_text(&device, &body)
            .map_err(|e| (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "detail": e }))))
    }) {
        Ok(body) => Json(body).into_response(),
        Err(err) => err.into_response(),
    }
}

#[cfg_attr(not(test), allow(dead_code))]
async fn cliplink_upload(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path((message_id, attachment_id)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    if let Err(err) = cliphost_core::lan::safe_identifier(&message_id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(cliphost_core::lan::problem_json(400, &err)),
        )
            .into_response();
    }
    if let Err(err) = cliphost_core::lan::safe_identifier(&attachment_id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(cliphost_core::lan::problem_json(400, &err)),
        )
            .into_response();
    }
    let content_length = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    match authenticate(&state, &headers).and_then(|device| {
        state
            .host
            .lan_upload_inbound(&device, &message_id, &attachment_id, &body, content_length)
            .map_err(|e| {
                let status = cliphost_core::lan::inbound_http_status(&e);
                let code = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST);
                (
                    code,
                    Json(cliphost_core::lan::problem_json(
                        status,
                        cliphost_core::lan::inbound_http_detail(&e),
                    )),
                )
            })
    }) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => err.into_response(),
    }
}

#[cfg_attr(not(test), allow(dead_code))]
async fn cliplink_commit(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(message_id): Path<String>,
) -> Response {
    if let Err(err) = cliphost_core::lan::safe_identifier(&message_id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(cliphost_core::lan::problem_json(400, &err)),
        )
            .into_response();
    }
    match authenticate(&state, &headers).and_then(|device| {
        state
            .host
            .lan_commit_inbound(&device, &message_id)
            .map_err(|e| (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "detail": e }))))
    }) {
        Ok(body) => Json(body).into_response(),
        Err(err) => err.into_response(),
    }
}

#[cfg_attr(not(test), allow(dead_code))]
async fn cliplink_cancel(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(message_id): Path<String>,
) -> Response {
    if let Err(err) = cliphost_core::lan::safe_identifier(&message_id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(cliphost_core::lan::problem_json(400, &err)),
        )
            .into_response();
    }
    match authenticate(&state, &headers) {
        Ok(device) => {
            state.host.lan_cancel_inbound(&device, &message_id);
            StatusCode::NO_CONTENT.into_response()
        }
        Err(err) => err.into_response(),
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn authenticate(state: &HttpState, headers: &HeaderMap) -> Result<String, (StatusCode, Json<Value>)> {
    let device = headers
        .get(cliphost_core::lan::DEVICE_HEADER)
        .and_then(|v| v.to_str().ok());
    let authorization = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok());
    state
        .host
        .lan_authenticate(device, authorization)
        .map_err(|e| (StatusCode::UNAUTHORIZED, Json(serde_json::json!({ "detail": e }))))
}

pub async fn serve_lan(state: HttpState) -> std::io::Result<()> {
    serve_lan_at(state, lan_listen_address()).await
}

pub(crate) fn lan_listen_address() -> std::net::Ipv4Addr {
    // Cargo gives test binaries new paths as builds change. Even the public
    // listener entry points must stay local in tests, without firewall grants.
    if cfg!(test) || cliphost_core::NATIVE_ACCEPTANCE_BUILD
        || std::env::var("CLIPHOST_GPUI_ISOLATED").as_deref() == Ok("1") {
        std::net::Ipv4Addr::LOCALHOST
    } else {
        std::net::Ipv4Addr::UNSPECIFIED
    }
}

// LAN state can wait for durable writes. Keep those waits off the async
// workers shared with desktop window commands and loopback HTTP requests.
async fn lan_host_work<T: Send + 'static>(
    host: HostState,
    work: impl FnOnce(HostState) -> T + Send + 'static,
) -> std::io::Result<T> {
    tokio::task::spawn_blocking(move || work(host)).await.map_err(std::io::Error::other)
}

async fn serve_lan_at(state: HttpState, address: std::net::Ipv4Addr) -> std::io::Result<()> {
    let mut settings_tick = tokio::time::interval(std::time::Duration::from_millis(250));
    loop {
        settings_tick.tick().await;
        let Some(port) = lan_host_work(state.host.clone(), |host| host.lan_listener_port()).await? else { continue; };
        let session = async {
            tokio::try_join!(
                crate::lan_tls::serve_lan_https_at(state.clone(), SocketAddr::from((address, port))),
                run_lan_udp(state.clone(), address, port),
            )?;
            Ok::<(), std::io::Error>(())
        };
        let settings_changed = async {
            loop {
                settings_tick.tick().await;
                if lan_host_work(state.host.clone(), |host| host.lan_listener_port()).await? != Some(port) {
                    return Ok::<(), std::io::Error>(());
                }
            }
        };
        tokio::select! {
            result = session => {
                if let Err(err) = result {
                    tracing::warn!("ClipLink LAN listener failed: {err}");
                    let message = format!("局域网监听失败（端口 {port}）：{err}");
                    lan_host_work(state.host.clone(), move |host| {
                        host.lan_set_running(false);
                        host.lan_set_listener_error(port, message);
                    }).await?;
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
            }
            result = settings_changed => result?,
        }
    }
}

const LAN_MULTICAST: std::net::Ipv4Addr = std::net::Ipv4Addr::new(239, 255, 77, 77);

#[derive(Default)]
struct LanUdpDiscoveryPlan {
    interfaces: Vec<std::net::Ipv4Addr>,
    targets: Vec<std::net::Ipv4Addr>,
}

fn lan_udp_discovery_plan(address: std::net::Ipv4Addr, interfaces: &[cliphost_core::lan::LanDiscoveryInterface]) -> LanUdpDiscoveryPlan {
    // Isolated listeners cannot gain broadcast targets even from real adapters.
    if address.is_loopback() { return LanUdpDiscoveryPlan::default(); }
    let mut plan = LanUdpDiscoveryPlan::default();
    for interface in interfaces {
        if let Some(ip) = interface.multicast_interface() {
            if !plan.interfaces.contains(&ip) { plan.interfaces.push(ip); }
        }
        if let Some(broadcast) = interface.broadcast_address() {
            if !plan.targets.contains(&broadcast) { plan.targets.push(broadcast); }
        }
    }
    for target in [LAN_MULTICAST, std::net::Ipv4Addr::BROADCAST] {
        if !plan.targets.contains(&target) { plan.targets.push(target); }
    }
    plan
}

async fn refresh_lan_udp_discovery(
    socket: &tokio::net::UdpSocket, address: std::net::Ipv4Addr,
    joined: &mut Vec<std::net::Ipv4Addr>, plan: &mut LanUdpDiscoveryPlan,
) {
    if address.is_loopback() { return; }
    let interfaces = match tokio::task::spawn_blocking(cliphost_core::lan::local_discovery_interfaces).await {
        Ok(Ok(interfaces)) => interfaces,
        result => { tracing::warn!(?result, "LAN discovery adapter refresh failed"); return; }
    };
    let next = lan_udp_discovery_plan(address, &interfaces);
    reconcile_lan_multicast(joined, &next.interfaces,
        |ip| socket.join_multicast_v4(LAN_MULTICAST, ip).map_err(|error| {
            tracing::warn!(%ip, %error, "LAN discovery multicast join failed"); error
        }),
        |ip| socket.leave_multicast_v4(LAN_MULTICAST, ip).map_err(|error| {
            tracing::debug!(%ip, %error, "LAN discovery multicast leave failed"); error
        }));
    if plan.interfaces != next.interfaces || plan.targets != next.targets {
        tracing::info!(interfaces = ?next.interfaces, targets = ?next.targets, "LAN discovery interfaces refreshed");
    }
    *plan = next;
}

fn reconcile_lan_multicast(
    joined: &mut Vec<std::net::Ipv4Addr>, desired: &[std::net::Ipv4Addr],
    mut join: impl FnMut(std::net::Ipv4Addr) -> std::io::Result<()>,
    mut leave: impl FnMut(std::net::Ipv4Addr) -> std::io::Result<()>,
) {
    // A failed leave is still a live membership. Retry it on the next refresh
    // instead of forgetting it and repeatedly attempting a duplicate join.
    joined.retain(|ip| desired.contains(ip) || leave(*ip).is_err());
    for ip in desired {
        if !joined.contains(ip) && join(*ip).is_ok() { joined.push(*ip); }
    }
}

async fn send_lan_discovery(socket: &tokio::net::UdpSocket, bytes: &[u8], port: u16, plan: &LanUdpDiscoveryPlan) -> usize {
    let mut sent = 0;
    for target in &plan.targets {
        match socket.send_to(bytes, (*target, port)).await {
            Ok(_) => sent += 1,
            Err(error) => tracing::warn!(%target, port, %error, "LAN discovery send failed"),
        }
    }
    sent
}

#[cfg(feature = "private-sync")]
fn is_lan_client_query(packet: &[u8]) -> bool {
    if packet.len() > 512 { return false; }
    let Ok(Value::Object(value)) = serde_json::from_slice::<Value>(packet) else { return false; };
    value.len() == 4
        && value.get("protocol").and_then(Value::as_str) == Some(cliphost_core::lan::PROTOCOL)
        && value.get("version").and_then(Value::as_i64) == Some(1)
        && value.get("kind").and_then(Value::as_str) == Some("query")
        && value.get("clientOnly").and_then(Value::as_bool) == Some(true)
}

#[cfg(feature = "private-sync")]
#[derive(Default)]
struct LanClientQueryLimit {
    sources: std::collections::HashMap<std::net::IpAddr, std::time::Instant>,
}

#[cfg(feature = "private-sync")]
impl LanClientQueryLimit {
    fn accept(&mut self, from: SocketAddr, now: std::time::Instant) -> bool {
        let valid = match from.ip() {
            std::net::IpAddr::V4(ip) => !ip.is_unspecified() && !ip.is_multicast() && !ip.is_broadcast(),
            std::net::IpAddr::V6(ip) => !ip.is_unspecified() && !ip.is_multicast(),
        };
        if !valid || from.port() == 0 { return false; }
        self.sources.retain(|_, time| now.saturating_duration_since(*time) < Duration::from_secs(1));
        // At most one response per source per second and 32 sources per second.
        // The request never supplies a destination or creates a peer identity.
        if self.sources.contains_key(&from.ip()) || self.sources.len() >= 32 { return false; }
        self.sources.insert(from.ip(), now);
        true
    }
}

/// The Android fixture can exercise the production discovery handler without
/// opening a LAN interface or emitting multicast/broadcast traffic.
#[cfg(debug_assertions)]
pub async fn serve_lan_udp_loopback(state: HttpState, port: u16) -> std::io::Result<()> {
    run_lan_udp(state, std::net::Ipv4Addr::LOCALHOST, port).await
}

async fn run_lan_udp(state: HttpState, address: std::net::Ipv4Addr, port: u16) -> std::io::Result<()> {
    use tokio::net::UdpSocket;
    use tokio::time::{interval, Duration};
    let socket = UdpSocket::bind((address, port)).await?;
    if !address.is_loopback() {
        socket.set_broadcast(true)?;
    }
    let mut joined = Vec::new();
    let mut plan = lan_udp_discovery_plan(address, &[]);
    #[cfg(feature = "private-sync")]
    let mut client_queries = LanClientQueryLimit::default();
    let mut buf = vec![0u8; 4096];
    let mut tick = interval(Duration::from_secs(10));
    let mut pending_tick = interval(Duration::from_millis(50));
    pending_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = pending_tick.tick() => {
                if let Some(bytes) = lan_host_work(state.host.clone(), |host| host.lan_take_pending_outbound())
                    .await?.filter(|_| !address.is_loopback()) {
                    refresh_lan_udp_discovery(&socket, address, &mut joined, &mut plan).await;
                    let sent = send_lan_discovery(&socket, &bytes, port, &plan).await;
                    tracing::info!(port, sent, targets = plan.targets.len(), "LAN pending discovery send completed");
                }
            }
            result = socket.recv_from(&mut buf) => {
                let (n, from) = result?;
                let packet = buf[..n].to_vec();
                #[cfg(feature = "private-sync")]
                if is_lan_client_query(&packet) {
                    if client_queries.accept(from, std::time::Instant::now()) {
                        let reply = lan_host_work(state.host.clone(), move |host| {
                            (host.lan_listener_port() == Some(port)).then(|| host.lan_announce_bytes("announce"))
                        }).await?;
                        if let Some(bytes) = reply {
                            if let Err(error) = socket.send_to(&bytes, from).await {
                                tracing::warn!(%from, %error, "LAN client discovery reply failed");
                            }
                        }
                    }
                    continue;
                }
                let ip = from.ip().to_string();
                let (changed, reply) = lan_host_work(state.host.clone(), move |host| {
                    let kind = host.lan_ingest_datagram(&packet, &ip);
                    let reply = (kind.as_deref() == Some("query")).then(|| host.lan_announce_bytes("announce"));
                    (kind.is_some(), reply)
                }).await?;
                if changed {
                    if let Some(bytes) = reply {
                        if let Err(error) = socket.send_to(&bytes, from).await {
                            tracing::warn!(%from, %error, "LAN discovery reply failed");
                        }
                    }
                    flush_host_events(&state);
                }
            }
            _ = tick.tick() => {
                refresh_lan_udp_discovery(&socket, address, &mut joined, &mut plan).await;
                let host = state.host.clone();
                tokio::task::spawn_blocking(move || {
                    host.lan_on_discovery_tick();
                });
                if let Some((bytes, burst)) = lan_host_work(state.host.clone(), |host| host.lan_take_outbound())
                    .await?.filter(|_| !address.is_loopback()) {
                    let sent = send_lan_discovery(&socket, &bytes, port, &plan).await;
                    if burst {
                        tracing::info!(port, sent, targets = plan.targets.len(), "LAN discovery query send completed");
                    }
                }
            }
        }
    }
}

#[cfg(debug_assertions)]
async fn qp_status(State(state): State<HttpState>) -> Json<Value> {
    Json(state.host.qp_debug_status())
}

#[cfg(debug_assertions)]
async fn qp_preview(State(state): State<HttpState>) -> Json<Value> {
    let status = state.host.qp_debug_begin();
    publish(
        &state.events,
        HostPushEnvelope::event("quickPasteState", state.host.qp_state_detail()),
    );
    Json(status)
}

#[cfg(debug_assertions)]
async fn qp_end(State(state): State<HttpState>) -> Json<Value> {
    let status = state.host.qp_debug_end();
    publish(
        &state.events,
        HostPushEnvelope::event("quickPasteState", state.host.qp_state_detail()),
    );
    Json(status)
}

#[cfg(debug_assertions)]
async fn debug_toast(State(state): State<HttpState>, Json(body): Json<Value>) -> Json<Value> {
    let message = body
        .get("message")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("ClipHost debug toast");
    let kind = body
        .get("kind")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("info")
        .to_ascii_lowercase();
    let duration_ms = body
        .get("durationMs")
        .and_then(|v| v.as_i64())
        .unwrap_or(2500) as i32;
    let placement = body.get("placement").and_then(|v| v.as_str());
    state
        .host
        .show_host_toast(message, &kind, None, Some(duration_ms), placement);
    Json(serde_json::json!({ "ok": true }))
}

pub fn publish(events: &broadcast::Sender<String>, envelope: HostPushEnvelope) {
    if let Ok(body) = serde_json::to_string(&envelope) {
        let _ = events.send(body);
    }
}

pub fn flush_host_events(state: &HttpState) {
    for (name, data) in state.host.drain_push_events() {
        publish(&state.events, HostPushEnvelope::event(name, data));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::put;
    use cliphost_core::paths::AppPaths;
    use tempfile::tempdir;
    use tower::ServiceExt;
    use serde_json::json;

    #[cfg(feature = "private-sync")]
    #[test]
    fn lan_client_discovery_query_is_strict_and_rate_limited_without_peer_identity() {
        let query = json!({"protocol":"cea-clip-link","version":1,"kind":"query","clientOnly":true});
        assert!(is_lan_client_query(&serde_json::to_vec(&query).unwrap()));
        for (key, value) in [("clientOnly", json!(false)), ("kind", json!("announce")),
            ("version", json!(2)), ("deviceId", json!("spoofed"))] {
            let mut invalid = query.clone();
            invalid[key] = value;
            assert!(!is_lan_client_query(&serde_json::to_vec(&invalid).unwrap()));
        }
        assert!(!is_lan_client_query(&vec![b' '; 513]));
        let now = std::time::Instant::now();
        let mut limit = LanClientQueryLimit::default();
        assert!(!limit.accept("0.0.0.0:1234".parse().unwrap(), now));
        assert!(!limit.accept("239.255.77.77:1234".parse().unwrap(), now));
        assert!(!limit.accept("255.255.255.255:1234".parse().unwrap(), now));
        let source = "127.0.0.1:1234".parse().unwrap();
        assert!(limit.accept(source, now));
        assert!(!limit.accept("127.0.0.1:4567".parse().unwrap(), now));
        for index in 2..=32 {
            assert!(limit.accept(format!("127.0.0.{index}:1234").parse().unwrap(), now));
        }
        assert!(!limit.accept("127.0.0.33:1234".parse().unwrap(), now));
        assert!(limit.accept(source, now + Duration::from_secs(1)));
        assert_eq!(limit.sources.len(), 1);
    }

    #[test]
    fn lan_discovery_plan_reaches_each_subnet_independent_of_adapter_order() {
        use std::net::Ipv4Addr;
        use cliphost_core::lan::LanDiscoveryInterface;
        let interface = |address: &str, prefix_length| LanDiscoveryInterface {
            address: address.parse().unwrap(), prefix_length,
        };
        let mut interfaces = vec![
            interface("28.0.0.1", 30), interface("100.121.100.55", 32),
            interface("172.24.96.1", 20), interface("192.168.31.100", 24),
            interface("192.168.31.101", 24), interface("192.168.31.100", 24),
            interface("127.0.0.1", 8), interface("224.0.0.1", 24),
        ];
        let plan = lan_udp_discovery_plan(Ipv4Addr::UNSPECIFIED, &interfaces);
        assert_eq!(plan.interfaces.len(), 5);
        assert_eq!(plan.targets, vec![
            Ipv4Addr::new(28, 0, 0, 3), Ipv4Addr::new(172, 24, 111, 255),
            Ipv4Addr::new(192, 168, 31, 255), LAN_MULTICAST, Ipv4Addr::BROADCAST,
        ]);
        interfaces.reverse();
        let reversed = lan_udp_discovery_plan(Ipv4Addr::UNSPECIFIED, &interfaces);
        assert!(plan.targets.iter().all(|target| reversed.targets.contains(target)));
        assert_eq!(plan.targets.len(), reversed.targets.len());
    }

    #[test]
    fn lan_discovery_plan_keeps_compatibility_when_adapters_are_empty() {
        use std::net::Ipv4Addr;
        let plan = lan_udp_discovery_plan(Ipv4Addr::UNSPECIFIED, &[]);
        assert!(plan.interfaces.is_empty());
        assert_eq!(plan.targets, vec![LAN_MULTICAST, Ipv4Addr::BROADCAST]);
    }

    #[test]
    fn lan_discovery_isolated_listener_has_no_multicast_or_broadcast_targets() {
        use std::net::Ipv4Addr;
        let adapters = [cliphost_core::lan::LanDiscoveryInterface {
            address: Ipv4Addr::new(192, 168, 31, 100), prefix_length: 24,
        }];
        assert_eq!(lan_listen_address(), Ipv4Addr::LOCALHOST);
        let plan = lan_udp_discovery_plan(lan_listen_address(), &adapters);
        assert!(plan.interfaces.is_empty());
        assert!(plan.targets.is_empty());
    }

    #[test]
    fn lan_discovery_memberships_keep_failed_leaves_and_retry_joins() {
        use std::net::Ipv4Addr;
        let old = Ipv4Addr::new(192, 168, 31, 100);
        let new = Ipv4Addr::new(192, 168, 42, 100);
        let mut joined = vec![old];
        let mut leaves = Vec::new();
        reconcile_lan_multicast(&mut joined, &[], |_| panic!("unexpected join"), |ip| {
            leaves.push(ip); Err(std::io::Error::other("fixture leave failed"))
        });
        assert_eq!(joined, vec![old]);
        // The same address returns before its failed leave succeeds: it is
        // already a member and must not enter an endless duplicate-join retry.
        reconcile_lan_multicast(&mut joined, &[old], |_| panic!("duplicate join"), |_| panic!("unexpected leave"));
        let mut joins = Vec::new();
        reconcile_lan_multicast(&mut joined, &[new], |ip| {
            joins.push(ip); Err(std::io::Error::other("fixture join failed"))
        }, |ip| { leaves.push(ip); Ok(()) });
        assert!(joined.is_empty());
        reconcile_lan_multicast(&mut joined, &[new], |ip| { joins.push(ip); Ok(()) }, |_| panic!("unexpected leave"));
        assert_eq!(joined, vec![new]);
        reconcile_lan_multicast(&mut joined, &[new], |_| panic!("duplicate join"), |_| panic!("unexpected leave"));
        assert_eq!(leaves, vec![old, old]);
        assert_eq!(joins, vec![new, new]);
    }

    fn test_state() -> HttpState {
        cliphost_core::force_headless(true);
        std::env::set_var("CLIPHOST_TAURI_HEADLESS", "1");
        let dir = tempdir().unwrap();
        let paths = AppPaths::from_data_root(dir.path().join("BetterClipTauri"));
        let host = HostState::open(paths, 28965).unwrap();
        HttpState {
            host,
            events: event_bus(),
        }
    }

    #[tokio::test]
    async fn process_filter_presets_share_authority_across_rest_rpc_and_core_events() {
        cliphost_core::force_headless(true);
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
        std::fs::create_dir_all(&root).unwrap();
        let dir = tempfile::Builder::new().prefix("preset-http-").tempdir_in(root).unwrap();
        let host = HostState::open(AppPaths::from_data_root(dir.path().to_path_buf()), 29065).unwrap();
        host.drain_push_events();
        let bus = event_bus();
        let mut pushed = bus.subscribe();
        let app = router(HttpState { host: host.clone(), events: bus });
        async fn request(app: &Router, method: &str, path: &str, params: Option<Value>) -> (StatusCode, Value) {
            let response = app.clone().oneshot(axum::http::Request::builder()
                .method(method).uri(path).header(header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(params.map(|value| value.to_string()).unwrap_or_default())).unwrap())
                .await.unwrap();
            let status = response.status();
            let data = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
            (status, serde_json::from_slice(&data).unwrap())
        }
        let (status, initial) = request(&app, "GET", "/api/clip/process-filter-presets", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(initial["initialized"], false);
        let presets = json!([{"id":"native","displayName":"工作","includeExeNames":["missing.EXE","foreground"]}]);
        let (status, saved) = request(&app, "POST", "/api/clip/process-filter-presets",
            Some(json!({"expectedRevision":initial["revision"],"presets":presets}))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(saved["presets"], presets);
        assert_eq!(host.process_filter_presets_get().unwrap(), saved);
        let event: Value = serde_json::from_str(&pushed.try_recv().unwrap()).unwrap();
        assert_eq!(event["eventName"], "processFilterPresetsChanged");
        assert_eq!(event["data"], saved);
        let (status, rpc) = request(&app, "POST", "/api/rpc",
            Some(json!({"id":"same-authority","method":"clip.processFilterPresets.get","params":{}}))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(rpc["result"], saved);
        let (status, rejected) = request(&app, "POST", "/api/clip/process-filter-presets",
            Some(json!({"expectedRevision":initial["revision"],"presets":[]}))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(rejected["error"].as_str().unwrap().contains("另一窗口"));
        assert_eq!(host.process_filter_presets_get().unwrap(), saved);
        assert!(pushed.try_recv().is_err(), "a rejected stale save emits no write event");
        let source = r#"[{"id":"web","displayName":"旧来源","includeExeNames":["Code.exe"]}]"#;
        let (status, imported) = request(&app, "POST", "/api/clip/process-filter-presets/import",
            Some(json!({"expectedRevision":saved["revision"],"origin":"https://tauri.localhost","sourceJson":source}))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(imported["presets"][0], presets[0]);
        assert_eq!(imported["presets"][1]["id"], "web");
        assert_eq!(imported["importedOrigins"], json!(["https://tauri.localhost"]));
        assert_eq!(host.process_filter_presets_get().unwrap(), imported);
        let event: Value = serde_json::from_str(&pushed.try_recv().unwrap()).unwrap();
        assert_eq!(event["eventName"], "processFilterPresetsChanged");
        assert_eq!(event["data"], imported);
    }

    /// Send the serialized callback through the real HTTP handler.
    async fn escape_callback_response(state: &HttpState, body: Value) -> Value {
        let response = router(state.clone()).oneshot(
            axum::http::Request::builder().method("POST").uri("/api/clip/host-window")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(body.to_string())).unwrap(),
        ).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        serde_json::from_slice(&axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap()
    }

    #[tokio::test]
    async fn escape_callback_http_keeps_new_displays_open() {
        for transition in ["hide-show", "show", "reload"] {
            let state = test_state();
            state.host.request_show();
            let old = json!({"host":"clipboard", "action":"escapeUnhandled",
                "escapeToken":state.host.clipboard_escape_session_token()});
            match transition {
                "hide-show" => { state.host.window_hide(); state.host.request_show(); }
                "show" => state.host.request_show(),
                _ => { state.host.set_clipboard_web_view_ready(false); state.host.set_clipboard_web_view_ready(true); }
            }
            while state.host.take_chrome_command().is_some() {}
            let response = escape_callback_response(&state, old).await;
            assert_eq!(response["host"], "clipboard");
            assert!(state.host.hide_snapshot().visible, "late HTTP Escape closed {transition}");
            assert!(state.host.take_chrome_command().is_none());
            escape_callback_response(&state, json!({"host":"clipboard", "action":"escapeUnhandled",
                "escapeToken":state.host.clipboard_escape_session_token()})).await;
            assert!(!state.host.hide_snapshot().visible, "current HTTP Escape must still hide");
        }
    }

    #[tokio::test]
    async fn escape_callback_http_rejects_missing_malformed_and_other_window_scopes() {
        let state = test_state();
        state.host.request_show();
        let token = state.host.clipboard_escape_session_token();
        for body in [
            json!({"host":"clipboard", "action":"escapeUnhandled"}),
            json!({"host":"clipboard", "action":"escapeUnhandled", "escapeToken":""}),
            json!({"host":"clipboard", "action":"escapeUnhandled", "escapeToken":"old"}),
            json!({"host":"clipboard", "action":"escapeUnhandled", "escapeToken":17}),
            json!({"host":"clipboard", "action":"escapeUnhandled", "escapeToken":token, "escapeInputToken":null}),
            json!({"host":"clipboard", "action":"escapeUnhandled", "escapeToken":token, "escapeInputToken":"old"}),
            json!({"host":"panel", "action":"escapeUnhandled", "escapeToken":token}),
            json!({"host":"main", "action":"escapeUnhandled", "escapeToken":token}),
        ] {
            while state.host.take_chrome_command().is_some() {}
            escape_callback_response(&state, body).await;
            assert!(state.host.hide_snapshot().visible, "invalid callback scope closed clipboard");
            assert!(state.host.take_chrome_command().is_none());
        }
    }

    #[tokio::test]
    async fn escape_callback_http_does_not_replay_after_a_successful_hide() {
        use cliphost_core::ChromeCommand;
        let state = test_state();
        state.host.request_show();
        while state.host.take_chrome_command().is_some() {}
        let body = json!({"host":"clipboard", "action":"escapeUnhandled",
            "escapeToken":state.host.clipboard_escape_session_token()});
        escape_callback_response(&state, body.clone()).await;
        assert_eq!(state.host.take_chrome_command(), Some(ChromeCommand::Hide));
        escape_callback_response(&state, body.clone()).await;
        assert!(state.host.take_chrome_command().is_none(), "duplicate callback queued another hide");
        state.host.request_show();
        while state.host.take_chrome_command().is_some() {}
        escape_callback_response(&state, body).await;
        assert!(state.host.hide_snapshot().visible);
        assert!(state.host.take_chrome_command().is_none());
    }

    #[tokio::test]
    async fn escape_callback_http_new_layer_cancels_old_reply() {
        for owner in ["setSpaModalOpen", "setSpaOverlayOpen", "editing"] {
            let state = test_state();
            state.host.request_show();
            let old = json!({"host":"clipboard", "action":"escapeUnhandled",
                "escapeToken":state.host.clipboard_escape_session_token()});
            for open in [true, false] {
                let (endpoint, body) = if owner == "editing" {
                    ("/api/clip/chrome", json!({"kind":"clipboardHostChrome", "isClipboardEditing":open}))
                } else {
                    ("/api/clip/host-window", json!({"host":"clipboard", "action":owner, "value":open}))
                };
                let response = router(state.clone()).oneshot(
                    axum::http::Request::builder().method("POST").uri(endpoint)
                        .header("content-type", "application/json")
                        .body(axum::body::Body::from(body.to_string())).unwrap(),
                ).await.unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                while state.host.take_chrome_command().is_some() {}
                escape_callback_response(&state, old.clone()).await;
                assert!(state.host.hide_snapshot().visible, "late HTTP Escape affected {owner}, open={open}");
                assert!(state.host.take_chrome_command().is_none());
                if open {
                    escape_callback_response(&state, json!({"host":"clipboard", "action":"escapeUnhandled",
                        "escapeToken":state.host.clipboard_escape_session_token()})).await;
                    assert!(state.host.hide_snapshot().visible, "an active layer keeps its parent");
                }
            }
            escape_callback_response(&state, json!({"host":"clipboard", "action":"escapeUnhandled",
                "escapeToken":state.host.clipboard_escape_session_token()})).await;
            assert!(!state.host.hide_snapshot().visible, "new HTTP Escape after layer close must hide");
        }
    }

    /// Run the reusable API workflow without launching Tauri or touching desktop input.
    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires PowerShell 7; starts an isolated loopback HTTP fixture"]
    async fn scripted_clipboard_workflow_without_desktop() {
        use std::os::windows::process::CommandExt;

        // Opt in explicitly: this variant uses the real Windows clipboard, but
        // still creates no Tauri windows and never injects keyboard/mouse input.
        let native_clipboard = std::env::var("CLIPHOST_TEST_NATIVE_CLIPBOARD").as_deref() == Ok("1");
        cliphost_core::force_headless(!native_clipboard);
        std::env::set_var("CLIPHOST_TAURI_HEADLESS", if native_clipboard { "0" } else { "1" });
        let dir = tempdir().unwrap();
        let root = dir.path().join("BetterClipTauri");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let host = HostState::open(AppPaths::from_data_root(root.clone()), port).unwrap();
        let state = HttpState { host: host.clone(), events: event_bus() };
        let server = tokio::spawn(async move {
            axum::serve(listener, router(state)).await.unwrap();
        });
        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../scripts/Test-ClipboardWorkflow.ps1");
        let run = tokio::task::spawn_blocking(move || {
            let mut command = std::process::Command::new("pwsh");
            command.args(["-NoProfile", "-File"])
                .arg(script)
                .arg("-ApiPort").arg(port.to_string())
                .arg("-ExpectedDataRoot").arg(root)
                .creation_flags(0x08000000); // CREATE_NO_WINDOW
            if native_clipboard {
                command.arg("-VerifyClipboard");
            }
            command.output().unwrap()
        });
        let output = tokio::time::timeout(std::time::Duration::from_secs(45), run)
            .await.expect("workflow timed out").unwrap();
        server.abort();
        assert!(output.status.success(), "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["ok"], true);
        assert_eq!(report["checks"].as_array().unwrap().len(), if native_clipboard { 7 } else { 5 });
        assert!(host.favor_tree().unwrap()["rootItems"].as_array().unwrap().is_empty());
        assert_eq!(host.config_get()["nonPinnedItemCount"], 0);
        println!("{}", String::from_utf8_lossy(&output.stdout));
    }

    fn cliplink_test_router(state: HttpState) -> Router {
        Router::new()
            .route("/cliplink/v1/info", get(cliplink_info))
            .route("/cliplink/v1/pair", post(cliplink_pair))
            .route("/cliplink/v1/messages/text", post(cliplink_text))
            .route("/cliplink/v1/messages/prepare", post(cliplink_prepare))
            .route(
                "/cliplink/v1/messages/{message_id}/attachments/{attachment_id}",
                put(cliplink_upload),
            )
            .route("/cliplink/v1/messages/{message_id}/commit", post(cliplink_commit))
            .route("/cliplink/v1/messages/{message_id}/cancel", post(cliplink_cancel))
            .with_state(state)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn panel_rpc_returns_operation_errors_and_allows_retry() {
        cliphost_core::force_headless(true);
        let dir = tempdir().unwrap();
        let state = HttpState {
            host: HostState::open(AppPaths::from_data_root(dir.path().join("BetterClipTauri")), 28965).unwrap(),
            events: event_bus(),
        };
        let folder = state.host.favor_folder_create(&serde_json::json!({ "title": "panel RPC" })).unwrap();
        let added = state.host.favor_add_text_items(&serde_json::json!({
            "parentId": folder["id"], "text": "panel RPC text",
        })).unwrap();
        for action in ["Copy", "Paste", "Open"] {
            state.host.settings_form_update(&serde_json::json!({
                "configId": "panel", "values": { "FileDefaultClickAction": action },
            })).unwrap();
            for fail in [true, false] {
                state.host.set_clipboard_ui_runner(move |job| {
                    if fail { Err("clipboard write rejected".into()) } else { job() }
                });
                let response = router(state.clone()).oneshot(
                    axum::http::Request::builder().method("POST").uri("/api/rpc")
                        .header("content-type", "application/json")
                        .body(axum::body::Body::from(serde_json::json!({
                            "id": "panel-click", "method": "panel.item.invoke",
                            "params": { "favorId": folder["id"], "itemId": added["createdIds"][0] },
                        }).to_string())).unwrap(),
                ).await.unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                let body: Value = serde_json::from_slice(&axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
                assert_eq!(body["id"], "panel-click");
                assert_eq!(body["ok"], !fail, "{action}: {body}");
                if fail {
                    assert!(body.get("result").is_none());
                    assert_eq!(body["error"]["code"], "INTERNAL_ERROR");
                    assert_eq!(body["error"]["message"], "clipboard write rejected");
                } else {
                    assert_eq!(body["result"]["action"], "executed");
                    assert!(body.get("error").is_none());
                }
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn slow_menu_work_keeps_the_http_executor_responsive() {
        let app = router(test_state());
        let (entered, started) = tokio::sync::oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let loader = tokio::spawn(blocking_host_work(move || {
            entered.send(()).unwrap();
            blocked.recv_timeout(Duration::from_secs(5)).unwrap();
            StatusCode::OK.into_response()
        }));
        started.await.unwrap();
        let response = app.oneshot(axum::http::Request::builder()
            .uri("/api/health").body(axum::body::Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!loader.is_finished(), "the HTTP executor waited for the blocking menu job");
        release.send(()).unwrap();
        assert_eq!(loader.await.unwrap().status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn health_uses_isolated_tauri_service_id() {
        let app = router(test_state());
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/health")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/json; charset=utf-8")
        );
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["service"], "intellitools-cliphost-tauri");
        assert_eq!(body["backend"], "tauri");
        assert!(body.get("processElevated").is_some());
        assert!(body.get("runAsAdministrator").is_some());
        assert!(body.get("runAsAdministratorEffective").is_some());
        assert!(body.get("coldStartElevationSuppressed").is_some());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn busy_host_state_keeps_health_and_static_http_responsive() {
        let state = test_state();
        let app = router(state.clone());
        let lock_host = state.host.clone();
        let (entered, started) = tokio::sync::oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        // Releasing on assertion failure avoids leaving a fixture thread behind.
        // The backup wait also lets a regressed single-thread executor fail
        // instead of deadlocking the entire test process.
        struct ReleaseOnDrop(Option<std::sync::mpsc::Sender<()>>);
        impl Drop for ReleaseOnDrop {
            fn drop(&mut self) {
                if let Some(release) = self.0.take() { let _ = release.send(()); }
            }
        }
        let guard = ReleaseOnDrop(Some(release));
        let holder = std::thread::spawn(move || lock_host.with_host_state_lock_for_test(|| {
            entered.send(()).unwrap();
            let _ = blocked.recv_timeout(Duration::from_secs(3));
        }));
        started.await.unwrap();
        let began = std::time::Instant::now();
        let mut waiting = Vec::new();
        for _ in 0..8 {
            for (path, method, body) in [
                ("/api/clip/host-window", Method::POST, r#"{"action":"status"}"#),
                ("/api/health/data-paths", Method::GET, ""),
            ] {
                waiting.push(tokio::spawn(app.clone().oneshot(axum::http::Request::builder()
                    .method(method).uri(path).header(header::CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from(body)).unwrap())));
            }
        }
        // Poll the real state-dependent handlers before asking the same
        // executor for routes that do not require that state.
        for _ in 0..4 { tokio::task::yield_now().await; }
        assert!(waiting.iter().all(|request| !request.is_finished()));
        for _ in 0..8 {
            let response = tokio::time::timeout(Duration::from_millis(500),
                app.clone().oneshot(axum::http::Request::builder().uri("/api/health")
                    .body(axum::body::Body::empty()).unwrap())).await.unwrap().unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body: Value = serde_json::from_slice(&axum::body::to_bytes(
                response.into_body(), 65536).await.unwrap()).unwrap();
            assert_eq!(body["ok"], false);
            assert_eq!(body["stateBusy"], true);
            for key in ["windowRuntimeReady", "clipboardWebViewReady", "panelWebViewReady"] {
                assert_eq!(body[key], false, "busy host incorrectly reported {key} ready");
            }
        }
        for (path, expected) in [("/does-not-exist", StatusCode::NOT_FOUND), ("/api/events", StatusCode::OK)] {
            let response = tokio::time::timeout(Duration::from_millis(500),
                app.clone().oneshot(axum::http::Request::builder().uri(path)
                    .body(axum::body::Body::empty()).unwrap())).await.unwrap().unwrap();
            assert_eq!(response.status(), expected, "{path}");
        }
        assert!(began.elapsed() < Duration::from_secs(1),
            "state wait occupied the async executor until the fixture's backup release");
        assert!(!holder.is_finished(), "state lock was released before responsiveness was verified");
        drop(guard);
        holder.join().unwrap();
        for request in waiting {
            let response = tokio::time::timeout(Duration::from_secs(3), request).await.unwrap().unwrap().unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }
        let response = app.oneshot(axum::http::Request::builder().uri("/api/health")
            .body(axum::body::Body::empty()).unwrap()).await.unwrap();
        let body: Value = serde_json::from_slice(&axum::body::to_bytes(
            response.into_body(), 65536).await.unwrap()).unwrap();
        assert_eq!(body["ok"], true);
        assert!(body.get("stateBusy").is_none());
    }

    // Occupy the only blocking worker so these real routes cannot finish until
    // released. Health must still run on the single async executor. Headless
    // dialogs return cancellation; no native windows or slow disk are needed.
    fn assert_routes_leave_http_executor(cases: &[(&str, Method, StatusCode)]) {
        let cases: Vec<_> = cases.iter().map(|(path, method, status)| (*path, method.clone(), *status, "{}")).collect();
        assert_routes_with_bodies_leave_http_executor(&cases);
    }

    fn assert_routes_with_bodies_leave_http_executor(cases: &[(&str, Method, StatusCode, &str)]) {
        let app = router(test_state());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all().max_blocking_threads(1).build().unwrap();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| runtime.block_on(async {
            for (path, method, status, body) in cases {
                let (entered, started) = tokio::sync::oneshot::channel();
                let (release, blocked) = std::sync::mpsc::channel();
                let worker = tokio::task::spawn_blocking(move || {
                    entered.send(()).unwrap();
                    let _ = blocked.recv_timeout(Duration::from_secs(15));
                });
                started.await.unwrap();
                let request = tokio::spawn(app.clone().oneshot(axum::http::Request::builder()
                    .method(method.clone()).uri(*path)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from(body.to_string())).unwrap()));
                // In-memory request bodies are immediately ready. Give the
                // request its turn, including the middleware's spawned work.
                for _ in 0..4 { tokio::task::yield_now().await; }
                let returned_before_worker = request.is_finished();
                let health = app.clone().oneshot(axum::http::Request::builder()
                    .uri("/api/health").body(axum::body::Body::empty()).unwrap()).await.unwrap();
                assert_eq!(health.status(), StatusCode::OK);
                release.send(()).unwrap();
                worker.await.unwrap();
                let response = tokio::time::timeout(Duration::from_secs(15), request).await
                    .unwrap_or_else(|_| panic!("{path} did not finish after its worker was released"))
                    .unwrap().unwrap();
                assert!(!returned_before_worker, "{path} ran synchronous host work on the HTTP executor");
                assert_eq!(response.status(), *status, "{path}");
            }
        })));
        // A lock regression in synchronous host work cannot be cancelled by
        // Tokio. Let a failed test report instead of hanging runtime shutdown.
        runtime.shutdown_background();
        if let Err(panic) = outcome { std::panic::resume_unwind(panic); }
    }

    #[test]
    fn collect_draft_routes_leave_the_http_executor_available() {
        assert_routes_with_bodies_leave_http_executor(&[
            ("/api/clip/collect/drafts/missing", Method::GET, StatusCode::NOT_FOUND, "{}"),
            ("/api/clip/collect/drafts/missing", Method::DELETE, StatusCode::NO_CONTENT, "{}"),
            ("/api/clip/collect/drafts/missing/commit", Method::POST, StatusCode::NOT_FOUND, r#"{"parentId":"fixture"}"#),
        ]);
    }

    #[test]
    fn file_menu_opened_folder_query_leaves_the_http_executor_available() {
        assert_routes_leave_http_executor(&[
            ("/api/file-menu/custom-paths", Method::GET, StatusCode::OK),
        ]);
    }

    #[test]
    fn file_menu_mutations_leave_the_http_executor_available() {
        assert_routes_with_bodies_leave_http_executor(&[
            ("/api/file-menu/custom-paths", Method::PUT, StatusCode::OK, r#"{"lines":["fixture-a","fixture-b"]}"#),
            ("/api/file-menu/custom-paths/add", Method::POST, StatusCode::BAD_REQUEST, r#"{"path":""}"#),
            ("/api/file-menu/custom-paths/move", Method::POST, StatusCode::OK, r#"{"path":"fixture-b","delta":-1}"#),
            ("/api/file-menu/custom-paths/remove", Method::POST, StatusCode::OK, r#"{"path":"fixture-a"}"#),
        ]);
    }

    #[tokio::test]
    async fn file_menu_save_errors_reach_http_and_picker_callers() {
        cliphost_core::force_headless(true);
        let dir = tempdir().unwrap();
        let paths = AppPaths::from_data_root(dir.path().join("BetterClipTauri"));
        let config = paths.configs.join("FileMenuConfig.json");
        let backup = paths.configs.join("FileMenuConfig.saved.json");
        let host = HostState::open(paths, 28965).unwrap();
        host.file_menu_replace(vec!["fixture-a".into(), "fixture-b".into()]).unwrap();
        let before = host.file_menu_snapshot();
        let app = router(HttpState { host: host.clone(), events: event_bus() });
        std::fs::rename(&config, &backup).unwrap();
        std::fs::create_dir(&config).unwrap();
        for (path, method, body) in [
            ("/api/file-menu/custom-paths", Method::PUT, serde_json::json!({"lines":["replacement"]})),
            ("/api/file-menu/custom-paths/add", Method::POST, serde_json::json!({"path":dir.path()})),
            ("/api/file-menu/custom-paths/move", Method::POST, serde_json::json!({"path":"fixture-b","delta":-1})),
            ("/api/file-menu/custom-paths/remove", Method::POST, serde_json::json!({"path":"fixture-a"})),
        ] {
            let response = app.clone().oneshot(axum::http::Request::builder().method(method).uri(path)
                .header(header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(body.to_string())).unwrap()).await.unwrap();
            assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR, "{path}");
            let body: Value = serde_json::from_slice(&axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
            assert!(!body["error"].as_str().unwrap().is_empty(), "{path}");
            assert_eq!(host.file_menu_snapshot(), before, "{path}");
        }
        let picked = serde_json::json!({"paths":[dir.path()]});
        assert_eq!(file_menu_finish_pick(&host, Ok(picked.clone())).status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(file_menu_finish_pick(&host, Err("dialog failure".into())).status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(host.file_menu_snapshot(), before);
        std::fs::remove_dir(&config).unwrap();
        std::fs::rename(&backup, &config).unwrap();
        assert_eq!(file_menu_finish_pick(&host, Ok(picked)).status(), StatusCode::OK);
        assert_eq!(host.file_menu_snapshot()["paths"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn resource_routes_leave_the_http_executor_available() {
        assert_routes_leave_http_executor(&[
            ("/api/clip/path-existence?path=", Method::GET, StatusCode::OK),
            ("/api/clip/items/missing/image", Method::GET, StatusCode::NOT_FOUND),
            ("/api/clip/favorites/missing/image", Method::HEAD, StatusCode::NOT_FOUND),
            ("/api/clip/local-image?path=", Method::GET, StatusCode::NOT_FOUND),
            ("/api/clip/file-icon", Method::GET, StatusCode::BAD_REQUEST),
            ("/api/clip/ui/window-background-image", Method::GET, StatusCode::NOT_FOUND),
            ("/api/menu/window-thumbnail?hwnd=1", Method::GET, StatusCode::NOT_FOUND),
        ]);
    }

    #[test]
    fn dialog_routes_leave_the_http_executor_available() {
        assert_routes_leave_http_executor(&[
            ("/api/clip/ui/pick-folder", Method::POST, StatusCode::OK),
            ("/api/clip/ui/pick-files", Method::POST, StatusCode::OK),
            ("/api/clip/ui/pick-directories", Method::POST, StatusCode::OK),
            ("/api/file-menu/custom-paths/pick-folders", Method::POST, StatusCode::OK),
            ("/api/clip/ui/pick-image-file", Method::POST, StatusCode::OK),
            ("/api/clip/ui/stage-window-background-image", Method::POST, StatusCode::OK),
        ]);
    }

    #[test]
    fn rpc_and_overlay_routes_leave_the_http_executor_available() {
        assert_routes_leave_http_executor(&[
            ("/api/rpc", Method::POST, StatusCode::OK),
            ("/api/clip/quick-paste", Method::POST, StatusCode::OK),
            ("/api/clip/toast", Method::POST, StatusCode::OK),
            ("/api/clip/super-menu", Method::POST, StatusCode::OK),
        ]);
    }

    #[tokio::test]
    async fn file_uri_http_and_rpc_resolve_special_filenames_and_reject_invalid_links() {
        let state = test_state();
        let app = router(state);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("%中文 +#.png");
        let bytes = b"local-image-fixture";
        std::fs::write(&file, bytes).unwrap();
        let path = file.to_string_lossy().replace('\\', "/");
        let literal = format!("file:///{}", path.trim_start_matches('/').replace('#', "%23"));
        let escaped = literal.replace("%中文", "%25%E4%B8%AD%E6%96%87");
        let query_escape = |value: &str| value.bytes().map(|byte| format!("%{byte:02X}")).collect::<String>();
        for uri in [&literal, &escaped, &format!("{escaped}?ignored=yes#part")] {
            let query = query_escape(uri);
            let exists = app.clone().oneshot(axum::http::Request::builder()
                .uri(format!("/api/clip/path-existence?path={query}"))
                .body(axum::body::Body::empty()).unwrap()).await.unwrap();
            assert_eq!(exists.status(), StatusCode::OK, "{uri}");
            let result: Value = serde_json::from_slice(&axum::body::to_bytes(exists.into_body(), usize::MAX).await.unwrap()).unwrap();
            assert_eq!(result, serde_json::json!({"ok":true, "exists":true}));
            let image = app.clone().oneshot(axum::http::Request::builder()
                .uri(format!("/api/clip/local-image?path={query}"))
                .body(axum::body::Body::empty()).unwrap()).await.unwrap();
            assert_eq!(image.status(), StatusCode::OK, "{uri}");
            assert_eq!(axum::body::to_bytes(image.into_body(), usize::MAX).await.unwrap().as_ref(), bytes);
            // Headless host validates the resolved file without opening any program.
            for method in ["clip.shell.openPath", "clip.shell.revealPath"] {
                let response = app.clone().oneshot(axum::http::Request::builder()
                    .method("POST").uri("/api/rpc").header(header::CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from(serde_json::json!({
                        "id":"file-uri", "method":method, "params":{"path":uri},
                    }).to_string())).unwrap()).await.unwrap();
                let result: Value = serde_json::from_slice(&axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
                assert_eq!(result["ok"], true, "{method}: {result}");
            }
        }
        for uri in ["file:relative", "file://user@server/share", "file:///C:/a%00b", "file://server/"] {
            let query = query_escape(uri);
            let exists = app.clone().oneshot(axum::http::Request::builder()
                .uri(format!("/api/clip/path-existence?path={query}"))
                .body(axum::body::Body::empty()).unwrap()).await.unwrap();
            assert_eq!(exists.status(), StatusCode::OK);
            let result: Value = serde_json::from_slice(&axum::body::to_bytes(exists.into_body(), usize::MAX).await.unwrap()).unwrap();
            assert_eq!(result, serde_json::json!({"ok":false, "exists":false}));
            let image = app.clone().oneshot(axum::http::Request::builder()
                .uri(format!("/api/clip/local-image?path={query}"))
                .body(axum::body::Body::empty()).unwrap()).await.unwrap();
            assert_eq!(image.status(), StatusCode::NOT_FOUND);
        }
    }

    #[tokio::test]
    async fn shell_open_failures_reach_menu_and_rpc_callers_and_allow_retry() {
        let state = test_state();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("fixture.exe");
        std::fs::write(&file, "fixture").unwrap();
        state.host.file_menu_add(file.to_str().unwrap()).unwrap();
        let session = state.host.menu_create_session("file");
        let node = session["root"]["items"].as_array().unwrap().iter()
            .find(|node| node["payload"]["path"] == file.to_str().unwrap()).unwrap();
        let uri = format!("/api/menu/sessions/{}/nodes/{}/invoke", session["sessionId"].as_str().unwrap(), node["id"].as_str().unwrap());
        let app = router(state.clone());
        for fail in [true, false] {
            state.host.set_shell_open_runner(move |_| if fail { Err("launcher rejected".into()) } else { Ok(()) });
            let response = app.clone().oneshot(axum::http::Request::builder()
                .method("POST").uri(&uri).header(header::CONTENT_TYPE,"application/json")
                .body(axum::body::Body::from("{}")).unwrap()).await.unwrap();
            assert_eq!(response.status().is_success(), !fail);
            let result: Value = serde_json::from_slice(&axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
            if fail { assert_eq!(result["message"], "launcher rejected"); }
            else { assert_eq!(result["closeMenu"], true); }
            for (method, params) in [
                ("clip.shell.openPath", serde_json::json!({"path":file.to_str().unwrap()})),
                ("clip.shell.openUri", serde_json::json!({"uri":"https://example.com"})),
            ] {
                let response = app.clone().oneshot(axum::http::Request::builder()
                    .method("POST").uri("/api/rpc").header(header::CONTENT_TYPE,"application/json")
                    .body(axum::body::Body::from(serde_json::json!({"id":"shell-open", "method":method, "params":params}).to_string())).unwrap()).await.unwrap();
                let result: Value = serde_json::from_slice(&axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
                assert_eq!(result["ok"], !fail);
                if fail { assert_eq!(result["error"]["message"], "launcher rejected"); }
            }
        }
    }

    #[tokio::test]
    async fn quicker_action_launch_errors_reach_panel_rpc_and_menu_without_consuming_usage() {
        cliphost_core::force_headless(true);
        let dir = tempdir().unwrap();
        let paths = AppPaths::from_data_root(dir.path().join("BetterClipTauri"));
        let host = HostState::open(paths.clone(), 28965).unwrap();
        let folder = host.favor_folder_create(&serde_json::json!({"title":"Quicker HTTP fixture"})).unwrap();
        let added = host.favor_add_text_items(&serde_json::json!({"parentId":folder["id"], "text":"action fixture"})).unwrap();
        let id = added["createdIds"][0].as_str().unwrap();
        let store = cliphost_core::AppStore::open(&paths.db).unwrap();
        let mut item = store.get_favor(id).unwrap();
        item.kind = cliphost_core::ClipKind::ActionClipItem;
        item.body_text = Some("fixture-action".into());
        store.save_favor(&item).unwrap();
        let session = host.menu_create_session("favor");
        let node = session["root"]["items"].as_array().unwrap().iter()
            .find(|node| node["payload"]["favorId"] == id).unwrap();
        let menu_uri = format!("/api/menu/sessions/{}/nodes/{}/invoke", session["sessionId"].as_str().unwrap(), node["id"].as_str().unwrap());
        let app = router(HttpState {host:host.clone(), events:event_bus()});
        for fail in [true, false] {
            host.set_shell_open_runner(move |uri| {
                assert_eq!(uri, "quicker:runaction:fixture-action?");
                if fail { Err("protocol handler unavailable".into()) } else { Ok(()) }
            });
            for (uri, body, rpc) in [
                ("/api/rpc", serde_json::json!({"id":"action", "method":"panel.item.invoke", "params":{"favorId":folder["id"], "itemId":id}}), true),
                (menu_uri.as_str(), serde_json::json!({}), false),
            ] {
                let response = app.clone().oneshot(axum::http::Request::builder()
                    .method("POST").uri(uri).header(header::CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from(body.to_string())).unwrap()).await.unwrap();
                assert_eq!(response.status().is_success(), rpc || !fail);
                let result: Value = serde_json::from_slice(&axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
                if rpc {
                    assert_eq!(result["ok"], !fail);
                    if fail { assert_eq!(result["error"]["message"], "protocol handler unavailable"); }
                    else { assert_eq!(result["result"]["action"], "executed"); }
                } else if fail {
                    assert_eq!(result["message"], "protocol handler unavailable");
                    assert_ne!(result["closeMenu"], true);
                } else { assert_eq!(result["closeMenu"], true); }
            }
            assert_eq!(store.get_usage(&cliphost_core::store::favor_usage_key(id)).unwrap().usage_count, if fail {0} else {2});
        }
    }

    #[tokio::test]
    async fn file_path_actions_rpc_preserves_native_errors_and_allows_retry() {
        cliphost_core::force_headless(true);
        let dir = tempdir().unwrap();
        let paths = AppPaths::from_data_root(dir.path().join("data"));
        let host = HostState::open(paths.clone(), 28965).unwrap();
        let file = dir.path().join("file %中文#.exe");
        std::fs::write(&file, "fixture").unwrap();
        let mut item = cliphost_core::ClipRecord::new_text("files", "fixture");
        item.kind = cliphost_core::ClipKind::FileClipItem;
        item.file_paths = vec![file.to_string_lossy().into_owned()];
        cliphost_core::AppStore::open(&paths.db).unwrap().insert_clip(&item, true).unwrap();
        let app = router(HttpState {host:host.clone(),events:event_bus()});
        for fail in [true, false] {
            host.set_shell_open_runner(move |_| if fail { Err("native path action failed".into()) } else { Ok(()) });
            for (method, params) in [
                ("clip.items.openPath", serde_json::json!({"id":item.id,"path":file})),
                ("clip.shell.revealPath", serde_json::json!({"path":dir.path()})),
                ("clip.items.reveal", serde_json::json!({"id":item.id,"target":"folder"})),
            ] {
                let response = app.clone().oneshot(axum::http::Request::builder()
                    .method("POST").uri("/api/rpc").header(header::CONTENT_TYPE,"application/json")
                    .body(axum::body::Body::from(serde_json::json!({"id":"file-path", "method":method,"params":params}).to_string())).unwrap()).await.unwrap();
                let body: Value = serde_json::from_slice(&axum::body::to_bytes(response.into_body(),usize::MAX).await.unwrap()).unwrap();
                assert_eq!(body["ok"], !fail, "{method}: {body}");
                if fail { assert_eq!(body["error"]["message"], "native path action failed"); }
                else { assert_eq!(body["result"]["ok"], true); }
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pending_clipboard_rpc_keeps_health_and_window_clicks_responsive() {
        let state = test_state();
        let id = state.host.add_text(&serde_json::json!({ "text": "slow UI clipboard" })).unwrap()["createdIds"][0].clone();
        let (entered, started) = tokio::sync::oneshot::channel();
        let entered = std::sync::Mutex::new(Some(entered));
        let (release, blocked) = std::sync::mpsc::channel();
        let blocked = std::sync::Mutex::new(blocked);
        state.host.set_clipboard_ui_runner(move |job| {
            entered.lock().unwrap().take().unwrap().send(()).unwrap();
            blocked.lock().unwrap().recv_timeout(Duration::from_secs(5)).map_err(|e| e.to_string())?;
            job()
        });
        let app = router(state);
        let pending = tokio::spawn(app.clone().oneshot(axum::http::Request::builder()
            .method("POST").uri("/api/rpc").header(header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(serde_json::json!({
                "id": "pending-copy", "method": "clip.items.copy", "params": { "ids": [id] },
            }).to_string())).unwrap()));
        started.await.unwrap();
        let health = app.clone().oneshot(axum::http::Request::builder()
            .uri("/api/health").body(axum::body::Body::empty()).unwrap()).await.unwrap();
        assert_eq!(health.status(), StatusCode::OK);
        let click = app.oneshot(axum::http::Request::builder()
            .method("POST").uri("/api/clip/host-window").header(header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(r#"{"action":"setPinned","value":true}"#)).unwrap()).await.unwrap();
        assert_eq!(click.status(), StatusCode::OK);
        let click_body: Value = serde_json::from_slice(&axum::body::to_bytes(click.into_body(), usize::MAX).await.unwrap()).unwrap();
        assert_eq!(click_body["isPinned"], true);
        assert!(!pending.is_finished(), "HTTP clicks waited for the clipboard job");
        release.send(()).unwrap();
        let response = pending.await.unwrap().unwrap();
        let body: Value = serde_json::from_slice(&axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
        assert_eq!(body["id"], "pending-copy");
        assert_eq!(body["ok"], true);
    }

    #[test]
    fn bind_loopback_keeps_listener_without_dropping() {
        let (port, listener) = bind_loopback_with_fallback(29111).unwrap();
        assert_eq!(listener.local_addr().unwrap().port(), port);
        assert!(port >= 29111);
        assert_ne!(port, cliphost_core::paths::CSHARP_DEFAULT_API_PORT);
    }

    #[tokio::test]
    async fn events_sse_matches_csharp_content_type() {
        let response = router(test_state())
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/events")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/event-stream; charset=utf-8")
        );
        assert_eq!(
            response
                .headers()
                .get(header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some("no-cache")
        );
    }

    #[tokio::test]
    async fn clip_config_update_rpc_does_not_broadcast_clip_changed() {
        let state = test_state();
        let mut rx = state.events.subscribe();
        let response = router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/rpc")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::json!({
                            "id": "1",
                            "method": "clip.config.update",
                            "params": { "clipMonitorOn": true }
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["ok"], true);

        let hide = router(state)
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/rpc")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::json!({
                            "id": "2",
                            "method": "clip.window.hide",
                            "params": {}
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(hide.status(), StatusCode::OK);

        while let Ok(raw) = rx.try_recv() {
            let env: Value = serde_json::from_str(&raw).unwrap();
            assert_ne!(
                env["eventName"], "clipChanged",
                "C# HandleClipConfigUpdate / HandleWindowHide do not broadcast clipChanged: {raw}"
            );
        }
    }

    #[tokio::test]
    async fn unknown_loopback_path_matches_csharp_not_found_json() {
        let response = router(test_state())
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/does-not-exist")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"], "Not found");

        let method_mismatch = router(test_state())
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/host/verb")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(method_mismatch.status(), StatusCode::NOT_FOUND);
        let mismatch_bytes =
            axum::body::to_bytes(method_mismatch.into_body(), usize::MAX).await.unwrap();
        let mismatch: Value = serde_json::from_slice(&mismatch_bytes).unwrap();
        assert_eq!(mismatch["error"], "Not found");
    }

    #[test]
    fn loopback_panic_payload_matches_csharp_exception_message() {
        assert_eq!(
            panic_payload_message(Box::new("boom".to_string())),
            "boom"
        );
        assert_eq!(panic_payload_message(Box::new("x")), "x");
        assert_eq!(
            panic_payload_message(Box::new(42u32)),
            "Internal Server Error"
        );
    }

    #[tokio::test]
    async fn ole_drag_returns_native_failure_to_the_renderer() {
        let state = test_state();
        let id = state.host.add_text(&serde_json::json!({"text":"OLE failure fixture"})).unwrap()["createdIds"][0].clone();
        state.host.set_ole_drag_ui_runner(|_| Err("native drag fixture failed".into()));
        let response = router(state).oneshot(
            axum::http::Request::builder().method("POST").uri("/api/clip/ole-drag")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(serde_json::json!({"ids":[id]}).to_string())).unwrap()
        ).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"], "native drag fixture failed");
    }

    #[tokio::test]
    async fn unmatched_menu_loopback_is_empty_404_like_csharp() {
        let sessions = router(test_state())
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/menu/sessions")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(sessions.status(), StatusCode::NOT_FOUND);
        let sessions_bytes = axum::body::to_bytes(sessions.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(sessions_bytes.is_empty());

        let unknown = router(test_state())
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/menu/does-not-exist")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
        let unknown_bytes = axum::body::to_bytes(unknown.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(unknown_bytes.is_empty());
    }

    #[tokio::test]
    async fn latest_log_matches_csharp_header_and_404() {
        cliphost_core::force_headless(true);
        std::env::set_var("CLIPHOST_TAURI_HEADLESS", "1");
        let dir = tempdir().unwrap();
        let paths = AppPaths::from_data_root(dir.path().join("BetterClipTauri"));
        let host = HostState::open(paths, 28965).unwrap();
        let state = HttpState {
            host,
            events: event_bus(),
        };

        let missing = router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/health/latest-log")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        let bytes = axum::body::to_bytes(missing.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        let err = body["error"].as_str().unwrap();
        assert!(err.starts_with("未找到日志文件。目录："), "{err}");

        std::fs::write(
            state.host.data_root().join("logs").join("cliphost-tauri.log"),
            "hello log",
        )
        .unwrap();
        let found = router(state)
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/health/latest-log")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(found.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(found.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["truncated"], false);
        let text = body["text"].as_str().unwrap();
        assert!(text.contains("// Log file:"));
        assert!(text.contains("// Exported:"));
        assert!(text.contains("hello log"));
    }

    #[tokio::test]
    async fn loopback_omits_cliplink_like_csharp() {
        let app = router(test_state());
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/cliplink/v1/info")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"], "Not found");
    }

    #[tokio::test]
    async fn verb_and_fa_vector_match_csharp_errors() {
        let state = test_state();
        let missing_line = router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/host/verb")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(r#"{}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing_line.status(), StatusCode::BAD_REQUEST);

        let hide = router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/host/verb")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(r#"{"line":"clip hide"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(hide.status(), StatusCode::OK);
        let hide_body: Value =
            serde_json::from_slice(&axum::body::to_bytes(hide.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(hide_body, serde_json::json!({ "ok": true }));

        let collect_icon = router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/host/verb")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(r#"{"line":"collect icon"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(collect_icon.status(), StatusCode::OK);
        let collect_body: Value = serde_json::from_slice(
            &axum::body::to_bytes(collect_icon.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(collect_body, serde_json::json!({ "ok": true }));

        let collect_clip = router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/host/verb")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(r#"{"line":"collect clip"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(collect_clip.status(), StatusCode::OK);
        let collect_clip_body: Value = serde_json::from_slice(
            &axum::body::to_bytes(collect_clip.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(collect_clip_body, serde_json::json!({ "ok": true }));

        let collect_unknown = router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/host/verb")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(r#"{"line":"collect nope"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(collect_unknown.status(), StatusCode::BAD_REQUEST);
        let collect_unknown_body: Value = serde_json::from_slice(
            &axum::body::to_bytes(collect_unknown.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(
            collect_unknown_body["error"]
                .as_str()
                .unwrap()
                .contains("collect verb parse errors"),
            "{collect_unknown_body}"
        );

        for line in ["menu nope", "panel nope"] {
            let response = router(state.clone())
                .oneshot(
                    axum::http::Request::builder()
                        .method("POST")
                        .uri("/api/host/verb")
                        .header("content-type", "application/json")
                        .body(axum::body::Body::from(format!(r#"{{"line":"{line}"}}"#)))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{line}");
            let body: Value = serde_json::from_slice(
                &axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(body, serde_json::json!({ "ok": true }), "{line}");
        }

        let missing_spec = router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/icons/fa-vector")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing_spec.status(), StatusCode::BAD_REQUEST);

        let unknown = router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/icons/fa-vector?spec=NotARealIcon")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unknown.status(), StatusCode::NOT_FOUND);

        let ok = router(state)
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/icons/fa-vector?spec=fa%3ALight_Text")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(ok.status(), StatusCode::OK);
        assert_eq!(
            ok.headers()
                .get(header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some(ICON_CACHE_CONTROL)
        );
        let vector: Value =
            serde_json::from_slice(&axum::body::to_bytes(ok.into_body(), usize::MAX).await.unwrap()).unwrap();
        assert_eq!(vector["icon"], "Light_Text");
        assert!(vector["path"].as_str().unwrap().len() > 20);
    }

    #[tokio::test]
    async fn invalid_loopback_json_matches_csharp_500_error_json() {
        let state = test_state();
        let empty_verb = router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/host/verb")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(empty_verb.status(), StatusCode::BAD_REQUEST);
        let empty_bytes = axum::body::to_bytes(empty_verb.into_body(), usize::MAX)
            .await
            .unwrap();
        let empty_body: Value = serde_json::from_slice(&empty_bytes).unwrap();
        assert_eq!(empty_body["error"], "line is required");

        let bad_verb = router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/host/verb")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from("{"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(bad_verb.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            bad_verb
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/json; charset=utf-8")
        );
        let bad_bytes = axum::body::to_bytes(bad_verb.into_body(), usize::MAX)
            .await
            .unwrap();
        let bad_body: Value = serde_json::from_slice(&bad_bytes).unwrap();
        assert!(bad_body["error"].as_str().unwrap().contains("EOF"));

        let bad_restart = router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/health/restart")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from("{"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(bad_restart.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let restart_bytes = axum::body::to_bytes(bad_restart.into_body(), usize::MAX)
            .await
            .unwrap();
        let restart_body: Value = serde_json::from_slice(&restart_bytes).unwrap();
        assert!(restart_body.get("error").and_then(|v| v.as_str()).is_some());

        let bad_pick = router(state)
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/clip/ui/pick-folder")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from("{"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(bad_pick.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let pick_bytes = axum::body::to_bytes(bad_pick.into_body(), usize::MAX)
            .await
            .unwrap();
        let pick_body: Value = serde_json::from_slice(&pick_bytes).unwrap();
        assert!(pick_body.get("error").and_then(|v| v.as_str()).is_some());
    }

    #[tokio::test]
    async fn titlebar_drag_keeps_followup_clicks_on_the_originating_window() {
        use cliphost_core::ChromeCommand;

        async fn action(state: &HttpState, host: &str, action: &str, value: bool) -> Value {
            let response = router(state.clone()).oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/clip/host-window")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(serde_json::json!({
                        "v": 1, "kind": "clipHostWindow", "host": host,
                        "action": action, "value": value,
                    }).to_string()))
                    .unwrap(),
            ).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            serde_json::from_slice(&axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap()
        }

        for (requested_host, label) in [("main", "main"), ("settings", "main"), ("panel", "panel")] {
            for native_started in [true, false] {
                let state = test_state();
                while state.host.take_chrome_command().is_some() {}
                let drag = action(&state, requested_host, "beginWindowMove", native_started).await;
                assert_eq!(drag["host"], label);
                assert!(!state.host.hide_snapshot().move_interaction_active,
                    "moving another window must not arm clipboard drag state");
                if native_started {
                    assert!(state.host.take_chrome_command().is_none());
                } else {
                    assert!(matches!(state.host.take_chrome_command(),
                        Some(ChromeCommand::BeginCaptionDrag(target)) if target == label));
                }

                // The SPA uses the returned host for the next titlebar action.
                let pinned = action(&state, drag["host"].as_str().unwrap(), "setPinned", true).await;
                assert_eq!(pinned["host"], label);
                assert_eq!(pinned["isPinned"], true);
                assert_eq!(state.host.window_state_dto()["isPinned"], false);
                while state.host.take_chrome_command().is_some() {}
                let hidden = action(&state, pinned["host"].as_str().unwrap(), "hide", false).await;
                assert_eq!(hidden["host"], label);
                let command = state.host.take_chrome_command();
                assert!(matches!((label, command),
                    ("main", Some(ChromeCommand::HideMain)) | ("panel", Some(ChromeCommand::HidePanel))));
            }
        }
    }

    #[cfg(debug_assertions)]
    #[tokio::test]
    async fn debug_loopback_matches_csharp_debug_shape() {
        let state = test_state();
        let collapse = router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/debug/toast/collapse")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(collapse.status(), StatusCode::OK);
        let collapse_body: Value = serde_json::from_slice(
            &axum::body::to_bytes(collapse.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(collapse_body["ok"], true);
        assert_eq!(collapse_body["collapsed"], true);

        let chrome = router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/debug/window-chrome/open")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(chrome.status(), StatusCode::OK);
        let chrome_body: Value = serde_json::from_slice(
            &axum::body::to_bytes(chrome.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(chrome_body["ok"], false);
        assert_eq!(chrome_body["hwnd"], 0);

        let toast = router(state)
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/debug/toast")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(r#"{"message":"  "}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(toast.status(), StatusCode::OK);
        let toast_body: Value = serde_json::from_slice(
            &axum::body::to_bytes(toast.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(toast_body["ok"], true);
    }

    #[tokio::test]
    async fn file_menu_and_menu_session_match_csharp_shapes() {
        let state = test_state();
        let unknown = router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/menu/sessions")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(r#"{"action":"nope"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unknown.status(), StatusCode::BAD_REQUEST);
        let unknown_body: Value = serde_json::from_slice(
            &axum::body::to_bytes(unknown.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(unknown_body["message"], "Unknown action: nope");

        let missing_close = router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("DELETE")
                    .uri("/api/menu/sessions/missing")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing_close.status(), StatusCode::NO_CONTENT);

        let created = router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/menu/sessions")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        r#"{"action":"super","targetWindowHandle":12345}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(created.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(created.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(body["sessionId"].as_str().unwrap().len() > 8);
        assert_eq!(body["root"]["status"], "Ready");
        let session_id = body["sessionId"].as_str().unwrap();
        let empty_value = router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("/api/menu/sessions/{session_id}/nodes/root/value"))
                    .header("content-type", "application/json")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(empty_value.status(), StatusCode::BAD_REQUEST);
        let empty_value_body: Value = serde_json::from_slice(
            &axum::body::to_bytes(empty_value.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(empty_value_body["message"], "value is required");

        let valued = router(state)
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("/api/menu/sessions/{session_id}/nodes/root/value"))
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(r#"{"value":"x"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(valued.status(), StatusCode::OK);
        let valued_body: Value =
            serde_json::from_slice(&axum::body::to_bytes(valued.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(valued_body["closeMenu"], false);
        assert_eq!(valued_body["refresh"], "None");

        let share = tempdir().unwrap();
        let share_path = share.path().to_string_lossy().into_owned();
        let file_state = test_state();
        let added = router(file_state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/file-menu/custom-paths/add")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::json!({ "path": share_path }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(added.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(added.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["ok"], true);
        assert_eq!(body["paths"][0], share_path);
        assert!(body.get("openedFolders").is_none());

        let listed = router(file_state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/file-menu/custom-paths")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(listed.status(), StatusCode::OK);
        let listed_body: Value = serde_json::from_slice(
            &axum::body::to_bytes(listed.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(listed_body.get("openedFolders").is_some());
        assert_eq!(listed_body["paths"][0], share_path);

        let replaced = router(file_state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("PUT")
                    .uri("/api/file-menu/custom-paths")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::json!({ "paths": [share_path] }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(replaced.status(), StatusCode::OK);
        let replaced_body: Value = serde_json::from_slice(
            &axum::body::to_bytes(replaced.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(replaced_body.get("openedFolders").is_none());
        assert_eq!(replaced_body["paths"][0], share_path);

        let bad_add = router(file_state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/file-menu/custom-paths/add")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from("{"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(bad_add.status(), StatusCode::BAD_REQUEST);
        let bad_add_body: Value = serde_json::from_slice(
            &axum::body::to_bytes(bad_add.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(bad_add_body["ok"], false);

        let bad_move = router(file_state)
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/file-menu/custom-paths/move")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(bad_move.status(), StatusCode::BAD_REQUEST);
        let bad_move_body: Value = serde_json::from_slice(
            &axum::body::to_bytes(bad_move.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(bad_move_body["message"], "path and delta required");
    }

    #[tokio::test]
    async fn cliplink_info_uses_cea_clip_link_protocol() {
        let app = cliplink_test_router(test_state());
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/cliplink/v1/info")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["protocol"], "cea-clip-link");
        assert_eq!(body["version"], 1);
        assert_eq!(body["certificateFingerprint"].as_str().unwrap().len(), 64);
    }

    #[tokio::test]
    async fn cliplink_pair_uses_problem_json_and_403_for_bad_code() {
        let app = cliplink_test_router(test_state());
        let bad_token = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/cliplink/v1/pair")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::json!({
                            "deviceId": "peerA",
                            "alias": "desk",
                            "baseUrl": "https://10.0.0.2:29766",
                            "certificateFingerprint": "a".repeat(64),
                            "pairingCode": "000000",
                            "tokenForReceiver": "recv-token",
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(bad_token.status(), StatusCode::BAD_REQUEST);
        let bad_bytes = axum::body::to_bytes(bad_token.into_body(), usize::MAX)
            .await
            .unwrap();
        let bad_body: Value = serde_json::from_slice(&bad_bytes).unwrap();
        assert_eq!(bad_body["status"], 400);
        assert_eq!(bad_body["title"], "Bad Request");
        assert_eq!(bad_body["detail"], "配对令牌格式无效。");

        let state = test_state();
        let app = cliplink_test_router(state);
        let forbidden = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/cliplink/v1/pair")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::json!({
                            "deviceId": "peerA",
                            "alias": "desk",
                            "baseUrl": "https://10.0.0.2:29766",
                            "certificateFingerprint": "a".repeat(64),
                            "pairingCode": "000000",
                            "tokenForReceiver": cliphost_core::lan::create_token(),
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);
        let bytes = axum::body::to_bytes(forbidden.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["status"], 403);
        assert_eq!(body["title"], "Forbidden");
        assert_eq!(body["detail"], "配对码无效或已过期。");
    }

    #[tokio::test]
    async fn cliplink_text_requires_paired_bearer() {
        let state = test_state();
        let code = state.host.lan_status()["pairingCode"]["code"]
            .as_str()
            .unwrap()
            .to_string();
        let pair = state
            .host
            .lan_accept_inbound_pair(&serde_json::json!({
                "deviceId": "peerA",
                "alias": "desk",
                "baseUrl": "https://10.0.0.2:29766",
                "certificateFingerprint": "a".repeat(64),
                "pairingCode": code,
                "tokenForReceiver": cliphost_core::lan::create_token(),
            }))
            .unwrap();
        let _token = pair["tokenForRequester"].as_str().unwrap().to_string();
        let app = cliplink_test_router(state);
        let denied = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/cliplink/v1/messages/text")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(r#"{"id":"m","senderDeviceId":"peerA","text":"x","targetKind":"direct","createdAtUtc":"2026-01-01T00:00:00Z"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);

        let state = test_state();
        let code = state.host.lan_status()["pairingCode"]["code"]
            .as_str()
            .unwrap()
            .to_string();
        let pair = state
            .host
            .lan_accept_inbound_pair(&serde_json::json!({
                "deviceId": "peerA",
                "alias": "desk",
                "baseUrl": "https://10.0.0.2:29766",
                "certificateFingerprint": "a".repeat(64),
                "pairingCode": code,
                "tokenForReceiver": cliphost_core::lan::create_token(),
            }))
            .unwrap();
        let token = pair["tokenForRequester"].as_str().unwrap().to_string();
        let now = chrono::Utc::now().to_rfc3339();
        let body = format!(
            r#"{{"id":"msg1","senderDeviceId":"peerA","senderAlias":"desk","targetKind":"direct","text":"hello-lan","createdAtUtc":"{now}","attachments":[]}}"#
        );
        let app = cliplink_test_router(state);
        let ok = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/cliplink/v1/messages/text")
                    .header("content-type", "application/json")
                    .header("X-ClipLink-Device", "peerA")
                    .header("Authorization", format!("Bearer {token}"))
                    .body(axum::body::Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(ok.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(ok.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["delivered"], true);
    }

    #[tokio::test]
    async fn window_thumbnail_requires_hwnd() {
        let app = router(test_state());
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/menu/window-thumbnail?hwnd=0")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn file_icon_requires_path_proc_or_spec() {
        let app = router(test_state());
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/clip/file-icon")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["error"], "path, proc, or spec is required.");

        let icon = router(test_state())
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/clip/file-icon?spec=.txt")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(icon.status(), StatusCode::OK);
        assert_eq!(
            icon.headers()
                .get(header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some(ICON_CACHE_CONTROL)
        );
        assert_eq!(
            icon.headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("image/png")
        );

        let stripped = router(test_state())
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/clip/file-icon?spec=icon:.txt")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(stripped.status(), StatusCode::OK);

        let fa_spec = router(test_state())
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/clip/file-icon?spec=fa%3ALight_Text")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(fa_spec.status(), StatusCode::BAD_REQUEST);
        let fa_body: Value = serde_json::from_slice(
            &axum::body::to_bytes(fa_spec.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(fa_body["error"], "spec is not a shell/file icon path.");

        let missing_image = router(test_state())
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/clip/items/missing/image")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing_image.status(), StatusCode::NOT_FOUND);
        let missing_bytes = axum::body::to_bytes(missing_image.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(missing_bytes.is_empty());
    }

    #[tokio::test]
    async fn unicode_icon_requests_return_normal_responses() {
        let app = router(test_state());
        for (path, status) in [
            ("/api/clip/file-icon?path=C%3A%5C%E4%B8%AD%E6%96%87%5Ccliphost-nonexistent.txt", StatusCode::NOT_FOUND),
            ("/api/clip/file-icon?spec=%2E%E6%96%87%E6%9C%AC", StatusCode::OK),
            ("/api/icons/fa-vector?spec=%F0%9F%A6%80", StatusCode::NOT_FOUND),
            ("/api/health", StatusCode::OK),
        ] {
            let response = app.clone().oneshot(
                axum::http::Request::builder().uri(path)
                    .body(axum::body::Body::empty()).unwrap(),
            ).await.unwrap();
            assert_eq!(response.status(), status, "{path}");
            if path.contains("spec=%2E") {
                assert_eq!(response.headers()[header::CONTENT_TYPE], "image/png");
                let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
                assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
            }
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn lan_persistence_failures_reach_rpc_callers() {
        use std::os::windows::fs::OpenOptionsExt;
        cliphost_core::force_headless(true);
        let directory = tempdir().unwrap();
        let paths = AppPaths::from_data_root(directory.path().join("host"));
        let host = HostState::open(paths.clone(), 28965).unwrap();
        host.lan_config_update(&serde_json::json!({"deviceAlias":"original"})).unwrap();
        host.lan_accept_inbound_pair(&serde_json::json!({
            "deviceId":"peer", "alias":"peer", "baseUrl":"https://127.0.0.1:29766",
            "certificateFingerprint":"b".repeat(64), "tokenForReceiver":cliphost_core::lan::create_token(),
            "pairingCode":host.lan_refresh_code()["code"]
        })).unwrap();
        let app = router(HttpState { host: host.clone(), events: event_bus() });
        for (method, params, file, expected) in [
            ("lan.config.update", serde_json::json!({"deviceAlias":"changed"}), "settings.json", "保存局域网设置失败"),
            ("lan.peer.forget", serde_json::json!({"deviceId":"peer"}), "peers.json", "保存配对设备失败"),
        ] {
            let locked = std::fs::OpenOptions::new().read(true).share_mode(1)
                .open(paths.configs.join("LanShare").join(file)).unwrap();
            let request = serde_json::json!({"id":"persist", "method":method, "params":params});
            let response = app.clone().oneshot(axum::http::Request::builder()
                .method("POST").uri("/api/rpc").header(header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(request.to_string())).unwrap()).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body: Value = serde_json::from_slice(&axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
            assert_eq!(body["ok"], false, "{method}: {body}");
            assert!(body["error"]["message"].as_str().unwrap().contains(expected));
            drop(locked);
            let response = app.clone().oneshot(axum::http::Request::builder()
                .method("POST").uri("/api/rpc").header(header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(request.to_string())).unwrap()).await.unwrap();
            let body: Value = serde_json::from_slice(&axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
            assert_eq!(body["ok"], true);
        }
        assert_eq!(host.lan_status()["deviceAlias"], "changed");
        assert!(host.lan_peers().as_array().unwrap().is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn lan_state_work_leaves_the_async_executor_available() {
        let host = test_state().host;
        let (entered, entering) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let (tick, ticked) = std::sync::mpsc::channel();
        let observer = std::thread::spawn(move || {
            entering.recv_timeout(Duration::from_secs(5)).unwrap();
            let responsive = ticked.recv_timeout(Duration::from_millis(500)).is_ok();
            release.send(()).unwrap();
            responsive
        });
        let worker = tokio::spawn(lan_host_work(host, move |host| {
            entered.send(()).unwrap();
            released.recv_timeout(Duration::from_secs(5)).unwrap();
            host.lan_listener_port()
        }));
        tokio::time::sleep(Duration::from_millis(20)).await;
        tick.send(()).unwrap();
        assert_eq!(worker.await.unwrap().unwrap(), None);
        assert!(observer.join().unwrap(), "LAN state wait stalled the single async worker");
    }

    #[tokio::test]
    async fn loopback_cors_allows_trusted_private_network_origin() {
        let response = router(test_state())
            .oneshot(
                axum::http::Request::builder()
                    .method("OPTIONS")
                    .uri("/api/health")
                    .header("Origin", "http://tauri.localhost")
                    .header("Access-Control-Request-Method", "POST")
                    .header(
                        "Access-Control-Request-Headers",
                        "content-type,x-clip-web-session",
                    )
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let headers = response.headers();
        assert_eq!(
            headers
                .get("access-control-allow-origin")
                .and_then(|v| v.to_str().ok()),
            Some("http://tauri.localhost")
        );
        assert_eq!(
            headers
                .get("access-control-allow-private-network")
                .and_then(|v| v.to_str().ok()),
            Some("true")
        );
        assert_eq!(
            headers
                .get("access-control-allow-methods")
                .and_then(|v| v.to_str().ok()),
            Some("GET, POST, DELETE, HEAD, OPTIONS")
        );
        assert_eq!(
            headers
                .get("access-control-allow-headers")
                .and_then(|v| v.to_str().ok()),
            Some("Content-Type, X-Clip-Web-Session")
        );
    }

    #[tokio::test]
    async fn leftover_rest_and_cloud_probe_match_csharp_keys() {
        let app = router(test_state());
        let probe = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/health/cloud-sync")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(probe.status(), StatusCode::OK);
        let probe_body: Value = serde_json::from_slice(
            &axum::body::to_bytes(probe.into_body(), usize::MAX).await.unwrap(),
        )
        .unwrap();
        assert_eq!(probe_body["rpcConnected"], false);
        assert_eq!(probe_body["ok"], false);

        let quicker = router(test_state())
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/health/quicker")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(quicker.status(), StatusCode::OK);
        let quicker_body: Value = serde_json::from_slice(
            &axum::body::to_bytes(quicker.into_body(), usize::MAX).await.unwrap(),
        )
        .unwrap();
        assert_eq!(quicker_body["connected"], false);
        assert_eq!(
            quicker_body["hint"],
            "Start IntelliTools in Quicker so the Quicker-side pipe RPC session can connect."
        );
        assert!(quicker_body.get("listening").is_none());
        assert!(quicker_body.get("optional").is_none());
        assert!(quicker_body.get("pipes").is_some());
        assert!(quicker_body.get("pipe").is_some());

        let pin = router(test_state())
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/clip/pin-overlay/status")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(pin.status(), StatusCode::OK);
        let pin_body: Value =
            serde_json::from_slice(&axum::body::to_bytes(pin.into_body(), usize::MAX).await.unwrap()).unwrap();
        assert!(pin_body.get("snipasteAvailable").is_some());
        assert!(pin_body.get("pixPinAvailable").is_some());

        let exists = router(test_state())
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/clip/path-existence?path=C%3A%5CWindows")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(exists.status(), StatusCode::OK);
        let exists_body: Value =
            serde_json::from_slice(&axum::body::to_bytes(exists.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(exists_body["ok"], true);
        assert!(exists_body.get("exists").is_some());

        let device = router(test_state())
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/clip/path-existence?path=%5C%5C.%5CC%3A")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(device.status(), StatusCode::OK);
        let device_body: Value =
            serde_json::from_slice(&axum::body::to_bytes(device.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(device_body["ok"], false);
        assert_eq!(device_body["exists"], false);

        let denied_image = router(test_state())
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/clip/local-image?path=%5C%5C.%5CC%3A")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(denied_image.status(), StatusCode::NOT_FOUND);

        let png = tempfile::NamedTempFile::new().unwrap();
        let png_path = png.path().with_extension("png");
        std::fs::write(&png_path, b"png-bytes").unwrap();
        let encoded: String = png_path
            .to_string_lossy()
            .bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b'_' => (b as char).to_string(),
                _ => format!("%{b:02X}"),
            })
            .collect();
        let local = router(test_state())
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/api/clip/local-image?path={encoded}"))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(local.status(), StatusCode::OK);
        let local_bytes = axum::body::to_bytes(local.into_body(), usize::MAX).await.unwrap();
        assert_eq!(local_bytes.as_ref(), b"png-bytes");
        let head = router(test_state())
            .oneshot(
                axum::http::Request::builder()
                    .method("HEAD")
                    .uri(format!("/api/clip/local-image?path={encoded}"))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(head.status(), StatusCode::OK);
        assert_eq!(
            head.headers().get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()),
            Some(local_bytes.len().to_string().as_str())
        );
        assert_eq!(
            head.headers().get(header::ACCEPT_RANGES).and_then(|v| v.to_str().ok()),
            Some("bytes")
        );
        let head_bytes = axum::body::to_bytes(head.into_body(), usize::MAX).await.unwrap();
        assert!(head_bytes.is_empty());

        let ranged = router(test_state())
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/api/clip/local-image?path={encoded}"))
                    .header("range", "bytes=0-2")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(ranged.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            ranged.headers().get(header::CONTENT_RANGE).and_then(|v| v.to_str().ok()),
            Some("bytes 0-2/9")
        );
        let ranged_bytes = axum::body::to_bytes(ranged.into_body(), usize::MAX).await.unwrap();
        assert_eq!(ranged_bytes.as_ref(), b"png");

        let unsat = router(test_state())
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/api/clip/local-image?path={encoded}"))
                    .header("range", "bytes=99-120")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unsat.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(
            unsat.headers().get(header::CONTENT_RANGE).and_then(|v| v.to_str().ok()),
            Some("bytes */9")
        );

        let boundary = "----ClipHostStage";
        let mut multipart = Vec::new();
        multipart.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        multipart.extend_from_slice(b"Content-Disposition: form-data; name=\"file\"; filename=\"wall.png\"\r\n");
        multipart.extend_from_slice(b"Content-Type: image/png\r\n\r\n");
        multipart.extend_from_slice(b"png-bytes");
        multipart.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let staged = router(test_state())
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/clip/ui/stage-window-background-image")
                    .header("content-type", format!("multipart/form-data; boundary={boundary}"))
                    .body(axum::body::Body::from(multipart))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(staged.status(), StatusCode::OK);
        let staged_body: Value =
            serde_json::from_slice(&axum::body::to_bytes(staged.into_body(), usize::MAX).await.unwrap()).unwrap();
        assert_eq!(staged_body["cancelled"], false);
        assert!(staged_body["path"].as_str().unwrap().replace('/', "\\").contains("\\WindowBackground\\"));

        let dirs = router(test_state())
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/clip/ui/pick-directories")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(dirs.status(), StatusCode::OK);
        let dirs_body: Value =
            serde_json::from_slice(&axum::body::to_bytes(dirs.into_body(), usize::MAX).await.unwrap()).unwrap();
        assert_eq!(dirs_body["cancelled"], true);
        assert_eq!(dirs_body["paths"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn collect_draft_http_matches_csharp_status() {
        use cliphost_core::{ClipKind, FavorRecord};
        use chrono::Utc;

        let state = test_state();
        let folder = state
            .host
            .favor_folder_create(&serde_json::json!({ "title": "inbox" }))
            .unwrap();
        let folder_id = folder["id"].as_str().unwrap().to_string();
        let draft_id = state.host.register_collect_draft(FavorRecord {
            storage_revision: None,
            id: "draft-http".into(),
            parent_id: None,
            is_folder: false,
            name: "draft".into(),
            kind: ClipKind::TextClipItem,
            body_text: Some("body".into()),
            file_paths: Vec::new(),
            image_path: None,
            children: Vec::new(),
            create_time: Utc::now(),
            update_time: Utc::now(),
            source_clip_id: None,
            html_data: None,
            html_source_url: None,
            qq_rich_text: None,
            source_process_name: String::new(),
            sub_index: 0,
            icon: String::new(),
            execute_param: String::new(),
        });
        let missing_parent = router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("/api/clip/collect/drafts/{draft_id}/commit"))
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing_parent.status(), StatusCode::BAD_REQUEST);
        let ok = router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("/api/clip/collect/drafts/{draft_id}/commit"))
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::json!({ "parentId": folder_id, "title": "saved" }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(ok.status(), StatusCode::NO_CONTENT);
        let gone = router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("/api/clip/collect/drafts/{draft_id}/commit"))
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::json!({ "parentId": folder_id }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(gone.status(), StatusCode::NOT_FOUND);
        let gone_body = axum::body::to_bytes(gone.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(gone_body.is_empty());
        let missing_draft = router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/clip/collect/drafts/missing")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing_draft.status(), StatusCode::NOT_FOUND);
        let missing_body = axum::body::to_bytes(missing_draft.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(missing_body.is_empty());
        let discard = router(state)
            .oneshot(
                axum::http::Request::builder()
                    .method("DELETE")
                    .uri("/api/clip/collect/drafts/any")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(discard.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn host_window_spa_modal_and_pin_match_csharp_actions() {
        let state = test_state();
        let app = router(state.clone());
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/clip/host-window")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::json!({ "kind": "clipHostWindow", "action": "setPinned", "value": true })
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value =
            serde_json::from_slice(&axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body["isPinned"], true);
        let spa = router(state)
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/clip/host-window")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::json!({ "kind": "clipHostWindow", "action": "setSpaModalOpen", "value": true })
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(spa.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn quick_paste_web_route_matches_csharp_overlay_actions() {
        let state = test_state();
        state
            .host
            .add_text(&serde_json::json!({ "text": "from-overlay" }))
            .unwrap();
        state.host.qp_debug_begin();
        let detail = state.host.qp_state_detail();
        let item_id = detail["items"][0]["id"].as_str().unwrap().to_string();
        let armed = router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/clip/quick-paste")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::json!({
                            "v": 1,
                            "kind": "clipQuickPaste",
                            "action": "armedAction",
                            "itemId": item_id,
                            "armedKind": "simulate"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(armed.status(), StatusCode::OK);
        let body: Value =
            serde_json::from_slice(&axum::body::to_bytes(armed.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body["ok"], true);
        assert_eq!(body["armedKind"], "simulate");
        let after = state.host.qp_state_detail();
        assert_eq!(after["armedItemId"], item_id);
        assert_eq!(after["armedKind"], "simulate");
    }

    #[tokio::test]
    async fn overlay_input_failures_return_errors_and_push_visible_feedback() {
        for (path, kind, action) in [
            ("/api/clip/quick-paste", "clipQuickPaste", "simulateText"),
            ("/api/clip/toast", "clipToast", "simulateText"),
            ("/api/clip/toast", "clipToast", "pasteEnter"),
        ] {
            let state = test_state();
            let created = state.host.add_text(&serde_json::json!({ "text": "HTTP input failure fixture" })).unwrap();
            let id = created["createdIds"][0].as_str().unwrap();
            state.host.qp_debug_begin();
            state.host.set_input_runner(|_| Err("input fixture rejected".into()));
            state.host.set_clipboard_ui_runner(|_| Err("clipboard fixture rejected".into()));
            let response = router(state.clone()).oneshot(
                axum::http::Request::builder()
                    .method("POST").uri(path).header("content-type", "application/json")
                    .body(axum::body::Body::from(serde_json::json!({
                        "v": 1, "kind": kind, "action": action, "itemId": id,
                    }).to_string())).unwrap()
            ).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body: Value = serde_json::from_slice(&axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
            assert_eq!(body["ok"], false, "{path} {action}: {body}");
            assert!(body["message"].as_str().is_some_and(|message| !message.is_empty()));
            let toast = state.host.take_toast_push().expect("fire-and-forget overlay messages need visible error feedback");
            assert_eq!(toast["kind"], "error");
            assert_eq!(toast["message"], body["message"]);
        }
    }

    #[tokio::test]
    async fn toast_web_route_matches_csharp_overlay_actions() {
        let state = test_state();
        let created = state
            .host
            .add_text(&serde_json::json!({ "text": "toast-http" }))
            .unwrap();
        let item_id = created["createdIds"][0].as_str().unwrap().to_string();
        state.host.toast_show();
        let hover = router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/clip/toast")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::json!({ "v": 1, "kind": "clipToast", "action": "hover" }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(hover.status(), StatusCode::OK);
        let sim = router(state)
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/clip/toast")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::json!({
                            "v": 1,
                            "kind": "clipToast",
                            "action": "simulateText",
                            "itemId": item_id
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(sim.status(), StatusCode::OK);
        let body: Value =
            serde_json::from_slice(&axum::body::to_bytes(sim.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body["ok"], true);
    }

    #[tokio::test]
    async fn super_menu_web_route_closes_session_on_idle() {
        let state = test_state();
        let created = state.host.menu_create_session("favor");
        assert_eq!(state.host.super_menu_state_detail()["visible"], true);
        let idle = router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/clip/super-menu")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::json!({
                            "v": 1,
                            "kind": "clipSuperMenu",
                            "action": "idle"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(idle.status(), StatusCode::OK);
        let body: Value =
            serde_json::from_slice(&axum::body::to_bytes(idle.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body["ok"], true);
        assert_eq!(state.host.super_menu_state_detail()["visible"], false);
        let _ = created;
    }

    #[tokio::test]
    async fn chrome_web_route_opens_menu_and_panel() {
        let state = test_state();
        let tray = router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/clip/chrome")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::json!({
                            "v": 1,
                            "kind": "mainHostCommand",
                            "command": "setShowTrayIcon",
                            "show": false
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(tray.status(), StatusCode::OK);
        let body: Value =
            serde_json::from_slice(&axum::body::to_bytes(tray.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body["showTrayIcon"], false);
        let open = router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/clip/chrome")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::json!({
                            "v": 1,
                            "kind": "appHostCommand",
                            "command": "open",
                            "route": "/main/home"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(open.status(), StatusCode::OK);
        let open_body: Value =
            serde_json::from_slice(&axum::body::to_bytes(open.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(open_body["route"], "/main/home");
        assert_eq!(state.host.take_pending_nav("main").as_deref(), Some("/main/home"));
    }

    #[tokio::test]
    async fn live_loopback_reports_tauri_health_and_skips_csharp_port() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        assert_ne!(
            cliphost_core::DEFAULT_API_PORT,
            cliphost_core::paths::CSHARP_DEFAULT_API_PORT
        );
        let state = test_state();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        assert_ne!(addr.port(), cliphost_core::paths::CSHARP_DEFAULT_API_PORT);
        assert_ne!(addr.port(), cliphost_core::paths::QUICKER_MCP_PORT);
        let app = router(state);
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        async fn http_get(addr: std::net::SocketAddr, path: &str) -> String {
            let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            let req = format!(
                "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
                addr.port()
            );
            stream.write_all(req.as_bytes()).await.unwrap();
            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).await.unwrap();
            String::from_utf8_lossy(&buf).into_owned()
        }

        let health = http_get(addr, "/api/health").await;
        assert!(health.contains("intellitools-cliphost-tauri"));
        assert!(health.contains("\"backend\":\"tauri\""));
        let paths = http_get(addr, "/api/health/data-paths").await;
        assert!(paths.contains("\"isolatedFromCsharp\":true"));
        assert!(paths.contains("BetterClipTauri"));
        assert!(!paths.contains("BetterClip\\\\userdata"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn lan_https_pair_and_text_between_two_hosts() {
        cliphost_core::force_headless(true);
        std::env::set_var("CLIPHOST_TAURI_HEADLESS", "1");

        let dir = tempdir().unwrap();
        // This exercises real TLS and transfer on this machine only. Binding
        // all adapters makes each Cargo test executable trigger firewall UI.
        let listener_a = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listener_b = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        assert!(listener_a.local_addr().unwrap().ip().is_loopback());
        assert!(listener_b.local_addr().unwrap().ip().is_loopback());
        let port_a = listener_a.local_addr().unwrap().port();
        let port_b = listener_b.local_addr().unwrap().port();
        assert_ne!(port_a, port_b);

        let host_a = HostState::open(AppPaths::from_data_root(dir.path().join("hostA")), 28965).unwrap();
        let host_b = HostState::open(AppPaths::from_data_root(dir.path().join("hostB")), 28966).unwrap();
        host_a
            .lan_config_update(&serde_json::json!({
                "enabled": true,
                "port": port_a,
                "deviceAlias": "Alpha"
            }))
            .unwrap();
        host_b
            .lan_config_update(&serde_json::json!({
                "enabled": true,
                "port": port_b,
                "deviceAlias": "Beta"
            }))
            .unwrap();

        let a = HttpState {
            host: host_a,
            events: event_bus(),
        };
        let b = HttpState {
            host: host_b,
            events: event_bus(),
        };
        tokio::spawn(crate::lan_tls::serve_lan_https_listener(a.clone(), listener_a));
        tokio::spawn(crate::lan_tls::serve_lan_https_listener(b.clone(), listener_b));

        for _ in 0..100 {
            if a.host.lan_status()["running"].as_bool() == Some(true)
                && b.host.lan_status()["running"].as_bool() == Some(true)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(a.host.lan_status()["running"].as_bool(), Some(true), "{:?}", a.host.lan_status());
        assert_eq!(b.host.lan_status()["running"].as_bool(), Some(true), "{:?}", b.host.lan_status());

        a.host.lan_ingest_datagram(&b.host.lan_announce_bytes("announce"), "127.0.0.1");
        b.host.lan_ingest_datagram(&a.host.lan_announce_bytes("announce"), "127.0.0.1");

        let b_id = b.host.lan_status()["deviceId"].as_str().unwrap().to_string();
        let code = b.host.lan_status()["pairingCode"]["code"]
            .as_str()
            .unwrap()
            .to_string();
        let a_host = a.host.clone();
        let pair = tokio::task::spawn_blocking(move || {
            a_host.lan_pair(&serde_json::json!({
                "deviceId": b_id,
                "pairingCode": code,
            }))
        })
        .await
        .unwrap()
        .expect("pair");
        assert_eq!(pair["paired"], true, "{pair}");

        let note = dir.path().join("note.bin");
        std::fs::write(&note, b"tls-file").unwrap();
        let note_path = note.to_string_lossy().into_owned();
        let a_host = a.host.clone();
        let sent = tokio::task::spawn_blocking(move || {
            a_host.lan_send(&serde_json::json!({
                "conversationId": cliphost_core::lan::ALL_CONVERSATION_ID,
                "text": "hello-live-tls",
                "filePaths": [note_path],
            }))
        })
        .await
        .unwrap()
        .expect("send");
        assert_eq!(sent["status"], "queued", "{sent}");

        let mut incoming = serde_json::json!([]);
        for _ in 0..100 {
            incoming = b
                .host
                .lan_messages(&serde_json::json!({
                    "conversationId": cliphost_core::lan::ALL_CONVERSATION_ID
                }))
                .unwrap();
            if incoming.as_array().is_some_and(|items| !items.is_empty()) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(incoming[0]["text"], "hello-live-tls", "{incoming}");
        assert_eq!(incoming[0]["direction"], "incoming");
        assert_eq!(incoming[0]["attachments"].as_array().unwrap().len(), 1);
        let local = incoming[0]["attachments"][0]["localPath"].as_str().unwrap();
        assert_eq!(std::fs::read(local).unwrap(), b"tls-file");
    }

    // An ephemeral port available to one protocol need not be available to the
    // other. Keep both reservations until a fixture deliberately releases them.
    async fn reserve_lan_loopback_port(
    ) -> std::io::Result<(tokio::net::TcpListener, tokio::net::UdpSocket)> {
        use std::io::ErrorKind;
        use std::net::Ipv4Addr;
        use tokio::net::{TcpListener, UdpSocket};

        let mut last_error = None;
        for _ in 0..32 {
            let udp = match UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await {
                Ok(udp) => udp,
                Err(error)
                    if matches!(
                        error.kind(),
                        ErrorKind::AddrInUse | ErrorKind::PermissionDenied
                    ) =>
                {
                    last_error = Some(error);
                    continue;
                }
                Err(error) => return Err(error),
            };
            let port = udp.local_addr()?.port();
            match TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await {
                Ok(tcp) => return Ok((tcp, udp)),
                Err(error)
                    if matches!(
                        error.kind(),
                        ErrorKind::AddrInUse | ErrorKind::PermissionDenied
                    ) =>
                {
                    last_error = Some(error);
                }
                Err(error) => return Err(error),
            }
        }
        Err(last_error.expect("at least one loopback reservation was attempted"))
    }

    async fn assert_lan_listener_failure_recovers(block_udp: bool) {
        use tokio::time::{sleep, timeout, Duration};
        cliphost_core::force_headless(true);
        let dir = tempdir().unwrap();
        let host = HostState::open(AppPaths::from_data_root(dir.path().join("host")), 28965).unwrap();
        let (tcp, udp) = reserve_lan_loopback_port().await.expect("reserve TCP and UDP loopback test port");
        let port = tcp.local_addr().unwrap().port();
        let (mut tcp, mut udp) = (Some(tcp), Some(udp));
        if block_udp { drop(tcp.take()); } else { drop(udp.take()); }
        host.lan_config_update(&serde_json::json!({"enabled": true, "port": port})).unwrap();
        host.drain_push_events();
        let state = HttpState { host: host.clone(), events: event_bus() };
        struct Server(tokio::task::JoinHandle<std::io::Result<()>>);
        impl Drop for Server { fn drop(&mut self) { self.0.abort(); } }
        let server = Server(tokio::spawn(serve_lan(state.clone())));
        timeout(Duration::from_secs(3), async {
            loop {
                if !host.lan_status()["error"].as_str().unwrap().is_empty() { break; }
                sleep(Duration::from_millis(20)).await;
            }
        }).await.expect("listener failure never reached the UI status");
        let failed = host.lan_status();
        assert_eq!(failed["running"], false);
        assert!(failed["error"].as_str().unwrap().contains(&port.to_string()), "{failed}");
        assert!(host.drain_push_events().iter().any(|(name, data)| {
            name == cliphost_core::lan::PEERS_CHANGED_EVENT && data["error"] == failed["error"]
        }), "listener failure did not request an existing UI status refresh");
        drop((tcp.take(), udp.take()));
        timeout(Duration::from_secs(8), async {
            loop {
                let status = host.lan_status();
                if status["running"] == true && status["error"] == "" { break; }
                sleep(Duration::from_millis(20)).await;
            }
        }).await.expect("listener did not recover and clear its error after releasing the port");
        assert!(host.drain_push_events().iter().any(|(name, _)| name == cliphost_core::lan::PEERS_CHANGED_EVENT));
        host.lan_config_update(&serde_json::json!({"enabled": false})).unwrap();
        timeout(Duration::from_secs(3), async {
            while host.lan_status()["running"] == true { sleep(Duration::from_millis(20)).await; }
        }).await.unwrap();
        assert_eq!(host.lan_status()["error"], "");
        server.0.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lan_listener_failures_tcp_are_visible_and_recover_on_loopback() {
        assert_lan_listener_failure_recovers(false).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lan_listener_failures_udp_are_visible_and_recover_on_loopback() {
        assert_lan_listener_failure_recovers(true).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lan_listeners_follow_enable_disable_and_port_changes_on_loopback() {
        use std::net::Ipv4Addr;
        use tokio::io::AsyncReadExt;
        use tokio::net::{TcpListener, TcpStream, UdpSocket};
        use tokio::time::{sleep, timeout, Duration};
        cliphost_core::force_headless(true);
        let dir = tempdir().unwrap();
        let host = HostState::open(AppPaths::from_data_root(dir.path().join("host")), 28965).unwrap();
        let reserve_a = reserve_lan_loopback_port().await.expect("reserve first TCP and UDP loopback test port");
        let reserve_b = reserve_lan_loopback_port().await.expect("reserve second TCP and UDP loopback test port");
        let port_a = reserve_a.0.local_addr().unwrap().port();
        let port_b = reserve_b.0.local_addr().unwrap().port();
        assert_ne!(port_a, port_b);
        host.lan_config_update(&serde_json::json!({"enabled": false, "port": port_a})).unwrap();
        assert_eq!(host.lan_listener_port(), None);
        drop(reserve_a);
        let state = HttpState { host: host.clone(), events: event_bus() };
        // Exercise the same public entry point as the app. The test build must
        // bind loopback even when a fixture enables LAN or changes its port.
        let server = tokio::spawn(serve_lan(state));
        sleep(Duration::from_millis(350)).await;
        assert_eq!(host.lan_status()["running"], false);
        let tcp = TcpListener::bind((Ipv4Addr::LOCALHOST, port_a)).await.expect("disabled LAN held TCP port");
        let udp = UdpSocket::bind((Ipv4Addr::LOCALHOST, port_a)).await.expect("disabled LAN held UDP port");
        drop((tcp, udp));

        async fn wait_for_listeners(host: &HostState, port: u16) {
            timeout(Duration::from_secs(10), async {
                loop {
                    let tcp_ready = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).await.is_ok();
                    let udp_ready = UdpSocket::bind((Ipv4Addr::LOCALHOST, port)).await.is_err();
                    if host.lan_status()["running"] == true && tcp_ready && udp_ready { break; }
                    sleep(Duration::from_millis(20)).await;
                }
            }).await.expect("enabled LAN did not bind TCP and UDP");
        }
        async fn assert_released(port: u16) {
            timeout(Duration::from_secs(10), async {
                loop {
                    if let (Ok(tcp), Ok(udp)) = (
                        TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await,
                        UdpSocket::bind((Ipv4Addr::LOCALHOST, port)).await,
                    ) { drop((tcp, udp)); break; }
                    sleep(Duration::from_millis(20)).await;
                }
            }).await.expect("old LAN port still bound");
        }
        host.lan_config_update(&serde_json::json!({"enabled": true})).unwrap();
        wait_for_listeners(&host, port_a).await;
        let mut pending_handshake = TcpStream::connect((Ipv4Addr::LOCALHOST, port_a)).await.unwrap();
        drop(reserve_b);
        host.lan_config_update(&serde_json::json!({"port": port_b})).unwrap();
        wait_for_listeners(&host, port_b).await;
        assert_released(port_a).await;
        let mut byte = [0u8; 1];
        let closed = timeout(Duration::from_secs(10), pending_handshake.read(&mut byte)).await.expect("old accepted connection survived restart");
        assert!(closed.is_err() || closed.unwrap() == 0);
        host.lan_config_update(&serde_json::json!({"enabled": false})).unwrap();
        assert_released(port_b).await;
        assert_eq!(host.lan_status()["running"], false);
        server.abort();
        let _ = server.await;
    }
}

#[cfg(all(test, not(feature = "private-sync")))]
mod private_sync_boundary_tests;
