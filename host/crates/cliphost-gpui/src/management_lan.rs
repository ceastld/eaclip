//! Native counterpart of the shipped LanSharePage. This page never owns a LAN
//! listener, peer store or transport: all operations use the existing core.

use gpui_rsx::rsx;
use crate::smooth_wheel::SmoothWheelExt;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Cursor,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Mutex, OnceLock, Weak,
    },
    time::{Duration, Instant, SystemTime},
};

use cliphost_core::{i18n::tr, HostState};
use gpui::{
    div, img, list, prelude::*, px, AnimationExt, AnyElement, Context, Entity, EventEmitter,
    FontWeight, Image, ImageFormat, KeyDownEvent, ListAlignment, ListOffset, ListState,
    MouseButton, SharedString, Subscription, Task, Timer, Window,
};
use serde_json::{json, Value};

use crate::{
    input::{InputColors, SearchEvent, SearchInput},
    motion,
    multiline::{MultilineEvent, MultilineInput, PasteTarget},
    preview::{DesignTokens, ThemeTokens},
    view::color,
};

#[path = "management_lan_aligned.rs"]
mod aligned;
#[path = "management_lan_drop.rs"]
mod incoming;

#[derive(Clone, Default)]
struct ConversationDraft {
    text: String,
    files: Vec<String>,
    recent_items: Vec<RecentComposeItem>,
    revision: u64,
    failed_send: Option<u64>,
}

#[derive(Clone)]
struct RecentComposeItem {
    id: String,
    label: String,
    kind: String,
    text: String,
    files: Vec<String>,
}

fn compose_text(text: &str, items: &[RecentComposeItem]) -> String {
    std::iter::once(text)
        .chain(items.iter().map(|item| item.text.as_str()))
        .filter(|text| !text.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(debug_assertions)]
pub struct LanFixtureData {
    pub status: Value,
    pub device_sync: Value,
    pub peers: Vec<Value>,
    pub conversations: Vec<Value>,
    pub messages: Vec<Value>,
    pub selected: String,
    pub devices: bool,
}

fn text(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap_or_default().into()
}
fn conversation_peer_id(conversation: &str) -> &str {
    conversation.strip_prefix("dm:").unwrap_or(conversation)
}
fn conversation_peer<'a>(peers: &'a [Value], conversation: &str) -> Option<&'a Value> {
    peers
        .iter()
        .find(|peer| peer["deviceId"] == conversation_peer_id(conversation))
}
fn conversation_has_target(peers: &[Value], conversation: &str) -> bool {
    if conversation == "all" {
        peers.iter().any(manual_transfer_peer)
    } else {
        conversation_peer(peers, conversation).is_some_and(manual_transfer_peer)
    }
}
fn manual_transfer_peer(peer: &Value) -> bool {
    // Older desktop peers did not advertise capabilities and remain supported.
    peer["paired"] == true && peer["supportsManualTransfer"] != false
}
fn values(value: &Value, key: &str) -> Vec<Value> {
    value[key].as_array().cloned().unwrap_or_default()
}
fn rows(value: Value) -> Vec<Value> {
    match value {
        Value::Array(rows) => rows,
        _ => Vec::new(),
    }
}

// Discovery timestamps are transport bookkeeping, not visible page state.
fn same_peer_presentation(previous: &[Value], next: &[Value]) -> bool {
    previous.len() == next.len() && previous.iter().zip(next).all(|(a, b)| {
        ["deviceId", "alias", "paired", "online", "deviceKind", "detectedDeviceKind",
         "deviceKindOverride", "pairingAvailable", "identityChanged", "supportsManualTransfer", "clipboardTransport"]
            .iter().all(|key| a[*key] == b[*key])
    })
}

fn same_object_except(previous: &Value, next: &Value, ignored: &str) -> bool {
    match (previous.as_object(), next.as_object()) {
        (Some(previous), Some(next)) => {
            previous.iter().filter(|(key, _)| key.as_str() != ignored)
                .eq(next.iter().filter(|(key, _)| key.as_str() != ignored))
        }
        _ => previous == next,
    }
}

fn same_status_presentation(previous: &Value, next: &Value) -> bool {
    same_object_except(previous, next, "pairingCode")
        && same_object_except(&previous["pairingCode"], &next["pairingCode"], "expiresAtUtc")
}

fn pairing_seconds(status: &Value) -> Option<i64> {
    status["pairingCode"]["expiresAtUtc"].as_str()
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map(|expiry| (expiry.signed_duration_since(chrono::Utc::now()).num_milliseconds().max(0) + 999) / 1000)
}

fn pairing_code_valid(status: &Value, now: chrono::DateTime<chrono::Utc>) -> bool {
    let code = &status["pairingCode"];
    code["code"].as_str().is_some_and(|value| value.len() == 6 && value.bytes().all(|byte| byte.is_ascii_digit()))
        && code["attemptsRemaining"].as_i64().is_none_or(|attempts| attempts > 0)
        && code["expiresAtUtc"].as_str()
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
            .is_some_and(|expiry| expiry > now)
}

/// A visible pairing session requests a fresh code once, then renews by actual
/// expiry. Accepted writes retain their lease when the page is hidden, so a
/// quick hide/reopen cannot enqueue a concurrent renewal.
#[derive(Default)]
struct PairingCodeRefresh {
    fresh_on_open: bool,
    in_flight: bool,
    retry_at: Option<Instant>,
}

impl PairingCodeRefresh {
    fn opened(&mut self) {
        self.fresh_on_open = true;
        self.retry_at = None;
    }

    fn begin(&mut self, eligible: bool, busy: bool, valid: bool, now: Instant) -> bool {
        if !eligible || busy || self.in_flight || self.retry_at.is_some_and(|retry| retry > now)
            || (!self.fresh_on_open && valid)
        {
            return false;
        }
        self.fresh_on_open = false;
        self.in_flight = true;
        true
    }

    fn completed(&mut self, success: bool, now: Instant) {
        self.in_flight = false;
        self.retry_at = (!success).then(|| now + Duration::from_secs(15));
    }
}

fn paired_device_ids(peers: &[Value]) -> BTreeSet<&str> {
    peers.iter().filter(|peer| peer["paired"] == true)
        .filter_map(|peer| peer["deviceId"].as_str()).collect()
}

fn reconcile_conversation_peers(conversations: &mut [Value], peers: &[Value]) -> bool {
    let mut changed = false;
    for conversation in conversations {
        let id = conversation["id"].as_str().unwrap_or_default().to_owned();
        let online = if id == "all" {
            peers.iter().any(|peer| manual_transfer_peer(peer) && peer["online"] == true)
        } else {
            conversation_peer(peers, &id).is_some_and(|peer| peer["paired"] == true && peer["online"] == true)
        };
        if conversation["online"] != online {
            conversation["online"] = json!(online);
            changed = true;
        }
        if id != "all" {
            if let Some(peer) = conversation_peer(peers, &id).filter(|peer| peer["paired"] == true) {
                for (target, source) in [("title", "alias"), ("deviceKind", "deviceKind"),
                    ("detectedDeviceKind", "detectedDeviceKind"), ("deviceKindOverride", "deviceKindOverride")] {
                    if conversation[target] != peer[source] {
                        conversation[target] = peer[source].clone();
                        changed = true;
                    }
                }
            }
        }
    }
    changed
}

struct MessageTextLayout {
    max_width: f32,
    font_family: String,
    scale_factor: f32,
    width: f32,
    height: f32,
}

fn message_text_widths(measured: f32, inset: f32, maximum: f32, scale: f32) -> (f32, f32) {
    // Taffy rounds the final content edges to physical pixels. A fractionally
    // narrower viewport can move a whole CJK/ASCII word onto another line.
    let pixel = 1. / scale;
    let body = (((measured * scale).ceil() + 1.) / scale + inset)
        .clamp(inset + 1., maximum);
    // At the maximum bubble width, reserve the same rounding uncertainty when
    // measuring height so the actual text cannot gain an unbudgeted final row.
    (body, (body - inset - pixel).max(1.))
}

fn retain_message_layouts(
    layouts: &mut BTreeMap<String, MessageTextLayout>,
    previous: &[Value],
    next: &[Value],
) {
    fn bodies(messages: &[Value]) -> BTreeMap<&str, &Value> {
        messages.iter().filter_map(|message| Some((message["id"].as_str()?, &message["text"]))).collect()
    }
    let previous = bodies(previous);
    let next = bodies(next);
    layouts.retain(|id, _| previous.get(id.as_str()).zip(next.get(id.as_str()))
        .is_some_and(|(previous, next)| previous == next));
}

#[cfg(debug_assertions)]
#[derive(Default)]
struct LanPerformanceCounters {
    page_renders: u64,
    message_layout_misses: u64,
    message_layout_hits: u64,
    chat_read_starts: u64,
    image_row_invalidations: u64,
}
fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or(path)
}
fn image_path(path: &str) -> bool {
    let path = path.trim().to_ascii_lowercase();
    !path.contains("://")
        && !path.starts_with("data:")
        && !path.starts_with("blob:")
        && [".png", ".jpg", ".jpeg", ".gif", ".bmp", ".webp"]
            .iter()
            .any(|suffix| file_name(&path).ends_with(suffix))
}
fn image_attachment(attachment: &Value) -> bool {
    text(attachment, "mimeType")
        .to_ascii_lowercase()
        .starts_with("image/")
        && image_path(&text(attachment, "name"))
}
fn format_bytes(value: u64) -> String {
    if value < 1024 {
        format!("{value} B")
    } else if value < 1024 * 1024 {
        format!("{:.1} KB", value as f64 / 1024.)
    } else if value < 1024 * 1024 * 1024 {
        format!("{:.1} MB", value as f64 / (1024. * 1024.))
    } else {
        format!("{:.1} GB", value as f64 / (1024. * 1024. * 1024.))
    }
}
fn format_time(value: &Value) -> String {
    value
        .as_str()
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%m/%d %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}
fn message_status(message: &Value) -> String {
    let status = text(message, "status");
    let label = match status.as_str() {
        "queued" => tr("等待发送", "Queued"),
        "sending" => tr("发送中", "Sending"),
        "delivered" => tr("已送达", "Delivered"),
        "partial" => tr("部分送达", "Partially delivered"),
        "failed" => tr("发送失败", "Send failed"),
        "received" => tr("已接收", "Received"),
        "staged" => tr("已暂存", "Staged"),
        "canceled" => tr("已取消", "Canceled"),
        _ => status.as_str(),
    };
    let deliveries = values(message, "deliveries");
    if message["direction"] == "outgoing" && deliveries.len() > 1 {
        format!(
            "{label} {}/{}",
            deliveries
                .iter()
                .filter(|delivery| delivery["status"] == "delivered")
                .count(),
            deliveries.len()
        )
    } else {
        label.into()
    }
}
fn message_progress(message: &Value) -> Option<u64> {
    let deliveries = values(message, "deliveries");
    let total = deliveries
        .iter()
        .map(|delivery| delivery["totalBytes"].as_u64().unwrap_or(0) as u128)
        .sum::<u128>();
    if total == 0 {
        return None;
    }
    let transferred = deliveries
        .iter()
        .map(|delivery| delivery["transferredBytes"].as_u64().unwrap_or(0) as u128)
        .sum::<u128>();
    Some(((transferred.saturating_mul(100).saturating_add(total / 2)) / total).min(100) as u64)
}
fn message_actions(message: &Value) -> (bool, bool) {
    let outgoing = message["direction"] == "outgoing";
    (
        outgoing
            && matches!(
                message["status"].as_str(),
                Some("failed" | "partial" | "canceled")
            ),
        outgoing && matches!(message["status"].as_str(), Some("queued" | "sending")),
    )
}
fn upsert(messages: &mut Vec<Value>, message: Value) {
    if let Some(index) = messages.iter().position(|old| old["id"] == message["id"]) {
        messages[index] = message;
    } else {
        messages.push(message);
    }
    if messages.len() > 200 {
        messages.drain(..messages.len() - 200);
    }
}
fn pairing_code(value: &str) -> String {
    value.chars().filter(char::is_ascii_digit).take(6).collect()
}
fn alias_input(value: &str) -> String {
    let mut units = 0;
    value
        .chars()
        .take_while(|ch| {
            units += ch.len_utf16();
            units <= 64
        })
        .collect()
}

fn send_reply_matches(
    revision: u64,
    current_revision: u64,
    text: &str,
    current_text: &str,
    files: &[String],
    current_files: &[String],
    composing: bool,
) -> bool {
    revision == current_revision && text == current_text && files == current_files && !composing
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RequestStamp {
    lifetime: u64,
    request: u64,
    conversation: String,
}
impl RequestStamp {
    fn is_current(&self, active: bool, lifetime: u64, request: u64, conversation: &str) -> bool {
        active
            && self.lifetime == lifetime
            && self.request == request
            && self.conversation == conversation
    }
}

#[derive(Clone)]
enum Action {
    Config(Value),
    DeviceSync(Value),
    Code,
    Scan,
    Pair(String, String),
    Forget(String),
    Send {
        conversation: String,
        text: String,
        files: Vec<String>,
        revision: u64,
    },
    HistorySend { ids: Vec<String>, conversation: String },
    Retry(String),
    RetryHistory { sender: String, message: String },
    Cancel(String),
    Open(String),
    Reveal(String),
    Pick(bool),
    Read(String),
    ComposeRecent,
    ComposeClipboard,
    DiscardCompose(Vec<String>),
    DiscardAllCompose,
}
impl Action {
    fn execute(&self, host: &HostState) -> Result<Value, String> {
        match self {
            Self::Config(params) => host.lan_config_update(params),
            Self::DeviceSync(params) => host.device_sync_config_update(params),
            Self::Code => {
                let value = host.lan_refresh_code();
                if value["ok"] == false {
                    Err(text(&value, "error"))
                } else {
                    Ok(value)
                }
            }
            Self::Scan => host.lan_discover(),
            Self::Pair(id, code) => host.lan_pair(&json!({"deviceId":id,"pairingCode":code})),
            Self::Forget(id) => host.lan_forget(&json!({"deviceId":id})),
            Self::Send {
                conversation,
                text,
                files,
                ..
            } => {
                host.lan_send(&json!({"conversationId":conversation,"text":text,"filePaths":files}))
            }
            Self::HistorySend { ids, conversation } => {
                host.lan_send_clips(&json!({"ids":ids,"conversationId":conversation}))
            }
            Self::Retry(id) => host.lan_retry(&json!({"messageId":id})),
            Self::RetryHistory { sender, message } => host.lan_commit_inbound(sender, message),
            Self::Cancel(id) => host.lan_cancel(&json!({"messageId":id})),
            Self::Open(id) => host.lan_attachment_open(&json!({"attachmentId":id})),
            Self::Reveal(id) => host.lan_attachment_reveal(&json!({"attachmentId":id})),
            Self::Pick(_) => Err("File picker requires its original window lease.".into()),
            Self::Read(conversation) => host.lan_read(&json!({"conversationId":conversation})),
            Self::ComposeRecent => host.lan_compose_recent(),
            Self::ComposeClipboard => host.lan_compose_clipboard(),
            Self::DiscardCompose(paths) => host.lan_compose_discard(paths).map(|_| Value::Null),
            Self::DiscardAllCompose => host.lan_compose_discard_all().map(|_| Value::Null),
        }
    }
}

fn record_settings_write_result(
    errors: &mut BTreeMap<String, String>,
    action: &Action,
    result: &Result<Value, String>,
) {
    let keys = match action {
        Action::Config(params) => params.as_object().into_iter().flat_map(|params| params.keys())
            .map(|key| format!("config:{key}")).collect::<Vec<_>>(),
        Action::Pair(id, _) => vec![format!("pair:{id}")],
        Action::Forget(id) => vec![format!("forget:{id}")],
        _ => return,
    };
    for key in keys {
        match result {
            Ok(_) => { errors.remove(&key); }
            Err(error) => { errors.insert(key, error.clone()); }
        }
    }
}

#[derive(Clone)]
struct FailedAction {
    id: u64,
    action: Action,
    error: String,
}
#[derive(Default)]
struct AcceptedWrites {
    next_id: u64,
    pending: usize,
    pending_retries: BTreeSet<u64>,
    failures: Vec<FailedAction>,
    waiters: Vec<tokio::sync::oneshot::Sender<Result<(), String>>>,
}
struct ActionQueue {
    host: Weak<HostState>,
    state: Arc<Mutex<AcceptedWrites>>,
    sender: mpsc::Sender<ActionCommand>,
}
struct ActionCommand {
    id: u64,
    action: Action,
    replaces: Option<u64>,
    execute: Box<dyn FnOnce() -> Result<Value, String> + Send>,
    reply: tokio::sync::oneshot::Sender<Result<Value, String>>,
}
static ACTION_QUEUES: OnceLock<Mutex<Vec<Arc<ActionQueue>>>> = OnceLock::new();
impl ActionQueue {
    fn for_host(host: &Arc<HostState>) -> Arc<Self> {
        let mut queues = ACTION_QUEUES
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(queue) = queues
            .iter()
            .find(|queue| Weak::ptr_eq(&queue.host, &Arc::downgrade(host)))
        {
            return queue.clone();
        }
        let queue = Self::start(Arc::downgrade(host));
        queues.push(queue.clone());
        queue
    }
    fn start(host: Weak<HostState>) -> Arc<Self> {
        let state = Arc::new(Mutex::new(AcceptedWrites::default()));
        let (sender, receiver) = mpsc::channel::<ActionCommand>();
        let worker_state = state.clone();
        // One dedicated worker per Host keeps acceptance order across window
        // replacement. No background executor thread waits on another worker.
        let _ = std::thread::Builder::new()
            .name("clip-lan-actions".into())
            .spawn(move || {
                while let Ok(command) = receiver.recv() {
                    let result =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(command.execute))
                            .unwrap_or_else(|_| {
                                Err(tr("局域网操作线程失败", "LAN operation worker failed").into())
                            });
                    let mut state = worker_state.lock().unwrap_or_else(|e| e.into_inner());
                    Self::finish_locked(
                        &mut state,
                        command.id,
                        command.action,
                        command.replaces,
                        &result,
                    );
                    drop(state);
                    let _ = command.reply.send(result);
                }
            });
        Arc::new(Self {
            host,
            state,
            sender,
        })
    }
    fn submit(
        &self,
        host: Arc<HostState>,
        action: Action,
        replaces: Option<u64>,
    ) -> (u64, tokio::sync::oneshot::Receiver<Result<Value, String>>) {
        let execute_action = action.clone();
        let accepted = self.state.clone();
        self.enqueue(
            action,
            replaces,
            Box::new(move || {
                let action = match execute_action {
                    Action::DiscardCompose(paths) => {
                        // Recheck on the FIFO worker: a send accepted before
                        // this cleanup may not have failed when it was queued.
                        let state = accepted.lock().unwrap_or_else(|error| error.into_inner());
                        Action::DiscardCompose(Self::discardable_paths(&state, &paths))
                    }
                    Action::DiscardAllCompose => {
                        let state = accepted.lock().unwrap_or_else(|error| error.into_inner());
                        if state
                            .failures
                            .iter()
                            .any(|failure| matches!(failure.action, Action::Send { .. }))
                        {
                            return Err(tr(
                                "仍有待重试的发送，已保留草稿附件。",
                                "A send still needs retrying; draft attachments were preserved.",
                            )
                            .into());
                        }
                        Action::DiscardAllCompose
                    }
                    action => action,
                };
                action.execute(&host)
            }),
        )
    }
    fn discardable_paths(state: &AcceptedWrites, paths: &[String]) -> Vec<String> {
        paths
            .iter()
            .filter(|path| {
                !state.failures.iter().any(|failure| {
            matches!(&failure.action,Action::Send{files,..} if files.contains(path))
        })
            })
            .cloned()
            .collect()
    }
    fn enqueue(
        &self,
        action: Action,
        replaces: Option<u64>,
        execute: Box<dyn FnOnce() -> Result<Value, String> + Send>,
    ) -> (u64, tokio::sync::oneshot::Receiver<Result<Value, String>>) {
        self.try_enqueue(action, replaces, execute)
            .unwrap_or_else(|error| {
                let (reply, receiver) = tokio::sync::oneshot::channel();
                let _ = reply.send(Err(error));
                (0, receiver)
            })
    }
    fn try_enqueue(
        &self,
        action: Action,
        replaces: Option<u64>,
        execute: Box<dyn FnOnce() -> Result<Value, String> + Send>,
    ) -> Result<(u64, tokio::sync::oneshot::Receiver<Result<Value, String>>), String> {
        let (reply, receiver) = tokio::sync::oneshot::channel();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(replaces) = replaces {
            if state.pending_retries.contains(&replaces) {
                return Err(tr("该请求正在重试", "This request is already being retried.").into());
            }
            if !state.failures.iter().any(|failure| failure.id == replaces) {
                return Err(tr(
                    "原发送请求已失效",
                    "Original send request is no longer available.",
                )
                .into());
            }
            state.pending_retries.insert(replaces);
        }
        state.next_id = state.next_id.wrapping_add(1);
        let id = state.next_id;
        state.pending += 1;
        // Registration and channel insertion share this lock. Concurrent
        // acceptors cannot insert B before A after A has received its ID.
        if let Err(error) = self.sender.send(ActionCommand {
            id,
            action,
            replaces,
            execute,
            reply,
        }) {
            let command = error.0;
            let result = Err(tr("局域网操作线程不可用", "LAN operation worker unavailable").into());
            Self::finish_locked(&mut state, id, command.action, replaces, &result);
            let _ = command.reply.send(result);
        }
        Ok((id, receiver))
    }
    fn finish_locked(
        state: &mut AcceptedWrites,
        id: u64,
        action: Action,
        replaces: Option<u64>,
        result: &Result<Value, String>,
    ) {
        state.pending = state.pending.saturating_sub(1);
        if let Some(replaces) = replaces {
            state.pending_retries.remove(&replaces);
            state.failures.retain(|failure| failure.id != replaces);
        }
        // Preparing a draft is a read operation. Its error is shown by the
        // live requester, but a retired requester must not block app exit.
        if !matches!(action, Action::ComposeRecent | Action::ComposeClipboard) {
            if let Err(error) = result {
                state.failures.push(FailedAction {
                    id,
                    action,
                    error: error.clone(),
                });
            }
        }
        if state.pending == 0 {
            let result = state
                .failures
                .first()
                .map_or(Ok(()), |failure| Err(failure.error.clone()));
            for waiter in std::mem::take(&mut state.waiters) {
                let _ = waiter.send(result.clone());
            }
        }
    }
    fn barrier(&self) -> tokio::sync::oneshot::Receiver<Result<(), String>> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.pending == 0 {
            let result = state
                .failures
                .first()
                .map_or(Ok(()), |failure| Err(failure.error.clone()));
            let _ = sender.send(result);
        } else {
            state.waiters.push(sender);
        }
        receiver
    }
    fn failures(&self) -> Vec<FailedAction> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .failures
            .clone()
    }
    fn acknowledge(&self, id: u64) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.pending_retries.contains(&id) {
            return;
        }
        let paths = state
            .failures
            .iter()
            .filter(|failure| failure.id == id)
            .filter_map(|failure| {
                if let Action::Send { files, .. } = &failure.action {
                    Some(files.clone())
                } else {
                    None
                }
            })
            .flatten()
            .collect::<Vec<_>>();
        state.failures.retain(|failure| failure.id != id);
        drop(state);
        if !paths.is_empty() {
            if let Some(host) = self.host.upgrade() {
                let (_, reply) = self.submit(host, Action::DiscardCompose(paths), None);
                drop(reply);
            }
        }
    }
    fn is_retrying(&self, id: u64) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending_retries
            .contains(&id)
    }
}

/// Accepted history sends share the LAN page's Host queue and exit barrier.
/// The root must call accept before spawning any UI observer/background work.
pub struct SharedHistorySend;
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistorySendFailure {
    pub id: u64,
    pub ids: Vec<String>,
    pub conversation_id: String,
    pub error: String,
}
impl SharedHistorySend {
    pub fn accept(
        host: &Arc<HostState>,
        ids: Vec<String>,
    ) -> (u64, tokio::sync::oneshot::Receiver<Result<Value, String>>) {
        Self::accept_to(host, ids, cliphost_core::lan::ALL_CONVERSATION_ID)
    }
    /// Capture the destination at acceptance, so switching the LAN page or a
    /// later retry cannot redirect a history send to a different device.
    pub fn accept_to(
        host: &Arc<HostState>,
        ids: Vec<String>,
        conversation: impl Into<String>,
    ) -> (u64, tokio::sync::oneshot::Receiver<Result<Value, String>>) {
        ActionQueue::for_host(host).submit(host.clone(), Action::HistorySend {
            ids, conversation: conversation.into(),
        }, None)
    }
    pub fn failures(host: &Arc<HostState>) -> Vec<HistorySendFailure> {
        ActionQueue::for_host(host)
            .failures()
            .into_iter()
            .filter_map(|failure| match failure.action {
                Action::HistorySend { ids, conversation } => Some(HistorySendFailure {
                    id: failure.id,
                    ids,
                    conversation_id: conversation,
                    error: failure.error,
                }),
                _ => None,
            })
            .collect()
    }
    pub fn retry(
        host: &Arc<HostState>,
        failed_id: u64,
    ) -> Result<(u64, tokio::sync::oneshot::Receiver<Result<Value, String>>), String> {
        let queue = ActionQueue::for_host(host);
        let failure = queue
            .failures()
            .into_iter()
            .find(|failure| {
                failure.id == failed_id && matches!(failure.action, Action::HistorySend { .. })
            })
            .ok_or_else(|| {
                tr(
                    "原发送请求已失效",
                    "Original send request is no longer available.",
                )
                .to_string()
            })?;
        let execute_action = failure.action.clone();
        let host = host.clone();
        queue.try_enqueue(
            failure.action,
            Some(failed_id),
            Box::new(move || execute_action.execute(&host)),
        )
    }
    pub fn acknowledge(host: &Arc<HostState>, failed_id: u64) {
        let queue = ActionQueue::for_host(host);
        if queue.failures().iter().any(|failure| {
            failure.id == failed_id && matches!(failure.action, Action::HistorySend { .. })
        }) {
            queue.acknowledge(failed_id);
        }
    }
    pub fn is_retrying(host: &Arc<HostState>, failed_id: u64) -> bool {
        ActionQueue::for_host(host).is_retrying(failed_id)
    }
}

pub(super) async fn flush_writes() -> Result<(), String> {
    let Some(registry) = ACTION_QUEUES.get() else {
        return Ok(());
    };
    let queues = registry.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let barriers = queues
        .iter()
        .map(|queue| queue.barrier())
        .collect::<Vec<_>>();
    let mut error = None;
    for barrier in barriers {
        if let Err(failure) = barrier
            .await
            .unwrap_or_else(|_| Err("LAN operation worker unavailable".into()))
        {
            error.get_or_insert(failure);
        }
    }
    if let Some(error) = error {
        return Err(error);
    }
    let mut error = None;
    for queue in queues {
        if let Some(host) = queue.host.upgrade() {
            let (_, cleanup) = queue.submit(host, Action::DiscardAllCompose, None);
            if let Err(failure) = cleanup
                .await
                .unwrap_or_else(|_| Err("LAN draft cleanup worker unavailable".into()))
            {
                error.get_or_insert(failure);
            }
        }
        let mut state = queue.state.lock().unwrap_or_else(|e| e.into_inner());
        state.failures.retain(|failure| {
            matches!(failure.action, Action::Send { .. } | Action::HistorySend { .. })
        });
    }
    error.map_or(Ok(()), Err)
}

#[derive(Default)]
struct PickerLifetime {
    generation: AtomicU64,
    active: AtomicBool,
}
impl PickerLifetime {
    fn retire(&self, active: bool) {
        self.active.store(false, Ordering::SeqCst);
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.active.store(active, Ordering::SeqCst);
    }
    fn is_current(&self, generation: u64) -> bool {
        self.active.load(Ordering::SeqCst) && self.generation.load(Ordering::SeqCst) == generation
    }
}
#[derive(Clone)]
struct PickerLease {
    state: Arc<PickerLifetime>,
    generation: u64,
    hwnd: isize,
}
impl PickerLease {
    fn is_current(&self, host: &HostState) -> bool {
        self.state.is_current(self.generation)
            && host.native_caption_drag_allowed("main", self.hwnd)
    }
    fn execute(&self, host: &HostState, image: bool) -> Result<Value, String> {
        // The core checks the same predicate immediately before COM Show,
        // after it returns, and before image staging. Captured HWND is never
        // replaced with the currently open management window's HWND.
        if !self.is_current(host) {
            return Ok(picker_cancelled(image));
        }
        let result = if image {
            host.pick_image_file_native_guarded(&json!({}), self.hwnd, || self.is_current(host))
        } else {
            host.pick_files_native_guarded(&json!({}), self.hwnd, || self.is_current(host))
        };
        if self.is_current(host) {
            result
        } else {
            Ok(picker_cancelled(image))
        }
    }
}
fn picker_cancelled(image: bool) -> Value {
    if image {
        json!({"path":null,"cancelled":true})
    } else {
        json!({"paths":[],"cancelled":true})
    }
}

struct Bitmap {
    image: Arc<Image>,
    width: f32,
    height: f32,
    allocation: usize,
}
struct CachedImage {
    bitmap: Option<Bitmap>,
    used: u64,
    version: Option<FileVersion>,
    revision: String,
    checked: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileVersion {
    length: u64,
    modified: Option<SystemTime>,
}
impl FileVersion {
    fn read(path: &str) -> Option<Self> {
        let metadata = std::fs::metadata(path).ok()?;
        metadata.is_file().then(|| Self {
            length: metadata.len(),
            modified: metadata.modified().ok(),
        })
    }
}
enum ImageLoad {
    Unchanged,
    Loaded(Bitmap, FileVersion),
    Failed,
}
#[derive(Clone, Copy)]
enum ImagePurpose {
    Chip,
    Attachment,
    Preview,
}
impl ImagePurpose {
    fn key(self, path: &str) -> String {
        format!(
            "{}:{path}",
            match self {
                Self::Chip => "chip",
                Self::Attachment => "attachment",
                Self::Preview => "preview",
            }
        )
    }
    fn dimensions(self) -> (u32, u32) {
        match self {
            Self::Chip => (64, 64),
            Self::Attachment => (400, 300),
            Self::Preview => (1600, 1200),
        }
    }
}

fn load_image(
    host: &HostState,
    path: &str,
    previous: Option<FileVersion>,
    purpose: ImagePurpose,
) -> ImageLoad {
    match prepare_image(host, path, previous, purpose) {
        Some(value) => value,
        None => ImageLoad::Failed,
    }
}
fn prepare_image(
    host: &HostState,
    path: &str,
    previous: Option<FileVersion>,
    purpose: ImagePurpose,
) -> Option<ImageLoad> {
    const INPUT_LIMIT: u64 = 32 * 1024 * 1024;
    let version = FileVersion::read(path)?;
    if !image_path(path) || version.length > INPUT_LIMIT {
        return None;
    }
    if previous == Some(version) {
        return Some(ImageLoad::Unchanged);
    }
    let (bytes, _) = host.local_image_bytes(path)?;
    if bytes.len() as u64 > INPUT_LIMIT {
        return None;
    }
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(64 * 1024 * 1024);
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    reader.limits(limits);
    let dimensions = purpose.dimensions();
    let image = reader.decode().ok()?;
    let image = image.thumbnail(
        image.width().min(dimensions.0),
        image.height().min(dimensions.1),
    );
    let (width, height) = (image.width() as f32, image.height() as f32);
    let allocation = image.width() as usize * image.height() as usize * 4;
    let mut encoded = Cursor::new(Vec::new());
    image.write_to(&mut encoded, image::ImageFormat::Png).ok()?;
    if FileVersion::read(path) != Some(version) {
        return None;
    }
    Some(ImageLoad::Loaded(
        Bitmap {
            image: Arc::new(Image::from_bytes(ImageFormat::Png, encoded.into_inner())),
            width,
            height,
            allocation,
        },
        version,
    ))
}

struct Tooltip {
    text: String,
    tokens: DesignTokens,
    dark: bool,
}
impl Render for Tooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let t = self.tokens.theme(self.dark);
        rsx! {
            <div max_w={px(420.)} p={px(7.)} rounded={px(5.)} border_1
                border_color={color(&t.border)} bg={color(&t.surface)} text_size={px(11.)}
                text_color={color(&t.text)}>
                {self.text.clone()}
            </div>
        }
    }
}
fn tooltip(
    text: String,
    tokens: DesignTokens,
    dark: bool,
) -> impl Fn(&mut Window, &mut gpui::App) -> gpui::AnyView + 'static {
    move |_, cx| {
        cx.new(|_| Tooltip {
            text: text.clone(),
            tokens: tokens.clone(),
            dark,
        })
        .into()
    }
}

pub struct LanManagementPage {
    host: Arc<HostState>,
    writes: Arc<ActionQueue>,
    tokens: DesignTokens,
    dark: bool,
    active: bool,
    companion: bool,
    /// Hosted as a page of the unified main window, which owns close and title.
    embedded: bool,
    /// Connection settings share the parent's scroll column with real sync controls.
    settings_only: bool,
    settings_content: Option<gpui::AnyView>,
    settings_leave_locked: bool,
    settings_write_errors: BTreeMap<String, String>,
    history_drop_hovered: bool,
    incoming_history: Option<incoming::IncomingHistory>,
    incoming_history_failed: bool,
    incoming_history_session: u64,
    lifetime: u64,
    picker_lifetime: Arc<PickerLifetime>,
    hwnd: isize,
    shell_request: u64,
    message_request: u64,
    shell_loading: bool,
    messages_loading: bool,
    shell_again: bool,
    messages_again: bool,
    chat_revision: u64,
    pairing_countdown: Option<i64>,
    pairing_refresh: PairingCodeRefresh,
    event_scheduled: bool,
    status: Value,
    device_sync: Value,
    peers: Vec<Value>,
    conversations: Vec<Value>,
    messages: Vec<Value>,
    selected: String,
    alias: Entity<SearchInput>,
    port: Entity<SearchInput>,
    codes: BTreeMap<String, Entity<SearchInput>>,
    code_subscriptions: BTreeMap<String, Subscription>,
    composer: Entity<MultilineInput>,
    files: Vec<String>,
    recent_items: Vec<RecentComposeItem>,
    composer_revision: u64,
    composer_text: String,
    identity_revision: u64,
    identity_alias: String,
    identity_port: String,
    identity_dirty: bool,
    busy: bool,
    exit_pending: bool,
    failed_send: Option<u64>,
    seen_failures: BTreeSet<u64>,
    action: u64,
    error: Option<String>,
    devices: bool,
    attach_menu: bool,
    /// Entrance-animation identities: advance only when each surface appears.
    devices_epoch: motion::OpenEpoch,
    attach_epoch: motion::OpenEpoch,
    preview: Option<Value>,
    images: BTreeMap<String, CachedImage>,
    image_loading: BTreeMap<String, u64>,
    image_request: u64,
    image_clock: u64,
    list: ListState,
    message_scrollbar: crate::scrollbar::ScrollbarState,
    session_scrollbar: crate::scrollbar::ScrollbarState,
    device_scrollbar: crate::scrollbar::ScrollbarState,
    session_scroll: gpui::ScrollHandle,
    device_scroll: gpui::ScrollHandle,
    scroll_after_load: bool,
    fixture: bool,
    #[cfg(debug_assertions)]
    fixture_composer_layout: Arc<Mutex<Value>>,
    #[cfg(debug_assertions)]
    fixture_message_layout: Arc<Mutex<Value>>,
    #[cfg(debug_assertions)]
    fixture_keyboard_submit_count: u64,
    #[cfg(debug_assertions)]
    pub(super) fixture_keyboard_read_only: bool,
    sessions: bool,
    advanced: bool,
    pair_target: Option<String>,
    forget_confirm: Option<String>,
    expanded_deliveries: BTreeSet<String>,
    drafts: BTreeMap<String, ConversationDraft>,
    message_inputs: BTreeMap<String, Entity<MultilineInput>>,
    message_layouts: BTreeMap<String, MessageTextLayout>,
    #[cfg(debug_assertions)]
    performance: LanPerformanceCounters,
    focus_handles: BTreeMap<String, gpui::FocusHandle>,
    hovered_controls: BTreeSet<String>,
    control_tweens: motion::Tweens<String>,
    previous_focus: Option<gpui::FocusHandle>,
    /// Like CSS `:focus-visible`: rings only after keyboard use, cleared by any pointer press.
    focus_visible: bool,
    overlay_focus: gpui::FocusHandle,
    notice: Option<String>,
    paste_target: Option<(PasteTarget, String, u64)>,
    owned_files: BTreeSet<String>,
    overlay_tab_order: Vec<gpui::FocusHandle>,
    _subscriptions: Vec<Subscription>,
    _watch: Task<()>,
}

pub(crate) enum LanManagementEvent {
    Error(String),
    CloseRequested,
    MoveRequested,
}
impl EventEmitter<LanManagementEvent> for LanManagementPage {}
impl Drop for LanManagementPage {
    fn drop(&mut self) {
        self.picker_lifetime.retire(false);
        crate::favorite_drag::clear_target_hwnd(self.hwnd);
        let paths = self
            .owned_files
            .iter()
            .chain(self.files.iter())
            .chain(self.drafts.values().flat_map(|draft| draft.files.iter()))
            .cloned()
            .collect::<BTreeSet<_>>();
        self.discard_compose_files(paths.into_iter().collect());
    }
}

impl LanManagementPage {
    /// Offline keyboard contract setup. Content is supplied by the fixture,
    /// and input still flows through the real multiline IME implementation.
    #[cfg(debug_assertions)]
    pub fn fixture_keyboard_prepare(
        &mut self,
        text: &str,
        read_only: bool,
        composing: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.fixture {
            return;
        }
        self.fixture_keyboard_submit_count = 0;
        self.fixture_keyboard_read_only = read_only;
        self.devices = false;
        self.sessions = false;
        self.preview = None;
        self.attach_menu = false;
        self.advanced = false;
        self.composer.update(cx, |input, cx| {
            input.set_read_only(false, cx);
            input.set_text(String::new(), window, cx);
            if composing {
                let end = text.encode_utf16().count();
                gpui::EntityInputHandler::replace_and_mark_text_in_range(
                    input,
                    None,
                    text,
                    Some(end..end),
                    window,
                    cx,
                );
            } else {
                gpui::EntityInputHandler::replace_text_in_range(input, None, text, window, cx);
            }
            input.set_read_only(read_only, cx);
            input.focus_handle().focus(window);
        });
        self.sync_composer(cx);
        cx.notify();
    }

    #[cfg(debug_assertions)]
    pub fn fixture_devices_bottom(&mut self, cx: &mut Context<Self>) -> bool {
        if self.fixture && self.devices && f32::from(self.device_scroll.bounds().size.height) > 0. {
            self.device_scroll.scroll_to_bottom();
            cx.notify();
            return true;
        }
        false
    }

    #[cfg(debug_assertions)]
    pub fn fixture_device_sync_snapshot(&self) -> Value {
        let offset = self.device_scroll.offset();
        let maximum = self.device_scroll.max_offset();
        json!({"displayed": self.device_sync,
            "deviceScrollOffset":{"x":f32::from(offset.x),"y":f32::from(offset.y)},
            "deviceScrollMaxY":f32::from(maximum.height),
            "deviceScrollLayoutReady":f32::from(self.device_scroll.bounds().size.height)>0.,
            "deviceScrollAtBottom":f32::from(offset.y+maximum.height).abs()<=1.})
    }

    #[cfg(debug_assertions)]
    pub fn fixture_keyboard_snapshot(&self, cx: &Context<Self>) -> Value {
        if !self.fixture {
            return Value::Null;
        }
        let input = self.composer.read(cx);
        json!({
            "text": input.text(),
            "submitCount": self.fixture_keyboard_submit_count,
            "isComposing": input.is_composing(),
            "readOnly": input.fixture_is_read_only(),
            "advanced": self.advanced,
        })
    }

    #[cfg(debug_assertions)]
    pub fn fixture_keyboard_open_devices(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.fixture { self.open_devices(false, window, cx); }
    }

    #[cfg(debug_assertions)]
    pub fn fixture_focus_control(&self, id: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.fixture { return false; }
        if let Some(focus) = self.focus_handles.get(id) {
            focus.focus(window);
            cx.notify();
            true
        } else { false }
    }

    #[cfg(debug_assertions)]
    pub fn fixture_performance_snapshot(&self) -> Value {
        json!({"pageRenders":self.performance.page_renders,
            "messageLayoutMisses":self.performance.message_layout_misses,
            "messageLayoutHits":self.performance.message_layout_hits,
            "chatReadStarts":self.performance.chat_read_starts,
            "imageRowInvalidations":self.performance.image_row_invalidations,
            "cachedTextLayouts":self.message_layouts.len(),
            "composerCharacters":self.composer_text.chars().count(),
            "composerRevision":self.composer_revision})
    }

    #[cfg(debug_assertions)]
    pub fn fixture_append_composer(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.fixture {
            self.composer.update(cx, |input, cx| {
                gpui::EntityInputHandler::replace_text_in_range(input, None, text, window, cx);
            });
        }
    }
    /// Message list scroll geometry for the isolated Debug fixture's wheel probe.
    #[cfg(debug_assertions)]
    pub fn fixture_scroll_snapshot(&self) -> Value {
        let top = self.list.logical_scroll_top();
        let viewport = self.list.viewport_bounds();
        json!({
            "messageCount": self.messages.len(), "itemCount": self.list.item_count(),
            "itemIx": top.item_ix, "offsetInItem": f32::from(top.offset_in_item),
            "scrollPx": -f32::from(self.list.scroll_px_offset_for_scrollbar().y),
            "maxOffset": f32::from(self.list.max_offset_for_scrollbar().height),
            "viewport": [f32::from(viewport.origin.x), f32::from(viewport.origin.y),
                f32::from(viewport.size.width), f32::from(viewport.size.height)],
        })
    }

    /// Actual production layout recorded only by the isolated Debug fixture.
    #[cfg(debug_assertions)]
    pub fn fixture_composer_layout(&self) -> Value {
        self.fixture_composer_layout
            .lock()
            .map(|layout| layout.clone())
            .unwrap_or(Value::Null)
    }

    #[cfg(debug_assertions)]
    pub fn fixture_message_layouts(&self, cx: &gpui::App) -> Value {
        if !self.fixture { return json!([]); }
        let painted = self.fixture_message_layout.lock().unwrap();
        Value::Array(self.message_inputs.iter().map(|(id, input)| {
            let mut value = input.read(cx).fixture_layout_snapshot();
            value["id"] = json!(id);
            value["bubbleBounds"] = painted[id]["bubbleBounds"].clone();
            value["singleLineWidth"] = painted[id]["singleLineWidth"].clone();
            value["expectedTextHeight"] = painted[id]["expectedTextHeight"].clone();
            value
        }).collect())
    }

    /// Offline preview only. Never prepares content from the user clipboard.
    #[cfg(debug_assertions)]
    pub fn set_fixture_draft(&mut self, draft: Value, window: &mut Window, cx: &mut Context<Self>) {
        if !self.fixture {
            return;
        }
        let body = text(&draft, "text");
        self.composer
            .update(cx, |input, cx| input.set_text(body.clone(), window, cx));
        self.composer_text = body;
        self.files = values(&draft, "filePaths")
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        self.recent_items = values(&draft, "recentItems")
            .iter()
            .map(|item| RecentComposeItem {
                id: text(item, "id"),
                label: text(item, "label"),
                kind: text(item, "kind"),
                text: text(item, "text"),
                files: values(item, "filePaths")
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect(),
            })
            .collect();
        for path in self.recent_items.iter().flat_map(|item| item.files.iter()) {
            if !self.files.contains(path) {
                self.files.push(path.clone());
            }
        }
        self.composer_revision = self.composer_revision.wrapping_add(1);
        cx.notify();
    }
    #[cfg(debug_assertions)]
    pub fn new_fixture(
        host: Arc<HostState>,
        tokens: DesignTokens,
        dark: bool,
        data: LanFixtureData,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut page = Self::new(host, tokens, dark, false, window, cx);
        page.fixture = true;
        page.status = data.status;
        page.device_sync = data.device_sync;
        page.peers = data.peers;
        page.conversations = data.conversations;
        page.selected = data.selected;
        page.devices = data.devices;
        page.replace_messages(data.messages);
        page.ensure_codes(window, cx);
        let alias = text(&page.status, "deviceAlias");
        let port = page.status["port"].as_u64().unwrap_or(28766).to_string();
        page.alias
            .update(cx, |input, cx| input.set_text(alias, window, cx));
        page.port
            .update(cx, |input, cx| input.set_text(port, window, cx));
        page.configure_inputs(cx);
        page
    }
    pub(crate) fn retire_pickers(&self) {
        self.picker_lifetime.retire(false);
    }
    pub(crate) fn picker_close_guard(&self) -> impl Fn() + Send + 'static {
        let lifetime = self.picker_lifetime.clone();
        move || lifetime.retire(false)
    }
    fn report_error(&mut self, error: String, cx: &mut Context<Self>) {
        self.error = Some(error.clone());
        cx.emit(LanManagementEvent::Error(error));
        cx.notify();
    }
    pub(crate) fn new(
        host: Arc<HostState>,
        tokens: DesignTokens,
        dark: bool,
        active: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_with_mode(host, tokens, dark, active, false, window, cx)
    }

    fn new_with_mode(
        host: Arc<HostState>,
        tokens: DesignTokens,
        dark: bool,
        active: bool,
        settings_only: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let alias = cx.new(|cx| SearchInput::new(window, cx));
        let port = cx.new(|cx| SearchInput::new(window, cx));
        let composer = cx.new(|cx| MultilineInput::new(window, cx));
        let alias_sub = cx.subscribe_in(
            &alias,
            window,
            |this, input, event, window, cx| match event {
                SearchEvent::Changed(_) => {
                    if !input.read(cx).is_composing() {
                        let value = input.read(cx).text().to_string();
                        let limited = alias_input(&value);
                        if limited != value {
                            input.update(cx, |input, cx| input.set_text(limited, window, cx));
                        }
                    }
                    this.sync_identity(cx)
                }
                SearchEvent::Escape => this.close_overlays(cx),
                _ => {}
            },
        );
        let port_sub = cx.subscribe_in(&port, window, |this, _, event, _, cx| match event {
            SearchEvent::Changed(_) => this.sync_identity(cx),
            SearchEvent::Escape => this.close_overlays(cx),
            _ => {}
        });
        let composer_sub =
            cx.subscribe_in(
                &composer,
                window,
                |this, _, event, window, cx| match event {
                    MultilineEvent::Changed(_) => {
                        let was_empty = this.composer_text.trim().is_empty();
                        let failed_send = this.failed_send;
                        this.sync_composer(cx);
                        this.discard_empty_failed_send(cx);
                        // The input entity repaints itself. Only its send
                        // availability boundary changes the surrounding page.
                        if was_empty != this.composer_text.trim().is_empty() || failed_send != this.failed_send {
                            cx.notify();
                        }
                    }
                    MultilineEvent::Submit => {
                        #[cfg(debug_assertions)]
                        if this.fixture {
                            this.fixture_keyboard_submit_count =
                                this.fixture_keyboard_submit_count.wrapping_add(1);
                            return;
                        }
                        this.send(window, cx);
                    }
                    MultilineEvent::Escape => this.close_overlays(cx),
                    MultilineEvent::PasteRequested(target) => {
                        if !this.busy && !this.fixture && !this.exit_pending {
                            this.composer.read(cx).focus_handle().focus(window);
                            this.paste_target =
                                Some((target.clone(), this.selected.clone(), this.lifetime));
                            this.run(Action::ComposeClipboard, window, cx);
                        }
                    }
                    _ => {}
                },
            );
        let watch = cx.spawn_in(window, async move |this, cx| {
          let mut last_reconcile = Instant::now();
          loop {
            Timer::after(Duration::from_millis(750)).await;
            if this
                .update_in(cx, |this, window, cx| {
                    if this.active {
                        this.drain_history_drop(window, cx);
                        this.report_write_failures(window, cx);
                        if this.pairing_visible(window) && this.status["enabled"] == true {
                            let seconds = pairing_seconds(&this.status);
                            if this.pairing_countdown != seconds {
                                this.pairing_countdown = seconds;
                                cx.notify();
                            }
                        } else {
                            this.pairing_countdown = None;
                        }
                        this.renew_pairing_code(window, cx);
                        // Events carry normal updates. Retain a slow recovery
                        // read for a missed event without polling chat at 750ms.
                        if last_reconcile.elapsed() >= Duration::from_secs(15) {
                            last_reconcile = Instant::now();
                            this.refresh_chat(window, cx, false);
                            this.refresh_shell(window, cx, false);
                        }
                    }
                })
                .is_err()
            {
                break;
            }
          }
        });
        #[cfg(windows)]
        let hwnd = crate::platform::hwnd(window).map_or(0, |hwnd| hwnd.0 as isize);
        #[cfg(not(windows))]
        let hwnd = 0;
        let picker_lifetime = Arc::new(PickerLifetime::default());
        picker_lifetime.retire(active);
        let mut page = Self {
            writes: ActionQueue::for_host(&host),
            host,
            tokens,
            dark,
            active,
            companion: false,
            embedded: settings_only,
            settings_only,
            settings_content: None,
            settings_leave_locked: false,
            settings_write_errors: BTreeMap::new(),
            history_drop_hovered: false,
            incoming_history: None,
            incoming_history_failed: false,
            incoming_history_session: 1,
            lifetime: 1,
            picker_lifetime,
            hwnd,
            shell_request: 0,
            message_request: 0,
            shell_loading: false,
            messages_loading: false,
            shell_again: false,
            messages_again: false,
            chat_revision: 0,
            pairing_countdown: None,
            pairing_refresh: PairingCodeRefresh::default(),
            event_scheduled: false,
            status: Value::Null,
            device_sync: Value::Null,
            peers: Vec::new(),
            conversations: Vec::new(),
            messages: Vec::new(),
            selected: "all".into(),
            alias,
            port,
            codes: BTreeMap::new(),
            code_subscriptions: BTreeMap::new(),
            composer,
            files: Vec::new(),
            recent_items: Vec::new(),
            composer_revision: 0,
            composer_text: String::new(),
            identity_revision: 0,
            identity_alias: String::new(),
            identity_port: String::new(),
            identity_dirty: false,
            busy: false,
            exit_pending: false,
            failed_send: None,
            seen_failures: BTreeSet::new(),
            action: 0,
            error: None,
            devices: false,
            attach_menu: false,
            devices_epoch: motion::OpenEpoch::default(),
            attach_epoch: motion::OpenEpoch::default(),
            preview: None,
            images: BTreeMap::new(),
            image_loading: BTreeMap::new(),
            image_request: 0,
            image_clock: 0,
            list: ListState::new(0, ListAlignment::Bottom, px(72.)),
            message_scrollbar: Default::default(),
            session_scrollbar: Default::default(),
            device_scrollbar: Default::default(),
            session_scroll: Default::default(),
            device_scroll: Default::default(),
            scroll_after_load: true,
            fixture: false,
            #[cfg(debug_assertions)]
            fixture_composer_layout: Arc::new(Mutex::new(json!({}))),
            #[cfg(debug_assertions)]
            fixture_message_layout: Arc::new(Mutex::new(json!({}))),
            #[cfg(debug_assertions)]
            fixture_keyboard_submit_count: 0,
            #[cfg(debug_assertions)]
            fixture_keyboard_read_only: false,
            sessions: false,
            advanced: false,
            pair_target: None,
            forget_confirm: None,
            expanded_deliveries: BTreeSet::new(),
            drafts: BTreeMap::new(),
            message_inputs: BTreeMap::new(),
            message_layouts: BTreeMap::new(),
            #[cfg(debug_assertions)]
            performance: LanPerformanceCounters::default(),
            focus_handles: BTreeMap::new(),
            hovered_controls: BTreeSet::new(),
            control_tweens: motion::Tweens::default(),
            previous_focus: None,
            focus_visible: false,
            overlay_focus: cx.focus_handle().tab_stop(true),
            notice: None,
            paste_target: None,
            owned_files: BTreeSet::new(),
            overlay_tab_order: Vec::new(),
            _subscriptions: vec![alias_sub, port_sub, composer_sub],
            _watch: watch,
        };
        page.configure_inputs(cx);
        if !settings_only {
            page.restore_failed_send(window, cx);
        }
        page._subscriptions
            .push(cx.observe_window_activation(window, |this, window, cx| {
                if this.active && window.is_window_active() {
                    this.refresh_chat(window, cx, false);
                    this.refresh_shell(window, cx, false);
                    this.renew_pairing_code(window, cx);
                }
            }));
        if active {
            page.pairing_refresh.opened();
            page.refresh_shell(window, cx, true);
            page.refresh_chat(window, cx, true);
        }
        page
    }

    pub(crate) fn new_settings(
        host: Arc<HostState>,
        tokens: DesignTokens,
        dark: bool,
        active: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Configure the mode before activation so the settings entity never
        // starts conversation reads or consumes a chat draft.
        Self::new_with_mode(host, tokens, dark, active, true, window, cx)
    }

    pub(crate) fn set_settings_content(&mut self, content: gpui::AnyView, cx: &mut Context<Self>) {
        self.settings_content = Some(content);
        cx.notify();
    }

    pub(crate) fn composition_active(&self, cx: &gpui::App) -> bool {
        self.composing(cx)
    }

    pub(crate) fn focus_handles(&self, _cx: &gpui::App) -> Vec<gpui::FocusHandle> {
        if self.active && !self.exit_pending && !self.settings_leave_locked { self.overlay_tab_order.clone() } else { Vec::new() }
    }

    pub(crate) fn pending_writes(&self) -> bool {
        self.busy || self.writes.state.lock().unwrap_or_else(|error| error.into_inner()).pending > 0
    }

    pub(crate) fn settings_persistence_error(&self) -> Option<String> {
        self.settings_write_errors.values().next().cloned()
    }

    pub(crate) fn set_leave_locked(&mut self, locked: bool, cx: &mut Context<Self>) {
        if self.settings_leave_locked == locked { return; }
        self.settings_leave_locked = locked;
        self.configure_inputs(cx);
        cx.notify();
    }

    pub(crate) fn keyboard_navigation(&mut self, cx: &mut Context<Self>) {
        if !self.focus_visible {
            self.focus_visible = true;
            cx.notify();
        }
    }

    fn pairing_visible(&self, window: &Window) -> bool {
        if !self.active || self.fixture || self.exit_pending || self.settings_leave_locked
            || !(self.settings_only || (self.devices && !self.sessions))
        {
            return false;
        }
        #[cfg(windows)]
        {
            use windows::Win32::UI::WindowsAndMessaging::{IsIconic, IsWindowVisible};
            crate::platform::hwnd(window).is_ok_and(|hwnd| unsafe {
                IsWindowVisible(hwnd).as_bool() && !IsIconic(hwnd).as_bool()
            })
        }
        #[cfg(not(windows))]
        {
            window.is_window_active()
        }
    }

    fn renew_pairing_code(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let eligible = self.pairing_visible(window) && self.status["enabled"] == true
            && self.status["running"] == true && !self.shell_loading;
        let valid = pairing_code_valid(&self.status, chrono::Utc::now());
        if self.pairing_refresh.begin(eligible, self.busy, valid, Instant::now()) {
            self.run(Action::Code, window, cx);
        }
    }

    fn refresh_input_colors(&mut self, cx: &mut Context<Self>) {
        let t = self.tokens.theme(self.dark);
        let colors = InputColors {
            background: color(&t.search_background),
            foreground: color(&t.text),
            placeholder: color(&t.text_muted),
            border: color(&t.border),
            selection: color(&t.selected),
            caret: color(&t.accent),
        };
        for input in [&self.alias, &self.port]
            .into_iter()
            .chain(self.codes.values())
        {
            input.update(cx, |input, cx| input.set_colors(colors, cx));
        }
        self.composer.update(cx, |input, cx| {
            let mut colors = colors;
            colors.background = gpui::hsla(0., 0., 0., 0.);
            input.set_colors(colors, cx);
        });
    }

    fn configure_inputs(&mut self, cx: &mut Context<Self>) {
        self.refresh_input_colors(cx);
        for input in [&self.alias, &self.port]
            .into_iter()
            .chain(self.codes.values())
        {
            let read_only = self.exit_pending || self.settings_only && (self.settings_leave_locked || !self.active);
            input.update(cx, |input, cx| {
                input.set_metrics(30., 13., 7., cx);
                input.set_double_click_clears(false);
                input.set_read_only(read_only, cx);
            });
        }
        self.composer.update(cx, |input, cx| {
            input.set_metrics(24., 13., 0., cx);
            input.set_auto_grow(24., 124., cx);
            input.set_external_paste(true, cx);
            input.set_submit_on_enter(true, cx);
            let read_only = self.exit_pending;
            #[cfg(debug_assertions)]
            let read_only = read_only || self.fixture && self.fixture_keyboard_read_only;
            input.set_read_only(read_only, cx);
        });
    }

    pub(super) fn set_theme(&mut self, dark: bool, cx: &mut Context<Self>) {
        self.set_design_tokens(self.tokens.clone(), dark, cx);
    }

    pub(crate) fn set_design_tokens(
        &mut self,
        tokens: DesignTokens,
        dark: bool,
        cx: &mut Context<Self>,
    ) {
        let changed = self.dark != dark || self.tokens.theme(dark) != tokens.theme(dark);
        self.tokens = tokens;
        self.dark = dark;
        if changed {
            self.control_tweens.clear();
            self.refresh_input_colors(cx);
            cx.notify();
        }
    }

    pub(crate) fn set_exit_pending(&mut self, pending: bool, cx: &mut Context<Self>) {
        if self.exit_pending != pending {
            self.picker_lifetime.retire(self.active && !pending);
        }
        self.exit_pending = pending;
        self.sync_history_drop();
        if !self.composer.read(cx).is_composing() {
            self.configure_inputs(cx);
        }
        cx.notify();
    }
    fn sync_identity(&mut self, cx: &Context<Self>) {
        let alias = self.alias.read(cx).text().to_string();
        let port = self.port.read(cx).text().to_string();
        if self.identity_alias != alias || self.identity_port != port {
            self.identity_alias = alias;
            self.identity_port = port;
            self.identity_dirty = true;
            self.identity_revision = self.identity_revision.wrapping_add(1);
        }
    }
    fn sync_composer(&mut self, cx: &Context<Self>) {
        let text = self.composer.read(cx).text().to_string();
        if self.composer_text != text {
            self.composer_text = text;
            self.composer_revision = self.composer_revision.wrapping_add(1);
        }
    }
    fn discard_empty_failed_send(&mut self, cx: &Context<Self>) {
        if self.composer.read(cx).text().is_empty()
            && self.files.is_empty()
            && self.recent_items.is_empty()
            && !self.composer.read(cx).is_composing()
        {
            if let Some(id) = self.failed_send.take() {
                self.writes.acknowledge(id);
            }
        }
    }
    fn restore_failed_send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(failure) = self
            .writes
            .failures()
            .into_iter()
            .rev()
            .find(|failure| matches!(failure.action, Action::Send { .. }))
        {
            if let Action::Send {
                conversation,
                text,
                files,
                ..
            } = failure.action
            {
                self.selected = conversation;
                self.files = files;
                self.recent_items.clear();
                self.composer_text = text.clone();
                self.composer_revision = self.composer_revision.wrapping_add(1);
                self.composer
                    .update(cx, |input, cx| input.set_text(text, window, cx));
                self.failed_send = Some(failure.id);
                self.seen_failures.insert(failure.id);
                self.report_error(failure.error, cx);
            }
        }
    }
    fn retire_shell_read(&mut self) {
        self.shell_request = self.shell_request.wrapping_add(1);
        self.shell_loading = false;
        self.shell_again = false;
    }

    fn report_write_failures(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for failure in self.writes.failures() {
            if !self.seen_failures.insert(failure.id) {
                continue;
            }
            self.report_error(failure.error, cx);
            if matches!(failure.action, Action::Send { .. }) {
                if !self.settings_only && !self.busy
                    && self.composer.read(cx).text().is_empty()
                    && self.files.is_empty()
                    && self.recent_items.is_empty()
                    && !self.composer.read(cx).is_composing()
                {
                    let selected = self.selected.clone();
                    self.restore_failed_send(window, cx);
                    if self.selected != selected {
                        self.retire_chat_read();
                        self.refresh_chat(window, cx, true);
                    }
                }
            } else if !matches!(failure.action, Action::HistorySend { .. }) {
                self.writes.acknowledge(failure.id);
            }
            cx.notify();
        }
    }
    fn retire_chat_read(&mut self) {
        self.message_request = self.message_request.wrapping_add(1);
        self.messages_loading = false;
        self.messages_again = false;
    }
    fn close_overlays(&mut self, cx: &mut Context<Self>) {
        if self.composer.read(cx).is_composing()
            || self.alias.read(cx).is_composing()
            || self.port.read(cx).is_composing()
            || self
                .codes
                .values()
                .any(|input| input.read(cx).is_composing())
        {
            return;
        }
        if self.preview.take().is_none() {
            self.devices = false;
            self.sessions = false;
            self.attach_menu = false;
        }
        cx.notify();
    }

    pub(crate) fn set_embedded(&mut self, embedded: bool, cx: &mut Context<Self>) {
        self.embedded = embedded;
        cx.notify();
    }

    pub(crate) fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.fixture {
            return;
        }
        if self.active == active {
            return;
        }
        self.active = active;
        if active {
            self.pairing_refresh.opened();
        }
        self.incoming_history_session = self.incoming_history_session.wrapping_add(1);
        self.history_drop_hovered = false;
        self.message_scrollbar.cancel();
        self.session_scrollbar.cancel();
        self.device_scrollbar.cancel();
        crate::favorite_drag::clear_target_hwnd(self.hwnd);
        self.lifetime = self.lifetime.wrapping_add(1);
        self.picker_lifetime.retire(active && !self.exit_pending);
        self.shell_loading = false;
        self.messages_loading = false;
        self.shell_again = false;
        self.messages_again = false;
        self.event_scheduled = false;
        self.devices = false;
        self.sessions = false;
        self.attach_menu = false;
        self.preview = None;
        self.image_loading.clear();
        self.sync_history_drop();
        self.configure_inputs(cx);
        if active {
            self.refresh_shell(window, cx, true);
            self.refresh_chat(window, cx, true);
        }
        cx.notify();
    }

    pub(crate) fn on_event(
        &mut self,
        name: &str,
        data: &Value,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.settings_only && name == "lan.share.changed" {
            return;
        }
        if self.active && name == "deviceSyncChanged" {
            if data["enabled"].is_boolean() && data["peers"].is_array() {
                self.retire_shell_read();
                if self.device_sync != *data {
                    self.device_sync = data.clone();
                    cx.notify();
                }
            } else {
                self.refresh_shell(window, cx, false);
            }
            return;
        }
        if !self.active || !matches!(name, "lan.share.changed" | "lan.peers.changed") {
            return;
        }
        let mut changed = false;
        if name == "lan.share.changed" {
            self.messages_again = true;
            if let Some(message) = data.get("message").filter(|message| {
                message["id"].is_string()
                    && message["attachments"].is_array()
                    && message["deliveries"].is_array()
            }) {
                if message["conversationId"] == self.selected {
                    changed = self.upsert_message(message.clone());
                    if changed {
                        self.chat_revision = self.chat_revision.wrapping_add(1);
                    }
                }
            }
        } else {
            self.shell_again = true;
            // A null event is broadcast-lag recovery; ordinary peer heartbeats
            // do not invalidate the conversation's message snapshot.
            if data.is_null() {
                self.messages_again = true;
            }
        }
        if changed {
            cx.notify();
        }
        if self.event_scheduled {
            return;
        }
        self.event_scheduled = true;
        let lifetime = self.lifetime;
        cx.spawn_in(window, async move |this, cx| {
            Timer::after(Duration::from_millis(24)).await;
            let _ = this.update_in(cx, |this, window, cx| {
                if !this.active || this.lifetime != lifetime {
                    return;
                }
                this.event_scheduled = false;
                if this.shell_again && !this.shell_loading {
                    this.shell_again = false;
                    this.refresh_shell(window, cx, false);
                }
                if this.messages_again && !this.messages_loading {
                    this.messages_again = false;
                    this.refresh_chat(window, cx, false);
                }
            });
        })
        .detach();
    }

    fn refresh_shell(&mut self, window: &mut Window, cx: &mut Context<Self>, show_error: bool) {
        if !self.active {
            return;
        }
        if self.shell_loading {
            self.shell_again = true;
            return;
        }
        self.shell_loading = true;
        self.shell_request = self.shell_request.wrapping_add(1);
        let lifetime = self.lifetime;
        let request = self.shell_request;
        let revision = self.identity_revision;
        let host = self.host.clone();
        cx.spawn_in(window, async move |this, cx| {
            let (status, peers, device_sync) = cx
                .background_executor()
                .spawn(async move { (host.lan_status(), rows(host.lan_peers()), host.device_sync_status()) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                if !this.active || this.lifetime != lifetime || this.shell_request != request {
                    return;
                }
                this.shell_loading = false;
                let mut changed = !same_status_presentation(&this.status, &status)
                    || !same_peer_presentation(&this.peers, &peers)
                    || this.device_sync != device_sync;
                let membership_changed = paired_device_ids(&this.peers) != paired_device_ids(&peers);
                if !this.identity_dirty
                    && this.identity_revision == revision
                    && !this.alias.read(cx).is_composing()
                    && !this.port.read(cx).is_composing()
                {
                    let alias = text(&status, "deviceAlias");
                    let port = status["port"].as_u64().unwrap_or(29766).to_string();
                    this.identity_alias = alias.clone();
                    this.identity_port = port.clone();
                    if this.alias.read(cx).text() != alias {
                        this.alias.update(cx, |input, cx| input.set_text(alias, window, cx));
                    }
                    if this.port.read(cx).text() != port {
                        this.port.update(cx, |input, cx| input.set_text(port, window, cx));
                    }
                }
                this.status = status;
                this.device_sync = device_sync;
                this.peers = peers;
                changed |= reconcile_conversation_peers(&mut this.conversations, &this.peers);
                let previous_codes = this.codes.keys().cloned().collect::<BTreeSet<_>>();
                this.ensure_codes(window, cx);
                this.renew_pairing_code(window, cx);
                if previous_codes != this.codes.keys().cloned().collect::<BTreeSet<_>>() {
                    this.configure_inputs(cx);
                }
                if membership_changed {
                    this.refresh_chat(window, cx, false);
                }
                if show_error && !this.status.is_object() {
                    this.error = Some(
                        tr(
                            "读取局域网互传状态失败",
                            "Failed to load LAN sharing status",
                        )
                        .into(),
                    );
                    changed = true;
                }
                if this.shell_again {
                    this.shell_again = false;
                    this.refresh_shell(window, cx, false);
                }
                if changed {
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn refresh_chat(&mut self, window: &mut Window, cx: &mut Context<Self>, show_error: bool) {
        if !self.active || self.exit_pending || self.settings_only {
            return;
        }
        if self.messages_loading {
            self.messages_again = true;
            return;
        }
        self.messages_loading = true;
        #[cfg(debug_assertions)]
        { self.performance.chat_read_starts += 1; }
        self.message_request = self.message_request.wrapping_add(1);
        let stamp = RequestStamp {
            lifetime: self.lifetime,
            request: self.message_request,
            conversation: self.selected.clone(),
        };
        let host = self.host.clone();
        let conversation = stamp.conversation.clone();
        let chat_revision = self.chat_revision;
        cx.spawn_in(window, async move |this, cx| {
            let (conversations, messages) = cx
                .background_executor()
                .spawn(async move {
                    (
                        rows(host.lan_conversations()),
                        host.lan_messages(&json!({"conversationId":conversation,"limit":200})),
                    )
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                if !stamp.is_current(
                    this.active,
                    this.lifetime,
                    this.message_request,
                    &this.selected,
                ) {
                    return;
                }
                this.messages_loading = false;
                let mut changed = false;
                // Keep the in-flight read single. A push received since its
                // start wins over this older snapshot; follow up once below.
                if chat_revision == this.chat_revision {
                  changed = this.conversations != conversations;
                  this.conversations = conversations;
                  changed |= reconcile_conversation_peers(&mut this.conversations, &this.peers);
                  match messages {
                    Ok(messages) => {
                        let messages = rows(messages);
                        let unread = messages.iter().any(|message| {
                            message["direction"] == "incoming" && message["isRead"] != true
                        });
                        changed |= this.replace_messages(messages);
                        if unread {
                            this.mark_read(stamp.clone(), cx);
                        }
                    }
                    Err(error) if show_error => this.report_error(error, cx),
                    Err(_) => {}
                  }
                } else {
                    this.messages_again = true;
                }
                if this.messages_again {
                    this.messages_again = false;
                    this.refresh_chat(window, cx, false);
                }
                if changed {
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn mark_read(&self, stamp: RequestStamp, cx: &mut Context<Self>) {
        if self.exit_pending {
            return;
        }
        let host = self.host.clone();
        let conversation = stamp.conversation.clone();
        let action = Action::Read(conversation);
        let (_, receiver) = self.writes.submit(host, action, None);
        cx.spawn(async move |this, cx| {
            if receiver.await.is_ok_and(|result| result.is_ok()) {
                let _ = this.update(cx, |this, cx| {
                    if !stamp.is_current(
                        this.active,
                        this.lifetime,
                        this.message_request,
                        &this.selected,
                    ) {
                        return;
                    }
                    for conversation in &mut this.conversations {
                        if conversation["id"] == this.selected {
                            conversation["unreadCount"] = json!(0);
                        }
                    }
                    for message in &mut this.messages {
                        if message["direction"] == "incoming" {
                            message["isRead"] = json!(true);
                        }
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn select(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        self.message_scrollbar.cancel();
        self.history_drop_hovered = false;
        self.incoming_history_session = self.incoming_history_session.wrapping_add(1);
        crate::favorite_drag::clear_target_hwnd(self.hwnd);
        if self.selected == id {
            self.sessions = false;
            return;
        }
        if self.composer.read(cx).is_composing() {
            return;
        }
        self.sync_composer(cx);
        self.drafts.insert(
            self.selected.clone(),
            ConversationDraft {
                text: self.composer.read(cx).text().to_string(),
                files: self.files.clone(),
                recent_items: self.recent_items.clone(),
                revision: self.composer_revision,
                failed_send: self.failed_send,
            },
        );
        self.selected = id;
        self.sync_history_drop();
        let draft = self.drafts.remove(&self.selected).unwrap_or_default();
        self.composer.update(cx, |input, cx| {
            input.set_text(draft.text.clone(), window, cx)
        });
        self.composer_text = draft.text;
        self.files = draft.files;
        self.recent_items = draft.recent_items;
        self.failed_send = draft.failed_send;
        self.composer_revision = self.composer_revision.max(draft.revision).wrapping_add(1);
        self.sessions = false;
        self.notice = None;
        self.hovered_controls.clear();
        self.message_request = self.message_request.wrapping_add(1);
        self.messages_loading = false;
        self.messages_again = false;
        self.messages.clear();
        self.message_inputs.clear();
        self.message_layouts.clear();
        self.list.reset(0);
        self.scroll_after_load = true;
        self.refresh_chat(window, cx, true);
        cx.notify();
    }

    fn near_bottom(&self) -> bool {
        self.messages.is_empty()
            || self.list.logical_scroll_top().item_ix >= self.messages.len()
            || self
                .list
                .bounds_for_item(self.messages.len() - 1)
                .is_some_and(|bounds| {
                    bounds.bottom() - self.list.viewport_bounds().bottom() <= px(72.)
                })
    }

    fn replace_messages(&mut self, messages: Vec<Value>) -> bool {
        if messages == self.messages {
            return false;
        }
        let follow = self.scroll_after_load || self.near_bottom();
        let offset = self.list.logical_scroll_top();
        let anchor = self
            .messages
            .get(offset.item_ix)
            .map(|message| text(message, "id"));
        // Layout dimensions depend on body/font/width, not delivery progress.
        // Keep cached text and input entities bounded to the current window.
        retain_message_layouts(&mut self.message_layouts, &self.messages, &messages);
        let visible_ids = messages.iter().filter_map(|message| message["id"].as_str()).collect::<BTreeSet<_>>();
        self.message_inputs.retain(|id, _| visible_ids.contains(id.as_str()));
        self.messages = messages;
        self.list.reset(self.messages.len());
        if !follow {
            let item_ix = anchor
                .and_then(|anchor| {
                    self.messages
                        .iter()
                        .position(|message| message["id"] == anchor)
                })
                .unwrap_or(offset.item_ix.min(self.messages.len().saturating_sub(1)));
            self.list.scroll_to(ListOffset {
                item_ix,
                offset_in_item: offset.offset_in_item,
            });
        }
        self.scroll_after_load = false;
        true
    }

    fn upsert_message(&mut self, message: Value) -> bool {
        let id = text(&message, "id");
        let index = self.messages.iter().position(|old| old["id"] == id);
        if index.is_some_and(|index| self.messages[index] == message) {
            return false;
        }
        let follow = self.scroll_after_load || self.near_bottom();
        let offset = self.list.logical_scroll_top();
        let anchor = self.messages.get(offset.item_ix).map(|message| text(message, "id"));
        if index.is_some_and(|index| self.messages[index]["text"] != message["text"]) {
            self.message_layouts.remove(&id);
        }
        upsert(&mut self.messages, message);
        let visible_ids = self.messages.iter().filter_map(|message| message["id"].as_str()).collect::<BTreeSet<_>>();
        self.message_inputs.retain(|id, _| visible_ids.contains(id.as_str()));
        self.message_layouts.retain(|id, _| visible_ids.contains(id.as_str()));
        self.list.reset(self.messages.len());
        if !follow {
            let item_ix = anchor.and_then(|anchor| self.messages.iter().position(|message| message["id"] == anchor))
                .unwrap_or(offset.item_ix.min(self.messages.len().saturating_sub(1)));
            self.list.scroll_to(ListOffset { item_ix, offset_in_item: offset.offset_in_item });
        }
        self.scroll_after_load = false;
        true
    }

    fn ensure_codes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ids = self
            .peers
            .iter()
            .map(|peer| text(peer, "deviceId"))
            .collect::<BTreeSet<_>>();
        self.codes.retain(|id, _| ids.contains(id));
        self.code_subscriptions.retain(|id, _| ids.contains(id));
        for id in ids {
            if self.codes.contains_key(&id) {
                continue;
            }
            let input = cx.new(|cx| {
                let mut input = SearchInput::new(window, cx);
                input.set_placeholder(tr("6 位配对码", "6-digit pairing code"), cx);
                input.set_double_click_clears(false);
                input
            });
            let subscription = cx.subscribe_in(&input, window, |this, input, event, window, cx| {
                if matches!(event, SearchEvent::Escape) {
                    this.close_overlays(cx);
                    return;
                }
                if matches!(event, SearchEvent::Changed(_)) && !input.read(cx).is_composing() {
                    let value = input.read(cx).text().to_string();
                    let filtered = pairing_code(&value);
                    if filtered != value {
                        input.update(cx, |input, cx| input.set_text(filtered, window, cx));
                    }
                    cx.notify();
                }
            });
            self.code_subscriptions.insert(id.clone(), subscription);
            self.codes.insert(id, input);
        }
    }

    fn config(&self, enabled: bool, cx: &Context<Self>) -> Value {
        // parseInt parity: an invalid draft retains the current core port.
        let typed = self.port.read(cx).text().trim();
        let digits = typed
            .strip_prefix('+')
            .unwrap_or(typed)
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>();
        let port = digits
            .parse::<u64>()
            .unwrap_or_else(|_| self.status["port"].as_u64().unwrap_or(29766));
        json!({"enabled":enabled,"deviceAlias":self.alias.read(cx).text().chars().take(64).collect::<String>(),"port":port,
            "inboxFolder":self.status["inboxFolder"],"maxIncomingFileSizeBytes":self.status["maxIncomingFileSizeBytes"],"maxParallelDeliveries":self.status["maxParallelDeliveries"]})
    }

    fn run(&mut self, action: Action, window: &mut Window, cx: &mut Context<Self>) {
        let replaces = matches!(action, Action::Send { .. })
            .then_some(self.failed_send)
            .flatten();
        self.run_replacing(action, replaces, window, cx);
    }
    fn run_replacing(
        &mut self,
        action: Action,
        replaces: Option<u64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.fixture || !self.active || self.busy || self.exit_pending || self.settings_leave_locked {
            return;
        }
        self.sync_identity(cx);
        self.busy = true;
        self.error = None;
        self.action = self.action.wrapping_add(1);
        let operation = self.action;
        let lifetime = self.lifetime;
        let identity_revision = self.identity_revision;
        let host = self.host.clone();
        let picker = matches!(action, Action::Pick(_)).then(|| PickerLease {
            state: self.picker_lifetime.clone(),
            generation: self.picker_lifetime.generation.load(Ordering::SeqCst),
            hwnd: self.hwnd,
        });
        // Even an image picker is tracked: its accepted selection stages an
        // image. The worker rechecks its lease before opening the real modal.
        let (write_id, receiver) = if let Action::Pick(image) = &action {
            let image = *image;
            let lease = picker.clone().unwrap();
            self.writes.enqueue(
                action.clone(),
                replaces,
                Box::new(move || lease.execute(&host, image)),
            )
        } else {
            self.writes.submit(host, action.clone(), replaces)
        };
        let cleanup_host = self.host.clone();
        let cleanup_queue = self.writes.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = receiver.await.unwrap_or_else(|_| {
                Err(tr("局域网操作线程不可用", "LAN operation worker unavailable").into())
            });
            let owned_reply = if matches!(action, Action::ComposeRecent | Action::ComposeClipboard)
            {
                result
                    .as_ref()
                    .ok()
                    .map(|value| {
                        values(value, "ownedFilePaths")
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            let orphaned_reply = owned_reply.clone();
            let update = this.update_in(cx, |this, window, cx| {
                if this.settings_only {
                    record_settings_write_result(&mut this.settings_write_errors, &action, &result);
                }
                if matches!(action, Action::Code) {
                    this.pairing_refresh.completed(result.is_ok(), Instant::now());
                }
                if this.action != operation {
                    this.discard_compose_files(owned_reply);
                    return;
                }
                this.busy = false;
                let presentation_current = this.active && this.lifetime == lifetime;
                if !presentation_current
                    && matches!(action, Action::ComposeRecent | Action::ComposeClipboard)
                    && result.is_err()
                {
                    this.paste_target = None;
                    this.writes.acknowledge(write_id);
                    cx.notify();
                    return;
                }
                if matches!(action, Action::Pick(_))
                    && (!presentation_current
                        || this.exit_pending
                        || !picker
                            .as_ref()
                            .is_some_and(|lease| lease.is_current(&this.host)))
                {
                    // Clearing busy belongs to the accepted operation; stale
                    // picker results and errors cannot touch a retired page.
                    this.writes.acknowledge(write_id);
                    cx.notify();
                    return;
                }
                match result {
                    Err(error) => {
                        if presentation_current || !matches!(action, Action::Code) {
                            this.report_error(error, cx);
                        }
                        this.seen_failures.insert(write_id);
                        if matches!(action, Action::Send { .. }) {
                            this.failed_send = Some(write_id);
                        } else if !matches!(action, Action::HistorySend { .. }) {
                            this.writes.acknowledge(write_id);
                        }
                    }
                    Ok(value) => match action {
                        Action::DeviceSync(_) => {
                            this.retire_shell_read();
                            this.device_sync = value;
                            this.refresh_shell(window, cx, false);
                        }
                        Action::Config(params) => {
                            this.retire_shell_read();
                            let became_enabled = this.status["enabled"] != true && value["enabled"] == true;
                            this.status = value;
                            if became_enabled {
                                this.pairing_refresh.opened();
                            }
                            if this.identity_revision == identity_revision
                                && (params.get("deviceAlias").is_some()
                                    || params.get("port").is_some())
                                && !this.alias.read(cx).is_composing()
                                && !this.port.read(cx).is_composing()
                            {
                                this.identity_dirty = false;
                                let alias = text(&this.status, "deviceAlias");
                                let port =
                                    this.status["port"].as_u64().unwrap_or(29766).to_string();
                                this.identity_alias = alias.clone();
                                this.identity_port = port.clone();
                                this.alias
                                    .update(cx, |input, cx| input.set_text(alias, window, cx));
                                this.port
                                    .update(cx, |input, cx| input.set_text(port, window, cx));
                            }
                            this.refresh_shell(window, cx, false);
                            this.configure_inputs(cx);
                        }
                        Action::Code => {
                            if presentation_current {
                                this.retire_shell_read();
                                this.status["pairingCode"] = value;
                            }
                        }
                        Action::Scan => {
                            if !presentation_current {
                                return;
                            }
                            this.retire_shell_read();
                            this.peers = rows(value);
                            this.ensure_codes(window, cx);
                            this.configure_inputs(cx);
                        }
                        Action::Pair(ref id, _) | Action::Forget(ref id) => {
                            if matches!(action, Action::Pair(_, _)) {
                                this.pair_target = None;
                                if !this.settings_only {
                                    this.devices = false;
                                    this.select(format!("dm:{id}"), window, cx);
                                }
                            }
                            this.forget_confirm = None;
                            this.retire_shell_read();
                            this.retire_chat_read();
                            this.refresh_shell(window, cx, false);
                            this.refresh_chat(window, cx, false);
                        }
                        Action::Send {
                            conversation,
                            revision,
                            text,
                            files,
                        } => {
                            this.retire_chat_read();
                            if presentation_current && this.selected == conversation {
                                this.upsert_message(value);
                            }
                            if presentation_current
                                && this.selected == conversation
                                && send_reply_matches(
                                    revision,
                                    this.composer_revision,
                                    &text,
                                    &compose_text(
                                        this.composer.read(cx).text(),
                                        &this.recent_items,
                                    ),
                                    &files,
                                    &this.files,
                                    this.composer.read(cx).is_composing(),
                                )
                            {
                                this.composer.update(cx, |input, cx| {
                                    input.set_text(String::new(), window, cx)
                                });
                                this.composer_text.clear();
                                this.failed_send = None;
                                this.files.clear();
                                this.recent_items.clear();
                                this.composer_revision = this.composer_revision.wrapping_add(1);
                            } else if let Some(draft) = this.drafts.get_mut(&conversation) {
                                if send_reply_matches(
                                    revision,
                                    draft.revision,
                                    &text,
                                    &compose_text(&draft.text, &draft.recent_items),
                                    &files,
                                    &draft.files,
                                    false,
                                ) {
                                    *draft = ConversationDraft::default();
                                }
                            }
                            // A later edit can retain the original attachment in
                            // this or another draft. Release only unreferenced
                            // clipboard-owned sources after the send is staged.
                            let released = files
                                .iter()
                                .filter(|path| {
                                    !this.files.contains(path)
                                        && !this
                                            .drafts
                                            .values()
                                            .any(|draft| draft.files.contains(path))
                                })
                                .cloned()
                                .collect::<Vec<_>>();
                            for path in &released {
                                this.owned_files.remove(path);
                            }
                            this.discard_compose_files(released);
                            this.refresh_chat(window, cx, false);
                        }
                        Action::HistorySend { .. } | Action::Retry(_) | Action::RetryHistory { .. } | Action::Cancel(_) => {
                            this.retire_chat_read();
                            this.refresh_chat(window, cx, false)
                        }
                        Action::Pick(image) => {
                            if !presentation_current {
                                return;
                            }
                            this.attach_menu = false;
                            if value["cancelled"] != true {
                                let paths = if image {
                                    vec![text(&value, "path")]
                                } else {
                                    values(&value, "paths")
                                        .iter()
                                        .filter_map(Value::as_str)
                                        .map(str::to_string)
                                        .collect()
                                };
                                for path in paths.into_iter().filter(|path| !path.is_empty()) {
                                    if !this.files.contains(&path) {
                                        this.invalidate_image(&path);
                                        this.files.push(path);
                                    }
                                }
                                this.composer_revision = this.composer_revision.wrapping_add(1);
                            }
                        }
                        Action::Open(_) | Action::Reveal(_) | Action::Read(_) => {}
                        Action::ComposeRecent | Action::ComposeClipboard => {
                            let owned = values(&value, "ownedFilePaths")
                                .iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect::<Vec<_>>();
                            if let Some((target, conversation, requested_lifetime)) =
                                this.paste_target.take()
                            {
                                if !presentation_current
                                    || this.selected != conversation
                                    || this.lifetime != requested_lifetime
                                {
                                    this.discard_compose_files(owned);
                                    cx.notify();
                                    return;
                                }
                                let recent = matches!(action, Action::ComposeRecent);
                                let focused = recent
                                    || (window.is_window_active()
                                        && this
                                            .composer
                                            .read(cx)
                                            .focus_handle()
                                            .is_focused(window));
                                let duplicate = recent
                                    && this
                                        .recent_items
                                        .iter()
                                        .any(|item| item.id == text(&value, "id"));
                                let inserted = focused
                                    && !duplicate
                                    && this.composer.update(cx, |input, cx| {
                                        input.apply_external_paste(
                                            &target,
                                            if recent {
                                                ""
                                            } else {
                                                value["text"].as_str().unwrap_or_default()
                                            },
                                            window,
                                            cx,
                                        )
                                    });
                                if inserted {
                                    if recent {
                                        this.recent_items.push(RecentComposeItem {
                                            id: text(&value, "id"),
                                            label: text(&value, "label"),
                                            kind: text(&value, "kind"),
                                            text: text(&value, "text"),
                                            files: values(&value, "filePaths")
                                                .iter()
                                                .filter_map(Value::as_str)
                                                .map(str::to_string)
                                                .collect(),
                                        });
                                    }
                                    this.owned_files.extend(owned);
                                    for path in
                                        values(&value, "filePaths").iter().filter_map(Value::as_str)
                                    {
                                        if !path.is_empty()
                                            && !this.files.iter().any(|old| old == path)
                                        {
                                            this.files.push(path.to_string());
                                            this.invalidate_image(path);
                                        }
                                    }
                                    this.sync_composer(cx);
                                    this.composer_revision = this.composer_revision.wrapping_add(1);
                                    this.notice =
                                        Some(if matches!(action, Action::ComposeRecent) {
                                            tr(
                                                "已加入最近复制的内容",
                                                "Recent copied content added",
                                            )
                                            .into()
                                        } else {
                                            tr("已粘贴到当前草稿", "Pasted into the current draft")
                                                .into()
                                        });
                                } else {
                                    this.discard_compose_files(owned);
                                }
                            } else {
                                this.discard_compose_files(owned);
                            }
                        }
                        Action::DiscardCompose(_) | Action::DiscardAllCompose => {}
                    },
                }
                this.renew_pairing_code(window, cx);
                cx.notify();
            });
            if update.is_err() && !orphaned_reply.is_empty() {
                let (_, reply) = cleanup_queue.submit(
                    cleanup_host,
                    Action::DiscardCompose(orphaned_reply),
                    None,
                );
                drop(reply);
            }
        })
        .detach();
        cx.notify();
    }

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.status["enabled"] != true
            || self.status["running"] != true
            || self.busy
            || self.exit_pending
            || self.composer.read(cx).is_composing()
            || !self.has_target()
        {
            return;
        }
        self.sync_composer(cx);
        let text = compose_text(self.composer.read(cx).text(), &self.recent_items);
        if text.trim().is_empty() && self.files.is_empty() {
            return;
        }
        self.run(
            Action::Send {
                conversation: self.selected.clone(),
                text,
                files: self.files.clone(),
                revision: self.composer_revision,
            },
            window,
            cx,
        );
    }

    fn image(
        &mut self,
        path: String,
        revision: String,
        purpose: ImagePurpose,
        cx: &mut Context<Self>,
    ) -> Option<(Arc<Image>, f32, f32)> {
        self.image_clock = self.image_clock.wrapping_add(1);
        let key = purpose.key(&path);
        if self
            .images
            .get(&key)
            .is_some_and(|entry| entry.revision != revision)
        {
            self.images.remove(&key);
            self.image_loading.remove(&key);
        }
        let mut current = None;
        let mut previous = None;
        if let Some(entry) = self.images.get_mut(&key) {
            entry.used = self.image_clock;
            current = entry
                .bitmap
                .as_ref()
                .map(|bitmap| (bitmap.image.clone(), bitmap.width, bitmap.height));
            if entry.checked.elapsed() < Duration::from_secs(5) {
                return current;
            }
            if entry.bitmap.is_some() {
                previous = entry.version;
            }
        }
        if self.image_loading.len() >= 4 || self.image_loading.contains_key(&key) {
            return current;
        }
        self.image_request = self.image_request.wrapping_add(1);
        let request = self.image_request;
        self.image_loading.insert(key.clone(), request);
        let host = self.host.clone();
        let lifetime = self.lifetime;
        let requested_path = path.clone();
        cx.spawn(async move |this, cx| {
            let bitmap = cx
                .background_executor()
                .spawn(async move { load_image(&host, &requested_path, previous, purpose) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.lifetime != lifetime
                    || (!this.active && !this.fixture)
                    || this.image_loading.get(&key) != Some(&request)
                {
                    return;
                }
                this.image_loading.remove(&key);
                // A successful file-version check does not change the bitmap
                // or row geometry. Preserve measured rows and stay idle.
                if matches!(bitmap, ImageLoad::Unchanged)
                    || (matches!(bitmap, ImageLoad::Failed)
                        && this.images.get(&key).is_some_and(|entry| entry.bitmap.is_none())) {
                    if let Some(entry) = this.images.get_mut(&key) {
                        entry.checked = Instant::now();
                    }
                    return;
                }
                match bitmap {
                    ImageLoad::Unchanged => {
                        if let Some(entry) = this.images.get_mut(&key) {
                            entry.checked = Instant::now();
                        }
                    }
                    ImageLoad::Loaded(bitmap, version) => {
                        this.images.insert(
                            key.clone(),
                            CachedImage {
                                bitmap: Some(bitmap),
                                used: this.image_clock,
                                version: Some(version),
                                revision,
                                checked: Instant::now(),
                            },
                        );
                    }
                    ImageLoad::Failed => {
                        this.images.insert(
                            key.clone(),
                            CachedImage {
                                bitmap: None,
                                used: this.image_clock,
                                version: None,
                                revision,
                                checked: Instant::now(),
                            },
                        );
                    }
                }
                let offset = this.list.logical_scroll_top();
                let affects_message = this.messages.iter().any(|message| {
                    message["attachments"].as_array().is_some_and(|attachments|
                        attachments.iter().any(|attachment| attachment["localPath"] == path))
                });
                if affects_message {
                    #[cfg(debug_assertions)]
                    { this.performance.image_row_invalidations += 1; }
                    // Recreate measured rows after async image readiness. A
                    // same-count splice can leave the old placeholder in a
                    // cached virtual-list layout until another window resize.
                    this.list.reset(this.messages.len());
                    this.list.scroll_to(offset);
                }
                while this.images.len() > 256
                    || this
                        .images
                        .values()
                        .filter_map(|entry| entry.bitmap.as_ref())
                        .map(|bitmap| bitmap.allocation)
                        .sum::<usize>()
                        > 32 * 1024 * 1024
                {
                    let oldest = this
                        .images
                        .iter()
                        .min_by_key(|(_, entry)| entry.used)
                        .map(|(path, _)| path.clone());
                    if let Some(path) = oldest {
                        this.images.remove(&path);
                    } else {
                        break;
                    }
                }
                cx.notify();
            });
        })
        .detach();
        current
    }

    fn invalidate_image(&mut self, path: &str) {
        for purpose in [
            ImagePurpose::Chip,
            ImagePurpose::Attachment,
            ImagePurpose::Preview,
        ] {
            let key = purpose.key(path);
            self.images.remove(&key);
            self.image_loading.remove(&key);
        }
    }

    fn button(
        &self,
        id: impl Into<SharedString>,
        label: impl Into<SharedString>,
        enabled: bool,
        t: &ThemeTokens,
    ) -> gpui::Stateful<gpui::Div> {
        let hover = color(&t.hover);
        rsx! {
            <div id={id.into()} px={px(8.)} py={px(5.)} rounded={px(5.)} border_1
                border_color={color(&t.border)} bg={color(&t.secondary)}
                text_color={color(&t.text_secondary)} cursor_pointer />
        }
            .opacity(if enabled { 1. } else { 0.45 })
            .hover(move |style| style.bg(hover))
            .child(label.into())
    }

    fn key_down(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        if event.keystroke.key == "escape"
            && !event.is_held
            && !self.composer.read(cx).is_composing()
            && !self.alias.read(cx).is_composing()
            && !self.port.read(cx).is_composing()
            && !self
                .codes
                .values()
                .any(|input| input.read(cx).is_composing())
        {
            self.close_overlays(cx);
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn dot(&self, online: bool, t: &ThemeTokens) -> AnyElement {
        div()
            .size(px(6.))
            .rounded_full()
            .flex_none()
            .bg(if online {
                motion::mix_hsla(color(&t.background), color(&t.text), 0.6)
            } else {
                color(&t.text_muted)
            })
            .into_any_element()
    }

    fn preview_view(
        &mut self,
        attachment: Value,
        t: &ThemeTokens,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = text(&attachment, "id");
        let open_id = id.clone();
        let bitmap = self.image(
            text(&attachment, "localPath"),
            text(&attachment, "sha256"),
            ImagePurpose::Preview,
            cx,
        );
        let mut panel = rsx! {
            <div w={px(720.)} max_w_full max_h_full p={px(12.)} rounded={px(9.)} border_1
                border_color={color(&t.border)} bg={color(&t.surface)} flex flex_col
                gap={px(10.)} />
        }
            .track_focus(&self.overlay_focus)
            .occlude()
            .on_any_mouse_down(|_, _, cx| cx.stop_propagation())
            .child(
                rsx! {
                    <div flex items_center justify_between gap={px(8.)}>
                        <div flex_1 min_w_0 flex flex_col gap={px(3.)}>
                            {text(&attachment, "name")}
                            <div text_size={px(11.)} text_color={color(&t.text_muted)}>
                                {format_bytes(
                                        attachment["sizeBytes"].as_u64().unwrap_or(0),
                                    )}
                            </div>
                        </div>
                    </div>
                }
                    .child(self.control(
                        "overlay:preview-close",
                        "×",
                        true,
                        t,
                        window,
                        cx,
                        |this, window, cx| this.dismiss_preview(window, cx),
                    )),
            );
        panel = panel.child(
            div()
                .id("lan-preview-image-stage")
                .min_h_0()
                .max_h(px(460.))
                .overflow_y_scroll()
                .flex()
                .items_center()
                .justify_center()
                .child(if let Some((image, width, height)) = bitmap {
                    let scale = (680. / width).min(420. / height).min(1.);
                    img(image)
                        .w(px(width * scale))
                        .h(px(height * scale))
                        .max_w_full()
                        .object_fit(gpui::ObjectFit::Contain)
                        .into_any_element()
                } else {
                    rsx! {
                        <div p={px(30.)} text_color={color(&t.text_muted)}>
                            {tr(
                            "图片预览正在加载或不可用",
                            "Image preview is loading or unavailable",
                        )}
                        </div>
                    }
                        .into_any_element()
                }),
        );
        panel = panel.child(
            rsx! {
                <div flex flex_wrap justify_end gap={px(6.)} />
            }
                .child(self.control(
                    "overlay:preview-open",
                    tr("用系统打开", "Open with system"),
                    !self.busy,
                    t,
                    window,
                    cx,
                    move |this, window, cx| this.run(Action::Open(open_id.clone()), window, cx),
                ))
                .child(self.control(
                    "overlay:preview-reveal",
                    tr("定位", "Reveal"),
                    !self.busy,
                    t,
                    window,
                    cx,
                    move |this, window, cx| this.run(Action::Reveal(id.clone()), window, cx),
                ))
                .child(self.control(
                    "overlay:preview-done",
                    tr("关闭", "Close"),
                    true,
                    t,
                    window,
                    cx,
                    |this, window, cx| this.dismiss_preview(window, cx),
                )),
        );
        div()
            .absolute()
            .inset_0()
            .p(px(16.))
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::rgba(0x00000088))
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| { cx.stop_propagation(); this.dismiss_preview(window, cx); }),
            )
            .child(panel)
            .into_any_element()
    }
}

#[cfg(test)]
mod model_tests {
    #[test]
    fn settings_persistence_failures_survive_unrelated_success_and_ignore_automatic_codes() {
        let mut errors = BTreeMap::new();
        let failure = Err("Could not save".to_string());
        let success = Ok(Value::Null);
        record_settings_write_result(&mut errors, &Action::Config(json!({"deviceAlias":"Desk"})), &failure);
        record_settings_write_result(&mut errors, &Action::Pair("peer-a".into(), "123456".into()), &failure);
        record_settings_write_result(&mut errors, &Action::Config(json!({"port":29766})), &success);
        assert_eq!(errors.len(), 2, "another field succeeding cannot discard a failed setting");
        record_settings_write_result(&mut errors, &Action::Code, &failure);
        record_settings_write_result(&mut errors, &Action::Code, &success);
        assert_eq!(errors.len(), 2, "automatic pairing-code results never affect close gates");
        record_settings_write_result(&mut errors, &Action::Config(json!({"deviceAlias":"Desk"})), &success);
        assert_eq!(errors.len(), 1);
        record_settings_write_result(&mut errors, &Action::Pair("peer-a".into(), "123456".into()), &success);
        assert!(errors.is_empty(), "the successful retry resolves that pairing failure");
    }

    #[test]
    fn pairing_refresh_opens_renews_and_pauses_without_concurrent_requests() {
        let now = Instant::now();
        let mut refresh = PairingCodeRefresh::default();
        refresh.opened();
        assert!(!refresh.begin(false, false, true, now), "hidden or disabled sessions stay idle");
        assert!(!refresh.begin(true, true, true, now), "other accepted work finishes first");
        assert!(refresh.begin(true, false, true, now), "opening presents a fresh code");
        assert!(!refresh.begin(true, false, false, now), "one renewal may be in flight");
        refresh.opened();
        assert!(!refresh.begin(true, false, false, now), "reopening retains the in-flight lease");
        refresh.completed(true, now);
        assert!(refresh.begin(true, false, true, now), "a reopened session gets its own fresh code");
        refresh.completed(true, now);
        assert!(!refresh.begin(true, false, true, now), "the valid code remains stable while visible");
        assert!(!refresh.begin(false, false, false, now), "expiry while hidden does not write");
        assert!(refresh.begin(true, false, false, now), "visibility restoration renews the expired code");
    }

    #[test]
    fn pairing_refresh_failure_waits_before_retrying_and_reopen_retries_immediately() {
        let now = Instant::now();
        let mut refresh = PairingCodeRefresh::default();
        refresh.opened();
        assert!(refresh.begin(true, false, false, now));
        refresh.completed(false, now);
        assert!(!refresh.begin(true, false, false, now + Duration::from_secs(1)));
        assert!(!refresh.begin(true, false, false, now + Duration::from_secs(14)));
        assert!(refresh.begin(true, false, false, now + Duration::from_secs(15)));
        refresh.completed(false, now + Duration::from_secs(15));
        refresh.opened();
        assert!(refresh.begin(true, false, false, now + Duration::from_secs(16)));
    }

    #[test]
    fn pairing_refresh_uses_actual_expiry_and_rejects_exhausted_or_malformed_codes() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-23T12:00:00Z").unwrap().with_timezone(&chrono::Utc);
        let mut status = json!({"pairingCode":{"code":"000007","attemptsRemaining":5,
            "expiresAtUtc":"2026-10-23T12:00:00.001Z"}});
        assert!(pairing_code_valid(&status, now));
        assert!(!pairing_code_valid(&status, now + chrono::Duration::milliseconds(1)));
        status["pairingCode"]["attemptsRemaining"] = json!(0);
        assert!(!pairing_code_valid(&status, now));
        status["pairingCode"]["attemptsRemaining"] = json!(5);
        status["pairingCode"]["code"] = json!("１２３４５６");
        assert!(!pairing_code_valid(&status, now));
        status["pairingCode"]["code"] = json!("12345");
        assert!(!pairing_code_valid(&status, now));
        status["pairingCode"]["code"] = json!("123456");
        status["pairingCode"]["expiresAtUtc"] = json!("invalid");
        assert!(!pairing_code_valid(&status, now));
    }

    #[test]
    fn clipboard_only_peer_remains_paired_but_is_not_a_manual_send_target() {
        let phone = serde_json::json!({"deviceId":"phone","paired":true,"online":true,
            "deviceKind":"phone","supportsManualTransfer":false,"clipboardTransport":"pull"});
        let desktop = serde_json::json!({"deviceId":"desktop","paired":true,"online":false});
        assert!(!super::conversation_has_target(&[phone.clone()], "all"));
        assert!(!super::conversation_has_target(&[phone.clone()], "dm:phone"));
        assert!(super::paired_device_ids(&[phone.clone()]).contains("phone"));
        assert!(super::conversation_has_target(&[phone.clone(), desktop], "all"));
        let mut upgraded = phone.clone();
        upgraded["supportsManualTransfer"] = serde_json::json!(true);
        assert!(!super::same_peer_presentation(&[phone], &[upgraded]));
    }
    use super::*;

    #[test]
    fn status_expiry_rounding_does_not_redraw_but_real_configuration_and_pairing_changes_do() {
        let status = json!({"enabled":true,"port":29766,"pairingCode":{
            "code":"000000","attemptsRemaining":5,"expiresAtUtc":"2026-10-18T00:00:00.000001Z"}});
        let mut next = status.clone();
        next["pairingCode"]["expiresAtUtc"] = json!("2026-10-18T00:00:00.000002Z");
        assert!(same_status_presentation(&status, &next));
        next["pairingCode"]["code"] = json!("123456");
        assert!(!same_status_presentation(&status, &next));
        next["pairingCode"]["code"] = json!("000000");
        next["pairingCode"]["attemptsRemaining"] = json!(4);
        assert!(!same_status_presentation(&status, &next));
        next = status.clone();
        next["port"] = json!(29767);
        assert!(!same_status_presentation(&status, &next));
        next = status.clone();
        next["futureField"] = json!(true);
        assert!(!same_status_presentation(&status, &next));
    }

    #[test]
    fn heartbeat_timestamps_do_not_redraw_peer_controls_but_visible_changes_do() {
        let peers = vec![json!({"deviceId":"p","alias":"设备","paired":true,
            "online":true,"deviceKind":"laptop","lastSeenAtUtc":"before"})];
        let mut next = peers.clone();
        next[0]["lastSeenAtUtc"] = json!("after");
        assert!(same_peer_presentation(&peers, &next));
        assert_eq!(paired_device_ids(&peers), paired_device_ids(&next));
        for (key, value) in [("alias", json!("新名称")), ("online", json!(false)),
            ("paired", json!(false)), ("deviceKind", json!("phone")), ("pairingAvailable", json!(false))] {
            let mut changed = next.clone();
            changed[0][key] = value;
            assert!(!same_peer_presentation(&peers, &changed), "{key}");
        }
        next[0]["paired"] = json!(false);
        assert_ne!(paired_device_ids(&peers), paired_device_ids(&next));
    }

    #[test]
    fn peer_refresh_updates_conversation_metadata_without_changing_message_summary() {
        let mut conversations = vec![json!({"id":"all","online":false,"unreadCount":3}),
            json!({"id":"dm:p","title":"旧名称","online":false,"lastMessage":"保留正文摘要","unreadCount":2}),
            json!({"id":"dm:forgotten","title":"旧设备","online":true})];
        let peers = vec![json!({"deviceId":"p","alias":"新名称","paired":true,
            "online":true,"deviceKind":"phone","detectedDeviceKind":"laptop","deviceKindOverride":"phone"})];
        assert!(reconcile_conversation_peers(&mut conversations, &peers));
        assert_eq!(conversations[0]["online"], true);
        assert_eq!(conversations[1]["title"], "新名称");
        assert_eq!(conversations[1]["deviceKind"], "phone");
        assert_eq!(conversations[1]["lastMessage"], "保留正文摘要");
        assert_eq!(conversations[1]["unreadCount"], 2);
        assert_eq!(conversations[2]["online"], false);
        assert_eq!(conversations[2]["title"], "旧设备");
        assert!(!reconcile_conversation_peers(&mut conversations, &peers));
    }

    #[test]
    fn message_dimensions_survive_progress_but_not_body_changes_or_removal() {
        let previous = vec![json!({"id":"a","text":"第一条","status":"sending"}),
            json!({"id":"b","text":"第二条"}), json!({"id":"c","text":"移除"})];
        let next = vec![json!({"id":"a","text":"第一条","status":"delivered"}),
            json!({"id":"b","text":"已修改正文"}), json!({"id":"d","text":"新消息"})];
        let mut layouts = ["a", "b", "c"].into_iter().map(|id| (id.to_owned(), MessageTextLayout {
            max_width: 320., font_family: "fixture".into(), scale_factor: 1., width: 150., height: 19.,
        })).collect();
        retain_message_layouts(&mut layouts, &previous, &next);
        assert_eq!(layouts.keys().map(String::as_str).collect::<Vec<_>>(), ["a"]);
        assert_eq!(layouts["a"].width, 150.);
    }

    #[test]
    fn short_message_text_fits_after_physical_pixel_rounding() {
        // Actual Microsoft YaHei UI measurements, including the reported
        // "测试123" case that used to receive only 48.8 DIP at 125% DPI.
        for measured in [13., 48.870605, 48.55957, 53.54883, 122.234] {
            for scale in [0.75, 1., 1.25, 1.5, 2.] {
                for inset in [20., 35.] {
                    let (body, wrap) = message_text_widths(measured, inset, 330., scale);
                    for offset in 0..16 {
                        let origin = offset as f32 / 16.;
                        let left = ((origin + 10.) * scale).round();
                        let right = ((origin + body - (inset - 10.)) * scale).round();
                        let actual = (right - left) / scale;
                        assert!(actual + 0.0001 >= measured, "{measured}/{scale}/{origin}");
                        assert!(wrap <= actual + 0.0001);
                    }
                }
            }
        }
        let (body, wrap) = message_text_widths(1000., 35., 260., 1.25);
        assert_eq!(body, 260.);
        assert!(wrap > 0. && wrap < body - 35.);
    }

    #[test]
    fn direct_conversation_ids_resolve_to_the_paired_device_identity() {
        let peers = vec![
            json!({"deviceId":"peer-device","paired":true,"online":false,"deviceKind":"laptop"}),
            json!({"deviceId":"unpaired","paired":false,"online":true,"deviceKind":"phone"}),
        ];
        assert_eq!(conversation_peer_id("dm:peer-device"), "peer-device");
        assert_eq!(conversation_peer_id("peer-device"), "peer-device");
        assert_eq!(conversation_peer_id("all"), "all");
        assert_eq!(
            conversation_peer(&peers, "dm:peer-device").unwrap()["deviceKind"],
            "laptop"
        );
        assert!(
            conversation_has_target(&peers, "dm:peer-device"),
            "offline paired devices stay sendable"
        );
        assert!(conversation_has_target(&peers, "all"));
        assert!(!conversation_has_target(&peers, "dm:unpaired"));
        assert!(!conversation_has_target(&peers, "dm:missing"));
        assert!(!conversation_has_target(&[], "all"));
    }

    #[test]
    fn recent_items_remain_independent_until_the_draft_is_sent() {
        let item = RecentComposeItem {
            id: "recent-a".into(),
            label: "preview".into(),
            kind: "text".into(),
            text: "最近复制\n第二行".into(),
            files: Vec::new(),
        };
        let mut draft = ConversationDraft {
            text: "草稿正文".into(),
            recent_items: vec![item],
            revision: 8,
            ..Default::default()
        };
        assert_eq!(
            compose_text(&draft.text, &draft.recent_items),
            "草稿正文\n\n最近复制\n第二行"
        );
        let combined = compose_text(&draft.text, &draft.recent_items);
        assert!(send_reply_matches(
            8,
            8,
            &combined,
            &combined,
            &[],
            &[],
            false
        ));
        draft.recent_items.clear();
        draft.revision += 1;
        assert_eq!(draft.text, "草稿正文");
        assert!(!send_reply_matches(
            8,
            draft.revision,
            &combined,
            &compose_text(&draft.text, &draft.recent_items),
            &[],
            &[],
            false
        ));
    }

    #[test]
    fn failed_draft_reads_with_retired_observers_do_not_block_exit() {
        let queue = ActionQueue::start(Weak::new());
        for action in [Action::ComposeRecent, Action::ComposeClipboard] {
            let (_, reply) = queue.enqueue(
                action,
                None,
                Box::new(|| Err("clipboard unavailable".into())),
            );
            drop(reply);
        }
        assert!(queue.barrier().blocking_recv().unwrap().is_ok());
        assert!(queue.failures().is_empty());
    }

    #[test]
    fn cleanup_queued_before_send_failure_preserves_sources_until_ack_or_successful_retry() {
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .unwrap();
        let root = repo.join(".tmp/gpui-management-lan").join(format!(
            "failed-source-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        for retry in [false, true] {
            let source = root.join(if retry { "retry.png" } else { "ack.png" });
            let unrelated = root.join(if retry {
                "unrelated-retry.png"
            } else {
                "unrelated-ack.png"
            });
            std::fs::write(&source, b"fixture-owned-png").unwrap();
            std::fs::write(&unrelated, b"unreferenced").unwrap();
            let source = source.to_string_lossy().into_owned();
            let unrelated = unrelated.to_string_lossy().into_owned();
            let queue = ActionQueue::start(Weak::new());
            let action = Action::Send {
                conversation: "dm:peer".into(),
                text: String::new(),
                files: vec![source.clone()],
                revision: 1,
            };
            let (started, ready) = mpsc::channel();
            let (release, wait) = mpsc::channel();
            let (failed_id, send) = queue.enqueue(
                action.clone(),
                None,
                Box::new(move || {
                    started.send(()).unwrap();
                    wait.recv_timeout(Duration::from_secs(3)).unwrap();
                    Err("send failed".into())
                }),
            );
            ready.recv_timeout(Duration::from_secs(3)).unwrap();
            let cleanup = |paths: Vec<String>| {
                let state = queue.state.clone();
                queue
                    .enqueue(
                        Action::DiscardCompose(paths.clone()),
                        None,
                        Box::new(move || {
                            let paths =
                                ActionQueue::discardable_paths(&state.lock().unwrap(), &paths);
                            for path in paths {
                                std::fs::remove_file(path).unwrap();
                            }
                            Ok(Value::Null)
                        }),
                    )
                    .1
            };
            // A window closes while its send is still running. The worker
            // must discover that failure only when the cleanup reaches it.
            drop(cleanup(vec![source.clone(), unrelated.clone()]));
            release.send(()).unwrap();
            assert!(send.blocking_recv().unwrap().is_err());
            assert!(queue.barrier().blocking_recv().unwrap().is_err());
            assert!(std::path::Path::new(&source).is_file());
            assert!(!std::path::Path::new(&unrelated).exists());
            if retry {
                let (_, reply) =
                    queue.enqueue(action, Some(failed_id), Box::new(|| Ok(Value::Null)));
                assert!(reply.blocking_recv().unwrap().is_ok());
            } else {
                queue.acknowledge(failed_id);
            }
            assert!(cleanup(vec![source.clone()])
                .blocking_recv()
                .unwrap()
                .is_ok());
            assert!(!std::path::Path::new(&source).exists());
            assert!(queue.barrier().blocking_recv().unwrap().is_ok());
        }
        std::fs::remove_dir(&root).unwrap();
    }

    #[test]
    fn old_conversation_and_hide_reopen_reads_cannot_become_current_again() {
        let request = RequestStamp {
            lifetime: 7,
            request: 19,
            conversation: "peer-a".into(),
        };
        assert!(request.is_current(true, 7, 19, "peer-a"));
        assert!(!request.is_current(true, 7, 20, "peer-b"));
        assert!(!request.is_current(true, 7, 21, "peer-a")); // A → B → A
        assert!(!request.is_current(false, 7, 19, "peer-a"));
        assert!(!request.is_current(true, 9, 19, "peer-a")); // hide → reopen
    }

    #[test]
    fn send_success_preserves_new_input_files_and_ime_composition() {
        let text = "  中😀\r\n末尾 \n";
        let files = vec![r"C:\A.png".into(), r"C:\a.png".into()];
        assert!(send_reply_matches(6, 6, text, text, &files, &files, false));
        assert!(!send_reply_matches(6, 7, text, text, &files, &files, false)); // edit/undo to same text
        assert!(!send_reply_matches(
            6,
            6,
            text,
            "new draft",
            &files,
            &files,
            false
        ));
        assert!(!send_reply_matches(
            6,
            6,
            text,
            text,
            &files,
            &files[..1],
            false
        ));
        assert!(!send_reply_matches(6, 6, text, text, &files, &files, true));
    }

    #[test]
    fn message_retry_cancel_and_progress_use_real_delivery_dto_shapes() {
        for status in ["failed", "partial", "canceled"] {
            assert_eq!(
                message_actions(&json!({"direction":"outgoing","status":status})),
                (true, false)
            );
        }
        for status in ["queued", "sending"] {
            assert_eq!(
                message_actions(&json!({"direction":"outgoing","status":status})),
                (false, true)
            );
        }
        assert_eq!(
            message_actions(&json!({"direction":"incoming","status":"failed"})),
            (false, false)
        );
        assert_eq!(
            message_actions(&json!({"direction":"outgoing","status":"delivered"})),
            (false, false)
        );
        let message = json!({"direction":"outgoing","status":"partial","deliveries":[
            {"status":"delivered","totalBytes":200,"transferredBytes":200},
            {"status":"failed","totalBytes":200,"transferredBytes":0,"lastError":"offline"}]});
        assert_eq!(message_progress(&message), Some(50));
        assert!(message_status(&message).ends_with(" 1/2"));
        assert_eq!(message_progress(&json!({"deliveries":[]})), None);
        assert_eq!(
            message_progress(&json!({"deliveries":[{"totalBytes":1,"transferredBytes":100}]})),
            Some(100)
        );
        assert_eq!(
            message_progress(
                &json!({"deliveries":[{"totalBytes":u64::MAX,"transferredBytes":u64::MAX},{"totalBytes":u64::MAX,"transferredBytes":0}]})
            ),
            Some(50)
        );
    }

    #[test]
    fn incoming_images_follow_web_mime_and_extension_guards() {
        assert!(image_attachment(
            &json!({"mimeType":"image/png","name":"图片.PNG"})
        ));
        assert!(!image_attachment(
            &json!({"mimeType":"application/octet-stream","name":"a.png"})
        ));
        for name in [
            "a.tiff",
            "a.svg",
            "a.ico",
            "a.png.exe",
            "https://x/a.png",
            "data:image/png",
        ] {
            assert!(!image_path(name));
        }
        assert!(image_path(r"\\server\share\a.WEBP"));
        assert_eq!(file_name(r"C:\目录\图片.png"), "图片.png");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_time(&json!("invalid")), "");
    }

    #[test]
    fn pushed_message_replaces_same_id_and_keeps_core_order_and_page_limit() {
        let mut messages = (0..200)
            .map(|i| json!({"id":i.to_string(),"text":"old"}))
            .collect::<Vec<_>>();
        upsert(&mut messages, json!({"id":"80","text":"new"}));
        assert_eq!(messages.len(), 200);
        assert_eq!(messages[80]["text"], "new");
        upsert(&mut messages, json!({"id":"200","text":"last"}));
        assert_eq!(messages.len(), 200);
        assert_eq!(messages[0]["id"], "1");
        assert_eq!(messages[199]["id"], "200");
    }

    #[test]
    fn pair_input_accepts_only_six_ascii_digits() {
        assert_eq!(pairing_code("a12 3-4中56７8"), "123456");
        assert_eq!(pairing_code("１２３４５６"), "");
        assert_eq!(pairing_code("123"), "123");
        assert_eq!(alias_input(&"😀".repeat(40)), "😀".repeat(32));
        assert_eq!(
            alias_input(&"a".repeat(63).chars().chain(['😀']).collect::<String>()),
            "a".repeat(63)
        );
    }
}

#[cfg(test)]
mod worker_tests {
    use super::*;
    #[test]
    fn replacement_owner_uses_acceptance_fifo_and_exit_waits_after_observers_drop() {
        let queue = ActionQueue::start(Weak::new());
        let order = Arc::new(Mutex::new(Vec::new()));
        let (started, began) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        let first_order = order.clone();
        let (first_id, first_reply) = queue.enqueue(
            Action::Config(json!({"deviceAlias":"A"})),
            None,
            Box::new(move || {
                started.send(()).unwrap();
                wait.recv_timeout(Duration::from_secs(3)).unwrap();
                first_order.lock().unwrap().push("A");
                Ok(json!({"deviceAlias":"A"}))
            }),
        );
        began.recv_timeout(Duration::from_secs(3)).unwrap();
        let reopened_owner = queue.clone();
        let second_order = order.clone();
        let (second_id, second_reply) = reopened_owner.enqueue(
            Action::Config(json!({"deviceAlias":"B"})),
            None,
            Box::new(move || {
                second_order.lock().unwrap().push("B");
                Ok(json!({"deviceAlias":"B"}))
            }),
        );
        assert!(second_id > first_id);
        let mut barrier = reopened_owner.barrier();
        assert!(matches!(
            barrier.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        drop(first_reply);
        drop(second_reply);
        drop(queue);
        release.send(()).unwrap();
        assert_eq!(barrier.blocking_recv().unwrap(), Ok(()));
        assert_eq!(*order.lock().unwrap(), ["A", "B"]);
        assert!(reopened_owner.failures().is_empty());
    }

    #[test]
    fn history_and_composer_failures_keep_exact_original_requests_and_retry_only_the_selected_one()
    {
        let queue = ActionQueue::start(Weak::new());
        let action = Action::Send {
            conversation: "peer-a".into(),
            text: "  中😀\r\n".into(),
            files: vec![r"C:\图片.png".into()],
            revision: 4,
        };
        let (id, reply) = queue.enqueue(action, None, Box::new(|| Err("disk full".into())));
        drop(reply);
        let ids = vec!["0007".into(), "中😀 id".into(), "0007".into()];
        let (history_id, reply) = queue.enqueue(
            Action::HistorySend { ids: ids.clone(), conversation: "dm:history-peer".into() },
            None,
            Box::new(|| Err("offline".into())),
        );
        drop(reply);
        assert_eq!(
            queue.barrier().blocking_recv().unwrap(),
            Err("disk full".into())
        );
        let failure = queue
            .failures()
            .into_iter()
            .find(|failure| failure.id == id)
            .unwrap();
        assert!(
            matches!(failure.action,Action::Send{conversation,text,files,revision:4}if conversation=="peer-a"&&text=="  中😀\r\n"&&files==vec![r"C:\图片.png".to_string()])
        );
        let history = queue
            .failures()
            .into_iter()
            .find(|failure| failure.id == history_id)
            .unwrap();
        assert!(matches!(history.action, Action::HistorySend { ids: ref original, ref conversation }
            if original == &ids && conversation == "dm:history-peer"));
        let (retry_id, reply) = queue
            .try_enqueue(
                history.action.clone(),
                Some(history_id),
                Box::new(|| Err("still offline".into())),
            )
            .unwrap();
        assert_eq!(reply.blocking_recv().unwrap(), Err("still offline".into()));
        let history = queue
            .failures()
            .into_iter()
            .find(|failure| failure.id == retry_id)
            .unwrap();
        assert!(matches!(history.action, Action::HistorySend { ids: ref original, ref conversation }
            if original == &ids && conversation == "dm:history-peer"));
        assert_eq!(queue.failures().len(), 2);
        let (_, reply) = queue
            .try_enqueue(
                history.action,
                Some(retry_id),
                Box::new(|| Ok(json!({"queued":3}))),
            )
            .unwrap();
        assert!(reply.blocking_recv().unwrap().is_ok());
        assert_eq!(
            queue.barrier().blocking_recv().unwrap(),
            Err("disk full".into())
        );
        assert_eq!(queue.failures().len(), 1);
        assert_eq!(queue.failures()[0].id, id);
        queue.acknowledge(id);
        assert_eq!(queue.barrier().blocking_recv().unwrap(), Ok(()));
    }

    #[test]
    fn reopened_owner_cannot_retry_the_same_history_request_twice_or_discard_an_active_retry() {
        let queue = ActionQueue::start(Weak::new());
        let ids = vec!["clip-a".into(), "clip-b".into()];
        let (failed_id, reply) = queue.enqueue(
            Action::HistorySend { ids: ids.clone(), conversation: "all".into() },
            None,
            Box::new(|| Err("offline".into())),
        );
        assert!(reply.blocking_recv().unwrap().is_err());
        let (started, began) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        let (_, retry) = queue
            .try_enqueue(
                Action::HistorySend { ids: ids.clone(), conversation: "all".into() },
                Some(failed_id),
                Box::new(move || {
                    started.send(()).unwrap();
                    wait.recv_timeout(Duration::from_secs(3)).unwrap();
                    Ok(json!({"queued":2}))
                }),
            )
            .unwrap();
        began.recv_timeout(Duration::from_secs(3)).unwrap();
        let reopened = queue.clone();
        assert!(reopened.is_retrying(failed_id));
        assert!(reopened
            .try_enqueue(
                Action::HistorySend { ids, conversation: "all".into() },
                Some(failed_id),
                Box::new(|| panic!("duplicate history send ran"))
            )
            .is_err());
        reopened.acknowledge(failed_id);
        assert_eq!(reopened.failures().len(), 1);
        release.send(()).unwrap();
        assert!(retry.blocking_recv().unwrap().is_ok());
        assert_eq!(reopened.barrier().blocking_recv().unwrap(), Ok(()));
        assert!(reopened.failures().is_empty());
    }

    #[test]
    fn picker_queued_before_retirement_never_opens_and_exit_retires_an_already_open_reply() {
        let queue = ActionQueue::start(Weak::new());
        let lifetime = Arc::new(PickerLifetime::default());
        lifetime.retire(true);
        let generation = lifetime.generation.load(Ordering::SeqCst);
        let (started, began) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        let (_, blocker) = queue.enqueue(
            Action::Code,
            None,
            Box::new(move || {
                started.send(()).unwrap();
                wait.recv_timeout(Duration::from_secs(3)).unwrap();
                Ok(Value::Null)
            }),
        );
        began.recv_timeout(Duration::from_secs(3)).unwrap();
        let modal_calls = Arc::new(AtomicU64::new(0));
        let picker_state = lifetime.clone();
        let calls = modal_calls.clone();
        let (_, picker) = queue.enqueue(
            Action::Pick(true),
            None,
            Box::new(move || {
                if !picker_state.is_current(generation) {
                    return Ok(picker_cancelled(true));
                }
                calls.fetch_add(1, Ordering::SeqCst);
                panic!("retired picker opened a modal")
            }),
        );
        lifetime.retire(false); // close/leave
        lifetime.retire(true); // replacement/reopen must not revive old permit
        release.send(()).unwrap();
        drop(blocker);
        assert_eq!(picker.blocking_recv().unwrap().unwrap()["cancelled"], true);
        assert_eq!(modal_calls.load(Ordering::SeqCst), 0);

        let generation = lifetime.generation.load(Ordering::SeqCst);
        let (opened, is_open) = mpsc::channel();
        let (complete, wait) = mpsc::channel();
        let picker_state = lifetime.clone();
        let (_, picker) = queue.enqueue(
            Action::Pick(false),
            None,
            Box::new(move || {
                assert!(picker_state.is_current(generation));
                opened.send(()).unwrap();
                wait.recv_timeout(Duration::from_secs(3)).unwrap();
                if !picker_state.is_current(generation) {
                    Ok(picker_cancelled(false))
                } else {
                    panic!("exit did not retire modal result")
                }
            }),
        );
        is_open.recv_timeout(Duration::from_secs(3)).unwrap();
        lifetime.retire(false); // Exit
        let mut barrier = queue.barrier();
        assert!(matches!(
            barrier.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        complete.send(()).unwrap();
        assert_eq!(picker.blocking_recv().unwrap().unwrap()["paths"], json!([]));
        assert_eq!(barrier.blocking_recv().unwrap(), Ok(()));
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;

    fn fixture_root(name: &str) -> std::path::PathBuf {
        let root = option_env!("CARGO_MANIFEST_DIR")
            .map(std::path::Path::new)
            .and_then(|path| path.ancestors().nth(3))
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| std::env::current_dir().unwrap());
        let stamp = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = root
            .join(".tmp")
            .join("gpui-management-lan")
            .join(format!("fixture-{name}-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn history_send_device_targets_are_isolated_and_retry_keeps_the_original_destination() {
        cliphost_core::force_headless(true);
        let root = fixture_root("history-targets");
        let host = Arc::new(HostState::open(
            cliphost_core::AppPaths::from_data_root(root.join("data")), 28965,
        ).unwrap());
        let source = host.add_text(&json!({"text":"history target fixture"})).unwrap();
        let ids = vec![source["createdIds"][0].as_str().unwrap().to_owned()];
        assert_eq!(host.lan_status()["enabled"], false);
        assert!(host.lan_listener_port().is_none());

        // Real public acceptance uses the same Host queue. A disabled transport
        // gives deterministic failures without a listener or any network input.
        let (all_id, all_reply) = SharedHistorySend::accept(&host, ids.clone());
        let (a_id, a_reply) = SharedHistorySend::accept_to(&host, ids.clone(), "dm:peer-a");
        let (b_id, b_reply) = SharedHistorySend::accept_to(&host, ids.clone(), "dm:peer-b");
        for reply in [all_reply, a_reply, b_reply] {
            assert!(reply.blocking_recv().unwrap().is_err());
        }
        let before = SharedHistorySend::failures(&host);
        assert_eq!(before.len(), 3);
        for (id, conversation) in [(all_id, "all"), (a_id, "dm:peer-a"), (b_id, "dm:peer-b")] {
            let failure = before.iter().find(|failure| failure.id == id).unwrap();
            assert_eq!(failure.ids, ids);
            assert_eq!(failure.conversation_id, conversation);
        }

        // Retry A after B has been accepted; neither B nor the default/all
        // request may supply a new destination for this accepted request.
        let (retry_id, retry) = SharedHistorySend::retry(&host, a_id).unwrap();
        assert!(retry.blocking_recv().unwrap().is_err());
        let after = SharedHistorySend::failures(&host);
        assert_eq!(after.len(), 3);
        assert!(!after.iter().any(|failure| failure.id == a_id));
        let retried = after.iter().find(|failure| failure.id == retry_id).unwrap();
        assert_eq!(retried.ids, ids);
        assert_eq!(retried.conversation_id, "dm:peer-a");
        assert_eq!(after.iter().find(|failure| failure.id == b_id).unwrap().conversation_id, "dm:peer-b");
        assert_eq!(after.iter().find(|failure| failure.id == all_id).unwrap().conversation_id, "all");
        for failure in after { SharedHistorySend::acknowledge(&host, failure.id); }
        assert_eq!(ActionQueue::for_host(&host).barrier().blocking_recv().unwrap(), Ok(()));
        assert!(host.lan_listener_port().is_none());
    }

    #[test]
    fn real_core_inbox_unread_cancel_and_disabled_send_remain_offline() {
        cliphost_core::force_headless(true);
        let root = fixture_root("messages");
        let paths = cliphost_core::AppPaths::from_data_root(root.join("data"));
        let host = HostState::open(paths.clone(), 28965).unwrap();
        assert_eq!(host.lan_status()["enabled"], false);
        assert!(host.lan_listener_port().is_none());
        let store = cliphost_core::AppStore::open(&paths.db).unwrap();
        let incoming = json!({"id":"incoming","conversationId":"all","direction":"incoming","targetKind":"group","senderDeviceId":"fixture-peer","senderAlias":"Fixture","text":"中😀\r\n正文","isRead":false,"status":"received","createdAtUtc":"2026-10-13T00:00:00Z","attachments":[],"deliveries":[]});
        store
            .insert_lan_message("incoming", "all", &incoming.to_string())
            .unwrap();
        assert_eq!(host.lan_conversations()[0]["unreadCount"], 1);
        assert_eq!(
            host.lan_messages(&json!({"conversationId":"all","limit":200}))
                .unwrap()[0]["text"],
            "中😀\r\n正文"
        );
        Action::Read("all".into()).execute(&host).unwrap();
        assert_eq!(host.lan_conversations()[0]["unreadCount"], 0);
        let outgoing = json!({"id":"outgoing","conversationId":"all","direction":"outgoing","targetKind":"group","text":"draft","isRead":true,"status":"failed","createdAtUtc":"2026-10-13T00:00:01Z","attachments":[],"deliveries":[]});
        store
            .insert_lan_message("outgoing", "all", &outgoing.to_string())
            .unwrap();
        Action::Cancel("outgoing".into()).execute(&host).unwrap();
        let canceled: Value =
            serde_json::from_str(&store.get_lan_message("outgoing").unwrap()).unwrap();
        assert_eq!(canceled["status"], "canceled");
        assert!(Action::Send {
            conversation: "all".into(),
            text: "  accepted draft \r\n".into(),
            files: Vec::new(),
            revision: 4
        }
        .execute(&host)
        .is_err());
        assert_eq!(
            host.lan_messages(&json!({"conversationId":"all"}))
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert!(host.lan_listener_port().is_none());
    }

    #[test]
    fn image_loader_revalidates_local_version_and_recovers_without_touching_source() {
        cliphost_core::force_headless(true);
        let root = fixture_root("images");
        let host = HostState::open(
            cliphost_core::AppPaths::from_data_root(root.join("data")),
            28965,
        )
        .unwrap();
        let path = root.join("fixture.png");
        let path_string = path.to_str().unwrap();
        let mut encoded = Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(1, 1)
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        let original = encoded.into_inner();
        std::fs::write(&path, &original).unwrap();
        let Some(ImageLoad::Loaded(bitmap, version)) =
            prepare_image(&host, path_string, None, ImagePurpose::Chip)
        else {
            panic!("fixture image did not load");
        };
        assert_eq!(bitmap.allocation, 4);
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert!(matches!(
            prepare_image(&host, path_string, Some(version), ImagePurpose::Chip),
            Some(ImageLoad::Unchanged)
        ));
        std::fs::write(&path, b"bad").unwrap();
        assert!(prepare_image(&host, path_string, Some(version), ImagePurpose::Chip).is_none());
        std::fs::write(&path, &original).unwrap();
        assert!(matches!(
            prepare_image(&host, path_string, None, ImagePurpose::Preview),
            Some(ImageLoad::Loaded(_, _))
        ));
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert!(host.lan_listener_port().is_none());
    }
}
