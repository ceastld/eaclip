//! Android's outbound HTTPS connection carries events immediately without a
//! per-poll delay. The clipboard state machine stays transport independent.
use crate::lan_tls::{header, HttpReq};
use cliphost_core::{
    clipboard_sync::{SyncAckStatus, SyncEvent, MAX_IMAGE_BYTES, MAX_TEXT_BYTES},
    HostState,
};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::Notify;

pub(super) const PREFIX: &str = "/cliplink/sync/v1";
pub(super) const PAIR_PATH: &str = "/cliplink/sync/v1/pair";
const MAX_JOURNAL_EVENTS: usize = 64;
// A legal 1 MiB text of control characters expands to 6 MiB in JSON. Retain
// at least that event while keeping batches below Android's 8 MiB response cap.
const MAX_JOURNAL_BYTES: usize = 7 * 1024 * 1024;

pub(super) fn is_sync_path(path: &str) -> bool {
    path == PREFIX || path.starts_with(&format!("{PREFIX}/"))
}
pub(super) fn body_limit(method: &str, path: &str) -> usize {
    if method == "GET" {
        0
    } else if path.ends_with("/media") {
        MAX_IMAGE_BYTES
    } else if path == format!("{PREFIX}/events") {
        // This bounds encoded transport bytes; core separately limits the
        // decoded UTF-8 text. U+0001 becomes the six-byte escape "\\u0001".
        MAX_TEXT_BYTES * 6 + 16 * 1024
    } else {
        64 * 1024
    }
}

pub(super) enum SyncResponse {
    Json(u16, Value),
    File(PathBuf, &'static str),
}
impl SyncResponse {
    pub(super) fn error(status: u16, message: &str) -> Self {
        Self::Json(status, json!({"error":message}))
    }
    fn ok(body: Value) -> Self {
        Self::Json(200, body)
    }
}

struct JournalEvent {
    cursor: u64,
    event: Value,
    bytes: usize,
}
#[derive(Clone)]
struct Upload {
    owner: String,
    replay: bool,
    expires: Instant,
}
#[derive(Default)]
struct Journal {
    cursor: u64,
    bytes: usize,
    events: VecDeque<JournalEvent>,
    uploads: HashMap<String, Upload>,
}

pub(super) struct SyncHub {
    host: HostState,
    journal: Mutex<Journal>,
    changed: Notify,
    pump: Mutex<Option<tokio::task::JoinHandle<()>>>,
}
impl Drop for SyncHub {
    fn drop(&mut self) {
        if let Some(pump) = self.pump.lock().unwrap().take() {
            pump.abort();
        }
    }
}

impl SyncHub {
    pub(super) fn new(host: HostState) -> Arc<Self> {
        let mut events = host.sync_subscribe();
        let hub = Arc::new(Self {
            host,
            journal: Mutex::new(Journal::default()),
            changed: Notify::new(),
            pump: Mutex::new(None),
        });
        let weak = Arc::downgrade(&hub);
        let pump = tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(event) => {
                        let Some(hub) = weak.upgrade() else { break };
                        hub.publish(event);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                        // Consumers see a gap rather than accidentally applying an old item.
                        if let Some(hub) = weak.upgrade() {
                            hub.journal.lock().unwrap().cursor += count;
                            hub.changed.notify_waiters();
                        } else {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        *hub.pump.lock().unwrap() = Some(pump);
        hub
    }

    pub(super) fn publish(&self, event: SyncEvent) {
        let event = serde_json::to_value(event).expect("sync event serializes");
        let bytes = event.to_string().len();
        let mut journal = self.journal.lock().unwrap();
        journal.cursor += 1;
        let cursor = journal.cursor;
        journal.bytes += bytes;
        journal.events.push_back(JournalEvent {
            cursor,
            event,
            bytes,
        });
        while journal.events.len() > MAX_JOURNAL_EVENTS || journal.bytes > MAX_JOURNAL_BYTES {
            if let Some(old) = journal.events.pop_front() {
                journal.bytes -= old.bytes;
            }
        }
        drop(journal);
        self.changed.notify_waiters();
    }

    async fn work<T: Send + 'static>(
        &self,
        work: impl FnOnce(HostState) -> Result<T, String> + Send + 'static,
    ) -> Result<T, String> {
        let host = self.host.clone();
        tokio::task::spawn_blocking(move || work(host))
            .await
            .map_err(|e| e.to_string())?
    }

    pub(super) async fn handle(&self, req: HttpReq) -> SyncResponse {
        match self.dispatch(req).await {
            Ok(response) => response,
            Err((code, error)) => SyncResponse::error(code, &error),
        }
    }

    async fn dispatch(&self, req: HttpReq) -> Result<SyncResponse, (u16, String)> {
        if req.path == PAIR_PATH && req.method == "POST" {
            let mut params: Value =
                serde_json::from_slice(&req.body).map_err(|e| (400, e.to_string()))?;
            if params.get("version").and_then(Value::as_u64) != Some(1) {
                return Err((400, "Unsupported sync version.".into()));
            }
            if params.get("name").is_none() {
                params["name"] = params["deviceName"].clone();
            }
            let mut result = self
                .work(move |host| host.sync_pair_client(&params))
                .await
                .map_err(|e| (403, e))?;
            result["cursor"] = json!(self.journal.lock().unwrap().cursor);
            return Ok(SyncResponse::ok(result));
        }
        let device = header(&req, "X-ClipLink-Device").map(str::to_owned);
        let authorization = header(&req, "Authorization").map(str::to_owned);
        let session_device = device.clone();
        let session_authorization = authorization.clone();
        let authenticated = self
            .work(move |host| {
                let id = host.sync_authenticate(device.as_deref(), authorization.as_deref())?;
                host.sync_mark_client_seen(&id);
                Ok(id)
            })
            .await
            .map_err(|e| (401, e))?;
        let suffix = req.path.strip_prefix(PREFIX).unwrap_or("");
        match (req.method.as_str(), suffix) {
            ("GET", "/events") => {
                let query = |name: &str| {
                    req.query.split('&').find_map(|part| {
                        part.split_once('=')
                            .filter(|(key, _)| *key == name)
                            .map(|(_, value)| value)
                    })
                };
                let after = query("after")
                    .unwrap_or("0")
                    .parse::<u64>()
                    .map_err(|_| (400, "Invalid cursor.".into()))?;
                let wait = query("waitMs")
                    .unwrap_or("25000")
                    .parse::<u64>()
                    .map_err(|_| (400, "Invalid waitMs.".into()))?
                    .min(25_000);
                let resuming = query("resuming") == Some("1");
                let mut response = self
                    .poll(after, Duration::from_millis(wait), resuming)
                    .await;
                let can_send = self
                    .work(move |host| {
                        host.sync_authenticate(
                            session_device.as_deref(),
                            session_authorization.as_deref(),
                        )?;
                        Ok(host.sync_status()["sendEnabled"] == true)
                    })
                    .await
                    .map_err(|e| (401, e))?;
                if !can_send {
                    response["events"] = json!([]);
                }
                Ok(SyncResponse::ok(response))
            }
            ("POST", "/events") => {
                let value: Value =
                    serde_json::from_slice(&req.body).map_err(|e| (400, e.to_string()))?;
                let replay = value
                    .get("replay")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let event: SyncEvent =
                    serde_json::from_value(value).map_err(|e| (400, e.to_string()))?;
                if event.origin_device_id != authenticated {
                    return Err((403, "Event origin does not match paired device.".into()));
                }
                let event_id = event.event_id.clone();
                let plan = self
                    .work(move |host| {
                        host.sync_receive_begin_authenticated(
                            event,
                            session_device.as_deref(),
                            session_authorization.as_deref(),
                        )
                    })
                    .await
                    .map_err(|e| (409, e))?;
                if let Some(ack) = plan.duplicate_ack {
                    return Ok(SyncResponse::ok(serde_json::to_value(ack).unwrap()));
                }
                if plan.needs_media {
                    let mut journal = self.journal.lock().unwrap();
                    journal
                        .uploads
                        .retain(|_, upload| upload.expires > Instant::now());
                    if journal.uploads.len() >= 32 && !journal.uploads.contains_key(&event_id) {
                        return Err((429, "Too many pending image uploads.".into()));
                    }
                    let replay = replay
                        || journal
                            .uploads
                            .get(&event_id)
                            .is_some_and(|upload| upload.replay);
                    journal.uploads.insert(
                        event_id.clone(),
                        Upload {
                            owner: authenticated,
                            replay,
                            expires: Instant::now() + Duration::from_secs(120),
                        },
                    );
                    return Ok(SyncResponse::ok(
                        json!({"eventId":event_id,"status":"pendingMedia"}),
                    ));
                }
                let ack = self
                    .work(move |host| {
                        let ack = host.sync_receive_accept(&event_id, None)?;
                        if ack.status != SyncAckStatus::Accepted {
                            Ok(ack)
                        } else if replay {
                            host.sync_receive_supersede(&event_id)
                        } else {
                            host.sync_receive_apply(&event_id)
                        }
                    })
                    .await
                    .map_err(|e| (422, e))?;
                Ok(SyncResponse::ok(serde_json::to_value(ack).unwrap()))
            }
            ("PUT", path) if path.starts_with("/events/") && path.ends_with("/media") => {
                let event_id = path
                    .trim_start_matches("/events/")
                    .trim_end_matches("/media")
                    .to_string();
                cliphost_core::clipboard_sync::validate_identifier(&event_id)
                    .map_err(|e| (400, e))?;
                let upload = self
                    .journal
                    .lock()
                    .unwrap()
                    .uploads
                    .get(&event_id)
                    .cloned()
                    .ok_or((404, "Unknown image upload.".into()))?;
                if upload.owner != authenticated {
                    return Err((403, "Image upload belongs to another device.".into()));
                }
                if upload.expires <= Instant::now() {
                    return Err((410, "Image upload expired.".into()));
                }
                let operation_id = event_id.clone();
                let ack = self
                    .work(move |host| {
                        let ack = host.sync_receive_accept(&operation_id, Some(&req.body))?;
                        if ack.status != SyncAckStatus::Accepted {
                            Ok(ack)
                        } else if upload.replay {
                            host.sync_receive_supersede(&operation_id)
                        } else {
                            host.sync_receive_apply(&operation_id)
                        }
                    })
                    .await
                    .map_err(|e| (422, e))?;
                // Retain this small ticket until expiry so a lost HTTP response
                // can retry the same media without reapplying the clipboard.
                Ok(SyncResponse::ok(serde_json::to_value(ack).unwrap()))
            }
            ("GET", path) if path.starts_with("/blobs/") => {
                let hash = path.trim_start_matches("/blobs/").to_string();
                let path = self
                    .work(move |host| host.sync_read_media(&hash))
                    .await
                    .map_err(|e| (404, e))?;
                let mime = if path.extension().and_then(|v| v.to_str()) == Some("jpg") {
                    "image/jpeg"
                } else {
                    "image/png"
                };
                Ok(SyncResponse::File(path, mime))
            }
            ("POST", "/acks") => {
                let ack: Value =
                    serde_json::from_slice(&req.body).map_err(|e| (400, e.to_string()))?;
                let id = ack
                    .get("eventId")
                    .and_then(Value::as_str)
                    .ok_or((400, "eventId missing.".into()))?;
                cliphost_core::clipboard_sync::validate_identifier(id).map_err(|e| (400, e))?;
                if !matches!(
                    ack.get("status").and_then(Value::as_str),
                    Some("accepted" | "applied" | "superseded" | "failed")
                ) {
                    return Err((400, "Invalid acknowledgement status.".into()));
                }
                // Receiving this endpoint is itself the receipt; it never
                // modifies paste history or injects a paste into another app.
                Ok(SyncResponse::ok(json!({"ok":true})))
            }
            _ => Err((404, "Unknown clipboard sync endpoint.".into())),
        }
    }

    pub(super) async fn poll(&self, after: u64, wait: Duration, resuming: bool) -> Value {
        let boundary = self.journal.lock().unwrap().cursor;
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            // Register before observing the journal to avoid losing an event
            // arriving between the empty check and awaiting the notification.
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let journal = self.journal.lock().unwrap();
                let reset = after > journal.cursor;
                let gap = journal
                    .events
                    .front()
                    .is_some_and(|e| after.saturating_add(1) < e.cursor);
                let events: Vec<Value> = journal
                    .events
                    .iter()
                    .filter(|e| e.cursor > after)
                    .map(|e| {
                        let mut value = e.event.clone();
                        value["replay"] = json!(gap || (resuming && e.cursor <= boundary));
                        value
                    })
                    .collect();
                if reset || !events.is_empty() || tokio::time::Instant::now() >= deadline {
                    return json!({"cursor":journal.cursor,"events":events,"reset":reset,"gap":gap});
                }
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                continue;
            }
        }
    }
}
