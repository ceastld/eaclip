use super::*;
use crate::clipboard_sync;
#[cfg(any(test, feature = "private-sync"))]
use crate::clipboard_sync::{
    SyncAck, SyncAckStatus, SyncClient, SyncEvent, SyncKind, SyncReceivePlan, SyncRuntime,
};

#[cfg(any(test, feature = "private-sync"))]
const SUPERSEDED: &str = "Clipboard sync event superseded.";

impl HostState {
    pub fn sync_device_id(&self) -> String {
        self.clipboard_sync.device_id.clone()
    }

    #[cfg(any(test, feature = "private-sync"))]
    pub fn sync_subscribe(&self) -> tokio::sync::broadcast::Receiver<SyncEvent> {
        self.clipboard_sync.events.subscribe()
    }

    #[cfg(any(test, feature = "private-sync"))]
    pub fn sync_status(&self) -> Value {
        let state = self.clipboard_sync.state.lock();
        let mut clients: Vec<_> = state
            .durable
            .clients
            .values()
            .map(|c| {
                json!({
                    "id":c.device_id,"deviceId":c.device_id,"name":c.name,"lastSeen":c.last_seen,
                    "role":"client", "capabilities":["text","image"]
                })
            })
            .collect();
        clients.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        let mut status = json!({"version":1,"desktopId":self.clipboard_sync.device_id,
            "enabled":state.durable.enabled,"receiveEnabled":state.durable.receive_enabled,
            "sendEnabled":state.durable.send_enabled,"clients":clients,"error":state.error,
            "limits":{"maxTextBytes":clipboard_sync::MAX_TEXT_BYTES,"maxImageBytes":clipboard_sync::MAX_IMAGE_BYTES,
                "maxImagePixels":clipboard_sync::MAX_IMAGE_PIXELS,"maxMediaCacheBytes":clipboard_sync::MAX_MEDIA_CACHE_BYTES}});
        drop(state);
        let lan = self.lan_status();
        status["listenerRunning"] = json!(self.lan_running.load(Ordering::Relaxed));
        if status["enabled"] == true && lan.get("error").and_then(Value::as_str).is_some() {
            status["error"] = lan["error"].clone();
        }
        status
    }

    #[cfg(any(test, feature = "private-sync"))]
    pub fn sync_config_update(&self, params: &Value) -> Result<Value, String> {
        self.require_external_effects("change clipboard synchronization")?;
        let _operation = self.settings_operation.lock();
        for name in ["enabled", "receiveEnabled", "sendEnabled"] {
            if params.get(name).is_some_and(|v| !v.is_boolean()) {
                return Err(format!("{name} must be a boolean."));
            }
        }
        // Starting sync must start the existing Rust TLS listener; stopping it
        // leaves manual LAN transfers available under their own configuration.
        let enabling = params
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or_else(|| self.clipboard_sync.state.lock().durable.enabled);
        if enabling {
            self.lan_config_update(&json!({"enabled":true}))?;
        }
        {
            let mut state = self.clipboard_sync.state.lock();
            let previous = state.durable.clone();
            if let Some(v) = params.get("enabled").and_then(Value::as_bool) {
                state.durable.enabled = v;
            }
            if let Some(v) = params.get("receiveEnabled").and_then(Value::as_bool) {
                state.durable.receive_enabled = v;
            }
            if let Some(v) = params.get("sendEnabled").and_then(Value::as_bool) {
                state.durable.send_enabled = v;
            }
            if let Err(error) = self.clipboard_sync.save(&state) {
                state.durable = previous;
                return Err(error);
            }
            if previous.enabled != state.durable.enabled
                || previous.receive_enabled != state.durable.receive_enabled
                || previous.send_enabled != state.durable.send_enabled
            {
                state.generation = state.generation.wrapping_add(1);
                state.revision = state.revision.wrapping_add(1);
            }
            state.error = None;
            self.clipboard_sync
                .raw_enabled
                .store(state.durable.enabled || self.clipboard_sync.devices.needs_raw_capture() || self.clipboard_sync.private_capture_enabled(), Ordering::Relaxed);
        }
        self.inner.lock().capture.set_raw_enabled(enabling || self.clipboard_sync.devices.needs_raw_capture() || self.clipboard_sync.private_capture_enabled());
        let status = self.sync_status();
        self.push_event("clipboardSyncChanged", status.clone());
        Ok(status)
    }

    #[cfg(any(test, feature = "private-sync"))]
    pub fn sync_set_enabled(&self, enabled: bool) -> Result<(), String> {
        self.sync_config_update(&json!({"enabled":enabled}))
            .map(|_| ())
    }

    #[cfg(any(test, feature = "private-sync"))]
    pub fn sync_verify_pair_code(&self, code: &str) -> Result<(), String> {
        self.require_external_effects("change sync pairing attempts")?;
        if self.lan.lock().pairing.accept(code) {
            Ok(())
        } else {
            Err("Pair code is invalid, expired, or exhausted.".into())
        }
    }

    #[cfg(any(test, feature = "private-sync"))]
    pub fn sync_invitation(&self) -> Result<Value, String> {
        self.require_external_effects("create a synchronization invitation")?;
        self.lan_config_update(&json!({"enabled":true}))?;
        let mut lan = self.lan.lock();
        if !lan.pairing.available() {
            lan.pairing = PairingCode::fresh();
        }
        let urls: Vec<_> = crate::lan::local_https_urls(lan.port)
            .into_iter()
            .map(|url| format!("{url}/cliplink/sync/v1"))
            .collect();
        Ok(
            json!({"protocolVersion":1,"desktopId":lan.identity.device_id,"name":lan.alias,
            "httpsUrl":urls.first(),"httpsUrls":urls,"certificateSha256":lan.identity.fingerprint,
            "pairCode":lan.pairing.code,"pairing":lan.pairing.view(),"capabilities":["text","image"]}),
        )
    }

    #[cfg(any(test, feature = "private-sync"))]
    pub fn sync_pair_client(&self, params: &Value) -> Result<Value, String> {
        self.require_external_effects("pair a synchronization client")?;
        let id = params
            .get("deviceId")
            .and_then(Value::as_str)
            .ok_or("deviceId missing.")?;
        clipboard_sync::validate_identifier(id)?;
        if id == self.clipboard_sync.device_id {
            return Err("Cannot pair the desktop with itself.".into());
        }
        let code = params
            .get("pairCode")
            .or_else(|| params.get("code"))
            .and_then(Value::as_str)
            .ok_or("pairCode missing.")?;
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("Android")
            .trim()
            .chars()
            .take(64)
            .collect::<String>();
        let mut state = self.clipboard_sync.state.lock();
        if state.durable.clients.len() >= 32 && !state.durable.clients.contains_key(id) {
            return Err("Too many paired sync clients.".into());
        }
        self.sync_verify_pair_code(code)?;
        let token = crate::lan::create_token();
        let previous = state.durable.clone();
        let previous_pending = state.pending.clone();
        state.durable.clients.insert(
            id.into(),
            SyncClient {
                device_id: id.into(),
                name,
                token_hash: crate::model::sha256_hex(token.as_bytes()),
                last_seen: Some(Utc::now()),
            },
        );
        clipboard_sync::invalidate_client_pending(&mut state, id);
        if let Err(error) = self.clipboard_sync.save(&state) {
            state.durable = previous;
            state.pending = previous_pending;
            return Err(error);
        }
        // Re-pairing or forgetting revokes every pending write from that device.
        drop(state);
        self.push_event("clipboardSyncChanged", self.sync_status());
        Ok(
            json!({"version":1,"desktopId":self.clipboard_sync.device_id,"token":token,
            "capabilities":["text","image"]}),
        )
    }

    #[cfg(any(test, feature = "private-sync"))]
    pub fn sync_authenticate(
        &self,
        device_id: Option<&str>,
        authorization: Option<&str>,
    ) -> Result<String, String> {
        let state = self.clipboard_sync.state.lock();
        clipboard_sync::authenticate_locked(&state, device_id, authorization)
    }

    #[cfg(any(test, feature = "private-sync"))]
    pub fn sync_forget_client(&self, id: &str) -> Result<(), String> {
        self.require_external_effects("change synchronization identities")?;
        let mut state = self.clipboard_sync.state.lock();
        let previous = state.durable.clone();
        let previous_pending = state.pending.clone();
        state.durable.clients.remove(id);
        clipboard_sync::invalidate_client_pending(&mut state, id);
        if let Err(error) = self.clipboard_sync.save(&state) {
            state.durable = previous;
            state.pending = previous_pending;
            return Err(error);
        }
        drop(state);
        self.push_event("clipboardSyncChanged", self.sync_status());
        Ok(())
    }

    #[cfg(any(test, feature = "private-sync"))]
    pub fn sync_mark_client_seen(&self, id: &str) {
        if self.require_external_effects("change synchronization client state").is_err() { return; }
        if let Some(client) = self.clipboard_sync.state.lock().durable.clients.get_mut(id) {
            client.last_seen = Some(Utc::now());
        }
    }

    #[cfg(any(test, feature = "private-sync"))]
    pub fn sync_receive_begin(&self, event: SyncEvent) -> Result<SyncReceivePlan, String> {
        self.require_external_effects("receive synchronization content")?;
        self.clipboard_sync.begin(
            event,
            (!crate::clipboard::is_headless()).then(crate::clipboard::sequence_number),
        )
    }

    /// Authenticate and reserve a receive ticket atomically with Forget/re-pair.
    #[cfg(any(test, feature = "private-sync"))]
    pub fn sync_receive_begin_authenticated(
        &self,
        event: SyncEvent,
        device_id: Option<&str>,
        authorization: Option<&str>,
    ) -> Result<SyncReceivePlan, String> {
        self.require_external_effects("receive authenticated synchronization content")?;
        self.clipboard_sync.begin_authenticated(
            event,
            (!crate::clipboard::is_headless()).then(crate::clipboard::sequence_number),
            device_id,
            authorization,
        )
    }

    /// Validation and history acceptance precede the independent native apply.
    /// A delayed image can be kept in history but cannot replace a newer copy.
    #[cfg(any(test, feature = "private-sync"))]
    pub fn sync_receive_accept(
        &self,
        event_id: &str,
        image_bytes: Option<&[u8]>,
    ) -> Result<SyncAck, String> {
        self.require_external_effects("accept synchronization content")?;
        let _operation = self.history_operation.lock();
        let pending = {
            let state = self.clipboard_sync.state.lock();
            if let Some(pending) = state.pending.get(event_id) {
                pending.clone()
            } else if let Some(receipt) = state
                .durable
                .receipts
                .iter()
                .find(|r| r.event_id == event_id)
            {
                return Ok(receipt.ack.clone());
            } else {
                return Err("Unknown or expired sync event.".into());
            }
        };
        if pending.accepted {
            return Ok(SyncAck::new(event_id, SyncAckStatus::Accepted));
        }
        let media = match pending.event.kind {
            SyncKind::Image => match self.clipboard_sync.store_media(
                &pending.event.sha256,
                pending.event.image.as_ref().unwrap(),
                image_bytes.ok_or("Image bytes missing.")?,
            ) {
                Ok(path) => Some(path),
                Err(error) => {
                    self.clipboard_sync.state.lock().error = Some(error.clone());
                    self.push_event("clipboardSyncChanged", self.sync_status());
                    return Err(error);
                }
            },
            SyncKind::Text => {
                if image_bytes.is_some() {
                    return Err("Text events do not accept image bytes.".into());
                }
                None
            }
        };
        let mut state = self.clipboard_sync.state.lock();
        if !state.pending.contains_key(event_id) {
            return Err("Sync event was revoked or expired.".into());
        }
        if !self.clipboard_sync.authorized(&state, &pending)
            || state.generation != pending.generation
            || pending.client_token_hash.as_ref().is_some_and(|hash| {
                state
                    .durable
                    .clients
                    .get(&pending.event.origin_device_id)
                    .is_none_or(|client| &client.token_hash != hash)
            })
        {
            let ack = SyncAck::new(event_id, SyncAckStatus::Superseded);
            SyncRuntime::receipt(&mut state, pending.event, ack.clone());
            state.pending.remove(event_id);
            self.clipboard_sync.save(&state)?;
            return Ok(ack);
        }
        let mut item =
            ClipRecord::new_text(pending.event.text.as_deref().unwrap_or(""), "ClipHost Sync");
        item.id = format!(
            "sync-{}",
            crate::model::sha256_hex(
                format!("{}:{}", pending.event.origin_device_id, event_id).as_bytes()
            )
        );
        if let Some(path) = &media {
            item.kind = ClipKind::ImageClipItem;
            item.image_path = Some(path.to_string_lossy().into_owned());
            item.content_hash = pending.event.sha256.clone();
        }
        let store = self.store();
        let saved = {
            let store = store.lock();
            if store.get_clip(&item.id).is_ok() {
                true
            } else {
                store
                    .insert_clip(&item, false)
                    .map_err(|e| e.to_string())?
                    .is_some()
            }
        };
        let ack = SyncAck::new(event_id, SyncAckStatus::Accepted);
        let previous = state.durable.clone();
        let current = state.pending.get_mut(event_id).unwrap();
        current.accepted = true;
        current.media = media;
        SyncRuntime::receipt(&mut state, pending.event.clone(), ack.clone());
        if let Err(error) = self.clipboard_sync.save(&state) {
            state.durable = previous;
            state.pending.insert(event_id.into(), pending);
            return Err(error);
        }
        drop(state);
        if saved {
            self.push_event("clipChanged", crate::dto::clip_changed_delta(&[item], &[]));
            self.trigger_cleanup();
        }
        Ok(ack)
    }

    #[cfg(any(test, feature = "private-sync"))]
    pub fn sync_receive_apply(&self, event_id: &str) -> Result<SyncAck, String> {
        self.require_external_effects("apply synchronization content")?;
        let pending = {
            let mut state = self.clipboard_sync.state.lock();
            let Some(pending) = state.pending.get_mut(event_id) else {
                return state
                    .durable
                    .receipts
                    .iter()
                    .find(|r| r.event_id == event_id)
                    .map(|r| r.ack.clone())
                    .ok_or_else(|| "Unknown sync event.".into());
            };
            if !pending.accepted {
                return Err("Sync event has not been accepted.".into());
            }
            if pending.applying {
                return Ok(SyncAck::new(event_id, SyncAckStatus::Accepted));
            }
            pending.applying = true;
            pending.clone()
        };
        let sync = self.clipboard_sync.clone();
        let ticket = pending.clone();
        let verify: Arc<dyn Fn() -> Result<(), String> + Send + Sync> = Arc::new(move || {
            let state = sync.state.lock();
            if sync.current(&state, &ticket)
                && state.pending.contains_key(&ticket.event.event_id)
            {
                Ok(())
            } else {
                Err(SUPERSEDED.into())
            }
        });
        let origin = pending.event.origin_tag();
        let result = self.run_clipboard_write_then_guarded(
            || {
                verify()?;
                match pending.event.kind {
                    SyncKind::Text => crate::clipboard_payload::prepare_sync_text(
                        pending.event.text.as_deref().unwrap(),
                        &origin,
                    ),
                    SyncKind::Image => {
                        let bytes =
                            std::fs::read(pending.media.as_ref().ok_or("Sync media missing.")?)
                                .map_err(|e| e.to_string())?;
                        if crate::model::sha256_hex(&bytes) != pending.event.sha256 {
                            return Err("Sync media changed on disk.".into());
                        }
                        crate::clipboard_payload::prepare_sync_image(&bytes, &origin)
                    }
                }
            },
            |_| Ok(()),
            pending.clipboard_sequence,
            verify.clone(),
        );
        let mut state = self.clipboard_sync.state.lock();
        let mut ack = SyncAck::new(
            event_id,
            match &result {
                Ok(()) => SyncAckStatus::Applied,
                Err(_)
                    if !self.clipboard_sync.current(&state, &pending)
                        || !state.pending.contains_key(event_id)
                        || pending.clipboard_sequence.is_some_and(|sequence| {
                            sequence != crate::clipboard::sequence_number()
                        }) =>
                {
                    SyncAckStatus::Superseded
                }
                Err(_) => SyncAckStatus::Failed,
            },
        );
        if ack.status == SyncAckStatus::Failed {
            ack.error = result.err();
            state.error = ack.error.clone();
        }
        state.pending.remove(event_id);
        SyncRuntime::receipt(&mut state, pending.event, ack.clone());
        self.clipboard_sync.save(&state)?;
        Ok(ack)
    }

    /// Archive an offline event without publishing it to the system clipboard.
    /// Removing the ticket also releases bounded receive capacity immediately.
    #[cfg(any(test, feature = "private-sync"))]
    pub fn sync_receive_supersede(&self, event_id: &str) -> Result<SyncAck, String> {
        self.require_external_effects("change synchronization receipts")?;
        let mut state = self.clipboard_sync.state.lock();
        let Some(pending) = state.pending.get(event_id).cloned() else {
            return state
                .durable
                .receipts
                .iter()
                .find(|r| r.event_id == event_id)
                .map(|r| r.ack.clone())
                .ok_or_else(|| "Unknown sync event.".into());
        };
        if !pending.accepted {
            return Err("Sync event has not been accepted.".into());
        }
        let previous = state.durable.clone();
        let ack = SyncAck::new(event_id, SyncAckStatus::Superseded);
        SyncRuntime::receipt(&mut state, pending.event, ack.clone());
        self.clipboard_sync
            .save(&state)
            .inspect_err(|_| state.durable = previous)?;
        state.pending.remove(event_id);
        Ok(ack)
    }

    pub fn sync_read_media(&self, hash: &str) -> Result<PathBuf, String> {
        clipboard_sync::read_media(&self.clipboard_sync, hash)
    }
}
#[cfg(feature = "private-sync")]
#[derive(Clone, Copy)]
pub struct RelayObservation {
    pub revision: u64,
    pub native_sequence: Option<u32>,
}
#[cfg(feature = "private-sync")]
impl HostState {
    pub(crate) fn relay_runtime(&self) -> &crate::relay_sync::Runtime { &self.clipboard_sync.relay }
    pub(crate) fn relay_require_effects(&self) -> Result<(), String> { self.require_external_effects("configure Relay synchronization") }
    pub(crate) fn relay_refresh_capture(&self) {
        let raw = self.clipboard_sync.state.lock().durable.enabled || self.clipboard_sync.devices.needs_raw_capture() || self.clipboard_sync.private_capture_enabled();
        self.clipboard_sync.raw_enabled.store(raw, Ordering::Relaxed);
        self.inner.lock().capture.set_raw_enabled(raw);
    }
    #[cfg(any(test, feature="test-support"))]
    pub fn relay_fixture_copy(&self,snapshot:&crate::clipboard::ClipboardSnapshot)->Result<Option<SyncEvent>,String> {
        if !crate::clipboard::is_headless() {return Err("Fixture requires headless mode.".into());}
        self.clipboard_sync.observe(0,snapshot)
    }
    #[cfg(any(test,feature="test-support"))]
    pub fn relay_fixture_records(&self)->Result<Vec<crate::model::ClipRecord>,String> {
        if !crate::clipboard::is_headless(){return Err("Fixture requires headless mode.".into());}
        self.store().lock().list_clips().map_err(|e|e.to_string())
    }
    pub fn relay_subscribe(&self)->tokio::sync::broadcast::Receiver<crate::relay_sync::CapturedEvent> {
        self.clipboard_sync.relay.events.subscribe()
    }
    pub fn relay_claim_worker(&self) -> bool {
        !self.clipboard_sync.relay.worker_started.swap(true, Ordering::SeqCst)
    }
    pub fn relay_session(&self) -> Option<(crate::relay_sync::Invitation, u64, bool, bool, u64)> {
        let s=self.clipboard_sync.relay.state.lock();
        if !s.saved.enabled { return None; }
        Some((s.invitation.clone()?,s.generation,s.saved.send_enabled,s.saved.receive_enabled,s.saved.cursor))
    }
    pub fn relay_session_current(&self, generation:u64) -> bool {
        let s=self.clipboard_sync.relay.state.lock(); s.saved.enabled && s.generation==generation
    }
    /// Called after response headers and before reading its potentially large body.
    pub fn relay_observation(&self) -> RelayObservation {
        RelayObservation {revision:self.clipboard_sync.state.lock().revision,native_sequence:(!crate::clipboard::is_headless()).then(crate::clipboard::sequence_number)}
    }
    pub fn relay_receive(&self,event:SyncEvent, media:&[u8], generation:u64, observed:RelayObservation, replay:bool)->Result<SyncAck,String> {
        self.require_external_effects("receive Relay synchronization")?;
        let stale = self.clipboard_sync.state.lock().revision != observed.revision;
        let event_id=event.event_id.clone();
        let image=event.kind==SyncKind::Image;
        let plan=self.clipboard_sync.begin_relay(event,observed.native_sequence,generation)?;
        if let Some(ack)=plan.duplicate_ack {return Ok(ack);}
        self.sync_receive_accept(&event_id,image.then_some(media))?;
        if replay || stale {self.sync_receive_supersede(&event_id)} else {self.sync_receive_apply(&event_id)}
    }
    pub fn relay_cursor_commit(&self,generation:u64,cursor:u64)->Result<(),String> {
        let runtime=&self.clipboard_sync.relay;
        let mut s=runtime.state.lock();
        if !s.saved.enabled || s.generation!=generation {return Err("Relay permission changed.".into());}
        let mut saved=s.saved.clone();saved.cursor=saved.cursor.max(cursor);
        if saved.pending_ack.as_ref().is_some_and(|a|a.cursor<=cursor){saved.pending_ack=None;}
        runtime.save(&saved)?;s.saved=saved;Ok(())
    }
    pub fn relay_pending_ack(&self,generation:u64)->Option<crate::relay_sync::PendingAck> {
        let s=self.clipboard_sync.relay.state.lock();
        if !s.saved.enabled || s.generation!=generation {return None;}
        s.saved.pending_ack.clone()
    }
    pub fn relay_save_pending_ack(&self,generation:u64,id:&str,packet:&[u8],cursor:u64)->Result<(),String> {
        crate::clipboard_sync::validate_identifier(id)?;
        if packet.len()>64*1024 {return Err("Relay ACK packet exceeds limit.".into());}
        let runtime=&self.clipboard_sync.relay;let mut s=runtime.state.lock();
        if !s.saved.enabled || s.generation!=generation {return Err("Relay permission changed.".into());}
        let mut saved=s.saved.clone();saved.pending_ack=Some(crate::relay_sync::PendingAck {id:id.into(),packet:packet.to_vec(),cursor});
        runtime.save(&saved)?;s.saved=saved;Ok(())
    }
    pub fn relay_record_ack(&self,generation:u64,ack:&SyncAck) {
        let mut s=self.clipboard_sync.relay.state.lock();
        if s.generation==generation {s.last_ack=Some(json!({"eventId":ack.event_id,"status":ack.status}));}
    }
    pub fn relay_error(&self,error:Option<&str>) {
        self.clipboard_sync.relay.state.lock().error=error.map(str::to_owned);
    }
}

#[cfg(all(test, feature = "private-sync"))]
mod relay_tests {
    use super::*;
    fn fixture()->(tempfile::TempDir,HostState,u64) {
        crate::clipboard::force_headless(true);
        let directory=tempfile::tempdir().unwrap();let host=HostState::open(AppPaths::from_data_root(directory.path().into()),28965).unwrap();
        let invitation=json!({"type":"cliprelay-invitation","version":1,"baseUrl":"https://relay.example.test/cliprelay","certificateSha256":"11".repeat(32),"roomId":"test-room","deviceId":host.sync_device_id(),"peerDeviceId":"relay-phone","token":"22".repeat(32),"keyHex":"33".repeat(32)});
        host.relay_configure(&json!({"invitation":invitation})).unwrap();assert_eq!(host.relay_status()["enabled"],false);
        host.relay_configure(&json!({"enabled":true,"sendEnabled":true,"receiveEnabled":true})).unwrap();let generation=host.relay_session().unwrap().1;
        (directory,host,generation)
    }
    fn event(id:&str,seq:u64)->SyncEvent {SyncEvent::text(id.into(),"relay-phone".into(),seq,"  exact\r\n🧪\n".into())}
    #[test]
    fn relay_enabled_without_lan_preserves_exact_event_and_receipt_across_restart() {
        let (directory,host,generation)=fixture();assert_eq!(host.lan_status()["enabled"],false);
        let observed=host.relay_observation();let event=event("relay-text",1);
        assert_eq!(host.relay_receive(event.clone(),&[],generation,observed,false).unwrap().status,SyncAckStatus::Applied);
        assert_eq!(host.relay_receive(event,&[],generation,host.relay_observation(),false).unwrap().status,SyncAckStatus::Applied);
        assert_eq!(host.store().lock().list_clips().unwrap().len(),1);
        let saved=std::fs::read_to_string(directory.path().join("Configs/ClipboardSync/relay.json")).unwrap();assert!(!saved.contains(&"22".repeat(32)));assert!(!saved.contains(&"33".repeat(32)));
        let reopened=HostState::open(AppPaths::from_data_root(directory.path().into()),28966).unwrap();assert_eq!(reopened.lan_status()["enabled"],false);assert!(reopened.relay_session().is_some());
    }
    #[test]
    fn relay_download_observation_and_replay_prevent_old_system_writes() {
        let (_directory,host,generation)=fixture();host.set_clipboard_ui_runner(|_|panic!("stale or replay event must not write"));
        let observed=host.relay_observation();
        host.inner.lock().capture.notify_raw_snapshot(77,&crate::clipboard::ClipboardSnapshot {text:Some("new local copy while download is pending".into()),..Default::default()});
        let mut encoded=std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(2,2).write_to(&mut encoded,image::ImageFormat::Png).unwrap();
        let image=encoded.into_inner();
        let mut slow=event("slow",1);slow.kind=SyncKind::Image;slow.text=None;
        slow.sha256=crate::model::sha256_hex(&image);slow.image=Some(crate::clipboard_sync::validate_image(&image).unwrap());
        assert_eq!(host.relay_receive(slow,&image,generation,observed,false).unwrap().status,SyncAckStatus::Superseded);
        assert_eq!(host.relay_receive(event("replay",2),&[],generation,host.relay_observation(),true).unwrap().status,SyncAckStatus::Superseded);
        assert_eq!(host.store().lock().list_clips().unwrap().len(),2);
    }
    #[test]
    fn relay_encrypted_ack_outbox_survives_restart_before_input_cursor_is_released() {
        let (directory,host,generation)=fixture();
        let packet=vec![0x5a;64];host.relay_save_pending_ack(generation,"synthetic-ack",&packet,64).unwrap();
        assert_eq!(host.relay_status()["cursor"],0);
        let reopened=HostState::open(AppPaths::from_data_root(directory.path().into()),28966).unwrap();let generation=reopened.relay_session().unwrap().1;
        let pending=reopened.relay_pending_ack(generation).unwrap();assert_eq!(pending.id,"synthetic-ack");assert_eq!(pending.packet,packet);assert_eq!(pending.cursor,64);
        reopened.relay_cursor_commit(generation,64).unwrap();assert!(reopened.relay_pending_ack(generation).is_none());assert_eq!(reopened.relay_status()["cursor"],64);
    }
    #[test]
    fn relay_stop_at_native_dispatch_revokes_an_already_accepted_ticket() {
        let (_directory,host,generation)=fixture();let stopper=host.clone();
        host.set_clipboard_ui_runner(move|job|{stopper.relay_configure(&json!({"enabled":false})).unwrap();job()});
        let ack=host.relay_receive(event("stop-at-dispatch",1),&[],generation,host.relay_observation(),false).unwrap();
        assert_eq!(ack.status,SyncAckStatus::Superseded);
    }
    #[test]
    fn relay_stop_or_reimport_revokes_inflight_and_wrong_device_invitation() {
        let (_directory,host,generation)=fixture();let observed=host.relay_observation();host.relay_configure(&json!({"enabled":false})).unwrap();
        assert!(host.relay_receive(event("revoked",1),&[],generation,observed,false).is_err());assert!(host.store().lock().list_clips().unwrap().is_empty());
        assert!(!host.relay_status().to_string().contains(&"22".repeat(32)));assert_eq!(host.lan_status()["enabled"],false);
        let bad=json!({"type":"cliprelay-invitation","version":1,"baseUrl":"https://relay.example.test/cliprelay","certificateSha256":"11".repeat(32),"roomId":"test-room","deviceId":"another-desktop","peerDeviceId":"relay-phone","token":"22".repeat(32),"keyHex":"33".repeat(32)});
        assert!(host.relay_configure(&json!({"invitation":bad})).is_err());
    }
}
