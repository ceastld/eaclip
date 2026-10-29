//! ClipRelay v1 cryptography and independent opt-in configuration.
//! Content keys and relay tokens are protected with current-user Windows DPAPI.
use crate::clipboard_sync::{SyncAck, SyncEvent, SyncKind};
use parking_lot::Mutex;
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{path::PathBuf, sync::atomic::AtomicBool};

pub const MAX_PACKET: usize = 34 * 1024 * 1024;
const MAX_HEADER: usize = 7 * 1024 * 1024;
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Invitation {
    #[serde(rename = "type")]
    pub kind: String,
    pub version: u32,
    pub base_url: String,
    #[serde(default = "default_send_sni")]
    pub send_sni: bool,
    pub certificate_sha256: String,
    pub room_id: String,
    pub device_id: String,
    pub peer_device_id: String,
    pub token: String,
    pub key_hex: String,
}
fn default_send_sni() -> bool {
    true
}
impl Invitation {
    pub fn validate(&self) -> Result<(), String> {
        if self.kind != "cliprelay-invitation" || self.version != 1 {
            return Err("Unsupported Relay invitation.".into());
        }
        let url = url::Url::parse(&self.base_url).map_err(|_| "Invalid Relay URL.")?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || self.base_url.ends_with('/')
        {
            return Err(
                "Relay URL must be HTTPS without credentials, query, fragment or trailing slash."
                    .into(),
            );
        }
        for id in [&self.room_id, &self.device_id, &self.peer_device_id] {
            crate::clipboard_sync::validate_identifier(id)?;
        }
        if self.device_id == self.peer_device_id {
            return Err("Relay peer must be another device.".into());
        }
        for secret in [&self.token, &self.key_hex, &self.certificate_sha256] {
            decode_hex32(secret)?;
        }
        Ok(())
    }
    pub fn aad(&self, sender: &str, recipient: &str, id: &str) -> Result<Vec<u8>, String> {
        for part in [sender, recipient, id] {
            crate::clipboard_sync::validate_identifier(part)?;
        }
        Ok(format!(
            "cliprelay-v1\n{}\n{sender}\n{recipient}\n{id}",
            self.room_id
        )
        .into_bytes())
    }
    pub fn seal(&self, id: &str, header: &Value, media: &[u8]) -> Result<Vec<u8>, String> {
        let mut nonce = [0u8; 12];
        getrandom::getrandom(&mut nonce).map_err(|_| "Relay random source failed.")?;
        self.seal_with_nonce(id, header, media, nonce)
    }
    fn seal_with_nonce(
        &self,
        id: &str,
        header: &Value,
        media: &[u8],
        nonce: [u8; 12],
    ) -> Result<Vec<u8>, String> {
        let header = serde_json::to_vec(header).map_err(|_| "Invalid Relay header.")?;
        if header.len() > MAX_HEADER || 4 + header.len() + media.len() + 28 > MAX_PACKET {
            return Err("Relay packet exceeds limit.".into());
        }
        let mut plain = Vec::with_capacity(4 + header.len() + media.len() + 16);
        plain.extend_from_slice(&(header.len() as u32).to_be_bytes());
        plain.extend_from_slice(&header);
        plain.extend_from_slice(media);
        let key = LessSafeKey::new(
            UnboundKey::new(&AES_256_GCM, &decode_hex32(&self.key_hex)?)
                .map_err(|_| "Invalid Relay key.")?,
        );
        key.seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(self.aad(&self.device_id, &self.peer_device_id, id)?),
            &mut plain,
        )
        .map_err(|_| "Relay encryption failed.")?;
        let mut packet = nonce.to_vec();
        packet.extend_from_slice(&plain);
        Ok(packet)
    }
    pub fn open(&self, sender: &str, id: &str, packet: &[u8]) -> Result<(Value, Vec<u8>), String> {
        if sender != self.peer_device_id || packet.len() < 32 || packet.len() > MAX_PACKET {
            return Err("Invalid Relay packet.".into());
        }
        let nonce: [u8; 12] = packet[..12].try_into().unwrap();
        let key = LessSafeKey::new(
            UnboundKey::new(&AES_256_GCM, &decode_hex32(&self.key_hex)?)
                .map_err(|_| "Invalid Relay key.")?,
        );
        let mut bytes = packet[12..].to_vec();
        let plain = key
            .open_in_place(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(self.aad(sender, &self.device_id, id)?),
                &mut bytes,
            )
            .map_err(|_| "Relay authentication failed.")?;
        if plain.len() < 4 {
            return Err("Invalid Relay frame.".into());
        }
        let size = u32::from_be_bytes(plain[..4].try_into().unwrap()) as usize;
        if size > MAX_HEADER || size > plain.len() - 4 {
            return Err("Invalid Relay frame length.".into());
        }
        let header: Value =
            serde_json::from_slice(&plain[4..4 + size]).map_err(|_| "Invalid Relay header.")?;
        let media = plain[4 + size..].to_vec();
        match header["type"].as_str() {
            Some("event") => {
                let event: SyncEvent = serde_json::from_value(header["event"].clone())
                    .map_err(|_| "Invalid Relay event.")?;
                event.validate()?;
                if event.event_id != id || event.origin_device_id != sender {
                    return Err("Relay event identity mismatch.".into());
                }
                match event.kind {
                    SyncKind::Text if !media.is_empty() => {
                        return Err("Text event has media.".into())
                    }
                    SyncKind::Image => {
                        if crate::model::sha256_hex(&media) != event.sha256
                            || Some(crate::clipboard_sync::validate_image(&media)?) != event.image
                        {
                            return Err("Relay image mismatch.".into());
                        }
                    }
                    _ => {}
                }
                if !header["replay"].is_boolean() {
                    return Err("Relay replay flag missing.".into());
                }
            }
            Some("ack") if media.is_empty() => {
                let ack: SyncAck = serde_json::from_value(header["ack"].clone())
                    .map_err(|_| "Invalid Relay ack.")?;
                if ack.message_type != "ack" || ack.version != 1 {
                    return Err("Unsupported Relay ack.".into());
                }
                crate::clipboard_sync::validate_identifier(&ack.event_id)?;
            }
            _ => return Err("Unsupported Relay frame.".into()),
        }
        Ok((header, media))
    }
}
pub fn decode_hex32(value: &str) -> Result<[u8; 32], String> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("Invalid Relay hexadecimal field.".into());
    }
    let mut out = [0; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[i * 2..i * 2 + 2], 16)
            .map_err(|_| "Invalid Relay hexadecimal field.")?;
    }
    Ok(out)
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct Saved {
    pub enabled: bool,
    pub send_enabled: bool,
    pub receive_enabled: bool,
    pub protected_invitation: Option<String>,
    pub cursor: u64,
    pub pending_ack: Option<PendingAck>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingAck {
    pub id: String,
    pub packet: Vec<u8>,
    pub cursor: u64,
}
pub(crate) struct State {
    pub saved: Saved,
    pub invitation: Option<Invitation>,
    pub generation: u64,
    pub error: Option<String>,
    pub last_ack: Option<Value>,
}
/// A local copy belongs only to the Relay sending grant that observed it.
#[derive(Clone)]
pub struct CapturedEvent {
    pub event: SyncEvent,
    pub generation: u64,
}
pub(crate) struct Runtime {
    pub path: PathBuf,
    pub state: Mutex<State>,
    pub worker_started: AtomicBool,
    pub events: tokio::sync::broadcast::Sender<CapturedEvent>,
}
impl Runtime {
    pub fn load(path: PathBuf) -> Self {
        let read = std::fs::read(&path);
        let loaded = match read {
            Ok(bytes) => serde_json::from_slice::<Saved>(&bytes)
                .map_err(|_| "Cannot load Relay settings.".to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Saved::default()),
            Err(_) => Err("Cannot read Relay settings.".to_string()),
        };
        let (saved, mut error) = match loaded {
            Ok(s) => (s, None),
            Err(e) => (Saved::default(), Some(e)),
        };
        let invitation = saved.protected_invitation.as_ref().and_then(|s| {
            match unprotect(s)
                .and_then(|bytes| {
                    serde_json::from_slice::<Invitation>(&bytes)
                        .map_err(|_| "Invalid protected Relay invitation.".into())
                })
                .and_then(|i| {
                    i.validate()?;
                    Ok(i)
                }) {
                Ok(i) => Some(i),
                Err(e) => {
                    error = Some(e);
                    None
                }
            }
        });
        let (events, _) = tokio::sync::broadcast::channel(64);
        Self {
            events,
            path,
            state: Mutex::new(State {
                saved,
                invitation,
                generation: 1,
                error,
                last_ack: None,
            }),
            worker_started: AtomicBool::new(false),
        }
    }
    pub fn enabled(&self) -> bool {
        let s = self.state.lock();
        s.saved.enabled && s.invitation.is_some()
    }
    pub fn sending_generation(&self) -> Option<u64> {
        let state = self.state.lock();
        (state.saved.enabled && state.saved.send_enabled && state.invitation.is_some())
            .then_some(state.generation)
    }
    pub fn publish_captured(&self, event: &SyncEvent, captured: Option<u64>) {
        let state = self.state.lock();
        if captured == Some(state.generation)
            && state.saved.enabled
            && state.saved.send_enabled
            && state.invitation.is_some()
        {
            let _ = self.events.send(CapturedEvent {
                event: event.clone(),
                generation: state.generation,
            });
        }
    }
    pub fn send_enabled(&self) -> bool {
        let s = self.state.lock();
        s.saved.enabled && s.saved.send_enabled && s.invitation.is_some()
    }
    pub fn save(&self, saved: &Saved) -> Result<(), String> {
        crate::config_file::save(&self.path, saved)
    }
}

#[cfg(windows)]
fn protect(bytes: &[u8]) -> Result<String, String> {
    use base64::Engine;
    use windows::Win32::{
        Foundation::{LocalFree, HLOCAL},
        Security::Cryptography::*,
    };
    let input = CRYPT_INTEGER_BLOB {
        cbData: bytes.len() as u32,
        pbData: bytes.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptProtectData(
            &input,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
        .map_err(|_| "Cannot protect Relay credentials.")?;
        let result = base64::engine::general_purpose::STANDARD.encode(std::slice::from_raw_parts(
            output.pbData,
            output.cbData as usize,
        ));
        let _ = LocalFree(HLOCAL(output.pbData.cast()));
        Ok(result)
    }
}
#[cfg(windows)]
fn unprotect(encoded: &str) -> Result<Vec<u8>, String> {
    use base64::Engine;
    use windows::Win32::{
        Foundation::{LocalFree, HLOCAL},
        Security::Cryptography::*,
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| "Invalid protected Relay credentials.")?;
    let input = CRYPT_INTEGER_BLOB {
        cbData: bytes.len() as u32,
        pbData: bytes.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptUnprotectData(
            &input,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
        .map_err(|_| "Cannot unlock Relay credentials for this Windows user.")?;
        let result = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        std::ptr::write_bytes(output.pbData, 0, output.cbData as usize);
        let _ = LocalFree(HLOCAL(output.pbData.cast()));
        Ok(result)
    }
}
#[cfg(not(windows))]
fn protect(_bytes: &[u8]) -> Result<String, String> {
    Err("Relay credential storage currently requires Windows DPAPI.".into())
}
#[cfg(not(windows))]
fn unprotect(_encoded: &str) -> Result<Vec<u8>, String> {
    Err("Relay credential storage currently requires Windows DPAPI.".into())
}

impl crate::HostState {
    pub fn relay_status(&self) -> Value {
        let s = self.relay_runtime().state.lock();
        json!({"version":1,"deviceId":self.sync_device_id(),"configured":s.invitation.is_some(),"enabled":s.saved.enabled,"sendEnabled":s.saved.send_enabled,"receiveEnabled":s.saved.receive_enabled,"baseUrl":s.invitation.as_ref().map(|i|&i.base_url),"roomId":s.invitation.as_ref().map(|i|&i.room_id),"peerDeviceId":s.invitation.as_ref().map(|i|&i.peer_device_id),"cursor":s.saved.cursor,"error":s.error,"lastAck":s.last_ack})
    }
    pub fn relay_configure(&self, params: &Value) -> Result<Value, String> {
        self.relay_require_effects()?;
        let runtime = self.relay_runtime();
        let mut state = runtime.state.lock();
        let mut saved = state.saved.clone();
        let mut invitation = state.invitation.clone();
        if params["forget"] == true {
            saved = Saved::default();
            invitation = None;
        }
        if let Some(value) = params.get("invitation") {
            let new: Invitation =
                serde_json::from_value(value.clone()).map_err(|_| "Invalid Relay invitation.")?;
            new.validate()?;
            if new.device_id != self.sync_device_id() {
                return Err(
                    "Invitation deviceId must match this desktop's stable deviceId.".into(),
                );
            }
            saved = Saved::default();
            saved.protected_invitation = Some(protect(
                &serde_json::to_vec(&new).map_err(|_| "Invalid Relay invitation.")?,
            )?);
            invitation = Some(new);
        }
        for name in ["enabled", "sendEnabled", "receiveEnabled"] {
            if params.get(name).is_some_and(|v| !v.is_boolean()) {
                return Err(format!("{name} must be boolean."));
            }
        }
        if let Some(v) = params["enabled"].as_bool() {
            saved.enabled = v;
        }
        if let Some(v) = params["sendEnabled"].as_bool() {
            saved.send_enabled = v;
        }
        if let Some(v) = params["receiveEnabled"].as_bool() {
            saved.receive_enabled = v;
        }
        if saved.enabled && (invitation.is_none() || !(saved.send_enabled || saved.receive_enabled))
        {
            return Err("Import an invitation and choose at least one direction before starting Relay sync.".into());
        }
        runtime.save(&saved)?;
        state.saved = saved;
        state.invitation = invitation;
        state.generation = state.generation.wrapping_add(1);
        state.error = None;
        drop(state);
        self.relay_refresh_capture();
        Ok(self.relay_status())
    }
    pub fn relay_verb(&self, line: &str) -> Result<Value, String> {
        let args = crate::verb::split_command_line(line);
        match args.get(1).map(String::as_str).unwrap_or("status") {
            "status" => Ok(self.relay_status()),
            "import" => {
                let path = args
                    .get(2)
                    .ok_or("relay-sync import requires an invitation file path.")?;
                let meta =
                    std::fs::metadata(path).map_err(|_| "Cannot read Relay invitation file.")?;
                if meta.len() > 16 * 1024 {
                    return Err("Relay invitation is oversized.".into());
                }
                let value: Value = serde_json::from_slice(
                    &std::fs::read(path).map_err(|_| "Cannot read Relay invitation file.")?,
                )
                .map_err(|_| "Invalid Relay invitation file.")?;
                self.relay_configure(&json!({"invitation":value}))
            }
            "start" => {
                let direction = args
                    .get(2)
                    .map(String::as_str)
                    .ok_or("Choose both, send or receive.")?;
                if !matches!(direction, "both" | "send" | "receive") {
                    return Err("Choose both, send or receive.".into());
                }
                self.relay_configure(&json!({"enabled":true,"sendEnabled":direction!="receive","receiveEnabled":direction!="send"}))
            }
            "stop" => self.relay_configure(&json!({"enabled":false})),
            "forget" => self.relay_configure(&json!({"forget":true})),
            _ => Err("Unknown relay-sync command.".into()),
        }
    }
}
pub fn new_packet_id() -> String {
    uuid::Uuid::new_v4().to_string()
}
#[cfg(test)]
mod tests {
    use super::*;
    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }
    fn invite(local: &str, peer: &str) -> Invitation {
        Invitation {
            kind: "cliprelay-invitation".into(),
            version: 1,
            base_url: "https://relay.example.test/cliprelay".into(),
            send_sni: true,
            certificate_sha256: "11".repeat(32),
            room_id: "test-room".into(),
            device_id: local.into(),
            peer_device_id: peer.into(),
            token: "22".repeat(32),
            key_hex: "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f".into(),
        }
    }
    #[test]
    fn relay_shared_vectors_authenticate_text_png_ack_and_reject_tampering() {
        let vectors: Value =
            serde_json::from_str(include_str!("../../../docs/test-vectors/relay-v1.json")).unwrap();
        for case in vectors["cases"].as_array().unwrap() {
            let sender = case["senderDeviceId"].as_str().unwrap();
            let recipient = case["recipientDeviceId"].as_str().unwrap();
            let id = case["eventId"].as_str().unwrap();
            let mut i = invite(recipient, sender);
            i.room_id = case["roomId"].as_str().unwrap().into();
            i.key_hex = case["keyHex"].as_str().unwrap().into();
            let mut packet = unhex(case["packetHex"].as_str().unwrap());
            let nonce: [u8; 12] = unhex(case["nonceHex"].as_str().unwrap())
                .try_into()
                .unwrap();
            let mut plain = unhex(case["plaintextHex"].as_str().unwrap());
            let key = LessSafeKey::new(
                UnboundKey::new(&AES_256_GCM, &decode_hex32(&i.key_hex).unwrap()).unwrap(),
            );
            key.seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(case["aadUtf8"].as_str().unwrap().as_bytes()),
                &mut plain,
            )
            .unwrap();
            let mut expected = nonce.to_vec();
            expected.extend_from_slice(&plain);
            assert_eq!(packet, expected);

            let (header, media) = i.open(sender, id, &packet).unwrap();
            assert_eq!(header, case["header"]);
            assert_eq!(media, unhex(case["mediaHex"].as_str().unwrap()));
            assert_eq!(
                String::from_utf8(i.aad(sender, recipient, id).unwrap()).unwrap(),
                case["aadUtf8"]
            );
            assert!(i.open(sender, "tampered-id", &packet).is_err());
            let mut wrong = i.clone();
            wrong.room_id = "wrong-room".into();
            assert!(wrong.open(sender, id, &packet).is_err());
            wrong = i.clone();
            wrong.key_hex = "ff".repeat(32);
            assert!(wrong.open(sender, id, &packet).is_err());
            *packet.last_mut().unwrap() ^= 1;
            assert!(i.open(sender, id, &packet).is_err());
            let outgoing = invite(sender, recipient);
            let fresh = outgoing.seal(id, &header, &media).unwrap();
            assert_eq!(i.open(sender, id, &fresh).unwrap(), (header, media));
        }
    }
    #[test]
    fn relay_invitation_rejects_unsafe_urls_and_weak_credentials() {
        let baseline = invite("desktop", "phone");
        baseline.validate().unwrap();
        for url in [
            "http://example.test/cliprelay",
            "https://user@example.test/cliprelay",
            "https://example.test/cliprelay?token=x",
            "https://example.test/cliprelay#x",
            "https://example.test/cliprelay/",
        ] {
            let mut i = baseline.clone();
            i.base_url = url.into();
            assert!(i.validate().is_err());
        }
        let mut i = baseline;
        i.key_hex = "123456".into();
        assert!(i.validate().is_err());
    }
    #[cfg(windows)]
    #[test]
    fn relay_dpapi_roundtrip_never_persists_the_token_or_key() {
        let plain = serde_json::to_vec(&invite("desktop", "phone")).unwrap();
        let protected = protect(&plain).unwrap();
        assert_eq!(unprotect(&protected).unwrap(), plain);
        assert!(!protected.contains(&"22".repeat(32)));
        assert!(unprotect("not-dpapi").is_err());
    }
}
