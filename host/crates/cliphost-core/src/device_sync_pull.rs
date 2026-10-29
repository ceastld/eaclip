//! Client-only LAN peers share the desktop device authority and latest-copy
//! queue. They never create a second mobile grant or an outbound callback.
use super::*;
use crate::{clipboard_sync::SyncKind, device_sync::{DeviceState, Delivery, TTL}};

fn valid_job(state: &DeviceState, job: &Delivery) -> bool {
    state.durable.enabled && state.durable.allows(job.event.kind)
        && state.generation == job.generation
        && state.current_key.as_deref() == Some(job.content_key.as_str())
        && job.started.elapsed() < TTL
}

impl HostState {
    pub fn device_sync_poll_subscribe(&self) -> tokio::sync::broadcast::Receiver<()> {
        self.clipboard_sync.devices.changes.subscribe()
    }

    /// Keep LAN authorization locked through the queue snapshot/mutation so
    /// Forget or re-pair cannot interleave with a stale credential operation.
    fn device_sync_pull_lan(&self, id: &str, auth: &str) -> Result<parking_lot::MutexGuard<'_, LanRuntime>, String> {
        let mut lan = self.lan.lock();
        if !lan.enabled { return Err("LAN sharing is disabled.".into()); }
        lan.authenticate(id, Some(auth))?;
        let peer = lan.discovered.get_mut(id).ok_or("Device is not paired.")?;
        if !peer.is_pull() || !peer.trusted() { return Err("Client-pull transport is not paired.".into()); }
        peer.last_seen_at_utc = Utc::now();
        peer.online = true;
        Ok(lan)
    }

    pub fn device_sync_poll(&self, id: &str, auth: &str, after: u64, resuming: bool) -> Result<Value, String> {
        let _lan = self.device_sync_pull_lan(id, auth)?;
        let mut state = self.clipboard_sync.devices.state.lock();
        if !state.durable.enabled || !state.durable.peers.contains_key(id) {
            return Err("Device clipboard synchronization is disabled.".into());
        }
        let cursor = state.durable.sequence;
        if after > cursor { return Ok(json!({"cursor":cursor,"events":[],"reset":true})); }
        let valid = state.outgoing.get(id).is_some_and(|job| valid_job(&state, job));
        if !valid { state.outgoing.remove(id); }
        let mut events = Vec::new();
        if let Some(job) = state.outgoing.get_mut(id).filter(|job| job.event.origin_seq > after) {
            let mut event = serde_json::to_value(&job.event).map_err(|e| e.to_string())?;
            event["replay"] = json!(resuming || job.retried || job.started.elapsed() > Duration::from_secs(5));
            job.retried = true;
            events.push(event);
        }
        Ok(json!({"cursor":cursor,"events":events,"reset":false}))
    }

    pub fn device_sync_pull_blob(&self, id: &str, auth: &str, hash: &str) -> Result<PathBuf, String> {
        let _lan = self.device_sync_pull_lan(id, auth)?;
        let state = self.clipboard_sync.devices.state.lock();
        let authorized = state.durable.peers.contains_key(id) && state.outgoing.get(id).is_some_and(|job| {
            valid_job(&state, job) && job.event.kind == SyncKind::Image && job.event.sha256 == hash
        });
        if !authorized { return Err("Sync media is not authorized for this device.".into()); }
        crate::clipboard_sync::read_media(&self.clipboard_sync, hash)
    }

    pub fn device_sync_pull_ack(&self, id: &str, auth: &str, params: &Value) -> Result<Value, String> {
        let event_id = params["eventId"].as_str().ok_or("eventId is required.")?;
        crate::lan::safe_identifier(event_id)?;
        let status = params["status"].as_str().ok_or("status is required.")?;
        if !matches!(status, "accepted" | "applied" | "superseded" | "failed") { return Err("Invalid acknowledgement status.".into()); }
        if params.get("error").is_some_and(|v| !v.is_string() || v.as_str().is_some_and(|s| s.len() > 512)) {
            return Err("Acknowledgement error must be a string of at most 512 bytes.".into());
        }
        let _lan = self.device_sync_pull_lan(id, auth)?;
        let mut state = self.clipboard_sync.devices.state.lock();
        if !state.durable.enabled || !state.durable.peers.contains_key(id) {
            return Err("Device clipboard synchronization is disabled.".into());
        }
        if state.outgoing.get(id).is_some_and(|job| job.event.event_id == event_id && valid_job(&state, job)) {
            state.outgoing.remove(id);
            if status == "failed" {
                state.errors.insert(id.into(), params["error"].as_str().unwrap_or("The peer could not apply the synchronized clipboard.").into());
            } else { state.errors.remove(id); }
        }
        Ok(json!({"ok":true}))
    }
}
