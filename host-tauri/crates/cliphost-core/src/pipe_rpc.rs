//! C# `PipeRpcConnection` frames: 4-byte little-endian length + UTF-8 JSON.
//! Tauri listens on isolated pipe names and never steals the C# verb pipes.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::paths::{CSHARP_VERB_RPC_PIPE_V1, CSHARP_VERB_RPC_PIPE_V2};
use crate::HostState;

pub const PROTOCOL_VERSION: i32 = 1;
pub const MAX_FRAME_LENGTH: usize = 64 * 1024 * 1024;
pub const METHOD_DEBUG: &str = "cliphost.getIsDebugConfiguration";
pub const METHOD_VERSION: &str = "cliphost.getFileVersion";
pub const METHOD_SHUTDOWN: &str = "cliphost.requestGracefulShutdown";
pub const METHOD_RUN_VERB: &str = "cliphost.runVerb";

const TYPE_REQUEST: &str = "request";
const TYPE_RESPONSE: &str = "response";
const TYPE_ERROR: &str = "error";
const TYPE_CANCEL: &str = "cancel";

/// Broadcast cancellation for listeners and connected sessions. Unlike a
/// periodic AtomicBool check, this leaves an idle RPC runtime asleep.
#[derive(Default)]
pub struct StopSignal {
    stopped: AtomicBool,
    wakeup: tokio::sync::Notify,
}

impl StopSignal {
    pub fn request(&self) {
        self.stopped.store(true, Ordering::Release);
        self.wakeup.notify_waiters();
    }

    pub fn reset(&self) {
        self.stopped.store(false, Ordering::Release);
    }

    pub(crate) async fn wait(&self) {
        loop {
            let notified = self.wakeup.notified();
            tokio::pin!(notified);
            // Register before reading the flag so a concurrent stop cannot
            // fall between checking the flag and subscribing to its wakeup.
            notified.as_mut().enable();
            if self.stopped.load(Ordering::Acquire) { return; }
            notified.await;
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct PipeRpcMessage {
    #[serde(alias = "version")]
    pub version: i32,
    #[serde(rename = "Type", alias = "type")]
    pub type_name: String,
    #[serde(alias = "id")]
    pub id: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(alias = "method")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(alias = "payload")]
    pub payload: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(alias = "errorCode")]
    pub error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(alias = "errorMessage")]
    pub error_message: Option<String>,
}

impl PipeRpcMessage {
    pub(crate) fn request(id: i64, method: &str, payload: Value) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            type_name: TYPE_REQUEST.into(),
            id,
            method: Some(method.into()),
            payload: Some(payload),
            error_code: None,
            error_message: None,
        }
    }
    pub(crate) fn error(id: i64, code: &str, message: &str) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            type_name: TYPE_ERROR.into(),
            id,
            method: None,
            payload: None,
            error_code: Some(code.into()),
            error_message: Some(message.into()),
        }
    }
    pub(crate) fn cancel(id: i64) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            type_name: TYPE_CANCEL.into(),
            id,
            method: None,
            payload: None,
            error_code: None,
            error_message: None,
        }
    }
}

pub fn encode_frame(message: &PipeRpcMessage) -> Result<Vec<u8>, String> {
    let payload = serde_json::to_vec(message).map_err(|e| e.to_string())?;
    if payload.is_empty() || payload.len() > MAX_FRAME_LENGTH {
        return Err(format!(
            "Pipe RPC frame length {} is outside the allowed range.",
            payload.len()
        ));
    }
    let len = payload.len() as u32;
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

pub fn decode_frame(bytes: &[u8]) -> Result<PipeRpcMessage, String> {
    let message: PipeRpcMessage = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    if message.version != PROTOCOL_VERSION {
        return Err(format!(
            "Unsupported pipe RPC protocol version {}; expected {PROTOCOL_VERSION}.",
            message.version
        ));
    }
    Ok(message)
}

pub fn read_message(reader: &mut impl Read) -> Result<Option<PipeRpcMessage>, String> {
    let mut header = [0u8; 4];
    match reader.read_exact(&mut header) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(err.to_string()),
    }
    let length = u32::from_le_bytes(header) as usize;
    if length == 0 || length > MAX_FRAME_LENGTH {
        return Err(format!("Invalid pipe RPC frame length {length}."));
    }
    let mut payload = vec![0u8; length];
    reader.read_exact(&mut payload).map_err(|e| e.to_string())?;
    decode_frame(&payload).map(Some)
}

pub fn write_message(writer: &mut impl Write, message: &PipeRpcMessage) -> Result<(), String> {
    let frame = encode_frame(message)?;
    writer.write_all(&frame).map_err(|e| e.to_string())?;
    writer.flush().map_err(|e| e.to_string())
}

pub fn dispatch(host: &HostState, method: &str, payload: &Value) -> Result<Value, String> {
    match method {
        METHOD_DEBUG => Ok(json!(cfg!(debug_assertions))),
        METHOD_VERSION => Ok(json!(env!("CARGO_PKG_VERSION"))),
        METHOD_SHUTDOWN => {
            host.request_graceful_shutdown();
            Ok(Value::Null)
        }
        METHOD_RUN_VERB => {
            let line = payload
                .get("Line")
                .or_else(|| payload.get("line"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            // C# `ClipHostVerbRpcService.RunVerbAsync`: blank → false; ExecuteLine throw → false
            // (boolean payload, not a pipe error frame); success including collect MessageBox-return → true.
            if line.is_empty() {
                return Ok(json!(false));
            }
            let ctx = crate::verb::VerbRunContext::from_json(
                payload
                    .get("RunContext")
                    .or_else(|| payload.get("runContext"))
                    .or_else(|| payload.get("Context"))
                    .or_else(|| payload.get("context")),
            );
            Ok(json!(host
                .execute_verb_with_context(line, ctx.as_ref())
                .is_ok()))
        }
        _ => Err(format!("Unknown pipe RPC method '{method}'.")),
    }
}

pub fn handle_inbound(host: &HostState, incoming: &PipeRpcMessage) -> Option<PipeRpcMessage> {
    match incoming.type_name.as_str() {
        TYPE_CANCEL => None,
        TYPE_REQUEST => {
            let method = incoming.method.clone().unwrap_or_default();
            let payload = incoming.payload.clone().unwrap_or(Value::Null);
            match dispatch(host, &method, &payload) {
                Ok(payload) => Some(PipeRpcMessage {
                    version: PROTOCOL_VERSION,
                    type_name: TYPE_RESPONSE.into(),
                    id: incoming.id,
                    method: None,
                    payload: if payload.is_null() {
                        None
                    } else {
                        Some(payload)
                    },
                    error_code: None,
                    error_message: None,
                }),
                Err(err) => Some(PipeRpcMessage {
                    version: PROTOCOL_VERSION,
                    type_name: TYPE_ERROR.into(),
                    id: incoming.id,
                    method: None,
                    payload: None,
                    error_code: Some(if err.contains("Unknown pipe RPC method") {
                        "method_not_found".into()
                    } else {
                        "remote_error".into()
                    }),
                    error_message: Some(err),
                }),
            }
        }
        _ => None,
    }
}

pub fn is_csharp_verb_pipe(name: &str) -> bool {
    name == CSHARP_VERB_RPC_PIPE_V1 || name == CSHARP_VERB_RPC_PIPE_V2
}

fn verb_pipe_enabled(headless: bool, setting: Option<&str>) -> bool {
    // Native acceptance uses real windows/hooks but explicitly disables the
    // shared connector namespace. Its opt-out must apply outside headless mode.
    match setting {
        Some("0") => false,
        Some("1") => true,
        _ => !headless,
    }
}

pub fn spawn_listeners(
    host: HostState,
    pipes: &[&str],
    stop: Arc<StopSignal>,
    listening: Arc<AtomicBool>,
) -> Result<Vec<JoinHandle<()>>, String> {
    if pipes.iter().any(|name| is_csharp_verb_pipe(name)) {
        return Err("Tauri verb RPC must not listen on C# ClipHost pipe names.".into());
    }
    if !verb_pipe_enabled(crate::clipboard::is_headless(),
        std::env::var("CLIPHOST_TAURI_VERB_PIPE").ok().as_deref()) {
        return Ok(Vec::new());
    }
    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ServerOptions;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        let mut servers = Vec::new();
        {
            let _enter = runtime.enter();
            for name in pipes {
                let path = format!(r"\\.\pipe\{name}");
                // Claim only our isolated namespace, and fail startup on a collision.
                let server = ServerOptions::new()
                    .first_pipe_instance(true)
                    .reject_remote_clients(true)
                    .create(&path)
                    .map_err(|e| e.to_string())?;
                servers.push(((*name).to_string(), path, server));
            }
        }
        listening.store(true, Ordering::Release);
        let handle = std::thread::Builder::new()
            .name("cliphost-verb-rpc".into())
            .spawn(move || {
                runtime.block_on(async {
                    let mut listeners = tokio::task::JoinSet::new();
                    for (name, path, server) in servers {
                        let (host, stop) = (host.clone(), stop.clone());
                        listeners.spawn(listen_loop(host, name, path, server, stop));
                    }
                    // Any listener failure closes all sessions; never report a partial startup as healthy.
                    if let Some(result) = listeners.join_next().await {
                        if let Ok(Err(error)) = result {
                            eprintln!("[cliphost-tauri] verb RPC: {error}");
                        }
                    }
                    listeners.abort_all();
                    while listeners.join_next().await.is_some() {}
                    host.quicker_rpc().disconnect_all();
                    listening.store(false, Ordering::Release);
                });
                runtime.shutdown_timeout(std::time::Duration::from_secs(1));
            })
            .map_err(|e| e.to_string())?;
        Ok(vec![handle])
    }
    #[cfg(not(windows))]
    {
        let _ = (host, pipes, stop, listening);
        Ok(Vec::new())
    }
}

#[cfg(windows)]
async fn listen_loop(
    host: HostState,
    pipe_name: String,
    path: String,
    mut server: tokio::net::windows::named_pipe::NamedPipeServer,
    stop: Arc<StopSignal>,
) -> Result<(), String> {
    use tokio::net::windows::named_pipe::ServerOptions;
    let mut sessions = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            result = server.connect(), if sessions.len() < 16 => {
                result.map_err(|e| e.to_string())?;
                // Keep an accepting instance present while an existing plugin is connected.
                let next = ServerOptions::new().reject_remote_clients(true).create(&path)
                    .map_err(|e| e.to_string())?;
                let connected = std::mem::replace(&mut server, next);
                let registry = host.quicker_rpc();
                let (callback_host, name, stop) = (host.clone(), pipe_name.clone(), stop.clone());
                sessions.spawn(async move {
                    crate::quicker_rpc::serve(connected, registry, &name, stop,
                        Arc::new(move |request| handle_inbound(&callback_host, request))).await
                });
            }
            _ = sessions.join_next(), if !sessions.is_empty() => {},
            _ = stop.wait() => break,
        }
    }
    sessions.abort_all();
    while sessions.join_next().await.is_some() {}
    Ok(())
}

#[cfg(windows)]
pub fn call_method(pipe_name: &str, method: &str, payload: Value) -> Result<Value, String> {
    use std::os::windows::io::FromRawHandle;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    if is_csharp_verb_pipe(pipe_name) {
        return Err("Test client must not connect to C# ClipHost pipes.".into());
    }
    let path = format!(r"\\.\pipe\{pipe_name}");
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            GENERIC_READ.0 | GENERIC_WRITE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
    .map_err(|e| e.to_string())?;
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let mut file = unsafe { std::fs::File::from_raw_handle(handle.0 as *mut std::ffi::c_void) };
    write_message(
        &mut file,
        &PipeRpcMessage {
            version: PROTOCOL_VERSION,
            type_name: TYPE_REQUEST.into(),
            id: 1,
            method: Some(method.into()),
            payload: Some(payload),
            error_code: None,
            error_message: None,
        },
    )?;
    loop {
        let reply = read_message(&mut file)?.ok_or_else(|| "Pipe RPC peer closed.".to_string())?;
        if reply.type_name == TYPE_REQUEST {
            // Ordinary verb clients do not implement Quicker callbacks.
            write_message(
                &mut file,
                &PipeRpcMessage::error(reply.id, "method_not_found", "Verb-only client."),
            )?;
        } else if reply.id == 1 {
            match reply.type_name.as_str() {
                TYPE_ERROR => {
                    return Err(reply
                        .error_message
                        .unwrap_or_else(|| "Remote pipe RPC call failed.".into()))
                }
                TYPE_RESPONSE => return Ok(reply.payload.unwrap_or(Value::Null)),
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::AppPaths;
    use tempfile::tempdir;

    fn host() -> HostState {
        crate::clipboard::force_headless(true);
        std::env::set_var("CLIPHOST_TAURI_HEADLESS", "1");
        HostState::open(
            AppPaths::from_data_root(tempdir().unwrap().path().join("BetterClipTauri")),
            28965,
        )
        .unwrap()
    }

    #[test]
    fn frame_roundtrip_uses_pascal_case() {
        let raw = encode_frame(&PipeRpcMessage {
            version: 1,
            type_name: "request".into(),
            id: 7,
            method: Some(METHOD_RUN_VERB.into()),
            payload: Some(json!({ "Line": "clip hide" })),
            error_code: None,
            error_message: None,
        })
        .unwrap();
        let json = String::from_utf8_lossy(&raw[4..]);
        assert!(json.contains("\"Version\":1"), "{json}");
        assert!(json.contains("\"Type\":\"request\""), "{json}");
        assert!(json.contains("\"Method\""), "{json}");
        let decoded = decode_frame(&raw[4..]).unwrap();
        assert_eq!(decoded.id, 7);
        assert_eq!(decoded.method.as_deref(), Some(METHOD_RUN_VERB));
    }

    #[test]
    fn dispatch_run_verb_and_rejects_unknown() {
        let host = host();
        let ok = dispatch(&host, METHOD_RUN_VERB, &json!({ "Line": "clip hide" })).unwrap();
        assert_eq!(ok, true);
        let auto = dispatch(
            &host,
            METHOD_RUN_VERB,
            &json!({
                "Line": "clip show",
                "RunContext": { "IsAutomaticRun": true, "TargetWindowHandle": 77 }
            }),
        )
        .unwrap();
        assert_eq!(auto, true);
        assert_eq!(
            dispatch(&host, METHOD_RUN_VERB, &json!({ "Line": "   " })).unwrap(),
            false
        );
        assert_eq!(
            dispatch(&host, METHOD_RUN_VERB, &json!({ "Line": "collect clip" })).unwrap(),
            true
        );
        assert_eq!(
            dispatch(&host, METHOD_RUN_VERB, &json!({ "Line": "collect icon" })).unwrap(),
            true
        );
        assert_eq!(
            dispatch(&host, METHOD_RUN_VERB, &json!({ "Line": "collect nope" })).unwrap(),
            false
        );
        let debug = dispatch(&host, METHOD_DEBUG, &Value::Null).unwrap();
        assert_eq!(debug, json!(cfg!(debug_assertions)));
        let err = dispatch(&host, "cliphost.missing", &Value::Null).unwrap_err();
        assert!(err.contains("Unknown pipe RPC method"));
    }

    #[test]
    fn refuses_csharp_pipe_names() {
        let host = host();
        let stop = Arc::new(StopSignal::default());
        stop.request();
        let listening = Arc::new(AtomicBool::new(false));
        let err = spawn_listeners(host, &[CSHARP_VERB_RPC_PIPE_V1], stop, listening).unwrap_err();
        assert!(err.contains("must not listen"));
    }

    #[tokio::test]
    async fn stop_signal_wakes_all_current_and_late_waiters() {
        let stop = Arc::new(StopSignal::default());
        let first = stop.clone();
        let second = stop.clone();
        let first = tokio::spawn(async move { first.wait().await });
        let second = tokio::spawn(async move { second.wait().await });
        tokio::task::yield_now().await;
        assert!(!first.is_finished());
        assert!(!second.is_finished());
        stop.request();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            first.await.unwrap();
            second.await.unwrap();
            stop.wait().await;
        }).await.unwrap();
        stop.reset();
        assert!(tokio::time::timeout(std::time::Duration::from_millis(20), stop.wait()).await.is_err());
        stop.request();
        stop.wait().await;
    }

    #[test]
    fn verb_pipe_explicit_opt_out_also_disables_desktop_listener() {
        assert!(!verb_pipe_enabled(false, Some("0")));
        assert!(!verb_pipe_enabled(true, Some("0")));
        assert!(!verb_pipe_enabled(true, None));
        assert!(verb_pipe_enabled(false, None));
        assert!(verb_pipe_enabled(false, Some("1")));
        assert!(verb_pipe_enabled(true, Some("1")));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn isolated_named_pipe_accepts_concurrent_clients_and_stops_with_open_client() {
        use tokio::net::windows::named_pipe::{ClientOptions, ServerOptions};
        let host = host();
        let name = format!("ClipHostTauriTest_{}", uuid::Uuid::new_v4().simple());
        let path = format!(r"\\.\pipe\{name}");
        let server = ServerOptions::new()
            .first_pipe_instance(true)
            .reject_remote_clients(true)
            .create(&path)
            .unwrap();
        let stop = Arc::new(StopSignal::default());
        let listening = tokio::spawn(listen_loop(
            host.clone(),
            name.clone(),
            path.clone(),
            server,
            stop.clone(),
        ));
        let client_name = name.clone();
        let result = tokio::task::spawn_blocking(move || {
            call_method(&client_name, METHOD_VERSION, Value::Null)
        });
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(3), result)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            env!("CARGO_PKG_VERSION")
        );
        async fn wait_state(host: &HostState, client: bool, callback: bool) {
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                loop {
                    let state = host.main_host_state_dto();
                    if state["verbRpcClientConnected"] == client
                        && state["quickerRpcConnected"] == callback { break; }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            }).await.unwrap();
        }
        async fn ping(peer: &mut tokio::net::windows::named_pipe::NamedPipeClient) -> PipeRpcMessage {
            let message = tokio::time::timeout(std::time::Duration::from_secs(3),
                crate::quicker_rpc::read_message(peer)).await.unwrap().unwrap().unwrap();
            assert_eq!(message.method.as_deref(), Some("quicker.ping"));
            message
        }
        async fn pong(peer: &mut tokio::net::windows::named_pipe::NamedPipeClient, id: i64) {
            use tokio::io::AsyncWriteExt;
            let mut reply = PipeRpcMessage::request(id, "", json!("pong"));
            reply.type_name = TYPE_RESPONSE.into();
            reply.method = None;
            peer.write_all(&encode_frame(&reply).unwrap()).await.unwrap();
        }
        wait_state(&host, false, false).await;
        assert!(host.take_main_host_connection_update().is_none());
        let mut peer = ClientOptions::new().open(&path).unwrap();
        let request = ping(&mut peer).await;
        let update = host.take_main_host_connection_update().unwrap();
        assert_eq!(update["verbRpcClientConnected"], true);
        assert_eq!(update["quickerRpcConnected"], false);
        assert!(host.take_main_host_connection_update().is_none());
        pong(&mut peer, request.id).await;
        wait_state(&host, true, true).await;
        assert_eq!(host.take_main_host_connection_update().unwrap()["quickerRpcConnected"], true);
        assert_eq!(host.main_window_state_dto()["quickerRpcConnected"], true);
        assert!(host.take_main_host_connection_update().is_none());

        let mut second = ClientOptions::new().open(&path).unwrap();
        let request = ping(&mut second).await;
        drop(peer);
        // Closing one ready client must leave the other inbound client connected.
        wait_state(&host, true, false).await;
        let update = host.take_main_host_connection_update().unwrap();
        assert_eq!(update["verbRpcClientConnected"], true);
        assert_eq!(update["quickerRpcConnected"], false);
        pong(&mut second, request.id).await;
        wait_state(&host, true, true).await;
        assert_eq!(host.take_main_host_connection_update().unwrap()["quickerRpcConnected"], true);
        drop(second);
        wait_state(&host, false, false).await;
        assert_eq!(host.take_main_host_connection_update().unwrap()["verbRpcClientConnected"], false);

        let mut peer = ClientOptions::new().open(&path).unwrap();
        let request = ping(&mut peer).await;
        pong(&mut peer, request.id).await;
        wait_state(&host, true, true).await;
        assert_eq!(host.take_main_host_connection_update().unwrap()["quickerRpcConnected"], true);
        stop.request();
        tokio::time::timeout(std::time::Duration::from_secs(1), listening)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        drop(peer);
        let update = host.take_main_host_connection_update().unwrap();
        assert_eq!(update["verbRpcClientConnected"], false);
        assert_eq!(update["quickerRpcConnected"], false);
        assert!(host.take_main_host_connection_update().is_none());
        // No blocked ConnectNamedPipe/read thread retains the namespace.
        let server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&path)
            .unwrap();
        drop(server);
    }
}
