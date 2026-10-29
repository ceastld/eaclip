//! Quicker callbacks share the inbound verb pipe. Never hold a host lock while
//! waiting for a callback: the plugin may call back into ClipHost before replying.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc as async_mpsc, Semaphore};

use crate::pipe_rpc::{decode_frame, encode_frame, PipeRpcMessage, StopSignal, MAX_FRAME_LENGTH};

pub(crate) const INTERACTIVE_TIMEOUT: Duration = Duration::from_secs(2);
pub(crate) const BROWSER_TIMEOUT: Duration = Duration::from_secs(4);
pub(crate) const ACTION_TIMEOUT: Duration = Duration::from_secs(8);
pub(crate) const BACKGROUND_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_PENDING: usize = 64;
const MAX_INBOUND: usize = 16;
type Reply = Result<Value, String>;
type Dispatch = Arc<dyn Fn(&PipeRpcMessage) -> Option<PipeRpcMessage> + Send + Sync>;

pub(crate) struct Outbound {
    message: PipeRpcMessage,
    deadline: Instant,
    verify: Option<Arc<dyn Fn() -> Result<(), String> + Send + Sync>>,
}

pub(crate) struct Session {
    pipe: String,
    next_id: AtomicI64,
    ready: AtomicBool,
    outbound: async_mpsc::Sender<Outbound>,
    state: Mutex<SessionState>,
    disconnected: StopSignal,
}

struct SessionState {
    alive: bool,
    pending: HashMap<i64, mpsc::SyncSender<Reply>>,
}

impl Session {
    fn new(pipe: &str) -> (Arc<Self>, async_mpsc::Receiver<Outbound>) {
        let (outbound, receiver) = async_mpsc::channel(MAX_PENDING);
        (
            Arc::new(Self {
                pipe: pipe.into(),
                next_id: AtomicI64::new(0),
                ready: AtomicBool::new(false),
                outbound,
                state: Mutex::new(SessionState {
                    alive: true,
                    pending: HashMap::new(),
                }),
                disconnected: StopSignal::default(),
            }),
            receiver,
        )
    }

    pub(crate) fn call(&self, method: &str, payload: Value, timeout: Duration) -> Reply {
        self.call_inner(method, payload, timeout, None, None)
    }

    /// Read-only searches stop waiting as soon as the query is retired. Remove
    /// the pending ID before notifying the peer, so a queued write or late
    /// response cannot revive it. Ordinary/action callbacks keep their deadline.
    pub(crate) fn call_cancellable(
        &self, method: &str, payload: Value, timeout: Duration,
        is_cancelled: &dyn Fn() -> bool,
    ) -> Reply {
        self.call_inner(method, payload, timeout, None, Some(is_cancelled))
    }

    /// Accepted native actions can retire while waiting for this queue or for
    /// the pipe writer. Ordinary callbacks retain their existing call behavior.
    pub(crate) fn call_guarded(
        &self, method: &str, payload: Value, timeout: Duration,
        verify: Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
    ) -> Reply {
        self.call_inner(method, payload, timeout, Some(verify), None)
    }

    fn call_inner(
        &self, method: &str, payload: Value, timeout: Duration,
        verify: Option<Arc<dyn Fn() -> Result<(), String> + Send + Sync>>,
        is_cancelled: Option<&dyn Fn() -> bool>,
    ) -> Reply {
        let cancelled = || is_cancelled.is_some_and(|check| check());
        if cancelled() { return Err("Quicker RPC 搜索已取消。".into()); }
        let deadline = Instant::now() + timeout;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (sender, receiver) = mpsc::sync_channel(1);
        {
            let mut state = self.state.lock();
            if !state.alive {
                return Err("Quicker RPC 连接已断开。".into());
            }
            if state.pending.len() >= MAX_PENDING {
                return Err("Quicker RPC 请求队列已满。".into());
            }
            if let Some(verify) = &verify { verify()?; }
            if cancelled() { return Err("Quicker RPC 搜索已取消。".into()); }
            state.pending.insert(id, sender);
            if self
                .outbound
                .try_send(Outbound {
                    message: PipeRpcMessage::request(id, method, payload),
                    deadline,
                    verify,
                })
                .is_err()
            {
                state.pending.remove(&id);
                return Err("Quicker RPC 发送队列不可用。".into());
            }
        }
        loop {
            if cancelled() {
                self.state.lock().pending.remove(&id);
                self.send(PipeRpcMessage::cancel(id));
                return Err("Quicker RPC 搜索已取消。".into());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            let wait = if is_cancelled.is_some() {
                remaining.min(Duration::from_millis(10))
            } else { remaining };
            match receiver.recv_timeout(wait) {
                Ok(reply) => return if cancelled() {
                    Err("Quicker RPC 搜索已取消。".into())
                } else { reply },
                Err(mpsc::RecvTimeoutError::Timeout) if Instant::now() < deadline => continue,
                Err(_) => {
                self.state.lock().pending.remove(&id);
                // Best effort cancellation; never resend an action after timeout.
                self.send(PipeRpcMessage::cancel(id));
                return Err(format!(
                    "Quicker RPC 调用超时：{method}（{} 秒）。",
                    timeout.as_secs_f32()
                ));
                }
            }
        }
    }

    fn send(&self, message: PipeRpcMessage) {
        if self
            .outbound
            .try_send(Outbound {
                message,
                deadline: Instant::now() + ACTION_TIMEOUT,
                verify: None,
            })
            .is_err()
        {
            // A lost response would leave the peer waiting. Close the session instead.
            self.disconnect();
        }
    }

    fn complete(&self, message: PipeRpcMessage) {
        if let Some(sender) = self.state.lock().pending.remove(&message.id) {
            let reply = if message.type_name == "error" {
                Err(format!(
                    "Quicker RPC {}：{}",
                    message.error_code.as_deref().unwrap_or("remote_error"),
                    message.error_message.as_deref().unwrap_or("插件调用失败。")
                ))
            } else {
                Ok(message.payload.unwrap_or(Value::Null))
            };
            let _ = sender.try_send(reply);
        }
    }

    fn disconnect(&self) {
        let mut state = self.state.lock();
        state.alive = false;
        self.ready.store(false, Ordering::Release);
        for (_, sender) in state.pending.drain() {
            let _ = sender.try_send(Err("Quicker RPC 连接已断开；请求未重试。".into()));
        }
        drop(state);
        self.disconnected.request();
    }

    fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire) && self.state.lock().alive
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ConnectionState {
    pub client_connected: bool,
    pub callback_ready: bool,
}

#[derive(Default)]
pub(crate) struct Registry {
    sessions: Mutex<Vec<Arc<Session>>>,
}

impl Registry {
    /// An inbound verb client need not implement Quicker callbacks. Keep the
    /// two directions distinct, and retain connected state while any peer lives.
    pub(crate) fn connection_state(&self) -> ConnectionState {
        let mut connection = ConnectionState::default();
        for session in self.sessions.lock().iter() {
            let state = session.state.lock();
            if state.alive {
                connection.client_connected = true;
                connection.callback_ready |= session.ready.load(Ordering::Acquire);
            }
        }
        connection
    }

    pub(crate) fn select(&self) -> Option<Arc<Session>> {
        let sessions = self.sessions.lock();
        // V1 first, as in VerbPipeQuickerRpcBridge. Newest ready session wins ties.
        sessions
            .iter()
            .rev()
            .filter(|session| session.is_ready())
            .min_by_key(|session| {
                if session.pipe == crate::paths::VERB_RPC_PIPE_V1 {
                    0
                } else {
                    1
                }
            })
            .cloned()
    }

    pub(crate) fn connected_pipe(&self) -> Option<String> {
        self.select().map(|session| session.pipe.clone())
    }

    pub(crate) fn disconnect_all(&self) {
        for session in self.sessions.lock().drain(..) {
            session.disconnect();
        }
    }
}

struct Registration {
    registry: Arc<Registry>,
    session: Arc<Session>,
}
impl Drop for Registration {
    fn drop(&mut self) {
        self.session.disconnect();
        self.registry
            .sessions
            .lock()
            .retain(|session| !Arc::ptr_eq(session, &self.session));
    }
}

pub(crate) async fn read_message(
    reader: &mut (impl AsyncRead + Unpin),
) -> Result<Option<PipeRpcMessage>, String> {
    let mut header = [0u8; 4];
    // EOF is valid only between frames, not in a partial header.
    if reader
        .read(&mut header[..1])
        .await
        .map_err(|e| e.to_string())?
        == 0
    {
        return Ok(None);
    }
    reader
        .read_exact(&mut header[1..])
        .await
        .map_err(|e| e.to_string())?;
    let length = u32::from_le_bytes(header) as usize;
    if length == 0 || length > MAX_FRAME_LENGTH {
        return Err(format!("Invalid pipe RPC frame length {length}."));
    }
    let mut payload = vec![0u8; length];
    reader
        .read_exact(&mut payload)
        .await
        .map_err(|e| e.to_string())?;
    decode_frame(&payload).map(Some)
}

async fn write_loop(
    mut writer: impl AsyncWrite + Unpin,
    mut receiver: async_mpsc::Receiver<Outbound>,
    session: Arc<Session>,
) -> Result<(), String> {
    while let Some(outbound) = receiver.recv().await {
        if outbound.message.type_name == "request"
            && (!session
                .state
                .lock()
                .pending
                .contains_key(&outbound.message.id)
                || Instant::now() >= outbound.deadline)
        {
            continue;
        }
        let bytes = encode_frame(&outbound.message)?;
        if let Some(verify) = outbound.verify {
            if let Err(error) = verify() {
                if let Some(reply) = session.state.lock().pending.remove(&outbound.message.id) {
                    let _ = reply.try_send(Err(error));
                }
                continue;
            }
        }
        // A lifecycle guard can itself wait on UI-state contention. The caller
        // may have timed out in that interval, so do not revive its request.
        if outbound.message.type_name == "request"
            && (Instant::now() >= outbound.deadline
                || !session.state.lock().pending.contains_key(&outbound.message.id)) {
            continue;
        }
        // On an interrupted/partial frame write the entire stream is closed.
        tokio::time::timeout_at(outbound.deadline.into(), writer.write_all(&bytes))
            .await
            .map_err(|_| "Pipe RPC write timed out.".to_string())?
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

async fn read_loop(
    mut reader: impl AsyncRead + Unpin,
    session: Arc<Session>,
    dispatch: Dispatch,
) -> Result<(), String> {
    let permits = Arc::new(Semaphore::new(MAX_INBOUND));
    let inbound = Arc::new(Mutex::new(HashMap::<i64, Arc<AtomicBool>>::new()));
    while let Some(message) = read_message(&mut reader).await? {
        match message.type_name.as_str() {
            "response" | "error" => session.complete(message),
            "cancel" => {
                if let Some(cancel) = inbound.lock().get(&message.id) {
                    cancel.store(true, Ordering::Release);
                }
            }
            "request" => {
                let id = message.id;
                let Ok(permit) = permits.clone().try_acquire_owned() else {
                    session.send(PipeRpcMessage::error(
                        id,
                        "remote_error",
                        "ClipHost RPC is busy.",
                    ));
                    continue;
                };
                let cancel = Arc::new(AtomicBool::new(false));
                if inbound.lock().insert(id, cancel.clone()).is_some() {
                    return Err("Duplicate inbound pipe RPC request ID.".into());
                }
                let (session, dispatch, inbound) =
                    (session.clone(), dispatch.clone(), inbound.clone());
                tokio::spawn(async move {
                    let work_cancel = cancel.clone();
                    let work_session = session.clone();
                    let reply = tokio::task::spawn_blocking(move || {
                        let _permit = permit;
                        if work_cancel.load(Ordering::Acquire) || !work_session.state.lock().alive {
                            None
                        } else {
                            dispatch(&message)
                        }
                    })
                    .await;
                    inbound.lock().remove(&id);
                    if !cancel.load(Ordering::Acquire) && session.state.lock().alive {
                        match reply {
                            Ok(Some(reply)) => session.send(reply),
                            Err(_) => session.send(PipeRpcMessage::error(
                                id,
                                "remote_error",
                                "ClipHost RPC handler failed.",
                            )),
                            Ok(None) => {}
                        }
                    }
                });
            }
            _ => return Err("Invalid pipe RPC message type.".into()),
        }
    }
    Ok(())
}

pub(crate) async fn serve(
    stream: impl AsyncRead + AsyncWrite + Unpin,
    registry: Arc<Registry>,
    pipe: &str,
    stop: Arc<StopSignal>,
    dispatch: Dispatch,
) -> Result<(), String> {
    let (session, receiver) = Session::new(pipe);
    registry.sessions.lock().push(session.clone());
    let _registration = Registration {
        registry,
        session: session.clone(),
    };
    let (reader, writer) = tokio::io::split(stream);
    // This task is selected (not detached), so closing the stream also stops binding.
    let bind_session = session.clone();
    let bind = async move {
        for _ in 0..40 {
            let client = bind_session.clone();
            let result = tokio::task::spawn_blocking(move || {
                client.call("quicker.ping", Value::Null, INTERACTIVE_TIMEOUT)
            })
            .await;
            if matches!(result, Ok(Ok(ref value)) if value.as_str() == Some("pong")) {
                let state = bind_session.state.lock();
                if state.alive {
                    bind_session.ready.store(true, Ordering::Release);
                }
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        std::future::pending::<()>().await;
    };
    tokio::select! {
        result = read_loop(reader, session.clone(), dispatch) => result,
        result = write_loop(writer, receiver, session.clone()) => result,
        _ = bind => Ok(()),
        _ = stop.wait() => Ok(()),
        _ = session.disconnected.wait() => Ok(()),
    }
}

/// Only typed presentation DTO keys are normalized. Arbitrary action JSON is
/// never passed here, because its nested dictionary keys belong to the plugin.
pub(crate) fn presentation_json(value: Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.into_iter().map(presentation_json).collect()),
        Value::Object(items) => Value::Object(
            items
                .into_iter()
                .map(|(key, value)| {
                    let mut chars = key.chars();
                    let key = chars
                        .next()
                        .map(|first| first.to_lowercase().collect::<String>() + chars.as_str())
                        .unwrap_or_default();
                    (key, presentation_json(value))
                })
                .collect(),
        ),
        value => value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::{VERB_RPC_PIPE_V1, VERB_RPC_PIPE_V2};
    use serde_json::json;

    #[tokio::test]
    async fn ceabox_retired_rpc_queue_never_writes_an_action_frame() {
        let (session, receiver) = Session::new("ceabox-memory-only");
        let valid = Arc::new(AtomicBool::new(true));
        let accepted = valid.clone();
        let caller = session.clone();
        let call = tokio::task::spawn_blocking(move || caller.call_guarded(
            "quicker.invokeAction", json!({"ActionId":"never-run"}), Duration::from_secs(2),
            Arc::new(move || if accepted.load(Ordering::Acquire) { Ok(()) } else { Err("retired ceabox".into()) }),
        ));
        tokio::time::timeout(Duration::from_secs(1), async {
            while session.state.lock().pending.is_empty() { tokio::task::yield_now().await; }
        }).await.unwrap();
        valid.store(false, Ordering::Release);
        let (writer, mut reader) = tokio::io::duplex(4096);
        let writing = tokio::spawn(write_loop(writer, receiver, session.clone()));
        assert_eq!(call.await.unwrap().unwrap_err(), "retired ceabox");
        assert!(session.state.lock().pending.is_empty());
        let mut byte = [0u8; 1];
        assert!(tokio::time::timeout(Duration::from_millis(25), reader.read(&mut byte)).await.is_err());
        writing.abort();
        let _ = writing.await;
    }

    fn reply(id: i64, payload: Value) -> PipeRpcMessage {
        PipeRpcMessage {
            version: 1,
            type_name: "response".into(),
            id,
            payload: Some(payload),
            method: None,
            error_code: None,
            error_message: None,
        }
    }

    async fn send(writer: &mut (impl AsyncWrite + Unpin), message: PipeRpcMessage) {
        writer
            .write_all(&encode_frame(&message).unwrap())
            .await
            .unwrap();
    }

    async fn receive(reader: &mut (impl AsyncRead + Unpin)) -> PipeRpcMessage {
        tokio::time::timeout(Duration::from_secs(3), read_message(reader))
            .await
            .unwrap()
            .unwrap()
            .unwrap()
    }

    async fn ready(registry: &Registry) -> Arc<Session> {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Some(session) = registry.select() {
                    break session;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap()
    }

    #[test]
    fn registry_prefers_v1_and_old_disconnect_cannot_clear_new_session() {
        let registry = Arc::new(Registry::default());
        let (v2, _r2) = Session::new(VERB_RPC_PIPE_V2);
        let (old, _r1) = Session::new(VERB_RPC_PIPE_V1);
        let (new, _r3) = Session::new(VERB_RPC_PIPE_V1);
        registry
            .sessions
            .lock()
            .extend([v2.clone(), old.clone(), new.clone()]);
        assert!(
            registry.select().is_none(),
            "unverified peers must not be selected"
        );
        assert_eq!(registry.connection_state(), ConnectionState {
            client_connected: true, callback_ready: false,
        });
        v2.ready.store(true, Ordering::Release);
        assert!(Arc::ptr_eq(&registry.select().unwrap(), &v2));
        old.ready.store(true, Ordering::Release);
        assert!(Arc::ptr_eq(&registry.select().unwrap(), &old));
        new.ready.store(true, Ordering::Release);
        drop(Registration {
            registry: registry.clone(),
            session: old,
        });
        assert!(Arc::ptr_eq(&registry.select().unwrap(), &new));
        new.disconnect();
        assert!(Arc::ptr_eq(&registry.select().unwrap(), &v2));
        assert_eq!(registry.connection_state(), ConnectionState {
            client_connected: true, callback_ready: true,
        });
        registry.disconnect_all();
        assert!(registry.select().is_none());
        assert_eq!(registry.connection_state(), ConnectionState::default());
    }

    #[tokio::test]
    async fn replies_can_arrive_out_of_order_and_unknown_ids_are_ignored() {
        let (session, mut outbound) = Session::new("test");
        let one = session.clone();
        let first =
            tokio::task::spawn_blocking(move || one.call("one", Value::Null, INTERACTIVE_TIMEOUT));
        let req1 = outbound.recv().await.unwrap().message;
        let two = session.clone();
        let second =
            tokio::task::spawn_blocking(move || two.call("two", Value::Null, INTERACTIVE_TIMEOUT));
        let req2 = outbound.recv().await.unwrap().message;
        session.complete(reply(999, json!("ignored")));
        session.complete(reply(req2.id, json!("two")));
        session.complete(reply(req1.id, json!("one")));
        assert_eq!(first.await.unwrap().unwrap(), "one");
        assert_eq!(second.await.unwrap().unwrap(), "two");
        assert!(session.state.lock().pending.is_empty());
    }

    #[tokio::test]
    async fn retired_search_stops_waiting_sends_cancel_and_does_not_consume_late_reply() {
        let (session, mut outbound) = Session::new("test");
        let cancelled = Arc::new(AtomicBool::new(false));
        let client = session.clone();
        let check = cancelled.clone();
        let call = tokio::task::spawn_blocking(move || client.call_cancellable(
            "quicker.searchActions", Value::Null, Duration::from_secs(30),
            &|| check.load(Ordering::Acquire),
        ));
        let request = outbound.recv().await.unwrap().message;
        cancelled.store(true, Ordering::Release);
        let result = tokio::time::timeout(Duration::from_secs(1), call).await.unwrap().unwrap();
        assert!(result.unwrap_err().contains("取消"));
        let cancel = outbound.recv().await.unwrap().message;
        assert_eq!((cancel.type_name.as_str(), cancel.id), ("cancel", request.id));
        assert!(session.state.lock().pending.is_empty());
        session.complete(reply(request.id, json!("late")));
        assert!(session.state.lock().pending.is_empty());
        let client = session.clone();
        let next = tokio::task::spawn_blocking(move || client.call("ping", Value::Null, INTERACTIVE_TIMEOUT));
        let request = outbound.recv().await.unwrap().message;
        session.complete(reply(request.id, json!("ready")));
        assert_eq!(next.await.unwrap().unwrap(), "ready");
        assert!(outbound.try_recv().is_err(), "cancellation never retries a request");
    }

    #[test]
    fn retired_search_does_not_enqueue_any_rpc() {
        let (session, mut outbound) = Session::new("test");
        assert!(session.call_cancellable("quicker.searchActions", Value::Null,
            INTERACTIVE_TIMEOUT, &|| true).unwrap_err().contains("取消"));
        assert!(outbound.try_recv().is_err());
        assert!(session.state.lock().pending.is_empty());
    }

    #[tokio::test]
    async fn timeout_sends_cancel_and_disconnect_fails_pending_calls_without_retry() {
        let (session, mut outbound) = Session::new("test");
        let client = session.clone();
        let call = tokio::task::spawn_blocking(move || {
            client.call("action", Value::Null, Duration::from_millis(40))
        });
        let request = outbound.recv().await.unwrap().message;
        assert!(call.await.unwrap().unwrap_err().contains("超时"));
        let cancel = outbound.recv().await.unwrap().message;
        assert_eq!(
            (cancel.type_name.as_str(), cancel.id),
            ("cancel", request.id)
        );
        session.complete(reply(request.id, json!("late")));
        assert!(session.state.lock().pending.is_empty());
        let client = session.clone();
        let call = tokio::task::spawn_blocking(move || {
            client.call("action", Value::Null, INTERACTIVE_TIMEOUT)
        });
        outbound.recv().await.unwrap();
        session.disconnect();
        assert!(call.await.unwrap().unwrap_err().contains("断开"));
        assert!(session
            .call("action", Value::Null, INTERACTIVE_TIMEOUT)
            .unwrap_err()
            .contains("断开"));
        assert!(outbound.try_recv().is_err(), "no action retries");
    }

    #[tokio::test]
    async fn duplex_callback_during_inbound_verb_does_not_deadlock_or_confuse_ids() {
        let registry = Arc::new(Registry::default());
        let callback_registry = registry.clone();
        let (server, mut peer) = tokio::io::duplex(128);
        let stop = Arc::new(StopSignal::default());
        let serving = tokio::spawn(serve(
            server,
            registry.clone(),
            "test",
            stop.clone(),
            Arc::new(move |message| {
                let result = callback_registry.select().unwrap().call(
                    "quicker.metadata",
                    json!({"Items":["a"]}),
                    INTERACTIVE_TIMEOUT,
                );
                Some(reply(message.id, result.unwrap()))
            }),
        ));
        let ping = receive(&mut peer).await;
        assert_eq!(ping.method.as_deref(), Some("quicker.ping"));
        send(&mut peer, reply(ping.id, json!("pong"))).await;
        ready(&registry).await;
        // Each direction owns its own ID space (Rust's next ID is also 2).
        send(
            &mut peer,
            PipeRpcMessage::request(2, "cliphost.runVerb", Value::Null),
        )
        .await;
        let callback = receive(&mut peer).await;
        assert_eq!(callback.id, 2);
        assert_eq!(callback.method.as_deref(), Some("quicker.metadata"));
        send(&mut peer, reply(callback.id, json!("callback finished"))).await;
        let response = receive(&mut peer).await;
        assert_eq!((response.type_name.as_str(), response.id), ("response", 2));
        assert_eq!(response.payload, Some(json!("callback finished")));
        stop.request();
        tokio::time::timeout(Duration::from_secs(1), serving)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(registry.select().is_none());
    }

    #[tokio::test]
    async fn registry_disconnect_wakes_an_idle_stream() {
        let (server, mut peer) = tokio::io::duplex(1024);
        let registry = Arc::new(Registry::default());
        let serving = tokio::spawn(serve(server, registry.clone(), "idle",
            Arc::new(StopSignal::default()), Arc::new(|_| None)));
        let ping = receive(&mut peer).await;
        send(&mut peer, reply(ping.id, json!("pong"))).await;
        ready(&registry).await;
        registry.disconnect_all();
        tokio::time::timeout(Duration::from_secs(1), serving).await.unwrap().unwrap().unwrap();
        assert!(registry.select().is_none());
    }

    #[tokio::test]
    async fn stream_handles_fragmentation_and_rejects_truncation_and_bad_lengths() {
        let message = PipeRpcMessage::request(7, "fixture", json!({"Value":"中文"}));
        let bytes = encode_frame(&message).unwrap();
        let (mut writer, mut reader) = tokio::io::duplex(16);
        tokio::spawn(async move {
            for byte in bytes {
                writer.write_all(&[byte]).await.unwrap();
            }
        });
        assert_eq!(receive(&mut reader).await.id, 7);
        assert!(read_message(&mut reader).await.unwrap().is_none());
        for bytes in [
            vec![1, 2],
            vec![0, 0, 0, 0],
            vec![255, 255, 255, 255],
            vec![3, 0, 0, 0, b'{'],
        ] {
            assert!(read_message(&mut bytes.as_slice()).await.is_err());
        }
        assert!(decode_frame(br#"{"version":2,"type":"request","id":1}"#).is_err());
        assert_eq!(decode_frame(br#"{"version":1,"type":"error","id":1,"errorCode":"fixture","errorMessage":"bad"}"#).unwrap().error_code.as_deref(), Some("fixture"));
    }

    #[tokio::test]
    async fn stalled_frame_write_closes_session_and_releases_pending_calls() {
        let (server, mut peer) = tokio::io::duplex(64);
        let registry = Arc::new(Registry::default());
        let serving = tokio::spawn(serve(
            server,
            registry.clone(),
            "test",
            Arc::new(StopSignal::default()),
            Arc::new(|_| None),
        ));
        let ping = receive(&mut peer).await;
        send(&mut peer, reply(ping.id, json!("pong"))).await;
        let client = ready(&registry).await;
        let call = tokio::task::spawn_blocking(move || {
            client.call("huge", json!("x".repeat(4096)), Duration::from_millis(50))
        });
        assert!(call.await.unwrap().is_err());
        assert!(tokio::time::timeout(Duration::from_secs(1), serving)
            .await
            .unwrap()
            .unwrap()
            .is_err());
        assert!(registry.select().is_none());
    }

    #[tokio::test]
    async fn cancelled_request_suppresses_reply_and_connection_remains_usable() {
        let (server, mut peer) = tokio::io::duplex(1024);
        let registry = Arc::new(Registry::default());
        let (started, started_rx) = tokio::sync::oneshot::channel();
        let started = Arc::new(Mutex::new(Some(started)));
        let (release, release_rx) = mpsc::sync_channel(1);
        let release_rx = Arc::new(Mutex::new(release_rx));
        let serving = tokio::spawn(serve(
            server,
            registry,
            "test",
            Arc::new(StopSignal::default()),
            Arc::new(move |message| {
                if message.id == 10 {
                    let _ = started.lock().take().unwrap().send(());
                    release_rx
                        .lock()
                        .recv_timeout(Duration::from_secs(2))
                        .unwrap();
                }
                Some(reply(message.id, json!(true)))
            }),
        ));
        let ping = receive(&mut peer).await;
        send(&mut peer, reply(ping.id, json!("pong"))).await;
        send(&mut peer, PipeRpcMessage::request(10, "slow", Value::Null)).await;
        started_rx.await.unwrap();
        send(&mut peer, PipeRpcMessage::cancel(10)).await;
        send(
            &mut peer,
            PipeRpcMessage::request(11, "barrier", Value::Null),
        )
        .await;
        assert_eq!(receive(&mut peer).await.id, 11); // cancellation was consumed before this
        release.send(()).unwrap();
        send(
            &mut peer,
            PipeRpcMessage::request(12, "still working", Value::Null),
        )
        .await;
        assert_eq!(receive(&mut peer).await.id, 12);
        drop(peer);
        serving.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn host_metadata_and_steps_use_plugin_dtos_then_fall_back_after_disconnect() {
        crate::clipboard::force_headless(true);
        let dir = tempfile::tempdir().unwrap();
        let host = crate::HostState::open(
            crate::AppPaths::from_data_root(dir.path().join("BetterClipTauri")),
            28965,
        )
        .unwrap();
        let (server, mut peer) = tokio::io::duplex(4096);
        let serving = tokio::spawn(serve(
            server,
            host.quicker_rpc(),
            "test",
            Arc::new(StopSignal::default()),
            Arc::new(|_| None),
        ));
        let ping = receive(&mut peer).await;
        assert_eq!(host.main_host_state_dto()["verbRpcClientConnected"], true);
        assert_eq!(host.main_host_state_dto()["quickerRpcConnected"], false);
        send(&mut peer, reply(ping.id, json!("pong"))).await;
        ready(&host.quicker_rpc()).await;
        assert_eq!(host.quicker_status()["connected"], true);
        let caller = host.clone();
        let call = tokio::task::spawn_blocking(move || {
            caller.quicker_metadata_batch(&json!({"actionIds":["test-id"]}))
        });
        let request = receive(&mut peer).await;
        assert_eq!(request.payload.unwrap()["Items"], json!(["test-id"]));
        send(
            &mut peer,
            reply(
                request.id,
                json!([{"ActionId":"test-id", "Title":"插件标题", "Icon":"bolt"}]),
            ),
        )
        .await;
        assert_eq!(call.await.unwrap().unwrap()[0]["title"], "插件标题");
        let caller = host.clone();
        let call = tokio::task::spawn_blocking(move || {
            caller.quicker_presentation(&json!({"stepData":"step-json"}))
        });
        let request = receive(&mut peer).await;
        assert_eq!(
            request.method.as_deref(),
            Some("quicker.getActionStepsPresentation")
        );
        assert_eq!(request.payload.unwrap()["Value"], "step-json");
        send(
            &mut peer,
            reply(
                request.id,
                json!({"StepCount":2,"PreviewLines":[{"Title":"line","Disabled":true}]}),
            ),
        )
        .await;
        let dto = call.await.unwrap().unwrap();
        assert_eq!(dto["stepCount"], 2);
        assert_eq!(dto["previewLines"][0]["disabled"], true);
        drop(peer);
        serving.await.unwrap().unwrap();
        assert_eq!(host.quicker_status()["connected"], false);
        assert_eq!(host.main_host_state_dto()["verbRpcClientConnected"], false);
        assert_eq!(host.main_window_state_dto()["quickerRpcConnected"], false);
        assert_eq!(
            host.quicker_metadata_batch(&json!({"ids":["test-id"]}))
                .unwrap(),
            json!([])
        );
    }
}
