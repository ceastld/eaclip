//! Android uses the paired LAN identity and shared device-sync policy while
//! keeping only outbound connections. Long polls never occupy blocking workers.
use crate::lan_tls::{header, HttpReq};
use crate::mobile_sync::SyncResponse;
use cliphost_core::{device_sync::PREFIX, HostState};
use serde_json::Value;
use std::time::Duration;

pub(super) fn handles(method: &str, path: &str) -> bool {
    match path.strip_prefix(PREFIX) {
        Some("/events") => method == "GET",
        Some("/acks") => method == "POST",
        Some(suffix) => method == "GET" && suffix.starts_with("/blobs/"),
        None => false,
    }
}

async fn work<T: Send + 'static>(
    host: &HostState,
    operation: impl FnOnce(HostState) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let host = host.clone();
    tokio::task::spawn_blocking(move || operation(host))
        .await
        .map_err(|error| error.to_string())?
}

pub(super) async fn handle(host: &HostState, req: HttpReq) -> SyncResponse {
    match dispatch(host, req).await {
        Ok(response) => response,
        Err((status, message)) => SyncResponse::error(status, &message),
    }
}

async fn dispatch(host: &HostState, req: HttpReq) -> Result<SyncResponse, (u16, String)> {
    if !req.body_fully_consumed {
        return Err((400, "Incomplete device sync request.".into()));
    }
    let device = header(&req, "X-ClipLink-Device").map(str::to_owned);
    let authorization = header(&req, "Authorization").map(str::to_owned);
    let auth = authorization.clone();
    let id = work(host, move |host| {
        host.device_sync_authenticate(device.as_deref(), auth.as_deref())
    }).await.map_err(|error| (401, error))?;
    let authorization = authorization.ok_or((401, "Authorization is required.".into()))?;
    let suffix = req.path.strip_prefix(PREFIX).unwrap_or("");
    match (req.method.as_str(), suffix) {
        ("GET", "/events") => {
            let query = |name: &str| req.query.split('&').find_map(|part| {
                part.split_once('=').filter(|(key, _)| *key == name).map(|(_, value)| value)
            });
            let after = query("after").unwrap_or("0").parse::<u64>()
                .map_err(|_| (400, "Invalid cursor.".into()))?;
            let wait = query("waitMs").unwrap_or("25000").parse::<u64>()
                .map_err(|_| (400, "Invalid waitMs.".into()))?.min(25_000);
            let mut resuming = match query("resuming") {
                None | Some("0") => false,
                Some("1") => true,
                _ => return Err((400, "Invalid resuming flag.".into())),
            };
            // Subscribe before the first snapshot so a publication between the
            // snapshot and await cannot leave a copied item waiting 25 seconds.
            let mut changed = host.device_sync_poll_subscribe();
            let deadline = tokio::time::Instant::now() + Duration::from_millis(wait);
            loop {
                let device = id.clone();
                let auth = authorization.clone();
                let response = work(host, move |host| host.device_sync_poll(&device, &auth, after, resuming))
                    .await.map_err(|error| (401, error))?;
                if response["reset"] == true
                    || response["events"].as_array().is_some_and(|events| !events.is_empty())
                    || tokio::time::Instant::now() >= deadline {
                    return Ok(SyncResponse::Json(200, response));
                }
                // The initial empty snapshot is the reconnect boundary. A
                // later copy which wakes this same poll is live, not backlog.
                resuming = false;
                match tokio::time::timeout_at(deadline, changed.recv()).await {
                    Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) =>
                        return Err((503, "Device sync stopped.".into())),
                    // Timeout and lag both return through a fresh authenticated
                    // snapshot, including revocation during an empty long poll.
                    _ => {}
                }
            }
        }
        ("GET", path) if path.starts_with("/blobs/") => {
            let hash = path.trim_start_matches("/blobs/").to_owned();
            if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err((400, "Invalid content hash.".into()));
            }
            let path = work(host, move |host| host.device_sync_pull_blob(&id, &authorization, &hash))
                .await.map_err(|error| (403, error))?;
            Ok(SyncResponse::File(path, "application/octet-stream"))
        }
        ("POST", "/acks") => {
            let ack: Value = serde_json::from_slice(&req.body).map_err(|error| (400, error.to_string()))?;
            let response = work(host, move |host| host.device_sync_pull_ack(&id, &authorization, &ack))
                .await.map_err(|error| (409, error))?;
            Ok(SyncResponse::Json(200, response))
        }
        _ => Err((404, "Unsupported device sync endpoint.".into())),
    }
}
