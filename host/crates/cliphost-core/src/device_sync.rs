//! Windows peer synchronization. Trusted LAN peers synchronize by default;
//! the master switch controls automatic effects. No clipboard contents
//! are retained in the configuration/receipt file.
use crate::{
    clipboard::ClipboardSnapshot,
    clipboard_sync::{SyncEvent, SyncKind, SyncRuntime},
    model::sha256_hex,
};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

pub const PREFIX: &str = "/cliplink/device-sync/v1";
pub(crate) const TTL: Duration = Duration::from_secs(120);

#[path = "device_sync_image.rs"]
mod image_identity;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DeviceSyncPolicy {
    pub send_enabled: bool,
    pub receive_enabled: bool,
    pub text_enabled: bool,
    pub image_enabled: bool,
    pub apply_clipboard: bool,
    pub save_history: bool,
}
impl Default for DeviceSyncPolicy {
    fn default() -> Self {
        Self {
            send_enabled: true,
            receive_enabled: true,
            text_enabled: true,
            image_enabled: true,
            apply_clipboard: true,
            save_history: true,
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct DeviceReceipt {
    pub origin: String,
    pub sequence: u64,
    pub id: String,
    pub hash: String,
    pub ack: Value,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct DeviceDurable {
    pub enabled: bool,
    pub text_enabled: bool,
    pub image_enabled: bool,
    pub sequence: u64,
    pub peers: HashMap<String, DeviceSyncPolicy>,
    pub receipts: VecDeque<DeviceReceipt>,
    pub high_water: HashMap<String, u64>,
}
impl Default for DeviceDurable {
    fn default() -> Self {
        Self {
            enabled: true,
            text_enabled: true,
            image_enabled: true,
            sequence: 0,
            peers: HashMap::new(),
            receipts: VecDeque::new(),
            high_water: HashMap::new(),
        }
    }
}
impl DeviceDurable {
    pub fn allows(&self, kind: SyncKind) -> bool {
        match kind {
            SyncKind::Text => self.text_enabled,
            SyncKind::Image => self.image_enabled,
        }
    }
}
#[derive(Clone)]
pub(crate) struct DevicePending {
    pub event: SyncEvent,
    pub policy: DeviceSyncPolicy,
    pub token_hash: String,
    pub fingerprint: String,
    pub generation: u64,
    pub revision: u64,
    pub native_sequence: Option<u32>,
    pub started: Instant,
    pub replay: bool,
}
#[derive(Clone)]
pub(crate) struct Delivery {
    pub event: SyncEvent,
    pub content_key: String,
    pub started: Instant,
    pub due: Instant,
    pub retried: bool,
    pub generation: u64,
}
pub(crate) struct DeviceState {
    pub durable: DeviceDurable,
    pub generation: u64,
    pub revision: u64,
    pub native_sequence: Option<u32>,
    pub current_key: Option<String>,
    pub pending: HashMap<String, DevicePending>,
    pub outgoing: HashMap<String, Delivery>,
    pub in_flight: HashSet<String>,
    pub errors: HashMap<String, String>,
    pub error: Option<String>,
}
pub(crate) struct DeviceSyncRuntime {
    pub changes: tokio::sync::broadcast::Sender<()>,
    pub state: Mutex<DeviceState>,
    pub receive_operation: Mutex<()>,
    pub path: PathBuf,
}
impl DeviceSyncRuntime {
    pub fn open(path: PathBuf) -> Arc<Self> {
        let (mut durable, error) = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(value) => (value, None),
                Err(e) => (
                    DeviceDurable::default(),
                    Some(format!("Cannot load device sync settings: {e}")),
                ),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (DeviceDurable::default(), None),
            Err(e) => (DeviceDurable::default(), Some(e.to_string())),
        };
        if error.is_some() {
            durable.enabled = false;
        }
        // Old directional/content/action switches are retained for wire and
        // file compatibility. The single master now controls synchronization.
        durable
            .peers
            .values_mut()
            .for_each(|policy| *policy = DeviceSyncPolicy::default());
        Arc::new(Self {
            changes: tokio::sync::broadcast::channel(16).0,
            path,
            receive_operation: Mutex::new(()),
            state: Mutex::new(DeviceState {
                durable,
                generation: 1,
                revision: 0,
                native_sequence: None,
                current_key: None,
                pending: HashMap::new(),
                outgoing: HashMap::new(),
                in_flight: HashSet::new(),
                errors: HashMap::new(),
                error,
            }),
        })
    }
    pub fn enabled(&self) -> bool {
        self.state.lock().durable.enabled
    }
    pub fn needs_raw_capture(&self) -> bool {
        let state = self.state.lock();
        state.durable.enabled
            && (state.durable.text_enabled || state.durable.image_enabled)
            && !state.durable.peers.is_empty()
    }
    pub fn register_paired_peers<'a>(&self, ids: impl Iterator<Item = &'a str>) {
        let mut state = self.state.lock();
        if state.error.is_none() {
            for id in ids {
                state
                    .durable
                    .peers
                    .insert(id.into(), DeviceSyncPolicy::default());
            }
        }
    }
    pub fn save(&self, state: &DeviceState) -> Result<(), String> {
        crate::config_file::save(&self.path, &state.durable)
    }
    pub fn observe(
        &self,
        parent: &SyncRuntime,
        sequence: u32,
        snapshot: &ClipboardSnapshot,
    ) -> Result<(), String> {
        let (generation, revision) = {
            let mut state = self.state.lock();
            if !state.durable.enabled {
                return Ok(());
            }
            if sequence != 0 && state.native_sequence == Some(sequence) {
                return Ok(());
            }
            state.native_sequence = (sequence != 0).then_some(sequence);
            // Every actual change invalidates an older receive, including a
            // file/unsupported/disabled-type copy. Never order by wall clock.
            state.revision = state.revision.wrapping_add(1);
            (state.generation, state.revision)
        };
        let key = match content_key(snapshot) {
            Ok(key) => key,
            Err(error) => {
                let mut state = self.state.lock();
                if state.generation == generation && state.revision == revision {
                    state.current_key = None;
                    state.outgoing.clear();
                }
                return Err(error);
            }
        };
        let mut state = self.state.lock();
        if state.generation != generation || state.revision != revision {
            return Ok(());
        }
        if snapshot.sync_origin.is_some() {
            state.current_key = key;
            state.outgoing.clear();
            return Ok(());
        }
        if key.is_some() && key == state.current_key {
            return Ok(());
        }
        state.current_key = key;
        if state.current_key.is_none() {
            state.outgoing.clear();
            return Ok(());
        }
        let kind = if snapshot.png.is_some() {
            SyncKind::Image
        } else {
            SyncKind::Text
        };
        if !state.durable.allows(kind) {
            state.outgoing.clear();
            return Ok(());
        }
        let peers: Vec<_> = state.durable.peers.keys().cloned().collect();
        if peers.is_empty() {
            state.outgoing.clear();
            return Ok(());
        }
        drop(state);
        let image = if let Some(bytes) = snapshot.png.as_deref() {
            let metadata = crate::clipboard_sync::validate_image(bytes)?;
            let hash = sha256_hex(bytes);
            parent.store_validated_media(&hash, &metadata, bytes)?;
            Some((metadata, hash))
        } else {
            None
        };
        let mut state = self.state.lock();
        if state.generation != generation || state.revision != revision {
            return Ok(());
        }
        let previous = state.durable.sequence;
        state.durable.sequence = previous
            .checked_add(1)
            .ok_or("Device sync sequence exhausted.")?;
        let event = if let Some((metadata, hash)) = image {
            SyncEvent {
                message_type: "clipboard".into(),
                version: 1,
                event_id: uuid::Uuid::new_v4().to_string(),
                origin_device_id: parent.device_id.clone(),
                origin_seq: state.durable.sequence,
                kind,
                sha256: hash,
                text: None,
                image: Some(metadata),
            }
        } else {
            SyncEvent::text(
                uuid::Uuid::new_v4().to_string(),
                parent.device_id.clone(),
                state.durable.sequence,
                snapshot.text.clone().unwrap(),
            )
        };
        event.validate()?;
        if let Err(error) = self.save(&state) {
            state.durable.sequence = previous;
            state.current_key = None;
            return Err(error);
        }
        let content_key = state.current_key.clone().unwrap();
        for id in peers {
            state.outgoing.insert(
                id,
                Delivery {
                    event: event.clone(),
                    content_key: content_key.clone(),
                    started: Instant::now(),
                    due: Instant::now(),
                    retried: false,
                    generation,
                },
            );
        }
        let _ = self.changes.send(());
        Ok(())
    }
}
pub(crate) fn pending_key(origin: &str, id: &str) -> String {
    format!("{origin}:{id}")
}
pub(crate) fn event_hash(event: &SyncEvent) -> String {
    sha256_hex(&serde_json::to_vec(event).expect("sync serialization"))
}
pub(crate) fn device_origin(event: &SyncEvent, key: &str) -> String {
    serde_json::json!({"deviceSyncVersion":1,"eventId":event.event_id,"originDeviceId":event.origin_device_id,"originSeq":event.origin_seq,"contentKey":key}).to_string()
}
pub(crate) fn origin_needs_validation(snapshot: &ClipboardSnapshot) -> bool {
    snapshot
        .sync_origin
        .as_ref()
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .is_some_and(|v| v["deviceSyncVersion"] == 1)
}
pub(crate) fn validate_snapshot_origin(snapshot: &mut ClipboardSnapshot) {
    if !origin_needs_validation(snapshot) {
        return;
    }
    let expected = snapshot
        .sync_origin
        .as_ref()
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .and_then(|v| v["contentKey"].as_str().map(str::to_owned));
    if content_key(snapshot).ok().flatten() != expected {
        snapshot.sync_origin = None;
    }
}
/// Strict transport equality: original text bytes; supported 8-bit PNG
/// dimensions/pixels; otherwise the original image bytes. Never perceptual.
pub(crate) fn content_key(snapshot: &ClipboardSnapshot) -> Result<Option<String>, String> {
    if snapshot.exclude_monitor
        || !snapshot.files.is_empty()
        || snapshot.quicker_action.is_some()
        || snapshot.quicker_steps.is_some()
    {
        return Ok(None);
    }
    if let Some(bytes) = snapshot.png.as_deref() {
        return image_key(bytes).map(Some);
    }
    Ok(snapshot
        .text
        .as_ref()
        .filter(|s| s.len() <= crate::clipboard_sync::MAX_TEXT_BYTES && !s.contains('\0'))
        .map(|s| format!("text:{}", sha256_hex(s.as_bytes()))))
}
pub(crate) fn image_key(bytes: &[u8]) -> Result<String, String> {
    image_identity::image_key(bytes)
}
