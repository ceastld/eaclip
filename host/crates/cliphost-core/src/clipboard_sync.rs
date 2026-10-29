//! Transport-independent clipboard events. LAN authentication belongs to the host adapter.
//! Payloads and receipts are independent of history trimming and paste usage counters.

use crate::{clipboard::ClipboardSnapshot, model::sha256_hex};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    io::{Cursor, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};
#[cfg(any(test, feature = "private-sync"))]
use tokio::sync::broadcast;
#[cfg(any(test, feature = "private-sync"))]
use std::time::{Duration, Instant};

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_TEXT_BYTES: usize = 1024 * 1024;
pub const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_IMAGE_PIXELS: u64 = 40_000_000;
pub const MAX_MEDIA_CACHE_BYTES: u64 = 512 * 1024 * 1024;
pub(crate) const ORIGIN_FORMAT: &str = "ClipHost.SyncOrigin.v1";
#[cfg(any(test, feature = "private-sync"))]
const MAX_PENDING: usize = 32;
#[cfg(any(test, feature = "private-sync"))]
const MAX_RECEIPTS: usize = 512;
#[cfg(any(test, feature = "private-sync"))]
const PENDING_TTL: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SyncKind {
    Text,
    Image,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncImage {
    pub mime_type: String,
    pub size_bytes: u64,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncEvent {
    #[serde(rename = "type")]
    pub message_type: String,
    pub version: u32,
    pub event_id: String,
    pub origin_device_id: String,
    pub origin_seq: u64,
    pub kind: SyncKind,
    pub sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<SyncImage>,
}

impl SyncEvent {
    pub fn text(event_id: String, origin_device_id: String, origin_seq: u64, text: String) -> Self {
        Self {
            message_type: "clipboard".into(),
            version: PROTOCOL_VERSION,
            event_id,
            origin_device_id,
            origin_seq,
            kind: SyncKind::Text,
            sha256: sha256_hex(text.as_bytes()),
            text: Some(text),
            image: None,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.message_type != "clipboard" || self.version != PROTOCOL_VERSION {
            return Err("Unsupported clipboard sync protocol.".into());
        }
        validate_identifier(&self.event_id)?;
        validate_identifier(&self.origin_device_id)?;
        if self.origin_seq == 0 {
            return Err("originSeq must be positive.".into());
        }
        validate_hash(&self.sha256)?;
        match self.kind {
            SyncKind::Text => {
                let text = self.text.as_deref().ok_or("Text payload missing.")?;
                if text.len() > MAX_TEXT_BYTES || text.contains('\0') {
                    return Err("Invalid or oversized text payload.".into());
                }
                if self.image.is_some() || sha256_hex(text.as_bytes()) != self.sha256 {
                    return Err("Text payload hash mismatch.".into());
                }
            }
            SyncKind::Image => {
                let image = self.image.as_ref().ok_or("Image metadata missing.")?;
                if self.text.is_some()
                    || !matches!(image.mime_type.as_str(), "image/png" | "image/jpeg")
                    || image.size_bytes == 0
                    || image.size_bytes > MAX_IMAGE_BYTES as u64
                    || image.width == 0
                    || image.height == 0
                    || image.width as u64 * image.height as u64 > MAX_IMAGE_PIXELS
                {
                    return Err("Unsupported or oversized image payload.".into());
                }
            }
        }
        Ok(())
    }

    #[cfg(any(test, feature = "private-sync"))]
    pub(crate) fn origin_tag(&self) -> String {
        serde_json::json!({"eventId":self.event_id,"originDeviceId":self.origin_device_id,"originSeq":self.origin_seq}).to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SyncAckStatus {
    Accepted,
    Applied,
    Superseded,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncAck {
    #[serde(rename = "type")]
    pub message_type: String,
    pub version: u32,
    pub event_id: String,
    pub status: SyncAckStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
impl SyncAck {
    #[cfg(any(test, feature = "private-sync"))]
    pub(crate) fn new(event_id: &str, status: SyncAckStatus) -> Self {
        Self {
            message_type: "ack".into(),
            version: PROTOCOL_VERSION,
            event_id: event_id.into(),
            status,
            error: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncReceivePlan {
    pub event_id: String,
    pub needs_media: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duplicate_ack: Option<SyncAck>,
}

pub fn validate_identifier(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return Err("Invalid sync identifier.".into());
    }
    Ok(())
}
fn validate_hash(value: &str) -> Result<(), String> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
    {
        return Err("Invalid SHA-256.".into());
    }
    Ok(())
}

/// Verify real encoded data before it can clear the system clipboard.
pub fn validate_image(bytes: &[u8]) -> Result<SyncImage, String> {
    if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
        return Err("Image exceeds sync size limit.".into());
    }
    let format = image::guess_format(bytes).map_err(|e| e.to_string())?;
    let mime = match format {
        image::ImageFormat::Png => {
            // An APNG contains acTL before IDAT. The general image decoder would
            // flatten it silently; the first protocol supports static images only.
            let mut offset = 8usize;
            while offset + 12 <= bytes.len() {
                let length =
                    u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
                let end = offset
                    .checked_add(length)
                    .and_then(|n| n.checked_add(12))
                    .ok_or("Invalid PNG chunk.")?;
                if end > bytes.len() {
                    return Err("Truncated PNG payload.".into());
                }
                if &bytes[offset + 4..offset + 8] == b"acTL" {
                    return Err("Animated images are not supported.".into());
                }
                offset = end;
            }
            "image/png"
        }
        image::ImageFormat::Jpeg => "image/jpeg",
        _ => return Err("Only static PNG and JPEG images are supported.".into()),
    };
    let reader = image::ImageReader::with_format(Cursor::new(bytes), format);
    let (width, height) = reader.into_dimensions().map_err(|e| e.to_string())?;
    if width == 0 || height == 0 || width as u64 * height as u64 > MAX_IMAGE_PIXELS {
        return Err("Image exceeds sync pixel limit.".into());
    }
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_IMAGE_PIXELS * 8);
    reader.limits(limits);
    reader
        .decode()
        .map_err(|e| format!("Invalid image payload: {e}"))?;
    Ok(SyncImage {
        mime_type: mime.into(),
        size_bytes: bytes.len() as u64,
        width,
        height,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SyncClient {
    pub device_id: String,
    pub name: String,
    pub token_hash: String,
    pub last_seen: Option<chrono::DateTime<chrono::Utc>>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Receipt {
    pub event_id: String,
    pub origin_device_id: String,
    pub origin_seq: u64,
    pub event_hash: String,
    pub ack: SyncAck,
}
#[cfg(any(test, feature = "private-sync"))]
fn event_hash(event: &SyncEvent) -> String {
    sha256_hex(&serde_json::to_vec(event).expect("sync event serialization"))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct DurableState {
    pub enabled: bool,
    pub receive_enabled: bool,
    pub send_enabled: bool,
    pub local_sequence: u64,
    pub clients: HashMap<String, SyncClient>,
    pub high_water: HashMap<String, u64>,
    pub receipts: VecDeque<Receipt>,
}
impl Default for DurableState {
    fn default() -> Self {
        Self {
            enabled: false,
            receive_enabled: true,
            send_enabled: true,
            local_sequence: 0,
            clients: HashMap::new(),
            high_water: HashMap::new(),
            receipts: VecDeque::new(),
        }
    }
}

#[cfg(any(test, feature = "private-sync"))]
#[derive(Clone)]
pub(crate) struct PendingEvent {
    pub event: SyncEvent,
    pub generation: u64,
    pub revision: u64,
    pub clipboard_sequence: Option<u32>,
    pub client_token_hash: Option<String>,
    #[cfg(feature = "private-sync")]
    pub relay_generation: Option<u64>,
    pub received_at: Instant,
    pub accepted: bool,
    pub applying: bool,
    pub media: Option<PathBuf>,
}
pub(crate) struct EngineState {
    pub durable: DurableState,
    pub generation: u64,
    pub revision: u64,
    pub last_native_sequence: Option<u32>,
    #[cfg(any(test, feature = "private-sync"))]
    pub pending: HashMap<String, PendingEvent>,
    pub error: Option<String>,
}
pub(crate) struct SyncRuntime {
    pub devices: std::sync::Arc<crate::device_sync::DeviceSyncRuntime>,
    #[cfg(feature = "private-sync")]
    pub relay: crate::relay_sync::Runtime,
    pub device_id: String,
    pub state_path: PathBuf,
    pub media_root: PathBuf,
    pub state: Mutex<EngineState>,
    media_operation: Mutex<()>,
    pub raw_enabled: AtomicBool,
    #[cfg(any(test, feature = "private-sync"))]
    pub events: broadcast::Sender<SyncEvent>,
}

impl SyncRuntime {
    pub(crate) fn private_capture_enabled(&self) -> bool {
        #[cfg(feature = "private-sync")]
        { return self.relay.enabled(); }
        #[cfg(not(feature = "private-sync"))]
        { false }
    }

    #[cfg(any(test, feature = "private-sync"))]
    fn private_send_enabled(&self) -> bool {
        #[cfg(feature = "private-sync")]
        { return self.relay.send_enabled(); }
        #[cfg(not(feature = "private-sync"))]
        { false }
    }

    pub(crate) fn open(device_id: String, state_path: PathBuf, media_root: PathBuf) -> Self {
        let devices = crate::device_sync::DeviceSyncRuntime::open(state_path.with_file_name("devices.json"));
        #[cfg(any(test, feature = "private-sync"))]
        let (durable, error) = match std::fs::read(&state_path) {
            Ok(bytes) => match serde_json::from_slice::<DurableState>(&bytes) {
                Ok(mut state) => {
                    // A process restart cannot finish an old accepted write. Keep
                    // its history receipt, but never replay it over current data.
                    for receipt in &mut state.receipts {
                        if receipt.ack.status == SyncAckStatus::Accepted {
                            receipt.ack.status = SyncAckStatus::Superseded;
                        }
                    }
                    (state, None)
                }
                Err(e) => (
                    DurableState::default(),
                    Some(format!("Cannot load sync state: {e}")),
                ),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (DurableState::default(), None),
            Err(e) => (DurableState::default(), Some(e.to_string())),
        };
        #[cfg(not(any(test, feature = "private-sync")))]
        let (durable, error) = (DurableState::default(), None);
        #[cfg(feature = "private-sync")]
        let relay = crate::relay_sync::Runtime::load(state_path.with_file_name("relay.json"));
        let private_enabled = false;
        #[cfg(feature = "private-sync")]
        let private_enabled = relay.enabled();
        let raw_enabled = AtomicBool::new(durable.enabled || devices.needs_raw_capture() || private_enabled);
        #[cfg(any(test, feature = "private-sync"))]
        let (events, _) = broadcast::channel(64);
        Self {
            #[cfg(feature = "private-sync")]
            relay,
            devices,
            device_id,
            state_path,
            media_root,
            raw_enabled,
            #[cfg(any(test, feature = "private-sync"))]
            events,
            media_operation: Mutex::new(()),
            state: Mutex::new(EngineState {
                durable,
                generation: 1,
                revision: 0,
                last_native_sequence: None,
                #[cfg(any(test, feature = "private-sync"))]
                pending: HashMap::new(),
                error,
            }),
        }
    }

    #[cfg(any(test, feature = "private-sync"))]
    pub(crate) fn save(&self, state: &EngineState) -> Result<(), String> {
        crate::config_file::save(&self.state_path, &state.durable)
    }

    #[cfg(any(test, feature = "private-sync"))]
    pub(crate) fn current(&self, state: &EngineState, pending: &PendingEvent) -> bool {
        self.authorized(state, pending)
            && state.generation == pending.generation
            && state.revision == pending.revision
            && pending.received_at.elapsed() < PENDING_TTL
            && pending.client_token_hash.as_ref().is_none_or(|hash| {
                state
                    .durable
                    .clients
                    .get(&pending.event.origin_device_id)
                    .is_some_and(|client| &client.token_hash == hash)
            })
    }

    #[cfg(any(test, feature = "private-sync"))]
    pub(crate) fn authorized(&self, state: &EngineState, pending: &PendingEvent) -> bool {
        #[cfg(feature = "private-sync")]
        if let Some(generation) = pending.relay_generation {
            let relay = self.relay.state.lock();
            return relay.saved.enabled && relay.saved.receive_enabled
                && relay.generation == generation
                && relay.invitation.as_ref().is_some_and(|i| i.peer_device_id == pending.event.origin_device_id);
        }
        state.durable.enabled && state.durable.receive_enabled
    }

    #[cfg(feature = "private-sync")]
    pub(crate) fn begin_relay(&self, event: SyncEvent, sequence: Option<u32>, generation: u64) -> Result<SyncReceivePlan, String> {
        event.validate()?;
        let mut state = self.state.lock();
        self.begin_locked(&mut state, event, sequence, None, Some(generation))
    }

    #[cfg(any(test, feature = "private-sync"))]
    pub(crate) fn receipt(state: &mut EngineState, event: SyncEvent, ack: SyncAck) {
        state
            .durable
            .receipts
            .retain(|r| r.event_id != event.event_id);
        state
            .durable
            .high_water
            .entry(event.origin_device_id.clone())
            .and_modify(|s| *s = (*s).max(event.origin_seq))
            .or_insert(event.origin_seq);
        state.durable.receipts.push_back(Receipt {
            event_hash: event_hash(&event),
            event_id: event.event_id,
            origin_device_id: event.origin_device_id,
            origin_seq: event.origin_seq,
            ack,
        });
        while state.durable.receipts.len() > MAX_RECEIPTS {
            state.durable.receipts.pop_front();
        }
    }

    #[cfg(any(test, feature = "private-sync"))]
    pub(crate) fn begin(
        &self,
        event: SyncEvent,
        sequence: Option<u32>,
    ) -> Result<SyncReceivePlan, String> {
        event.validate()?;
        let mut state = self.state.lock();
        self.begin_locked(&mut state, event, sequence, None, None)
    }

    #[cfg(any(test, feature = "private-sync"))]
    pub(crate) fn begin_authenticated(
        &self,
        event: SyncEvent,
        sequence: Option<u32>,
        device: Option<&str>,
        authorization: Option<&str>,
    ) -> Result<SyncReceivePlan, String> {
        event.validate()?;
        let mut state = self.state.lock();
        let id = authenticate_locked(&state, device, authorization)?;
        if event.origin_device_id != id {
            return Err("Event origin does not match paired device.".into());
        }
        let token_hash = state.durable.clients[&id].token_hash.clone();
        self.begin_locked(&mut state, event, sequence, Some(token_hash), None)
    }

    #[cfg(any(test, feature = "private-sync"))]
    fn begin_locked(
        &self,
        state: &mut EngineState,
        event: SyncEvent,
        sequence: Option<u32>,
        client_token_hash: Option<String>,
        relay_generation: Option<u64>,
    ) -> Result<SyncReceivePlan, String> {
        #[cfg(feature = "private-sync")]
        if let Some(generation) = relay_generation {
            let relay = self.relay.state.lock();
            if !relay.saved.enabled || !relay.saved.receive_enabled || relay.generation != generation
                || relay.invitation.as_ref().is_none_or(|i| i.peer_device_id != event.origin_device_id) {
                return Err("Relay receiving permission changed.".into());
            }
        } else if !state.durable.enabled || !state.durable.receive_enabled {
            return Err("Receiving clipboard sync is disabled.".into());
        }
        #[cfg(not(feature = "private-sync"))]
        if !state.durable.enabled || !state.durable.receive_enabled || relay_generation.is_some() {
            return Err("Receiving clipboard sync is disabled.".into());
        }
        if event.origin_device_id == self.device_id {
            return Ok(SyncReceivePlan {
                event_id: event.event_id.clone(),
                needs_media: false,
                duplicate_ack: Some(SyncAck::new(&event.event_id, SyncAckStatus::Superseded)),
            });
        }
        if let Some(receipt) = state
            .durable
            .receipts
            .iter()
            .find(|r| r.event_id == event.event_id)
        {
            if receipt.event_hash != event_hash(&event) {
                return Err("Sync event identity was reused for a different payload.".into());
            }
            return Ok(SyncReceivePlan {
                event_id: event.event_id,
                needs_media: false,
                duplicate_ack: Some(receipt.ack.clone()),
            });
        }
        state
            .pending
            .retain(|_, p| p.received_at.elapsed() < PENDING_TTL);
        if let Some(pending) = state.pending.get(&event.event_id) {
            if pending.event != event {
                return Err("Sync event identity was reused for a different payload.".into());
            }
            return Ok(SyncReceivePlan {
                event_id: event.event_id,
                needs_media: pending.event.kind == SyncKind::Image && !pending.accepted,
                duplicate_ack: pending
                    .accepted
                    .then(|| SyncAck::new(&pending.event.event_id, SyncAckStatus::Accepted)),
            });
        }
        let newest = state
            .pending
            .values()
            .filter(|p| p.event.origin_device_id == event.origin_device_id)
            .map(|p| p.event.origin_seq)
            .chain(
                state
                    .durable
                    .high_water
                    .get(&event.origin_device_id)
                    .copied(),
            )
            .max()
            .unwrap_or(0);
        if event.origin_seq <= newest {
            return Ok(SyncReceivePlan {
                event_id: event.event_id.clone(),
                needs_media: false,
                duplicate_ack: Some(SyncAck::new(&event.event_id, SyncAckStatus::Superseded)),
            });
        }
        if state.pending.len() >= MAX_PENDING {
            return Err("Too many pending clipboard events.".into());
        }
        state.revision = state.revision.wrapping_add(1);
        let pending = PendingEvent {
            event: event.clone(),
            generation: state.generation,
            revision: state.revision,
            clipboard_sequence: sequence,
            client_token_hash,
            #[cfg(feature = "private-sync")]
            relay_generation,
            received_at: Instant::now(),
            accepted: false,
            applying: false,
            media: None,
        };
        state.pending.insert(event.event_id.clone(), pending);
        Ok(SyncReceivePlan {
            event_id: event.event_id,
            needs_media: event.kind == SyncKind::Image,
            duplicate_ack: None,
        })
    }

    pub(crate) fn media_path(&self, hash: &str, mime: &str) -> Result<PathBuf, String> {
        validate_hash(hash)?;
        let extension = match mime {
            "image/png" => "png",
            "image/jpeg" => "jpg",
            _ => return Err("Unsupported image type.".into()),
        };
        Ok(self.media_root.join(format!("{hash}.{extension}")))
    }

    pub(crate) fn store_media(
        &self,
        hash: &str,
        metadata: &SyncImage,
        bytes: &[u8],
    ) -> Result<PathBuf, String> {
        if sha256_hex(bytes) != hash || &validate_image(bytes)? != metadata {
            return Err("Image metadata or hash mismatch.".into());
        }
        self.store_validated_media(hash, metadata, bytes)
    }

    pub(crate) fn store_validated_media(
        &self,
        hash: &str,
        metadata: &SyncImage,
        bytes: &[u8],
    ) -> Result<PathBuf, String> {
        let _operation = self.media_operation.lock();
        let path = self.media_path(hash, &metadata.mime_type)?;
        if path.is_file() {
            if sha256_hex(&std::fs::read(&path).map_err(|e| e.to_string())?) == hash {
                return Ok(path);
            }
        }
        std::fs::create_dir_all(&self.media_root).map_err(|e| e.to_string())?;
        let used = std::fs::read_dir(&self.media_root)
            .map_err(|e| e.to_string())?
            .filter_map(Result::ok)
            .filter_map(|entry| entry.metadata().ok())
            .filter(|metadata| metadata.is_file())
            .fold(0u64, |total, metadata| total.saturating_add(metadata.len()));
        if used.saturating_add(bytes.len() as u64) > MAX_MEDIA_CACHE_BYTES {
            return Err("Clipboard sync media storage reached its 512 MiB limit.".into());
        }
        let mut staged =
            tempfile::NamedTempFile::new_in(&self.media_root).map_err(|e| e.to_string())?;
        staged.write_all(bytes).map_err(|e| e.to_string())?;
        staged.as_file().sync_all().map_err(|e| e.to_string())?;
        staged.persist(&path).map_err(|e| e.to_string())?;
        Ok(path)
    }

    pub(crate) fn observe(
        &self,
        sequence: u32,
        snapshot: &ClipboardSnapshot,
    ) -> Result<Option<SyncEvent>, String> {
        if !self.raw_enabled.load(Ordering::Relaxed) {
            return Ok(None);
        }
        // Capture the sending grant before origin/image validation or LAN work
        // can block. A later enable/restart must not adopt this older snapshot.
        #[cfg(feature = "private-sync")]
        let relay_generation = self.relay.sending_generation();
        let mut checked;
        let snapshot = if crate::device_sync::origin_needs_validation(snapshot) {
            checked = snapshot.clone();
            crate::device_sync::validate_snapshot_origin(&mut checked);
            &checked
        } else { snapshot };
        if let Err(error) = self.devices.observe(self, sequence, snapshot) {
            self.devices.state.lock().errors.insert("capture".into(),error.clone());
            tracing::warn!(%error,"Device sync capture failed");
        } else {
            self.devices.state.lock().errors.remove("capture");
        }
        #[cfg(not(any(test, feature = "private-sync")))]
        { return Ok(None); }
        #[cfg(any(test, feature = "private-sync"))]
        {
        // The origin format survives delayed rendering; no time-based exclusion
        // window can swallow a real copy made immediately after remote apply.
        if snapshot.sync_origin.is_some() {
            return Ok(None);
        }
        let (generation, revision) = {
            let mut state = self.state.lock();
            if !state.durable.enabled && !self.private_capture_enabled() {
                return Ok(None);
            }
            if sequence != 0 && state.last_native_sequence == Some(sequence) {
                return Ok(None);
            }
            state.last_native_sequence = (sequence != 0).then_some(sequence);
            state.revision = state.revision.wrapping_add(1);
            if !(state.durable.enabled && state.durable.send_enabled) && !self.private_send_enabled() {
                return Ok(None);
            }
            (state.generation, state.revision)
        };
        if snapshot.exclude_monitor
            || !snapshot.files.is_empty()
            || snapshot.quicker_action.is_some()
            || snapshot.quicker_steps.is_some()
        {
            return Ok(None);
        }
        let image = if let Some(bytes) = snapshot.png.as_deref() {
            let metadata = validate_image(bytes)?;
            let hash = sha256_hex(bytes);
            self.store_validated_media(&hash, &metadata, bytes)?;
            Some((metadata, hash))
        } else {
            None
        };
        if image.is_none() && snapshot.text.is_none() {
            return Ok(None);
        }
        let mut state = self.state.lock();
        if (!(state.durable.enabled && state.durable.send_enabled) && !self.private_send_enabled())
            || state.generation != generation
            || state.revision != revision
        {
            return Ok(None);
        }
        // New events are sequence identities, even when their content is equal.
        let previous = state.durable.local_sequence;
        state.durable.local_sequence = previous
            .checked_add(1)
            .ok_or("Clipboard sync sequence exhausted.")?;
        let event = if let Some((metadata, hash)) = image {
            SyncEvent {
                message_type: "clipboard".into(),
                version: PROTOCOL_VERSION,
                event_id: uuid::Uuid::new_v4().to_string(),
                origin_device_id: self.device_id.clone(),
                origin_seq: state.durable.local_sequence,
                kind: SyncKind::Image,
                sha256: hash,
                text: None,
                image: Some(metadata),
            }
        } else {
            SyncEvent::text(
                uuid::Uuid::new_v4().to_string(),
                self.device_id.clone(),
                state.durable.local_sequence,
                snapshot.text.clone().unwrap(),
            )
        };
        event.validate()?;
        if let Err(error) = self.save(&state) {
            state.durable.local_sequence = previous;
            return Err(error);
        }
        let _ = self.events.send(event.clone());
        #[cfg(feature = "private-sync")]
        self.relay.publish_captured(&event,relay_generation);
        Ok(Some(event))
        }
    }
}

#[cfg(any(test, feature = "private-sync"))]
pub(crate) fn authenticate_locked(
    state: &EngineState,
    device_id: Option<&str>,
    authorization: Option<&str>,
) -> Result<String, String> {
    let id = device_id.ok_or("Sync device header missing.")?;
    let token = authorization
        .and_then(|a| a.strip_prefix("Bearer "))
        .ok_or("Sync bearer token missing.")?;
    crate::lan::validate_token(token)?;
    if !state.durable.enabled {
        return Err("Clipboard sync is disabled.".into());
    }
    let client = state
        .durable
        .clients
        .get(id)
        .ok_or("Sync client is not paired.")?;
    if !crate::lan::fixed_time_eq_hex(&client.token_hash, &sha256_hex(token.as_bytes())) {
        return Err("Invalid sync credentials.".into());
    }
    Ok(id.into())
}

#[cfg(any(test, feature = "private-sync"))]
pub(crate) fn invalidate_client_pending(state: &mut EngineState, id: &str) {
    for receipt in &mut state.durable.receipts {
        if receipt.origin_device_id == id && receipt.ack.status == SyncAckStatus::Accepted {
            receipt.ack.status = SyncAckStatus::Superseded;
        }
    }
    state
        .pending
        .retain(|_, pending| pending.event.origin_device_id != id);
}

pub(crate) fn read_media(runtime: &SyncRuntime, hash: &str) -> Result<PathBuf, String> {
    validate_hash(hash)?;
    for mime in ["image/png", "image/jpeg"] {
        let path = runtime.media_path(hash, mime)?;
        if Path::new(&path).is_file() {
            return Ok(path);
        }
    }
    Err("Sync media not found.".into())
}

#[cfg(all(test, feature = "private-sync"))]
mod relay_capture_tests {
    use super::*;
    use std::sync::Arc;
    fn runtime(relay_enabled:bool)->(tempfile::TempDir,Arc<SyncRuntime>) {
        let dir=tempfile::tempdir().unwrap();
        let sync=Arc::new(SyncRuntime::open("capture-desktop".into(),dir.path().join("state.json"),dir.path().join("media")));
        {let mut state=sync.state.lock();state.durable.enabled=true;state.durable.send_enabled=true;}
        {let mut state=sync.relay.state.lock();state.saved.enabled=relay_enabled;state.saved.send_enabled=true;
        state.invitation=Some(serde_json::from_value(serde_json::json!({"type":"cliprelay-invitation","version":1,"baseUrl":"https://relay.example.test/cliprelay","certificateSha256":"11".repeat(32),"roomId":"capture-room","deviceId":"capture-desktop","peerDeviceId":"capture-phone","token":"22".repeat(32),"keyHex":"33".repeat(32)})).unwrap());}
        sync.raw_enabled.store(true,Ordering::Relaxed);(dir,sync)
    }
    #[test]
    fn relay_capture_generation_rejects_slow_image_across_stop_start_without_revoking_lan() {
        let (_dir,sync)=runtime(true);let mut relay_events=sync.relay.events.subscribe();let mut lan_events=sync.events.subscribe();
        let mut png=Cursor::new(Vec::new());image::DynamicImage::new_rgba8(2,2).write_to(&mut png,image::ImageFormat::Png).unwrap();
        let snapshot=ClipboardSnapshot {png:Some(png.into_inner()),..Default::default()};
        let gate=sync.media_operation.lock();let capture=sync.clone();
        let worker=std::thread::spawn(move||capture.observe(1,&snapshot));
        let started=Instant::now();
        while sync.state.lock().revision==0 {assert!(started.elapsed()<Duration::from_secs(5),"capture did not enter its reserved revision");std::thread::sleep(Duration::from_millis(5));}
        let mobile_generation=sync.state.lock().generation;
        {let mut state=sync.relay.state.lock();state.saved.enabled=false;state.generation+=1;state.saved.enabled=true;state.generation+=1;}
        drop(gate);let old=worker.join().unwrap().unwrap().unwrap();
        assert_eq!(lan_events.try_recv().unwrap().event_id,old.event_id,"original mobile/LAN broadcast is preserved");
        assert!(matches!(relay_events.try_recv(),Err(broadcast::error::TryRecvError::Empty)),"a new Relay grant must not adopt a prior slow snapshot");
        assert_eq!(sync.state.lock().generation,mobile_generation,"Relay does not revoke unrelated mobile receive tickets");
        let next=sync.observe(2,&ClipboardSnapshot {text:Some("new copy under new grant".into()),..Default::default()}).unwrap().unwrap();
        let emitted=relay_events.try_recv().unwrap();assert_eq!(emitted.event.event_id,next.event_id);assert_eq!(emitted.generation,sync.relay.sending_generation().unwrap());
    }
    #[test]
    fn relay_capture_does_not_adopt_an_existing_lan_event_when_later_enabled() {
        let (_dir,sync)=runtime(false);let mut relay_events=sync.relay.events.subscribe();let mut lan_events=sync.events.subscribe();
        let original=sync.observe(1,&ClipboardSnapshot {text:Some("LAN-only copy".into()),..Default::default()}).unwrap().unwrap();
        assert_eq!(lan_events.try_recv().unwrap().event_id,original.event_id);
        {let mut state=sync.relay.state.lock();state.saved.enabled=true;state.generation+=1;}
        assert!(matches!(relay_events.try_recv(),Err(broadcast::error::TryRecvError::Empty)));
        let new=sync.observe(2,&ClipboardSnapshot {text:Some("copy after enabling Relay".into()),..Default::default()}).unwrap().unwrap();
        assert_eq!(relay_events.try_recv().unwrap().event.event_id,new.event_id);
    }
}
