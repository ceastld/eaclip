use std::io::ErrorKind;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use rustls::ServerConfig;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio::time::timeout;
use tokio_rustls::TlsAcceptor;

use crate::HttpState;

const JSON_CONTENT_TYPE: &str = "application/json; charset=utf-8";
const EMPTY_CONTENT_TYPE: &str = "application/octet-stream";
/// C# `LanShareNetworkHost._connectionLimit`.
const CONNECTION_LIMIT: usize = 64;
/// C# handshake / header `CancelAfter(TimeSpan.FromSeconds(30))`.
const IO_TIMEOUT: Duration = Duration::from_secs(30);
/// C# `MaxHeaderBytes`.
const MAX_HEADER_BYTES: usize = 64 * 1024;
/// C# `MaxPairJsonBytes`.
const MAX_PAIR_JSON_BYTES: usize = 64 * 1024;
/// C# `MaxPrepareJsonBytes` = `MaxTextLength + 512 KiB`.
const MAX_PREPARE_JSON_BYTES: usize = cliphost_core::lan::MAX_TEXT_LENGTH + (512 * 1024);

pub async fn serve_lan_https(state: HttpState, port: u16) -> std::io::Result<()> {
    serve_lan_https_at(state, SocketAddr::from((crate::lan_listen_address(), port))).await
}

pub(crate) async fn serve_lan_https_at(state: HttpState, address: SocketAddr) -> std::io::Result<()> {
    let listener = TcpListener::bind(address).await?;
    serve_lan_https_listener(state, listener).await
}

/// Serve an already-bound listener. Integration fixtures explicitly bind
/// loopback rather than inheriting the user-enabled LAN listen address.
pub async fn serve_lan_https_listener(state: HttpState, listener: TcpListener) -> std::io::Result<()> {
    let Some((cert, key)) = crate::lan_host_work(state.host.clone(), |host| host.lan_tls_der()).await? else {
        return Err(std::io::Error::new(ErrorKind::Other, "LAN TLS identity missing"));
    };
    let _ = rustls::crypto::ring::default_provider().install_default();
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(cert)],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)),
        )
        .map_err(|e| std::io::Error::new(ErrorKind::InvalidData, e))?;
    let acceptor = TlsAcceptor::from(Arc::new(config));
    #[cfg(feature = "private-sync")]
    let sync = Some(crate::mobile_sync::SyncHub::new(state.host.clone()));
    #[cfg(not(feature = "private-sync"))]
    let sync: Option<PrivateSyncHub> = None;
    let limit = Arc::new(Semaphore::new(CONNECTION_LIMIT));
    let mut connections = tokio::task::JoinSet::new();
    let port = listener.local_addr()?.port();
    crate::lan_host_work(state.host.clone(), move |host| {
        host.lan_set_listener_error(port, String::new());
    }).await?;
    state.host.lan_set_running(true);
    struct Running(cliphost_core::HostState);
    impl Drop for Running {
        fn drop(&mut self) { self.0.lan_set_running(false); }
    }
    let _running = Running(state.host.clone());
    tracing::info!("ClipLink HTTPS listening on {}", listener.local_addr()?);
    loop {
        while connections.try_join_next().is_some() {}
        let (tcp, peer) = listener.accept().await?;
        let remote_ip = socket_ip_string(peer);
        let acceptor = acceptor.clone();
        let host = state.host.clone();
        let sync = sync.clone();
        let permit = match limit.clone().acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => break,
        };
        // Dropping the listener future (disable, port change or shutdown) also
        // closes accepted connections instead of leaving detached LAN tasks.
        connections.spawn(async move {
            let _permit = permit;
            let Ok(Ok(mut tls)) = timeout(IO_TIMEOUT, acceptor.accept(tcp)).await else {
                return;
            };
            serve_http_connection_with_sync(&mut tls, host, Some(remote_ip), sync).await;
            let _ = tls.shutdown().await;
        });
    }
    Ok(())
}

fn socket_ip_string(addr: SocketAddr) -> String {
    match addr.ip() {
        IpAddr::V4(v) => v.to_string(),
        IpAddr::V6(v) => v
            .to_ipv4_mapped()
            .map(|mapped| mapped.to_string())
            .unwrap_or_else(|| v.to_string()),
    }
}

pub(super) struct HttpReq {
    pub(super) device_sync_observation: Option<(Option<u32>,u64)>,
    pub(super) method: String,
    pub(super) path: String,
    pub(super) query: String,
    pub(super) headers: Vec<(String, String)>,
    pub(super) body: Vec<u8>,
    pub(super) keep_alive: bool,
    pub(super) body_fully_consumed: bool,
    pub(super) content_length: i64,
}

enum ReadError {
    Io,
    Invalid(String),
    Rejected(u16, String),
}

struct ConnBuf {
    data: Vec<u8>,
}

impl ConnBuf {
    fn new() -> Self {
        Self { data: Vec::new() }
    }
}

pub(super) fn header<'a>(req: &'a HttpReq, name: &str) -> Option<&'a str> {
    req.headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

#[cfg(test)]
async fn serve_http_connection<S>(stream: &mut S, host: cliphost_core::HostState, remote_ip: Option<String>)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    serve_http_connection_with_sync(stream, host, remote_ip, None).await
}

#[cfg(feature = "private-sync")]
type PrivateSyncHub = Arc<crate::mobile_sync::SyncHub>;
#[cfg(not(feature = "private-sync"))]
type PrivateSyncHub = ();

async fn serve_http_connection_with_sync<S>(stream: &mut S, host: cliphost_core::HostState, remote_ip: Option<String>, _sync: Option<PrivateSyncHub>)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut buf = ConnBuf::new();
    loop {
        let req = match timeout(IO_TIMEOUT, read_http_guarded(stream, &mut buf, Some(&host))).await {
            Ok(Ok(req)) => req,
            Ok(Err(ReadError::Io)) => return,
            Ok(Err(ReadError::Invalid(msg))) => {
                let (status, content_type, body) = json_err(400, &msg);
                let _ = write_http(stream, status, content_type, &body, false).await;
                return;
            }
            Ok(Err(ReadError::Rejected(status, msg))) => {
                let (_, content_type, body) = json_err(status, &msg);
                let _ = write_http(stream, status, content_type, &body, false).await;
                return;
            }
            Err(_) => return,
        };
        let keep = req.keep_alive && req.body_fully_consumed;
        #[cfg(feature = "private-sync")]
        if crate::device_sync_pull::handles(&req.method, &req.path) {
            let response = crate::device_sync_pull::handle(&host, req).await;
            if write_sync_response(stream, response, keep).await.is_err() || !keep { return; }
            continue;
        }
        #[cfg(feature = "private-sync")]
        if crate::mobile_sync::is_sync_path(&req.path) {
            let response = if let Some(sync) = &_sync {
                if req.body_fully_consumed { sync.handle(req).await }
                else { crate::mobile_sync::SyncResponse::error(400, "请求正文不完整。") }
            } else { crate::mobile_sync::SyncResponse::error(503, "同步服务未启动。") };
            if write_sync_response(stream, response, keep).await.is_err() || !keep { return; }
            continue;
        }
        // Hashing, file writes and waiting for another receiver operation must
        // not occupy the async workers that also drain desktop window commands.
        let request_host = host.clone();
        let request_ip = remote_ip.clone();
        let result = tokio::task::spawn_blocking(move || {
            dispatch(&request_host, &req, request_ip.as_deref())
        }).await;
        let (status, content_type, body) = match result {
            Ok(response) => response,
            Err(error) => {
                tracing::error!(%error, "ClipLink request worker failed");
                json_err(500, "接收请求处理失败。")
            }
        };
        if write_http(stream, status, content_type, &body, keep).await.is_err() {
            return;
        }
        if !keep {
            return;
        }
    }
}

async fn read_http_guarded<S: AsyncRead + Unpin>(stream: &mut S, buf: &mut ConnBuf, host: Option<&cliphost_core::HostState>) -> Result<HttpReq, ReadError> {
    loop {
        if header_end(&buf.data).is_some() {
            break;
        }
        if buf.data.len() > MAX_HEADER_BYTES {
            return Err(ReadError::Invalid("HTTP 请求头过大。".into()));
        }
        let mut scratch = [0u8; 1024];
        let n = stream.read(&mut scratch).await.map_err(|_| ReadError::Io)?;
        if n == 0 {
            return Err(ReadError::Io);
        }
        buf.data.extend_from_slice(&scratch[..n]);
        if buf.data.len() > MAX_HEADER_BYTES && header_end(&buf.data).is_none() {
            return Err(ReadError::Invalid("HTTP 请求头过大。".into()));
        }
    }
    let header_end = header_end(&buf.data).ok_or(ReadError::Io)?;
    let head = String::from_utf8_lossy(&buf.data[..header_end]);
    let mut lines = head.split("\r\n");
    let start = lines.next().unwrap_or("");
    if start.is_empty() {
        return Err(ReadError::Invalid("HTTP 请求行缺失。".into()));
    }
    let mut parts = start.split_whitespace();
    let method = parts.next().unwrap_or("").to_ascii_uppercase();
    let target = parts.next().unwrap_or("/");
    let version = parts.next().unwrap_or("");
    if !version.starts_with("HTTP/1.") {
        return Err(ReadError::Invalid("仅支持 HTTP/1.x 请求。".into()));
    }
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some(separator) = line.find(':') else {
            return Err(ReadError::Invalid("HTTP 请求头格式无效。".into()));
        };
        if separator == 0 {
            return Err(ReadError::Invalid("HTTP 请求头格式无效。".into()));
        }
        let name = line[..separator].trim().to_string();
        let value = line[separator + 1..].trim().to_string();
        if let Some(existing) = headers
            .iter_mut()
            .find(|(k, _)| k.eq_ignore_ascii_case(&name))
        {
            existing.1 = format!("{}, {value}", existing.1);
        } else {
            headers.push((name, value));
        }
    }
    if let Some(te) = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("Transfer-Encoding"))
        .map(|(_, v)| v.as_str())
    {
        if !te.eq_ignore_ascii_case("identity") {
            return Err(ReadError::Invalid("不支持分块请求正文。".into()));
        }
    }
    let need = match headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("Content-Length"))
        .map(|(_, v)| v.as_str())
    {
        Some(raw) => raw
            .parse::<usize>()
            .map_err(|_| ReadError::Invalid("Content-Length 无效。".into()))?,
        None => 0,
    };
    let query_at = target.find('?').unwrap_or(target.len());
    let path = unescape_path(&target[..query_at]);
    let query = target.get(query_at + 1..).unwrap_or("").to_string();
    if path.starts_with(cliphost_core::device_sync::PREFIX) {
        let cap=if method=="PUT"&&path.ends_with("/media"){cliphost_core::clipboard_sync::MAX_IMAGE_BYTES}
            else if method=="POST"&&path.ends_with("/events"){cliphost_core::clipboard_sync::MAX_TEXT_BYTES*6+16*1024}
            else if method=="POST"&&path==format!("{}/acks",cliphost_core::device_sync::PREFIX){4096}else{0};
        if need>cap{return Err(ReadError::Rejected(413,"Device sync content exceeds the size limit.".into()));}
        let get_header=|key:&str|headers.iter().find(|(name,_)|name.eq_ignore_ascii_case(key)).map(|(_,value)|value.clone());
        let device=get_header("X-ClipLink-Device");let authorization=get_header("Authorization");
        if let Some(host)=host {
            let host=host.clone();
            tokio::task::spawn_blocking(move||host.device_sync_authenticate(device.as_deref(),authorization.as_deref()))
                .await.map_err(|_|ReadError::Rejected(500,"Device sync authentication failed.".into()))?
                .map_err(|e|ReadError::Rejected(401,e))?;
        }
    }
    #[cfg(feature = "private-sync")]
    if crate::mobile_sync::is_sync_path(&path) {
        let cap = crate::mobile_sync::body_limit(&method, &path);
        if need > cap { return Err(ReadError::Rejected(413, "同步内容超过大小限制。".into())); }
        if path != crate::mobile_sync::PAIR_PATH {
            let get_header = |key: &str| headers.iter().find(|(name, _)| name.eq_ignore_ascii_case(key)).map(|(_, value)| value.clone());
            let device = get_header("X-ClipLink-Device");
            let authorization = get_header("Authorization");
            if let Some(host) = host {
                let host = host.clone();
                tokio::task::spawn_blocking(move || host.sync_authenticate(device.as_deref(), authorization.as_deref()))
                    .await.map_err(|_| ReadError::Rejected(500, "认证失败。".into()))?
                    .map_err(|e| ReadError::Rejected(401, e))?;
            }
        }
    }
    let device_sync_observation=if path.starts_with(cliphost_core::device_sync::PREFIX){host.map(|host|host.device_sync_observation())}else{None};
    let connection = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("Connection"))
        .map(|(_, v)| v.as_str())
        .unwrap_or("");
    let keep_alive = version.eq_ignore_ascii_case("HTTP/1.1")
        && !connection
            .split(',')
            .any(|token| token.trim().eq_ignore_ascii_case("close"));
    buf.data.drain(..header_end + 4);
    while buf.data.len() < need {
        let mut scratch = [0u8; 8 * 1024];
        let n = stream.read(&mut scratch).await.map_err(|_| ReadError::Io)?;
        if n == 0 {
            break;
        }
        buf.data.extend_from_slice(&scratch[..n]);
    }
    let body_fully_consumed = buf.data.len() >= need;
    let body = if body_fully_consumed {
        // Move the payload allocation into the request. Keeping an emptied
        // 32 MiB Vec on every persistent upload connection wastes idle memory.
        let remainder = buf.data.split_off(need);
        std::mem::replace(&mut buf.data, remainder)
    } else {
        std::mem::take(&mut buf.data)
    };
    Ok(HttpReq {
        device_sync_observation,
        method,
        path,
        query,
        headers,
        body,
        keep_alive,
        body_fully_consumed,
        content_length: need as i64,
    })
}

#[cfg(feature = "private-sync")]
async fn write_sync_response<S: AsyncWrite + Unpin>(stream: &mut S, response: crate::mobile_sync::SyncResponse, keep: bool) -> std::io::Result<()> {
    match response {
        crate::mobile_sync::SyncResponse::Json(status, body) => write_http(stream, status, JSON_CONTENT_TYPE, &body.to_string(), keep).await,
        crate::mobile_sync::SyncResponse::File(path, mime) => {
            let mut file = tokio::fs::File::open(path).await?;
            let size = file.metadata().await?.len();
            let connection = if keep { "keep-alive" } else { "close" };
            let head = format!("HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {size}\r\nCache-Control: no-store\r\nConnection: {connection}\r\n\r\n");
            stream.write_all(head.as_bytes()).await?;
            tokio::io::copy(&mut file, stream).await?;
            stream.flush().await
        }
    }
}

fn header_end(data: &[u8]) -> Option<usize> {
    data.windows(4).position(|w| w == b"\r\n\r\n")
}

/// C# `Uri.UnescapeDataString` on the path (query already stripped).
fn unescape_path(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (from_hex(bytes[i + 1]), from_hex(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn from_hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

async fn write_http<S: AsyncWrite + Unpin>(
    stream: &mut S,
    status: u16,
    content_type: &str,
    body: &str,
    keep_alive: bool,
) -> std::io::Result<()> {
    let reason = cliphost_core::lan::reason_phrase(status);
    let connection = if keep_alive { "keep-alive" } else { "close" };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: {connection}\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    if !body.is_empty() {
        stream.write_all(body.as_bytes()).await?;
    }
    stream.flush().await?;
    Ok(())
}

fn parse_json_body(body: &[u8], max_bytes: usize) -> Result<serde_json::Value, String> {
    if body.is_empty() {
        return Err("请求正文为空。".into());
    }
    if body.len() > max_bytes {
        return Err("请求正文过大。".into());
    }
    serde_json::from_slice(body).map_err(|e| e.to_string())
}

fn dispatch(
    host: &cliphost_core::HostState,
    req: &HttpReq,
    remote_ip: Option<&str>,
) -> (u16, &'static str, String) {
    if req.path.starts_with(cliphost_core::device_sync::PREFIX) {
        let device=header(req,"X-ClipLink-Device");let authorization=header(req,"Authorization");
        let id=match host.device_sync_authenticate(device,authorization){Ok(id)=>id,Err(error)=>return json_err(401,&error)};
        if !req.body_fully_consumed{return json_err(400,"Incomplete device sync request.");}
        let suffix=req.path.strip_prefix(cliphost_core::device_sync::PREFIX).unwrap_or("");
        let result=match (req.method.as_str(),suffix) {
            ("GET","/info")=>Ok(serde_json::json!({"version":1,"capabilities":["text","image"]})),
            ("POST","/events")=>{
                let value=match parse_json_body(&req.body,cliphost_core::clipboard_sync::MAX_TEXT_BYTES*6+16*1024){Ok(value)=>value,Err(error)=>return json_err(400,&error)};
                let replay=value.get("replay").and_then(serde_json::Value::as_bool).unwrap_or(false);
                let event=match serde_json::from_value(value){Ok(event)=>event,Err(error)=>return json_err(400,&error.to_string())};
                host.device_sync_receive_begin_observed(&id,authorization.unwrap(),event,replay,req.device_sync_observation.unwrap_or_else(||host.device_sync_observation()))
            },
            ("PUT",path) if path.starts_with("/events/")&&path.ends_with("/media")=>{
                if req.body.len()>cliphost_core::clipboard_sync::MAX_IMAGE_BYTES{return json_err(413,"Device sync image is too large.");}
                let event_id=path.trim_start_matches("/events/").trim_end_matches("/media");
                if let Err(error)=cliphost_core::clipboard_sync::validate_identifier(event_id){return json_err(400,&error);}
                host.device_sync_receive_media(&id,authorization.unwrap(),event_id,Some(&req.body))
            },
            _=>return json_err(404,"Unsupported device sync endpoint."),
        };
        return match result{Ok(value)=>json_ok(value),Err(error)=>json_err(409,&error)};
    }
    if req.method == "GET" && req.path == "/cliplink/v1/info" {
        return json_ok(host.lan_info());
    }
    if req.method == "POST" && req.path == "/cliplink/v1/pair" {
        let body = match parse_json_body(&req.body, MAX_PAIR_JSON_BYTES) {
            Ok(v) => v,
            Err(err) => return json_err(400, &err),
        };
        return match host.lan_accept_inbound_pair_from(&body, remote_ip) {
            Ok(v) => json_ok(v),
            Err(err) => json_err(cliphost_core::lan::pair_http_status(&err), &err),
        };
    }
    let device = match host.lan_authenticate(header(req, cliphost_core::lan::DEVICE_HEADER), header(req, "Authorization"))
    {
        Ok(id) => id,
        Err(err) => return json_err(401, &err),
    };
    if req.method == "POST" && req.path == "/cliplink/v1/messages/prepare" {
        let body = match parse_json_body(&req.body, MAX_PREPARE_JSON_BYTES) {
            Ok(v) => v,
            Err(err) => return json_err(400, &err),
        };
        return match host.lan_prepare_inbound(&device, &body) {
            Ok(v) => json_ok(v),
            Err(err) => json_err(cliphost_core::lan::inbound_http_status(&err), cliphost_core::lan::inbound_http_detail(&err)),
        };
    }
    if req.method == "POST" && req.path == "/cliplink/v1/messages/text" {
        let body = match parse_json_body(&req.body, MAX_PREPARE_JSON_BYTES) {
            Ok(v) => v,
            Err(err) => return json_err(400, &err),
        };
        return match host.lan_receive_text(&device, &body) {
            Ok(v) => json_ok(v),
            Err(err) => json_err(cliphost_core::lan::inbound_http_status(&err), cliphost_core::lan::inbound_http_detail(&err)),
        };
    }
    match match_message_path(&req.path) {
        Err(err) => return json_err(400, &err),
        Ok(Some((message_id, operation, attachment_id))) => {
        if req.method == "PUT" && operation == "attachments" {
            if let Some(attachment_id) = attachment_id {
                return match host.lan_upload_inbound(
                    &device,
                    &message_id,
                    &attachment_id,
                    &req.body,
                    req.content_length,
                ) {
                    Ok(()) => empty(204),
                    Err(err) => json_err(
                        cliphost_core::lan::inbound_http_status(&err),
                        cliphost_core::lan::inbound_http_detail(&err),
                    ),
                };
            }
        }
        if req.method == "POST" && operation == "commit" {
            return match host.lan_commit_inbound(&device, &message_id) {
                Ok(v) => json_ok(v),
                Err(err) => json_err(
                    cliphost_core::lan::inbound_http_status(&err),
                    cliphost_core::lan::inbound_http_detail(&err),
                ),
            };
        }
        if req.method == "POST" && operation == "cancel" {
            host.lan_cancel_inbound(&device, &message_id);
            return empty(204);
        }
        }
        Ok(None) => {}
    }
    json_err(404, "接口不存在。")
}

fn match_message_path(path: &str) -> Result<Option<(String, String, Option<String>)>, String> {
    let prefix = "/cliplink/v1/messages/";
    let Some(rest) = path.strip_prefix(prefix) else {
        return Ok(None);
    };
    let segments: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() == 2 && matches!(segments[1], "commit" | "cancel") {
        let message_id = cliphost_core::lan::safe_identifier(segments[0])?;
        return Ok(Some((message_id.to_string(), segments[1].to_string(), None)));
    }
    if segments.len() == 3 && segments[1] == "attachments" {
        let message_id = cliphost_core::lan::safe_identifier(segments[0])?;
        let attachment_id = cliphost_core::lan::safe_identifier(segments[2])?;
        return Ok(Some((
            message_id.to_string(),
            "attachments".into(),
            Some(attachment_id.to_string()),
        )));
    }
    Ok(None)
}

fn json_ok(value: serde_json::Value) -> (u16, &'static str, String) {
    (200, JSON_CONTENT_TYPE, value.to_string())
}

fn json_err(status: u16, message: &str) -> (u16, &'static str, String) {
    (
        status,
        JSON_CONTENT_TYPE,
        cliphost_core::lan::problem_json(status, message).to_string(),
    )
}

fn empty(status: u16) -> (u16, &'static str, String) {
    (status, EMPTY_CONTENT_TYPE, String::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cliphost_core::paths::AppPaths;
    use cliphost_core::HostState;
    use tempfile::tempdir;
    use tokio::io::duplex;

    fn test_host() -> HostState {
        cliphost_core::force_headless(true);
        std::env::set_var("CLIPHOST_TAURI_HEADLESS", "1");
        let dir = tempdir().unwrap();
        HostState::open(AppPaths::from_data_root(dir.path().join("BetterClipTauri")), 28965).unwrap()
    }

    async fn read_response(stream: &mut (impl AsyncRead + Unpin)) -> (u16, String, String) {
        let mut data = Vec::new();
        let mut scratch = [0u8; 1024];
        loop {
            let n = stream.read(&mut scratch).await.unwrap();
            assert!(n > 0, "server closed before HTTP headers");
            data.extend_from_slice(&scratch[..n]);
            if header_end(&data).is_some() {
                break;
            }
        }
        let end = header_end(&data).unwrap();
        let head = String::from_utf8_lossy(&data[..end]).into_owned();
        let status = head
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse::<u16>()
            .unwrap();
        let connection = head
            .lines()
            .find(|l| l.to_ascii_lowercase().starts_with("connection:"))
            .map(|l| l.split_once(':').unwrap().1.trim().to_ascii_lowercase())
            .unwrap_or_default();
        let len = head
            .lines()
            .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
            .and_then(|l| l.split_once(':').unwrap().1.trim().parse::<usize>().ok())
            .unwrap_or(0);
        data.drain(..end + 4);
        while data.len() < len {
            let n = stream.read(&mut scratch).await.unwrap();
            data.extend_from_slice(&scratch[..n]);
        }
        let body = String::from_utf8_lossy(&data[..len]).into_owned();
        (status, connection, body)
    }

    #[tokio::test]
    async fn lan_https_keep_alive_matches_csharp() {
        let host = test_host();
        let (mut client, mut server) = duplex(64 * 1024);
        let serve_host = host.clone();
        tokio::spawn(async move {
            serve_http_connection(&mut server, serve_host, None).await;
        });
        client
            .write_all(b"GET /cliplink/v1/info HTTP/1.1\r\nHost: peer\r\n\r\n")
            .await
            .unwrap();
        let (s1, c1, b1) = read_response(&mut client).await;
        client
            .write_all(b"GET /cliplink/v1/info HTTP/1.1\r\nHost: peer\r\n\r\n")
            .await
            .unwrap();
        let (s2, c2, b2) = read_response(&mut client).await;
        assert_eq!(s1, 200);
        assert_eq!(c1, "keep-alive");
        assert!(b1.contains("cea-clip-link") || b1.contains("deviceId") || b1.contains("port"));
        assert_eq!(s2, 200);
        assert_eq!(c2, "keep-alive");
        assert!(b2.contains("deviceId") || b2.contains("port"));
    }

    #[tokio::test]
    async fn http11_connection_close_and_http10_do_not_keep_alive() {
        let host = test_host();
        let (mut client, mut server) = duplex(16 * 1024);
        let serve_host = host.clone();
        tokio::spawn(async move {
            serve_http_connection(&mut server, serve_host, None).await;
        });
        client
            .write_all(b"GET /cliplink/v1/info HTTP/1.1\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let (status, connection, _) = read_response(&mut client).await;
        assert_eq!(status, 200);
        assert_eq!(connection, "close");

        let (mut client, mut server) = duplex(16 * 1024);
        let serve_host = host;
        tokio::spawn(async move {
            serve_http_connection(&mut server, serve_host, None).await;
        });
        client
            .write_all(b"GET /cliplink/v1/info HTTP/1.0\r\n\r\n")
            .await
            .unwrap();
        let (status, connection, _) = read_response(&mut client).await;
        assert_eq!(status, 200);
        assert_eq!(connection, "close");
    }

    #[tokio::test]
    async fn invalid_request_line_is_400_problem_json() {
        let host = test_host();
        let (mut client, mut server) = duplex(8 * 1024);
        tokio::spawn(async move {
            serve_http_connection(&mut server, host, None).await;
        });
        client.write_all(b"GET /cliplink/v1/info\r\n\r\n").await.unwrap();
        let (status, connection, body) = read_response(&mut client).await;
        assert_eq!(status, 400);
        assert_eq!(connection, "close");
        assert!(body.contains("仅支持 HTTP/1.x 请求。"));
    }

    #[tokio::test]
    #[cfg(feature = "private-sync")]
    async fn mobile_sync_rejects_oversize_and_unauthenticated_body_before_reading() {
        for (request, expected) in [
            ("PUT /cliplink/sync/v1/events/test/media HTTP/1.1\r\nContent-Length: 33554433\r\n\r\n", 413),
            ("POST /cliplink/sync/v1/events HTTP/1.1\r\nContent-Length: 100\r\n\r\n", 401),
        ] {
            let (mut client, mut server) = duplex(8 * 1024);
            let host = test_host();
            tokio::spawn(async move { serve_http_connection(&mut server, host, None).await; });
            // Only headers: a buggy pre-auth body read would hang this test.
            client.write_all(request.as_bytes()).await.unwrap();
            let (status, connection, _) = timeout(Duration::from_secs(1), read_response(&mut client)).await.unwrap();
            assert_eq!(status, expected);
            assert_eq!(connection,"close");
        }
    }

    #[tokio::test]
    async fn device_sync_rejects_oversize_and_unauthenticated_body_before_reading() {
        for (request,expected) in [
            ("PUT /cliplink/device-sync/v1/events/test/media HTTP/1.1\r\nContent-Length: 33554433\r\n\r\n",413),
            ("POST /cliplink/device-sync/v1/events HTTP/1.1\r\nContent-Length: 6307841\r\n\r\n",413),
            ("POST /cliplink/device-sync/v1/events HTTP/1.1\r\nContent-Length: 100\r\n\r\n",401),
            ("PUT /cliplink/device-sync/v1/events/test/media HTTP/1.1\r\nContent-Length: 100\r\n\r\n",401),
        ] {
            let (mut client,mut server)=duplex(8*1024);let host=test_host();
            tokio::spawn(async move{serve_http_connection(&mut server,host,None).await;});
            // No body is supplied. Waiting for one before rejecting would hang.
            client.write_all(request.as_bytes()).await.unwrap();
            let (status,connection,_)=timeout(Duration::from_secs(1),read_response(&mut client)).await.unwrap();
            assert_eq!(status,expected);assert_eq!(connection,"close");
        }
    }

    #[tokio::test]
    #[cfg(feature = "private-sync")]
    async fn mobile_sync_escaped_maximum_text_crosses_http_and_remains_in_poll_journal() {
        use cliphost_core::clipboard_sync::{SyncEvent, MAX_TEXT_BYTES};
        use crate::mobile_sync::{SyncHub, PREFIX};
        cliphost_core::force_headless(true);
        let directory = tempdir().unwrap();
        let host = HostState::open(AppPaths::from_data_root(directory.path().to_path_buf()), 28965).unwrap();
        host.sync_set_enabled(true).unwrap();
        let invitation = host.sync_invitation().unwrap();
        let paired = host.sync_pair_client(&serde_json::json!({
            "deviceId":"escaped-text-phone", "pairCode":invitation["pairCode"]
        })).unwrap();
        let token = paired["token"].as_str().unwrap();
        let text = "\u{0001}".repeat(MAX_TEXT_BYTES);
        let event = SyncEvent::text("escaped-incoming".into(), "escaped-text-phone".into(), 1, text.clone());
        event.validate().unwrap();
        let body = serde_json::to_vec(&event).unwrap();
        assert!(body.len() > MAX_TEXT_BYTES * 6, "the test must exercise JSON's worst-case escaping");

        let hub = SyncHub::new(host.clone());
        let (mut client, mut server) = duplex(64 * 1024);
        let serve_host = host.clone();
        let serve_hub = hub.clone();
        let serving = tokio::spawn(async move {
            serve_http_connection_with_sync(&mut server, serve_host, None, Some(serve_hub)).await;
        });
        let headers = format!("POST {PREFIX}/events HTTP/1.1\r\nHost: desktop\r\nX-ClipLink-Device: escaped-text-phone\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n", body.len());
        client.write_all(headers.as_bytes()).await.unwrap();
        client.write_all(&body).await.unwrap();
        let (status, _, ack) = timeout(Duration::from_secs(10), read_response(&mut client)).await.unwrap();
        assert_eq!(status, 200);
        assert_eq!(serde_json::from_str::<serde_json::Value>(&ack).unwrap()["status"], "applied");
        let history = host.search_items(&serde_json::json!({"webSessionId":"escaped-text-test","projection":"idsMatch","take":1})).unwrap();
        assert_eq!(history["total"], 1);
        let detail = host.detail(&serde_json::json!({"id":history["hits"][0]["id"]})).unwrap();
        assert_eq!(detail["bodyText"].as_str(), Some(text.as_str()));

        // Desktop-to-phone uses the same legal payload. It must survive the
        // bounded outgoing journal, then pass through the real HTTP response.
        hub.publish(SyncEvent::text("escaped-outgoing".into(), host.sync_device_id(), 1, text.clone()));
        let headers = format!("GET {PREFIX}/events?after=0&waitMs=0 HTTP/1.1\r\nHost: desktop\r\nX-ClipLink-Device: escaped-text-phone\r\nAuthorization: Bearer {token}\r\n\r\n");
        client.write_all(headers.as_bytes()).await.unwrap();
        let (status, _, batch) = timeout(Duration::from_secs(10), read_response(&mut client)).await.unwrap();
        assert_eq!(status, 200);
        assert!(batch.len() < 8 * 1024 * 1024, "leave room inside Android's control response bound");
        let batch: serde_json::Value = serde_json::from_str(&batch).unwrap();
        assert_eq!(batch["events"].as_array().unwrap().len(), 1, "a legal large event must not evict itself");
        assert_eq!(batch["events"][0]["text"].as_str(), Some(text.as_str()));
        serving.abort();
    }

    #[test]
    fn message_path_rejects_unsafe_ids_like_csharp() {
        assert_eq!(
            match_message_path("/cliplink/v1/messages/bad.id/commit").unwrap_err(),
            "标识符格式无效。"
        );
        let ok = match_message_path("/cliplink/v1/messages/msg_1/attachments/att_2").unwrap();
        assert_eq!(ok.unwrap().0, "msg_1");
        assert!(match_message_path("/cliplink/v1/info").unwrap().is_none());
    }
}
