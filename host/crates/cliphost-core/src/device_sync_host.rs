use super::*;
use crate::clipboard_sync::{SyncEvent, SyncKind};
use crate::device_sync::{self, DevicePending, DeviceReceipt, TTL};

impl HostState {
    fn device_sync_stored_receipt(&self, key: &str) -> Result<Option<DeviceReceipt>, String> {
        self.store()
            .lock()
            .kv_get(&format!(
                "device-sync-receipt:{}",
                crate::model::sha256_hex(key.as_bytes())
            ))
            .map_err(|e| e.to_string())?
            .map(|value| {
                serde_json::from_str(&value)
                    .map_err(|e| format!("Invalid stored device sync receipt: {e}"))
            })
            .transpose()
    }
    pub fn device_sync_status(&self) -> Value {
        let peers: Vec<_> = self
            .lan
            .lock()
            .discovered
            .values()
            .filter(|p| p.paired)
            .cloned()
            .collect();
        let state = self.clipboard_sync.devices.state.lock();
        let mut rows: Vec<_> = peers
            .into_iter()
            .map(|peer| {
                let policy = state
                    .durable
                    .peers
                    .get(&peer.device_id)
                    .cloned()
                    .unwrap_or_default();
                let mut row = serde_json::to_value(policy).unwrap();
                row["deviceId"] = json!(peer.device_id);
                row["name"] = json!(peer.alias);
                row["online"] = json!(peer.online && (!peer.is_pull() || peer.last_seen_at_utc >= Utc::now() - crate::lan::DISCOVERY_PEER_TTL));
                row["lastError"] = json!(state.errors.get(&peer.device_id));
                row["pending"] = json!(state.outgoing.contains_key(&peer.device_id));
                row
            })
            .collect();
        rows.sort_by(|a, b| a["deviceId"].as_str().cmp(&b["deviceId"].as_str()));
        json!({"version":1,"enabled":state.durable.enabled,"textEnabled":state.durable.text_enabled,"imageEnabled":state.durable.image_enabled,"peers":rows,"error":state.error.as_ref().or_else(||state.errors.get("capture")),
            "capabilities":["text","image"],"limits":{"maxTextBytes":crate::clipboard_sync::MAX_TEXT_BYTES,"maxImageBytes":crate::clipboard_sync::MAX_IMAGE_BYTES}})
    }
    pub fn device_sync_config_update(&self, params: &Value) -> Result<Value, String> {
        self.require_external_effects("change device clipboard synchronization")?;
        let _operation = self.settings_operation.lock();
        if let Some(error) = self.clipboard_sync.devices.state.lock().error.clone() {
            return Err(error);
        }
        for name in [
            "enabled",
            "sendEnabled",
            "receiveEnabled",
            "textEnabled",
            "imageEnabled",
            "applyClipboard",
            "saveHistory",
        ] {
            if params.get(name).is_some_and(|v| !v.is_boolean()) {
                return Err(format!("{name} must be a boolean."));
            }
        }
        let peer = params
            .get("deviceId")
            .map(|v| {
                v.as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or("deviceId must be a string.")
            })
            .transpose()?;
        if let Some(id) = peer {
            if !self.lan.lock().discovered.get(id).is_some_and(|p| p.paired) {
                return Err("Device is not paired.".into());
            }
        } else if [
            "sendEnabled",
            "receiveEnabled",
            "applyClipboard",
            "saveHistory",
        ]
        .iter()
        .any(|key| params.get(key).is_some())
        {
            return Err("deviceId is required for a device policy.".into());
        }
        let enabling = params
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or_else(|| self.clipboard_sync.devices.enabled());
        let (text_enabled, image_enabled) = {
            let state = self.clipboard_sync.devices.state.lock();
            let global_type = |key: &str, previous| {
                if peer.is_none() {
                    params.get(key).and_then(Value::as_bool).unwrap_or(previous)
                } else {
                    previous
                }
            };
            (
                global_type("textEnabled", state.durable.text_enabled),
                global_type("imageEnabled", state.durable.image_enabled),
            )
        };
        if !text_enabled && !image_enabled {
            return Err("Enable at least one clipboard type.".into());
        }
        if params.get("enabled").and_then(Value::as_bool) == Some(true) {
            self.lan_config_update(&json!({"enabled":true}))?;
        }
        {
            let runtime = &self.clipboard_sync.devices;
            let mut state = runtime.state.lock();
            let previous = state.durable.clone();
            state.durable.enabled = enabling;
            state.durable.text_enabled = text_enabled;
            state.durable.image_enabled = image_enabled;
            if let Some(id) = peer {
                if state.durable.peers.len() >= 32 && !state.durable.peers.contains_key(id) {
                    state.durable = previous;
                    return Err("Too many configured sync devices.".into());
                }
                state.durable.peers.entry(id.into()).or_default();
            }
            // Accept legacy fields without restoring hidden restrictions.
            state.durable.peers.values_mut().for_each(|policy| {
                *policy = crate::device_sync::DeviceSyncPolicy::default();
            });
            if let Err(error) = runtime.save(&state) {
                state.durable = previous;
                return Err(error);
            }
            state.generation = state.generation.wrapping_add(1);
            state.pending.clear();
            state.outgoing.clear();
            state.current_key = None;
            state.error = None;
        }
        self.device_sync_refresh_raw_capture();
        let _ = self.clipboard_sync.devices.changes.send(());
        let status = self.device_sync_status();
        self.push_event("deviceSyncChanged", status.clone());
        Ok(status)
    }
    fn device_sync_refresh_raw_capture(&self) {
        let raw = self.clipboard_sync.devices.needs_raw_capture()
            || self.clipboard_sync.state.lock().durable.enabled
            || self.clipboard_sync.private_capture_enabled();
        self.clipboard_sync
            .raw_enabled
            .store(raw, Ordering::Relaxed);
        self.inner.lock().capture.set_raw_enabled(raw);
    }
    pub fn device_sync_authenticate(
        &self,
        id: Option<&str>,
        authorization: Option<&str>,
    ) -> Result<String, String> {
        if !self.lan.lock().enabled {
            return Err("LAN sharing is disabled.".into());
        }
        let id = self.lan_authenticate(id, authorization)?;
        if !self.clipboard_sync.devices.enabled() {
            return Err("Device clipboard synchronization is disabled.".into());
        }
        if let Some(peer) = self.lan.lock().discovered.get_mut(&id) {
            if peer.is_pull() { peer.last_seen_at_utc = Utc::now(); peer.online = true; }
        }
        Ok(id)
    }
    pub(super) fn device_sync_pair_peer(&self, id: &str) -> Result<(), String> {
        let _settings = self.settings_operation.lock();
        let runtime = &self.clipboard_sync.devices;
        let enabled = {
            let mut state = runtime.state.lock();
            if state.error.is_some() {
                return Ok(());
            }
            state
                .durable
                .peers
                .insert(id.into(), crate::device_sync::DeviceSyncPolicy::default());
            // Re-pairing rotates credentials and invalidates old work.
            state.generation = state.generation.wrapping_add(1);
            state.pending.clear();
            state.outgoing.clear();
            state.current_key = None;
            state.errors.remove(id);
            runtime.save(&state)?;
            state.durable.enabled
        };
        if enabled && !self.lan.lock().enabled {
            self.lan_config_update(&json!({"enabled":true}))?;
        }
        self.device_sync_refresh_raw_capture();
        let _ = runtime.changes.send(());
        self.push_event("deviceSyncChanged", self.device_sync_status());
        Ok(())
    }
    pub(super) fn device_sync_forget_peer(&self, id: &str) -> Result<(), String> {
        let _settings = self.settings_operation.lock();
        let runtime = &self.clipboard_sync.devices;
        let mut state = runtime.state.lock();
        if state.error.is_some() {
            return Ok(());
        }
        state.durable.peers.remove(id);
        state.outgoing.remove(id);
        state
            .pending
            .retain(|_, pending| pending.event.origin_device_id != id);
        state.errors.remove(id);
        state.generation = state.generation.wrapping_add(1);
        // Failure still revokes in-memory effects; the deleted LAN credential
        // independently prevents any restored stale grant from authenticating.
        let result = runtime.save(&state);
        drop(state);
        let _ = runtime.changes.send(());
        self.device_sync_refresh_raw_capture();
        result
    }
    pub fn device_sync_receive_begin(
        &self,
        device_id: &str,
        authorization: &str,
        event: SyncEvent,
        replay: bool,
    ) -> Result<Value, String> {
        let observation = self.device_sync_observation();
        self.device_sync_receive_begin_observed(
            device_id,
            authorization,
            event,
            replay,
            observation,
        )
    }
    /// Snapshot when the request headers arrive, before a slow POST body. This
    /// keeps a delayed text transfer from replacing a newer local clipboard.
    pub fn device_sync_observation(&self) -> (Option<u32>, u64) {
        (
            (!crate::clipboard::is_headless()).then(crate::clipboard::sequence_number),
            self.clipboard_sync.devices.state.lock().revision,
        )
    }
    pub fn device_sync_receive_begin_observed(
        &self,
        device_id: &str,
        authorization: &str,
        event: SyncEvent,
        replay: bool,
        observation: (Option<u32>, u64),
    ) -> Result<Value, String> {
        self.require_external_effects("receive device clipboard synchronization")?;
        event.validate()?;
        if event.origin_device_id != device_id || device_id == self.sync_device_id() {
            return Err("Event origin does not match the paired device.".into());
        }
        self.device_sync_authenticate(Some(device_id), Some(authorization))?;
        let peer = self
            .lan
            .lock()
            .discovered
            .get(device_id)
            .cloned()
            .ok_or("Device is not paired.")?;
        let runtime = &self.clipboard_sync.devices;
        let receive = runtime.receive_operation.lock();
        let key = device_sync::pending_key(device_id, &event.event_id);
        // The database receipt is authoritative across a crash between history
        // commit and state.json. Retrying never resurrects a deleted item.
        if let Some(receipt) = self.device_sync_stored_receipt(&key)? {
            if receipt.hash != device_sync::event_hash(&event) {
                return Err("Sync identity was reused for another payload.".into());
            }
            return Ok(receipt.ack);
        }
        let stored_water = self
            .store()
            .lock()
            .kv_get(&format!("device-sync-high-water:{device_id}"))
            .map_err(|e| e.to_string())?
            .map(|value| {
                value
                    .parse::<u64>()
                    .map_err(|_| "Invalid device sync sequence receipt.")
            })
            .transpose()?
            .unwrap_or(0);
        let mut state = runtime.state.lock();
        if !state.durable.allows(event.kind) {
            return Err("Receiving this clipboard type is disabled.".into());
        }
        let policy = crate::device_sync::DeviceSyncPolicy::default();
        if let Some(receipt) = state
            .durable
            .receipts
            .iter()
            .find(|r| r.id == event.event_id && r.origin == device_id)
        {
            if receipt.hash != device_sync::event_hash(&event) {
                return Err("Sync identity was reused for another payload.".into());
            }
            return Ok(receipt.ack.clone());
        }
        state.pending.retain(|_, p| p.started.elapsed() < TTL);
        if let Some(pending) = state.pending.get_mut(&key) {
            if pending.event != event {
                return Err("Sync identity was reused for another payload.".into());
            }
            pending.replay |= replay;
            return Ok(json!({"eventId":event.event_id,"status":"pendingMedia"}));
        }
        let newest = state
            .durable
            .high_water
            .get(device_id)
            .copied()
            .into_iter()
            .chain(
                state
                    .pending
                    .values()
                    .filter(|p| p.event.origin_device_id == device_id)
                    .map(|p| p.event.origin_seq),
            )
            .max()
            .unwrap_or(0)
            .max(stored_water);
        if event.origin_seq <= newest {
            return Ok(
                json!({"eventId":event.event_id,"status":"superseded","history":"skipped","clipboard":"superseded"}),
            );
        }
        if state.pending.len() >= 32 {
            return Err("Too many pending device sync events.".into());
        }
        let observed_current = state.revision == observation.1;
        state.revision = state.revision.wrapping_add(1);
        let pending = DevicePending {
            event: event.clone(),
            policy,
            token_hash: peer.inbound_token_hash.unwrap_or_default(),
            fingerprint: peer.paired_certificate_fingerprint.unwrap_or_default(),
            generation: state.generation,
            revision: if observed_current {
                state.revision
            } else {
                observation.1
            },
            native_sequence: observation.0,
            started: Instant::now(),
            replay,
        };
        state.pending.insert(key, pending);
        drop(state);
        drop(receive);
        if event.kind == SyncKind::Text {
            self.device_sync_receive_media(device_id, authorization, &event.event_id, None)
        } else {
            Ok(json!({"eventId":event.event_id,"status":"pendingMedia"}))
        }
    }
    fn device_sync_ticket_authorized(&self, pending: &DevicePending) -> Result<(), String> {
        let paired = {
            let lan = self.lan.lock();
            lan.enabled
                && lan
                    .discovered
                    .get(&pending.event.origin_device_id)
                    .is_some_and(|peer| {
                        peer.trusted()
                            && peer.inbound_token_hash.as_deref() == Some(&pending.token_hash)
                            && if peer.is_pull() { pending.fingerprint.is_empty() } else {
                                peer.certificate_fingerprint == pending.fingerprint
                                    && peer.paired_certificate_fingerprint.as_deref() == Some(&pending.fingerprint)
                            }
                    })
        };
        let state = self.clipboard_sync.devices.state.lock();
        if paired
            && state.durable.enabled
            && state.durable.allows(pending.event.kind)
            && state.generation == pending.generation
            && pending.started.elapsed() < TTL
        {
            Ok(())
        } else {
            Err("Device sync event superseded or permission revoked.".into())
        }
    }
    fn device_sync_ticket_current(&self, pending: &DevicePending) -> Result<(), String> {
        self.device_sync_ticket_authorized(pending)?;
        if self.clipboard_sync.devices.state.lock().revision == pending.revision {
            Ok(())
        } else {
            Err("A newer clipboard event superseded this event.".into())
        }
    }
    pub fn device_sync_receive_media(
        &self,
        device_id: &str,
        authorization: &str,
        event_id: &str,
        bytes: Option<&[u8]>,
    ) -> Result<Value, String> {
        self.require_external_effects("apply device clipboard synchronization")?;
        self.device_sync_authenticate(Some(device_id), Some(authorization))?;
        let runtime = &self.clipboard_sync.devices;
        let _receive = runtime.receive_operation.lock();
        let key = device_sync::pending_key(device_id, event_id);
        if let Some(receipt) = self.device_sync_stored_receipt(&key)? {
            return Ok(receipt.ack);
        }
        let pending = {
            let state = runtime.state.lock();
            if let Some(receipt) = state
                .durable
                .receipts
                .iter()
                .find(|r| r.id == event_id && r.origin == device_id)
            {
                return Ok(receipt.ack.clone());
            }
            state
                .pending
                .get(&key)
                .cloned()
                .ok_or("Unknown or expired device sync event.")?
        };
        // Recheck authorization before media I/O and history, and again on the
        // actual clipboard owner thread before publishing any native format.
        self.device_sync_ticket_authorized(&pending)?;
        let media = match pending.event.kind {
            SyncKind::Text => {
                if bytes.is_some() {
                    return Err("Text event does not accept media.".into());
                }
                None
            }
            SyncKind::Image => Some(self.clipboard_sync.store_media(
                &pending.event.sha256,
                pending.event.image.as_ref().unwrap(),
                bytes.ok_or("Image data missing.")?,
            )?),
        };
        let content_key = match pending.event.kind {
            SyncKind::Text => format!("text:{}", pending.event.sha256),
            SyncKind::Image => device_sync::image_key(bytes.unwrap())?,
        };
        self.device_sync_ticket_authorized(&pending)?;
        let mut history = "disabled";
        let mut history_id = None;
        let mut new_item = None;
        if pending.policy.save_history {
            let mut item = ClipRecord::new_text(
                pending.event.text.as_deref().unwrap_or(""),
                "ClipHost Device Sync",
            );
            item.id = format!("device-sync-{}", crate::model::sha256_hex(key.as_bytes()));
            if let Some(path) = &media {
                item.kind = ClipKind::ImageClipItem;
                item.image_path = Some(path.to_string_lossy().into_owned());
                item.content_hash = pending.event.sha256.clone();
            }
            history = "saved";
            history_id = Some(item.id.clone());
            new_item = Some(item);
        }
        let mut ack = json!({"eventId":event_id,"status":"accepted","history":history,"historyId":history_id,"clipboard":"disabled"});
        let receipt = DeviceReceipt {
            origin: device_id.into(),
            sequence: pending.event.origin_seq,
            id: event_id.into(),
            hash: device_sync::event_hash(&pending.event),
            ack: ack.clone(),
        };
        let receipt_key = format!(
            "device-sync-receipt:{}",
            crate::model::sha256_hex(key.as_bytes())
        );
        let store = self.store();
        if let Some(item) = new_item
            .as_ref()
            .filter(|item| item.kind == ClipKind::ImageClipItem)
        {
            if content_key.starts_with("image-pixels-v1:") {
                // Media bytes and their file SHA have already been validated.
                store
                    .lock()
                    .cache_image_identity(&item.content_hash, &content_key)
                    .map_err(|e| e.to_string())?;
            }
        }
        let serialized_receipt = serde_json::to_string(&receipt).map_err(|e| e.to_string())?;
        let mut attempt = 0;
        let committed = loop {
            self.device_sync_ticket_authorized(&pending)?;
            let prepared = match new_item.as_ref() {
                Some(item) => crate::clipboard::image_identity_preparation::prepare(&store, item)
                    .map_err(|e| e.to_string())?,
                None => crate::store::PreparedImageIdentities::default(),
            };
            let result = {
                let _settings = self.settings_operation.lock();
                let _history = self.history_operation.lock();
                self.device_sync_ticket_authorized(&pending)?;
                store.lock().commit_device_sync_receipt_prepared(
                    &receipt_key,
                    &serialized_receipt,
                    new_item.as_ref(),
                    &prepared,
                )
            };
            match result {
                Ok(committed) => break committed,
                // Another collector can insert a different PNG encoding after
                // preparation. Reprepare outside every database/history lock.
                Err(crate::store::StoreError::ImageIdentityPreparationNeeded) if attempt < 2 => {
                    attempt += 1;
                }
                Err(error) => return Err(error.to_string()),
            }
        };
        if !committed {
            return self
                .device_sync_stored_receipt(&key)?
                .map(|receipt| receipt.ack)
                .ok_or_else(|| "Device sync receipt disappeared.".into());
        }
        if let Some(item) = new_item {
            // The atomic commit may have found a native capture which landed
            // after the earlier snapshot. Publish only the actual saved item.
            ack = self
                .device_sync_stored_receipt(&key)?
                .ok_or("Device sync receipt disappeared.")?
                .ack;
            if ack["history"] == "saved" && ack["historyId"] == item.id {
                self.push_event("clipChanged", crate::dto::clip_changed_delta(&[item], &[]));
            } else {
                history = "duplicate";
            }
        }
        {
            let mut state = runtime.state.lock();
            let previous = state.durable.clone();
            state
                .durable
                .high_water
                .entry(device_id.into())
                .and_modify(|value| *value = (*value).max(pending.event.origin_seq))
                .or_insert(pending.event.origin_seq);
            state.durable.receipts.push_back(DeviceReceipt {
                origin: device_id.into(),
                sequence: pending.event.origin_seq,
                id: event_id.into(),
                hash: device_sync::event_hash(&pending.event),
                ack: ack.clone(),
            });
            while state.durable.receipts.len() > 512 {
                state.durable.receipts.pop_front();
            }
            if let Err(error) = runtime.save(&state) {
                state.durable = previous;
                return Err(error);
            }
        }
        if pending.policy.apply_clipboard && !pending.replay {
            let already_current = {
                let state = runtime.state.lock();
                state.current_key.as_deref() == Some(&content_key)
                    && state.native_sequence == pending.native_sequence
            };
            if already_current && self.device_sync_ticket_current(&pending).is_ok() {
                ack["clipboard"] = json!("duplicate");
                ack["status"] = json!("applied");
            } else {
                let host = self.clone();
                let ticket = pending.clone();
                let guard: Arc<dyn Fn() -> Result<(), String> + Send + Sync> =
                    Arc::new(move || host.device_sync_ticket_current(&ticket));
                let origin = device_sync::device_origin(&pending.event, &content_key);
                let applied_key = if pending.event.kind == SyncKind::Image {
                    crate::clipboard_payload::sync_image_platform_key(bytes.unwrap())?
                } else {
                    content_key.clone()
                };
                let result = self.device_sync_ticket_current(&pending).and_then(|_| {
                    self.run_clipboard_write_then_guarded(
                        || match pending.event.kind {
                            SyncKind::Text => crate::clipboard_payload::prepare_sync_text(
                                pending.event.text.as_deref().unwrap(),
                                &origin,
                            ),
                            SyncKind::Image => crate::clipboard_payload::prepare_sync_image(
                                bytes.unwrap(),
                                &origin,
                            ),
                        },
                        |sequence| {
                            let mut state = runtime.state.lock();
                            state.native_sequence = sequence;
                            state.current_key = Some(applied_key.clone());
                            Ok(())
                        },
                        pending.native_sequence,
                        guard,
                    )
                });
                match result {
                    Ok(()) => {
                        ack["clipboard"] = json!("applied");
                        ack["status"] = json!("applied");
                    }
                    Err(error) => {
                        let superseded = self.device_sync_ticket_current(&pending).is_err()
                            || pending
                                .native_sequence
                                .is_some_and(|s| s != crate::clipboard::sequence_number());
                        ack["clipboard"] = json!(if superseded { "superseded" } else { "failed" });
                        ack["status"] = ack["clipboard"].clone();
                        if !superseded {
                            ack["error"] = json!(error);
                        }
                    }
                }
            }
        } else if pending.replay {
            ack["clipboard"] = json!("replaySkipped");
        }
        let receipt = DeviceReceipt {
            ack: ack.clone(),
            ..receipt
        };
        self.store()
            .lock()
            .kv_set(
                &receipt_key,
                &serde_json::to_string(&receipt).map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;
        let mut state = runtime.state.lock();
        state.pending.remove(&key);
        if let Some(receipt) = state
            .durable
            .receipts
            .iter_mut()
            .find(|r| r.origin == device_id && r.id == event_id)
        {
            receipt.ack = ack.clone();
        }
        runtime.save(&state)?;
        drop(state);
        if history == "saved" {
            self.trigger_cleanup();
        }
        Ok(ack)
    }
    pub(super) fn spawn_device_sync_sender(&self) {
        if crate::clipboard::is_headless() {
            return;
        }
        let runtime = Arc::downgrade(&self.clipboard_sync);
        let lan = Arc::downgrade(&self.lan);
        let notifications = Arc::downgrade(&self.inner);
        let _ = thread::Builder::new()
            .name("clip-device-sync".into())
            .spawn(move || loop {
                thread::sleep(Duration::from_millis(100));
                let (Some(sync), Some(lan)) = (runtime.upgrade(), lan.upgrade()) else {
                    break;
                };
                if !lan.lock().enabled {
                    sync.devices
                        .state
                        .lock()
                        .outgoing
                        .retain(|_, job| job.started.elapsed() < TTL);
                    continue;
                }
                let delivery = {
                    let push_peers: std::collections::HashSet<_> = lan.lock().discovered.values()
                        .filter(|peer| peer.trusted() && !peer.is_pull()).map(|peer| peer.device_id.clone()).collect();
                    let mut state = sync.devices.state.lock();
                    state.outgoing.retain(|_, job| job.started.elapsed() < TTL);
                    let id = if state.in_flight.len() >= 4 {
                        None
                    } else {
                        state
                            .outgoing
                            .iter()
                            .filter(|(id, job)| {
                                push_peers.contains(*id) && !state.in_flight.contains(*id) && job.due <= Instant::now()
                            })
                            .min_by_key(|(_, job)| job.due)
                            .map(|(id, _)| id.clone())
                    };
                    id.and_then(|id| {
                        state.outgoing.remove(&id).map(|job| {
                            state.in_flight.insert(id.clone());
                            (id, job)
                        })
                    })
                };
                let Some((id, mut job)) = delivery else {
                    continue;
                };
                let notifications = notifications.clone();
                thread::spawn(move || {
                    let peer = lan.lock().discovered.get(&id).cloned();
                    let authorized = || {
                        let still_paired = peer.as_ref().is_some_and(|expected| {
                            let lan = lan.lock();
                            lan.enabled
                                && lan.discovered.get(&id).is_some_and(|current| {
                                    current.paired
                                        && current.token == expected.token
                                        && current.inbound_token_hash == expected.inbound_token_hash
                                        && current.paired_certificate_fingerprint
                                            == expected.paired_certificate_fingerprint
                                        && current.certificate_fingerprint
                                            == expected.certificate_fingerprint
                                })
                        });
                        let state = sync.devices.state.lock();
                        still_paired
                            && state.durable.enabled
                            && state.durable.allows(job.event.kind)
                            && state.generation == job.generation
                            && state.current_key.as_deref() == Some(job.content_key.as_str())
                            && state.durable.peers.contains_key(&id)
                    };
                    let mut terminal_failure = false;
                    let result = (|| {
                        if !authorized() {
                            return Err("Device sync permission changed.".into());
                        }
                        let peer = peer.as_ref().ok_or("Device is not paired.")?;
                        if !peer.paired
                            || peer.paired_certificate_fingerprint.as_deref()
                                != Some(peer.certificate_fingerprint.as_str())
                        {
                            return Err("Device identity changed; pair again.".into());
                        }
                        let token = peer.token.as_ref().ok_or("Device credential missing.")?;
                        let (host, port) = crate::lan::parse_https_host_port(&peer.base_url)
                            .ok_or("Invalid device address.")?;
                        let bearer = format!("Bearer {token}");
                        let headers = [
                            ("X-ClipLink-Device", sync.device_id.as_str()),
                            ("Authorization", bearer.as_str()),
                        ];
                        let mut event = serde_json::to_value(&job.event).unwrap();
                        event["replay"] =
                            json!(job.retried || job.started.elapsed() > Duration::from_secs(5));
                        let response = crate::lan::https_device_sync_request(
                            &host,
                            port,
                            "POST",
                            &format!("{}/events", device_sync::PREFIX),
                            &peer.certificate_fingerprint,
                            &serde_json::to_vec(&event).unwrap(),
                            "application/json",
                            &headers,
                        )?;
                        if response["status"] == "pendingMedia" {
                            if !authorized() {
                                return Err("Device sync permission changed.".into());
                            }
                            let path = crate::clipboard_sync::read_media(&sync, &job.event.sha256)?;
                            let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
                            if crate::model::sha256_hex(&bytes) != job.event.sha256 {
                                return Err("Sync media changed on disk.".into());
                            }
                            if !authorized() {
                                return Err("Device sync permission changed.".into());
                            }
                            let ack = crate::lan::https_device_sync_request(
                                &host,
                                port,
                                "PUT",
                                &format!(
                                    "{}/events/{}/media",
                                    device_sync::PREFIX,
                                    job.event.event_id
                                ),
                                &peer.certificate_fingerprint,
                                &bytes,
                                "application/octet-stream",
                                &headers,
                            )?;
                            if ack["eventId"] != job.event.event_id
                                || !matches!(
                                    ack["status"].as_str(),
                                    Some("applied" | "accepted" | "superseded" | "failed")
                                )
                            {
                                return Err("Invalid device sync acknowledgement.".into());
                            }
                            if ack["status"] == "failed" {
                                terminal_failure = true;
                                return Err(ack["error"]
                                    .as_str()
                                    .unwrap_or(
                                        "The peer could not apply the synchronized clipboard.",
                                    )
                                    .to_owned());
                            }
                        } else if response["eventId"] != job.event.event_id
                            || !matches!(
                                response["status"].as_str(),
                                Some("applied" | "accepted" | "superseded" | "failed")
                            )
                        {
                            return Err(
                                "Peer does not support device clipboard synchronization.".into()
                            );
                        }
                        if response["status"] == "failed" {
                            terminal_failure = true;
                            return Err(response["error"]
                                .as_str()
                                .unwrap_or("The peer could not apply the synchronized clipboard.")
                                .to_owned());
                        }
                        Ok::<(), String>(())
                    })();
                    let mut state = sync.devices.state.lock();
                    state.in_flight.remove(&id);
                    match result {
                        Ok(()) => {
                            state.errors.remove(&id);
                        }
                        Err(error) => {
                            state.errors.insert(id.clone(), error);
                            job.retried = true;
                            job.due = Instant::now() + Duration::from_secs(3);
                            if !terminal_failure
                                && state.durable.enabled
                                && state.generation == job.generation
                            {
                                state.outgoing.entry(id).or_insert(job);
                            }
                        }
                    }
                    drop(state);
                    if let Some(inner) = notifications.upgrade() {
                        let capture = inner.lock().capture.clone();
                        capture.push("deviceSyncChanged", json!({"reason":"delivery"}));
                    }
                });
            });
    }
}
