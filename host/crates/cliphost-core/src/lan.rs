//! ClipLink LAN protocol aligned with C# `LanShareConstants` / discovery packets.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

pub const PROTOCOL: &str = "cea-clip-link";
pub const PROTOCOL_VERSION: i32 = 1;
pub const ALL_CONVERSATION_ID: &str = "all";
pub const DEVICE_HEADER: &str = "X-ClipLink-Device";
pub const API_PREFIX: &str = "/cliplink/v1";
/// C# `LanShareConfig.MaxIncomingFileSizeBytes` default 2 GiB.
pub const DEFAULT_MAX_INCOMING_FILE_SIZE_BYTES: i64 = 2 * 1024 * 1024 * 1024;
/// C# `LanShareConfig.MaxParallelDeliveries` default, clamped 1..=8.
pub const DEFAULT_MAX_PARALLEL_DELIVERIES: i32 = 3;
pub const PEERS_CHANGED_EVENT: &str = "lan.peers.changed";
pub const SHARE_CHANGED_EVENT: &str = "lan.share.changed";
pub const MULTICAST_ADDR: &str = "239.255.77.77";
pub const PAIRING_TTL: Duration = Duration::from_secs(5 * 60);
pub const PAIRING_MAX_ATTEMPTS: i32 = 5;
pub const MAX_TEXT_LENGTH: usize = 1024 * 1024;
pub const MAX_ATTACHMENTS: usize = 100;
pub const CANCELED_ERROR: &str = "已取消";
/// C# `DiscoverPeersAsync` waits 350ms after `ScanAsync` for UDP replies.
pub const DISCOVER_REPLY_WAIT: Duration = Duration::from_millis(350);
/// C# `LanShareDiscoveryService.RemoveExpired` / `ListPeers` online threshold.
pub const DISCOVERY_PEER_TTL: chrono::Duration = chrono::Duration::seconds(35);
/// C# outbox reports `{ deliveryId, transferred }` at least every 1 MiB.
pub const DELIVERY_PROGRESS_QUANTUM: u64 = 1024 * 1024;

thread_local! {
    static DELIVERY_CANCEL: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) };
}

pub fn with_delivery_cancel<T>(flag: Arc<AtomicBool>, f: impl FnOnce() -> T) -> T {
    DELIVERY_CANCEL.with(|slot| {
        *slot.borrow_mut() = Some(flag);
    });
    let result = f();
    DELIVERY_CANCEL.with(|slot| {
        *slot.borrow_mut() = None;
    });
    result
}

pub fn throw_if_canceled() -> Result<(), String> {
    DELIVERY_CANCEL.with(|slot| {
        if slot
            .borrow()
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
        {
            Err(CANCELED_ERROR.into())
        } else {
            Ok(())
        }
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LanDiscoveryPacket {
    pub protocol: String,
    pub version: i32,
    pub kind: String,
    pub device_id: String,
    pub alias: String,
    pub port: u16,
    pub certificate_fingerprint: String,
    pub pairing_available: bool,
    /// Optional metadata, independent of the announce/query packet kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_kind: Option<crate::lan_device::DeviceKind>,
}

impl LanDiscoveryPacket {
    pub fn announce(identity: &LanIdentity, alias: &str, port: u16, pairing_available: bool) -> Self {
        Self {
            protocol: PROTOCOL.into(),
            version: PROTOCOL_VERSION,
            kind: "announce".into(),
            device_id: identity.device_id.clone(),
            alias: alias.chars().take(64).collect(),
            port,
            certificate_fingerprint: identity.fingerprint.clone(),
            pairing_available,
            device_kind: None,
        }
    }

    pub fn query(identity: &LanIdentity, alias: &str, port: u16, pairing_available: bool) -> Self {
        let mut packet = Self::announce(identity, alias, port, pairing_available);
        packet.kind = "query".into();
        packet
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let value = serde_json::to_value(self).unwrap_or(Value::Null);
        #[cfg(feature = "private-sync")]
        let mut value = value;
        #[cfg(feature = "private-sync")]
        { value["capabilities"] = json!(["text", "image", "pull"]); }
        serde_json::to_vec(&value).unwrap_or_default()
    }

    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let packet: Self = serde_json::from_slice(bytes).ok()?;
        if packet.protocol != PROTOCOL || packet.version != PROTOCOL_VERSION {
            return None;
        }
        if packet.device_id.trim().is_empty() || !(1..=65535).contains(&packet.port) {
            return None;
        }
        if packet.certificate_fingerprint.len() != 64
            || !packet
                .certificate_fingerprint
                .chars()
                .all(|c| c.is_ascii_hexdigit())
        {
            return None;
        }
        Some(packet)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LanIdentity {
    pub device_id: String,
    pub fingerprint: String,
    #[serde(skip)]
    pub cert_der: Vec<u8>,
    #[serde(skip)]
    pub key_der: Vec<u8>,
}

impl LanIdentity {
    pub fn generate() -> Self {
        issue_identity(&Uuid::new_v4().simple().to_string())
    }

    pub fn load_or_create(path: &Path) -> Self {
        let dir = path.parent().unwrap_or(path);
        let cert_path = dir.join("cert.der");
        let key_path = dir.join("key.der");
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(mut id) = serde_json::from_str::<Self>(&text) {
                if let (Ok(cert), Ok(key)) = (std::fs::read(&cert_path), std::fs::read(&key_path)) {
                    if sha256_hex(&cert) == id.fingerprint && id.fingerprint.len() == 64 {
                        id.cert_der = cert;
                        id.key_der = key;
                        return id;
                    }
                }
            }
        }
        let id = Self::generate();
        let _ = std::fs::create_dir_all(dir);
        if let Ok(text) = serde_json::to_string_pretty(&LanIdentityMeta {
            device_id: id.device_id.clone(),
            fingerprint: id.fingerprint.clone(),
        }) {
            let _ = std::fs::write(path, text);
        }
        let _ = std::fs::write(cert_path, &id.cert_der);
        let _ = std::fs::write(key_path, &id.key_der);
        id
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LanIdentityMeta {
    device_id: String,
    fingerprint: String,
}

fn issue_identity(device_id: &str) -> LanIdentity {
    // C# `LanShareIdentityService.LoadOrCreate`: CN ClipHost-{id[..12]}, SAN localhost /
    // Environment.MachineName / 127.0.0.1 / ::1, notBefore now-1d, notAfter now+10y.
    let mut dns_names = vec!["localhost".to_string()];
    if let Ok(machine) = std::env::var("COMPUTERNAME") {
        let machine = machine.trim();
        if !machine.is_empty() && !machine.eq_ignore_ascii_case("localhost") {
            dns_names.push(machine.to_string());
        }
    }
    let mut params = match rcgen::CertificateParams::new(dns_names.clone()) {
        Ok(params) => params,
        Err(_) => rcgen::CertificateParams::new(vec!["localhost".into()]).expect("localhost SAN"),
    };
    let start = Utc::now() - chrono::Duration::days(1);
    let end = Utc::now() + chrono::Duration::days(3652);
    use chrono::Datelike;
    params.not_before = rcgen::date_time_ymd(start.year(), start.month() as u8, start.day() as u8);
    params.not_after = rcgen::date_time_ymd(end.year(), end.month() as u8, end.day() as u8);
    params.distinguished_name = rcgen::DistinguishedName::new();
    params.distinguished_name.push(
        rcgen::DnType::CommonName,
        format!("ClipHost-{}", &device_id[..12.min(device_id.len())]),
    );
    params
        .subject_alt_names
        .push(rcgen::SanType::IpAddress(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)));
    params
        .subject_alt_names
        .push(rcgen::SanType::IpAddress(std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)));
    params.is_ca = rcgen::IsCa::NoCa;
    params.key_usages = vec![
        rcgen::KeyUsagePurpose::DigitalSignature,
        rcgen::KeyUsagePurpose::KeyEncipherment,
    ];
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    let key_pair = lan_tls_key_pair();
    let cert = params.self_signed(&key_pair).expect("self-signed");
    let cert_der = cert.der().to_vec();
    let fingerprint = sha256_hex(&cert_der);
    LanIdentity {
        device_id: device_id.to_string(),
        fingerprint,
        cert_der,
        key_der: key_pair.serialize_der(),
    }
}

/// C# `RSA.Create(3072)` for live LAN identity. Headless tests keep ECDSA so `HostState::open` stays fast.
fn lan_tls_key_pair() -> rcgen::KeyPair {
    if crate::clipboard::is_headless() {
        return rcgen::KeyPair::generate().expect("LAN test key");
    }
    rsa_3072_key_pair()
}

fn rsa_3072_key_pair() -> rcgen::KeyPair {
    #[cfg(windows)]
    match crate::lan_key::rsa_3072_key_pair() {
        Ok(key) => return key,
        Err(error) => tracing::warn!(%error, "Windows RSA generation unavailable; using portable RSA"),
    }
    portable_rsa_3072_key_pair()
}

fn portable_rsa_3072_key_pair() -> rcgen::KeyPair {
    use rand::rngs::OsRng;
    use rsa::pkcs8::EncodePrivateKey;
    use rsa::RsaPrivateKey;
    let mut rng = OsRng;
    let key = RsaPrivateKey::new(&mut rng, 3072).expect("C# RSA.Create(3072)");
    let der = key.to_pkcs8_der().expect("pkcs8");
    rcgen::KeyPair::try_from(der.as_bytes()).expect("rcgen RSA PKCS8")
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

pub fn sha256_utf8_hex(value: &str) -> String {
    sha256_hex(value.as_bytes())
}

/// C# `LanShareClient.CreateToken`: `RandomNumberGenerator.GetBytes(32)` as standard Base64.
pub fn create_token() -> String {
    let mut bytes = [0u8; 32];
    if getrandom::getrandom(&mut bytes).is_err() {
        bytes[..16].copy_from_slice(Uuid::new_v4().as_bytes());
        bytes[16..].copy_from_slice(Uuid::new_v4().as_bytes());
    }
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// C# `LanShareClient.ValidateToken`.
pub fn validate_token(token: &str) -> Result<(), String> {
    let token = token.trim();
    if token.is_empty() {
        return Err("配对令牌不能为空。".into());
    }
    use base64::Engine;
    match base64::engine::general_purpose::STANDARD.decode(token) {
        Ok(raw) if raw.len() >= 32 => Ok(()),
        Ok(_) => Err("配对令牌长度不足。".into()),
        Err(_) => Err("配对令牌格式无效。".into()),
    }
}

/// C# `LanShareNetworkHost.ReasonPhrase`.
pub fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        507 => "Insufficient Storage",
        _ => "Internal Server Error",
    }
}

/// C# `LanShareClient.EnsureSuccessAsync` error text.
pub fn lan_status_error(status: u16, details: impl std::fmt::Display) -> String {
    format!("局域网设备返回 {status} {}: {details}", reason_phrase(status))
}

/// C# `LanShareNetworkHost.Problem` JSON `{status,title,detail}`.
pub fn problem_json(status: u16, detail: &str) -> serde_json::Value {
    serde_json::json!({
        "status": status,
        "title": reason_phrase(status),
        "detail": detail,
    })
}

/// C# `PairAsync`: only an expired/invalid pairing code is 403; request validation is 400.
pub fn pair_http_status(err: &str) -> u16 {
    if err == "配对码无效或已过期。" {
        403
    } else {
        400
    }
}

/// C# `DispatchAsync`: `IOException` → 507; validation → 400.
pub fn inbound_http_status(err: &str) -> u16 {
    if err.starts_with("io:") {
        507
    } else {
        400
    }
}

pub fn inbound_http_detail(err: &str) -> &str {
    err.strip_prefix("io:").unwrap_or(err)
}

/// C# `LanShareIdentityService.GetLocalUrls`: IPv4 LAN addresses then loopback, no path suffix.
pub fn local_https_urls(port: u16) -> Vec<String> {
    let mut urls: Vec<String> = local_ipv4_unicast()
        .into_iter()
        .map(|ip| format!("https://{ip}:{port}"))
        .collect();
    let loopback = format!("https://127.0.0.1:{port}");
    if !urls.iter().any(|u| u == &loopback) {
        urls.push(loopback);
    }
    urls
}

/// C# pair/status `FirstOrDefault` URL that is not 127.0.0.1.
pub fn preferred_local_base_url(port: u16) -> String {
    local_https_urls(port)
        .into_iter()
        .find(|url| !url.contains("127.0.0.1"))
        .unwrap_or_else(|| format!("https://127.0.0.1:{port}"))
}

fn local_ipv4_unicast() -> Vec<std::net::Ipv4Addr> {
    #[cfg(windows)]
    {
        // Reading status/identity must not open a wildcard UDP socket, including
        // when Windows reports no adapters or adapter enumeration fails.
        local_discovery_interfaces().unwrap_or_default().into_iter()
            .map(|interface| interface.address).collect()
    }
    #[cfg(not(windows))]
    {
        primary_ipv4().into_iter().collect()
    }
}

/// IPv4 address and on-link prefix from the same active adapter record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LanDiscoveryInterface {
    pub address: std::net::Ipv4Addr,
    pub prefix_length: u8,
}

impl LanDiscoveryInterface {
    pub fn multicast_interface(self) -> Option<std::net::Ipv4Addr> {
        let first = self.address.octets()[0];
        (!self.address.is_loopback() && first != 0 && first < 224).then_some(self.address)
    }

    pub fn broadcast_address(self) -> Option<std::net::Ipv4Addr> {
        self.multicast_interface()?;
        // /0 is not an on-link subnet; /31 and /32 have no broadcast hosts.
        if !(1..=30).contains(&self.prefix_length) { return None; }
        let address = u32::from(self.address);
        let mask = u32::MAX << (32 - self.prefix_length);
        let network = address & mask;
        let broadcast = network | !mask;
        let target = Self { address: broadcast.into(), prefix_length: self.prefix_length };
        (address != network && address != broadcast && target.multicast_interface().is_some())
            .then_some(target.address)
    }
}

pub fn local_discovery_interfaces() -> Result<Vec<LanDiscoveryInterface>, String> {
    #[cfg(windows)]
    { ipv4_interfaces_from_adapters() }
    #[cfg(not(windows))]
    {
        // The existing fallback has no prefix; do not invent a broadcast subnet.
        Ok(primary_ipv4().into_iter().map(|address| LanDiscoveryInterface {
            address, prefix_length: 32,
        }).collect())
    }
}

#[cfg(not(windows))]
fn primary_ipv4() -> Option<std::net::Ipv4Addr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    match socket.local_addr().ok()? {
        std::net::SocketAddr::V4(addr) if !addr.ip().is_loopback() => Some(*addr.ip()),
        _ => None,
    }
}

/// C# `NetworkInterface.GetAllNetworkInterfaces` IPv4 filter: Up, not loopback/tunnel.
#[cfg(windows)]
fn ipv4_interfaces_from_adapters() -> Result<Vec<LanDiscoveryInterface>, String> {
    use std::net::Ipv4Addr;
    use windows::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, ERROR_SUCCESS};
    use windows::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST,
        IF_TYPE_SOFTWARE_LOOPBACK, IF_TYPE_TUNNEL, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    use windows::Win32::Networking::WinSock::{AF_INET, AF_UNSPEC, SOCKADDR_IN, IpDadStateDeprecated, IpDadStatePreferred};

    unsafe {
        // C# `GetAllNetworkInterfaces` enumerates AF_UNSPEC then Distinct first-seen IPv4.
        let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
        let family = AF_UNSPEC.0 as u32;
        let mut size = 15_000u32;
        for _ in 0..4 {
            let words = (size as usize + 7) / 8;
            let mut buf = vec![0u64; words.max(1)];
            let head = buf.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
            let status = GetAdaptersAddresses(family, flags, None, Some(head), &mut size);
            if status == ERROR_SUCCESS.0 {
                let mut out = Vec::new();
                let mut seen = std::collections::HashSet::new();
                let mut adapter = head;
                while !adapter.is_null() {
                    let a = &*adapter;
                    if a.OperStatus == IfOperStatusUp
                        && a.IfType != IF_TYPE_SOFTWARE_LOOPBACK
                        && a.IfType != IF_TYPE_TUNNEL
                    {
                        let mut unicast = a.FirstUnicastAddress;
                        while !unicast.is_null() {
                            let ua = &*unicast;
                            let sa = ua.Address.lpSockaddr;
                            if (ua.DadState == IpDadStatePreferred || ua.DadState == IpDadStateDeprecated)
                                && !sa.is_null() && ua.Address.iSockaddrLength >= std::mem::size_of::<SOCKADDR_IN>() as i32
                                && (*sa).sa_family == AF_INET {
                                let sin = sa.cast::<SOCKADDR_IN>();
                                let b = (*sin).sin_addr.S_un.S_un_b;
                                let ip = Ipv4Addr::new(b.s_b1, b.s_b2, b.s_b3, b.s_b4);
                                let interface = LanDiscoveryInterface { address: ip, prefix_length: ua.OnLinkPrefixLength };
                                if interface.multicast_interface().is_some() && seen.insert(ip) {
                                    out.push(interface);
                                }
                            }
                            unicast = ua.Next;
                        }
                    }
                    adapter = a.Next;
                }
                return Ok(out);
            }
            if status != ERROR_BUFFER_OVERFLOW.0 {
                return Err(format!("GetAdaptersAddresses failed: {status}"));
            }
        }
        Err("GetAdaptersAddresses buffer kept changing.".into())
    }
}

/// C# `LanShareNetworkHost.ResolveRemoteBaseUrl`.
pub fn resolve_remote_base_url(claimed: &str, remote_ip: Option<&str>) -> Result<String, String> {
    let (host, port) = parse_https_host_port(claimed)
        .ok_or_else(|| "局域网设备地址必须是带端口的 HTTPS 地址。".to_string())?;
    if port == 443 {
        return Err("局域网设备地址必须是带端口的 HTTPS 地址。".into());
    }
    if let Some(ip) = remote_ip.map(str::trim).filter(|s| !s.is_empty()) {
        let host = if ip.contains(':') {
            format!("[{ip}]")
        } else {
            ip.to_string()
        };
        return Ok(format!("https://{host}:{port}"));
    }
    if host.contains(':') {
        Ok(format!("https://[{host}]:{port}"))
    } else {
        Ok(format!("https://{host}:{port}"))
    }
}

pub fn safe_identifier(value: &str) -> Result<&str, String> {
    // C# `LanShareFileStore.SafeIdentifier`: IsNullOrWhiteSpace, length, ASCII letter/digit/-/_.
    if value.trim().is_empty()
        || value.len() > 128
        || !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("标识符格式无效。".into());
    }
    Ok(value)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LanProtocolAttachment {
    pub id: String,
    pub name: String,
    #[serde(default = "default_mime")]
    pub mime_type: String,
    pub size_bytes: i64,
    pub sha256: String,
}

/// C# `LanShareAppService.ResolveMimeType`.
pub fn resolve_mime(path: &str) -> String {
    match Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "txt" => "text/plain",
        "json" => "application/json",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "webp" => "image/webp",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
    .into()
}

fn default_mime() -> String {
    "application/octet-stream".into()
}

/// C# outbox delay: `5 * 2^clamp(preIncrementAttempt, 0, 6)` capped at 300s.
pub fn delivery_backoff_secs(pre_increment_attempt: i32) -> i64 {
    let exponent = pre_increment_attempt.clamp(0, 6);
    (5.0 * 2f64.powi(exponent)).min(300.0) as i64
}

/// C# `Uri.EscapeDataString` for ClipLink path segments.
pub fn encode_path_segment(value: &str) -> String {
    let mut out = String::new();
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LanPrepareMessageRequest {
    pub id: String,
    pub sender_device_id: String,
    #[serde(default)]
    pub sender_alias: String,
    #[serde(default = "default_direct")]
    pub target_kind: String,
    #[serde(default)]
    pub text: String,
    pub created_at_utc: DateTime<Utc>,
    #[serde(default)]
    pub attachments: Vec<LanProtocolAttachment>,
}

fn default_direct() -> String {
    "direct".into()
}

pub fn stage_outgoing(
    lan_root: &Path,
    message_id: &str,
    source: &Path,
) -> Result<(LanProtocolAttachment, PathBuf), String> {
    let bytes = std::fs::read(source).map_err(|e| e.to_string())?;
    let id = Uuid::new_v4().simple().to_string();
    let name = source
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("file")
        .to_string();
    let mime_type = resolve_mime(&source.to_string_lossy());
    let folder = lan_root.join("Outbox").join(safe_identifier(message_id)?);
    let _ = std::fs::create_dir_all(&folder);
    let staged = folder.join(format!("{id}-{name}"));
    std::fs::write(&staged, &bytes).map_err(|e| e.to_string())?;
    Ok((
        LanProtocolAttachment {
            id,
            name,
            mime_type,
            size_bytes: bytes.len() as i64,
            sha256: sha256_hex(&bytes),
        },
        staged,
    ))
}

#[derive(Debug, Clone)]
pub struct ReceiveSession {
    pub session_id: String,
    pub sender_device_id: String,
    pub request: LanPrepareMessageRequest,
    pub folder: PathBuf,
    pub uploaded: HashMap<String, PathBuf>,
    pub created_at: Instant,
}

pub fn fixed_time_eq_hex(a: &str, b: &str) -> bool {
    let a = a.to_ascii_lowercase();
    let b = b.to_ascii_lowercase();
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn nonempty_alias(alias: &str) -> String {
    let trimmed = alias.trim();
    if trimmed.is_empty() {
        "未命名设备".into()
    } else {
        trimmed.to_string()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClipboardTransport {
    #[default]
    Push,
    Pull,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LanDiscoveredPeer {
    #[serde(default)]
    pub clipboard_transport: ClipboardTransport,
    pub device_id: String,
    pub alias: String,
    pub base_url: String,
    pub certificate_fingerprint: String,
    pub last_seen_at_utc: DateTime<Utc>,
    pub pairing_available: bool,
    #[serde(default)]
    pub paired: bool,
    #[serde(default)]
    pub online: bool,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub inbound_token_hash: Option<String>,
    /// Fingerprint stored at pairing time (C# `LanShareDeviceEntity.CertificateFingerprint`).
    #[serde(default)]
    pub paired_certificate_fingerprint: Option<String>,
    /// Original automatic report. Local icon overrides live in settings.
    #[serde(default)]
    pub device_kind: crate::lan_device::DeviceKind,
}

impl LanDiscoveredPeer {
    pub fn is_pull(&self) -> bool {
        self.clipboard_transport == ClipboardTransport::Pull
    }

    pub fn trusted(&self) -> bool {
        self.paired && if self.is_pull() {
            #[cfg(feature = "private-sync")]
            { self.inbound_token_hash.is_some()
                && self.base_url.is_empty()
                && self.certificate_fingerprint.is_empty()
                && self.paired_certificate_fingerprint.is_none()
                && self.token.is_none() }
            #[cfg(not(feature = "private-sync"))]
            { false }
        } else {
            self.paired_certificate_fingerprint.as_ref().is_some_and(|saved| {
                fixed_time_eq_hex(saved, &self.certificate_fingerprint)
            })
        }
    }
}

#[derive(Debug, Clone)]
pub struct PairingCode {
    pub code: String,
    pub expires_at: Instant,
    pub attempts_remaining: i32,
}

impl PairingCode {
    pub fn fresh() -> Self {
        Self {
            code: format!("{:06}", Uuid::new_v4().as_u128() % 1_000_000),
            expires_at: Instant::now() + PAIRING_TTL,
            attempts_remaining: PAIRING_MAX_ATTEMPTS,
        }
    }

    pub fn available(&self) -> bool {
        Instant::now() < self.expires_at && self.attempts_remaining > 0
    }

    pub fn accept(&mut self, offered: &str) -> bool {
        if !self.available() {
            return false;
        }
        if self.code == offered {
            *self = Self::fresh();
            return true;
        }
        self.attempts_remaining -= 1;
        false
    }

    pub fn view(&self) -> Value {
        json!({
            "code": self.code,
            "expiresAtUtc": Utc::now() + chrono::Duration::from_std(self.expires_at.saturating_duration_since(Instant::now())).unwrap_or_default(),
            "attemptsRemaining": self.attempts_remaining,
        })
    }
}

#[derive(Debug, Clone)]
pub struct LanRuntime {
    pub identity: LanIdentity,
    pub enabled: bool,
    pub alias: String,
    pub port: u16,
    pub pairing: PairingCode,
    pub discovered: HashMap<String, LanDiscoveredPeer>,
    pub conversations: Vec<Value>,
    pub inbox: String,
    pub pending_kind: Option<String>,
    pub receiver: Arc<parking_lot::Mutex<LanReceiver>>,
    pub lan_root: PathBuf,
    pub peers_path: PathBuf,
    pub max_incoming_file_size_bytes: i64,
    pub max_parallel_deliveries: i32,
    pub received_files_to_history: bool,
    pub device_presentation: crate::lan_device::DevicePresentation,
}

impl LanRuntime {
    /// Build only from the captured automatic detection value. A local icon
    /// preference must never replace the device's report on the network.
    pub fn discovery_packet(&self, kind: &str) -> LanDiscoveryPacket {
        let mut packet = LanDiscoveryPacket::announce(
            &self.identity, &self.alias, self.port, self.pairing.available(),
        );
        packet.kind = kind.into();
        packet.device_kind = Some(self.device_presentation.detected());
        packet
    }

    /// Called inside the existing config save/rollback transaction. Validate
    /// the complete patch before changing any local presentation preference.
    pub fn apply_device_preferences(&mut self, params: &Value) -> Result<(), String> {
        let paired = self.discovered.values().filter(|peer| peer.trusted())
            .map(|peer| peer.device_id.clone()).collect();
        self.device_presentation.apply_patch(params, &paired)
    }

    pub fn ingest(&mut self, packet: LanDiscoveryPacket, from_ip: &str) -> bool {
        if packet.device_id == self.identity.device_id {
            return false;
        }
        let prev = self.discovered.get(&packet.device_id);
        // A client-only peer has no callback identity. Unauthenticated UDP
        // cannot upgrade it to a server or replace its paired credentials.
        if prev.is_some_and(LanDiscoveredPeer::is_pull) {
            return false;
        }
        let peer = LanDiscoveredPeer {
            clipboard_transport: ClipboardTransport::Push,
            device_id: packet.device_id.clone(),
            alias: packet.alias.chars().take(64).collect(),
            base_url: format!("https://{from_ip}:{}", packet.port),
            certificate_fingerprint: packet.certificate_fingerprint.to_ascii_lowercase(),
            last_seen_at_utc: Utc::now(),
            pairing_available: packet.pairing_available,
            paired: prev.map(|p| p.paired).unwrap_or(false),
            online: true,
            token: prev.and_then(|p| p.token.clone()),
            inbound_token_hash: prev.and_then(|p| p.inbound_token_hash.clone()),
            paired_certificate_fingerprint: prev.and_then(|p| p.paired_certificate_fingerprint.clone()),
            device_kind: packet.device_kind.unwrap_or_else(|| prev.map(|peer| peer.device_kind).unwrap_or_default()),
        };
        self.discovered.insert(peer.device_id.clone(), peer);
        true
    }

    /// C# `LanShareDiscoveryService.RemoveExpired`: drop unpaired UDP peers older than 35s.
    /// Paired devices stay in the map (C# keeps them in the durable store).
    pub fn expire_unpaired_discovered(&mut self) {
        let threshold = Utc::now() - DISCOVERY_PEER_TTL;
        self.discovered
            .retain(|_, peer| peer.paired || peer.last_seen_at_utc >= threshold);
    }

    pub fn forget(&mut self, device_id: &str) -> bool {
        self.discovered.remove(device_id).is_some()
    }

    pub fn mark_paired(
        &mut self,
        device_id: &str,
        outbound_token: String,
        inbound_token_hash: Option<String>,
    ) -> Option<LanDiscoveredPeer> {
        let peer = self.discovered.get_mut(device_id)?;
        peer.paired = true;
        peer.token = Some(outbound_token);
        if inbound_token_hash.is_some() {
            peer.inbound_token_hash = inbound_token_hash;
        }
        peer.paired_certificate_fingerprint = Some(peer.certificate_fingerprint.clone());
        Some(peer.clone())
    }

    pub fn persist_paired_peers(&self) -> Result<(), String> {
        #[cfg(test)]
        crate::host::lan_persistence_tests::before_save();
        let peers: Vec<LanDiscoveredPeer> = self
            .discovered
            .values()
            .filter(|p| p.paired)
            .cloned()
            .collect();
        crate::config_file::save(&self.peers_path, &peers)
            .map_err(|error| format!("保存配对设备失败: {error}"))
    }

    /// Call while holding the independent LAN lock: publish changes only after
    /// durable replacement, and restore the trusted peer set on failure.
    pub fn update_peers<T>(&mut self, update: impl FnOnce(&mut Self) -> Result<T, String>) -> Result<T, String> {
        let previous = self.discovered.clone();
        let result = update(self).and_then(|value| self.persist_paired_peers().map(|()| value));
        if result.is_err() {
            self.discovered = previous;
        }
        result
    }

    pub fn refresh_presence(&mut self, probes: Vec<(LanDiscoveredPeer, LanDiscoveredPeer)>) -> Result<bool, String> {
        // A probe can finish after Forget, re-pairing or a newer discovery.
        // Never restore old credentials or overwrite a changed endpoint.
        let updates: Vec<_> = probes.into_iter().filter(|(previous, live)| {
            previous.device_id == live.device_id && self.discovered.get(&previous.device_id).is_some_and(|current| {
                current.paired
                    && current.token == previous.token
                    && current.inbound_token_hash == previous.inbound_token_hash
                    && current.paired_certificate_fingerprint == previous.paired_certificate_fingerprint
                    && current.certificate_fingerprint == previous.certificate_fingerprint
                    && current.base_url == previous.base_url
            })
        }).collect();
        if updates.is_empty() { return Ok(false); }
        self.update_peers(move |lan| {
            for (_, live) in updates {
                lan.discovered.insert(live.device_id.clone(), live);
            }
            Ok(true)
        })
    }

    pub fn load_paired_peers(&mut self) {
        let Ok(text) = std::fs::read_to_string(&self.peers_path) else {
            return;
        };
        let Ok(peers) = serde_json::from_str::<Vec<LanDiscoveredPeer>>(&text) else {
            return;
        };
        for mut peer in peers {
            peer.online = false;
            if peer.paired && !peer.is_pull() && peer.paired_certificate_fingerprint.is_none() {
                peer.paired_certificate_fingerprint = Some(peer.certificate_fingerprint.clone());
            }
            self.discovered.insert(peer.device_id.clone(), peer);
        }
    }

    fn settings_path(&self) -> PathBuf {
        self.peers_path
            .parent()
            .map(|dir| dir.join("settings.json"))
            .unwrap_or_else(|| PathBuf::from("settings.json"))
    }

    /// C# `LanShareConfig.NormalizeAlias`.
    pub fn normalize_alias(value: &str) -> String {
        let trimmed = value.trim();
        let alias = if trimmed.is_empty() {
            std::env::var("COMPUTERNAME").unwrap_or_else(|_| "未命名设备".into())
        } else {
            trimmed.to_string()
        };
        alias.chars().take(64).collect()
    }

    pub fn normalize_settings(&mut self) {
        self.alias = Self::normalize_alias(&self.alias);
        if self.port == 0 {
            self.port = crate::paths::DEFAULT_LAN_PORT;
        }
        if self.max_incoming_file_size_bytes <= 0 {
            self.max_incoming_file_size_bytes = DEFAULT_MAX_INCOMING_FILE_SIZE_BYTES;
        }
        self.max_parallel_deliveries = self.max_parallel_deliveries.clamp(1, 8);
    }

    pub fn load_settings(&mut self) {
        let Ok(text) = std::fs::read_to_string(self.settings_path()) else {
            return;
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            return;
        };
        if let Some(enabled) = value.get("enabled").and_then(|v| v.as_bool()) {
            self.enabled = enabled;
        }
        if let Some(alias) = value.get("deviceAlias").and_then(|v| v.as_str()) {
            self.alias = alias.to_string();
        }
        if let Some(port) = value.get("port").and_then(|v| v.as_u64()) {
            if (1..=65535).contains(&port) && port != crate::paths::CSHARP_LAN_PORT as u64 {
                self.port = port as u16;
            }
        }
        if let Some(inbox) = value.get("inboxFolder").and_then(|v| v.as_str()) {
            self.inbox = inbox.trim().to_string();
        }
        if let Some(size) = value
            .get("maxIncomingFileSizeBytes")
            .and_then(|v| v.as_i64())
        {
            self.max_incoming_file_size_bytes = size;
        }
        if let Some(parallel) = value.get("maxParallelDeliveries").and_then(|v| v.as_i64()) {
            self.max_parallel_deliveries = parallel as i32;
        }
        if let Some(enabled) = value.get("receivedFilesToHistory").and_then(Value::as_bool) {
            self.received_files_to_history = enabled;
        }
        self.device_presentation.load_preferences(&value);
        self.normalize_settings();
    }

    pub fn save_settings(&self) -> Result<(), String> {
        #[cfg(test)]
        crate::host::lan_persistence_tests::before_save();
        let body = json!({
            "enabled": self.enabled,
            "deviceAlias": self.alias,
            "port": self.port,
            "inboxFolder": self.inbox,
            "maxIncomingFileSizeBytes": self.max_incoming_file_size_bytes,
            "maxParallelDeliveries": self.max_parallel_deliveries,
            "receivedFilesToHistory": self.received_files_to_history,
            "deviceKindOverride": self.device_presentation.override_name(),
            "peerDeviceKindOverrides": self.device_presentation.preferences()["peerDeviceKindOverrides"],
        });
        crate::config_file::save(&self.settings_path(), &body)
            .map_err(|error| format!("保存局域网设置失败: {error}"))
    }

    pub fn authenticate(&self, device_id: &str, authorization: Option<&str>) -> Result<(), String> {
        let peer = self
            .discovered
            .get(device_id)
            .ok_or_else(|| "设备尚未配对。".to_string())?;
        if !peer.paired {
            return Err("设备尚未配对。".into());
        }
        #[cfg(not(feature = "private-sync"))]
        if peer.is_pull() {
            return Err("设备尚未配对。".into());
        }
        let bearer = authorization
            .and_then(|h| h.strip_prefix("Bearer ").or_else(|| h.strip_prefix("bearer ")))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "设备令牌无效。".to_string())?;
        let expected = peer
            .inbound_token_hash
            .as_deref()
            .ok_or_else(|| "设备令牌无效。".to_string())?;
        if !fixed_time_eq_hex(expected, &sha256_utf8_hex(bearer)) {
            return Err("设备令牌无效。".into());
        }
        Ok(())
    }

    pub fn peers_dto(&self) -> Vec<Value> {
        // C# `ListPeers`: paired desc, online desc, alias CurrentCultureIgnoreCase.
        let threshold = Utc::now() - DISCOVERY_PEER_TTL;
        let mut rows: Vec<(bool, bool, String, Value)> = self
            .discovered
            .values()
            .map(|p| {
                let seen_recent = p.last_seen_at_utc >= threshold;
                let identity_matches = p.is_pull() || p.paired_certificate_fingerprint.as_ref().is_some_and(|saved| {
                    fixed_time_eq_hex(saved, &p.certificate_fingerprint)
                });
                let paired = p.trusted();
                let online = seen_recent;
                let alias = if p.paired_certificate_fingerprint.is_some() && !identity_matches {
                    format!("{}（身份已更改）", nonempty_alias(&p.alias))
                } else {
                    nonempty_alias(&p.alias)
                };
                let dto = json!({
                    "deviceId": p.device_id,
                    "alias": alias,
                    "baseUrl": p.base_url,
                    "certificateFingerprint": p.certificate_fingerprint,
                    "lastSeenAtUtc": p.last_seen_at_utc,
                    "online": online,
                    "paired": paired,
                    "pairingAvailable": p.pairing_available,
                    "clipboardTransport": p.clipboard_transport,
                    "supportsManualTransfer": !p.is_pull(),
                    "deviceKind": self.device_presentation.peer_display_kind(&p.device_id, p.device_kind),
                    "detectedDeviceKind": p.device_kind,
                    "deviceKindOverride": self.device_presentation.peer_override_name(&p.device_id),
                });
                (paired, online, alias.to_lowercase(), dto)
            })
            .collect();
        rows.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then_with(|| a.2.cmp(&b.2)));
        rows.into_iter().map(|row| row.3).collect()
    }

    pub fn status(&self, running: bool) -> Value {
        json!({
            "enabled": self.enabled,
            "running": running,
            "port": self.port,
            "deviceId": self.identity.device_id,
            "deviceAlias": self.alias,
            "certificateFingerprint": self.identity.fingerprint,
            "inboxFolder": self.inbox,
            "maxIncomingFileSizeBytes": self.max_incoming_file_size_bytes,
            "maxParallelDeliveries": self.max_parallel_deliveries,
            "receivedFilesToHistory": self.received_files_to_history,
            "pairingCode": self.pairing.view(),
            "localUrls": local_https_urls(self.port),
            "error": "",
            "backend": "tauri",
            "deviceKind": self.device_presentation.display_kind(),
            "detectedDeviceKind": self.device_presentation.detected(),
            "deviceKindOverride": self.device_presentation.override_name(),
        })
    }

}

/// Receive-file I/O is serialized independently from the host/window state.
#[derive(Debug)]
pub struct LanReceiver {
    sessions: HashMap<String, ReceiveSession>,
    lan_root: PathBuf,
}

impl LanReceiver {
    pub fn new(lan_root: PathBuf) -> Self {
        Self { sessions: HashMap::new(), lan_root }
    }

    fn session_key(device_id: &str, message_id: &str) -> String {
        format!("{device_id}:{message_id}")
    }

    pub fn prepare_inbound(
        &mut self,
        request: LanPrepareMessageRequest,
        auth_device: &str,
        already_committed: bool,
        max_file_size: i64,
    ) -> Result<Value, String> {
        if already_committed {
            return Ok(json!({ "alreadyCommitted": true, "sessionId": "", "requiredAttachmentIds": [] }));
        }
        validate_prepare(&request, auth_device, max_file_size)?;
        let key = Self::session_key(auth_device, &request.id);
        if let Some(existing) = self.sessions.get(&key) {
            let required: Vec<String> = request
                .attachments
                .iter()
                .filter(|a| !existing.uploaded.contains_key(&a.id))
                .map(|a| a.id.clone())
                .collect();
            return Ok(json!({
                "alreadyCommitted": false,
                "sessionId": existing.session_id,
                "requiredAttachmentIds": required,
            }));
        }
        let folder = self
            .lan_root
            .join("Temp")
            .join(safe_identifier(auth_device)?)
            .join(safe_identifier(&request.id)?);
        #[cfg(test)]
        crate::host::lan_receive_tests::before_receive_io();
        let _ = std::fs::create_dir_all(&folder);
        let session = ReceiveSession {
            session_id: Uuid::new_v4().simple().to_string(),
            sender_device_id: auth_device.to_string(),
            request: request.clone(),
            folder,
            uploaded: HashMap::new(),
            created_at: Instant::now(),
        };
        let required: Vec<String> = request.attachments.iter().map(|a| a.id.clone()).collect();
        let session_id = session.session_id.clone();
        self.sessions.insert(key, session);
        Ok(json!({
            "alreadyCommitted": false,
            "sessionId": session_id,
            "requiredAttachmentIds": required,
        }))
    }

    pub fn upload_inbound(
        &mut self,
        auth_device: &str,
        message_id: &str,
        attachment_id: &str,
        body: &[u8],
        content_length: i64,
    ) -> Result<(), String> {
        let message_id = safe_identifier(message_id)?;
        let attachment_id = safe_identifier(attachment_id)?;
        let key = Self::session_key(auth_device, message_id);
        let session = self
            .sessions
            .get_mut(&key)
            .ok_or_else(|| "接收会话不存在或已过期。请重新准备消息。".to_string())?;
        let attachment = session
            .request
            .attachments
            .iter()
            .find(|a| a.id == attachment_id)
            .cloned()
            .ok_or_else(|| "附件不属于当前消息。".to_string())?;
        // C# `UploadAsync`: header length first, then overflow / incomplete / hash.
        if content_length != attachment.size_bytes {
            return Err("附件长度与声明不一致。".into());
        }
        if body.len() as i64 > attachment.size_bytes {
            return Err("附件内容超过声明大小。".into());
        }
        if body.len() as i64 != attachment.size_bytes {
            return Err("附件内容不完整。".into());
        }
        #[cfg(test)]
        crate::host::lan_receive_tests::before_receive_io();
        let actual = sha256_hex(body);
        if !fixed_time_eq_hex(&attachment.sha256, &actual) {
            return Err("附件 SHA-256 校验失败。".into());
        }
        let path = session.folder.join(format!("{}.part", attachment_id));
        if let Err(err) = std::fs::write(&path, body) {
            let _ = std::fs::remove_file(&path);
            return Err(format!("io:{err}"));
        }
        session.uploaded.insert(attachment.id, path);
        Ok(())
    }

    pub fn take_commit(
        &mut self,
        auth_device: &str,
        message_id: &str,
    ) -> Result<ReceiveSession, String> {
        let key = Self::session_key(auth_device, message_id);
        let session = self
            .sessions
            .get(&key)
            .ok_or_else(|| "接收会话不存在或已过期。".to_string())?;
        let missing = session
            .request
            .attachments
            .iter()
            .filter(|a| !session.uploaded.contains_key(&a.id))
            .count();
        if missing > 0 {
            return Err(format!("仍有 {missing} 个附件未上传。"));
        }
        Ok(self.sessions.remove(&key).expect("session"))
    }

    pub fn cancel_inbound(&mut self, auth_device: &str, message_id: &str) {
        if let Some(session) = self.sessions.remove(&Self::session_key(auth_device, message_id)) {
            #[cfg(test)]
            crate::host::lan_receive_tests::before_receive_io();
            let _ = std::fs::remove_dir_all(session.folder);
        }
    }
}

fn validate_prepare(
    request: &LanPrepareMessageRequest,
    sender_device_id: &str,
    max_file_size: i64,
) -> Result<(), String> {
    safe_identifier(&request.id)?;
    if request.sender_device_id != sender_device_id {
        return Err("发送设备身份不匹配。".into());
    }
    if request.text.len() > MAX_TEXT_LENGTH {
        return Err("消息正文过长。".into());
    }
    if request.attachments.len() > MAX_ATTACHMENTS {
        return Err("附件数量过多。".into());
    }
    if request.text.is_empty() && request.attachments.is_empty() {
        return Err("消息不能为空。".into());
    }
    if request.target_kind != "direct" && request.target_kind != "group" {
        return Err("消息目标类型无效。".into());
    }
    let now = Utc::now();
    if request.created_at_utc > now + chrono::Duration::hours(12)
        || request.created_at_utc < now - chrono::Duration::days(365 * 10)
    {
        return Err("消息时间无效。".into());
    }
    let mut ids = std::collections::HashSet::new();
    for attachment in &request.attachments {
        safe_identifier(&attachment.id)?;
        if !ids.insert(attachment.id.clone()) {
            return Err("附件标识重复。".into());
        }
        if attachment.size_bytes < 0 || attachment.size_bytes > max_file_size {
            return Err("附件超过本机接收上限。".into());
        }
        if attachment.sha256.len() != 64 || !attachment.sha256.chars().all(|c| c.is_ascii_hexdigit())
        {
            return Err("附件 SHA-256 格式无效。".into());
        }
    }
    Ok(())
}

/// C# `LanShareClient.ValidatePeerEndpoint`.
pub fn validate_peer_endpoint(base_url: &str, fingerprint: &str) -> Result<(), String> {
    if base_url.trim().is_empty() {
        return Err("局域网设备地址不能为空。".into());
    }
    let Some((_, port)) = parse_https_host_port(base_url) else {
        return Err("局域网设备地址必须是带端口的 HTTPS 地址。".into());
    };
    if port == 443 {
        return Err("局域网设备地址必须是带端口的 HTTPS 地址。".into());
    }
    if fingerprint.len() != 64 || !fingerprint.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("设备证书指纹无效。".into());
    }
    Ok(())
}

pub fn parse_https_host_port(url: &str) -> Option<(String, u16)> {
    let rest = url.strip_prefix("https://")?;
    let hostport = rest.split('/').next()?;
    if let Some((host, port)) = hostport.rsplit_once(':') {
        Some((
            host.trim_start_matches('[').trim_end_matches(']').to_string(),
            port.parse().ok()?,
        ))
    } else {
        Some((hostport.to_string(), 443))
    }
}

/// C# `LanShareClient.TryProbeAsync` uses a 2s `HttpClient.Timeout`.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

enum HttpBody<'a> {
    Bytes(&'a [u8]),
    File(&'a Path),
}

pub fn https_get(
    host: &str,
    port: u16,
    path: &str,
    fingerprint: &str,
    extra_headers: &[(&str, &str)],
) -> Result<Value, String> {
    let (_status, text) = https_request(
        host,
        port,
        "GET",
        path,
        fingerprint,
        HttpBody::Bytes(&[]),
        "application/octet-stream",
        extra_headers,
        Some(PROBE_TIMEOUT),
    )?;
    serde_json::from_str(&text).or_else(|_| Ok(json!({ "raw": text })))
}

pub fn probe_paired_peer(peer: &LanDiscoveredPeer) -> Option<LanDiscoveredPeer> {
    let (host, port) = parse_https_host_port(&peer.base_url)?;
    let info = https_get(&host, port, "/cliplink/v1/info", &peer.certificate_fingerprint, &[]).ok()?;
    let device_id = info.get("deviceId").and_then(|v| v.as_str())?;
    if device_id != peer.device_id {
        return None;
    }
    let fingerprint = info
        .get("certificateFingerprint")
        .and_then(|v| v.as_str())?;
    if !fixed_time_eq_hex(fingerprint, &peer.certificate_fingerprint) {
        return None;
    }
    let alias = info
        .get("alias")
        .and_then(|v| v.as_str())
        .unwrap_or(&peer.alias)
        .to_string();
    Some(LanDiscoveredPeer {
        alias,
        last_seen_at_utc: Utc::now(),
        pairing_available: info
            .get("pairingAvailable")
            .and_then(|v| v.as_bool())
            .unwrap_or(peer.pairing_available),
        online: true,
        device_kind: info.get("deviceKind").map(|value| crate::lan_device::DeviceKind::from_metadata(Some(value))).unwrap_or(peer.device_kind),
        ..peer.clone()
    })
}

pub fn https_post_json(
    host: &str,
    port: u16,
    path: &str,
    fingerprint: &str,
    body: &Value,
    extra_headers: &[(&str, &str)],
) -> Result<Value, String> {
    let (status, value) = https_post_json_status(host, port, path, fingerprint, body, extra_headers)?;
    if status >= 400 {
        return Err(lan_status_error(status, value));
    }
    Ok(value)
}

/// Automatic clipboard requests have a bounded timeout independent of manual
/// file deliveries. The caller already pins the paired device certificate.
pub(crate) fn https_device_sync_request(
    host:&str, port:u16, method:&str, path:&str, fingerprint:&str,
    body:&[u8], content_type:&str, headers:&[(&str,&str)],
) -> Result<Value,String> {
    let (status,text)=https_request(host,port,method,path,fingerprint,HttpBody::Bytes(body),content_type,headers,Some(Duration::from_secs(5)))?;
    let value=serde_json::from_str(&text).map_err(|_|"Invalid device synchronization response.".to_string())?;
    if status>=400{return Err(lan_status_error(status,value));}
    Ok(value)
}

pub fn https_post_json_status(
    host: &str,
    port: u16,
    path: &str,
    fingerprint: &str,
    body: &Value,
    extra_headers: &[(&str, &str)],
) -> Result<(u16, Value), String> {
    let payload = serde_json::to_vec(body).map_err(|e| e.to_string())?;
    let (status, text) = https_exchange(
        host,
        port,
        "POST",
        path,
        fingerprint,
        HttpBody::Bytes(&payload),
        "application/json; charset=utf-8",
        extra_headers,
        None,
        None,
    )?;
    let value = serde_json::from_str(&text).unwrap_or_else(|_| json!({ "raw": text }));
    Ok((status, value))
}

pub fn https_put_bytes(
    host: &str,
    port: u16,
    path: &str,
    fingerprint: &str,
    body: &[u8],
    content_type: &str,
    extra_headers: &[(&str, &str)],
) -> Result<u16, String> {
    let (status, _) = https_request(
        host,
        port,
        "PUT",
        path,
        fingerprint,
        HttpBody::Bytes(body),
        content_type,
        extra_headers,
        None,
    )?;
    Ok(status)
}

/// C# `ProgressStreamContent` PUT: stream the file in 1MiB chunks, no HTTP timeout.
pub fn https_put_file(
    host: &str,
    port: u16,
    path: &str,
    fingerprint: &str,
    file: &Path,
    content_type: &str,
    extra_headers: &[(&str, &str)],
    mut progress: impl FnMut(u64),
) -> Result<u16, String> {
    let (status, text) = https_exchange(
        host,
        port,
        "PUT",
        path,
        fingerprint,
        HttpBody::File(file),
        content_type,
        extra_headers,
        None,
        Some(&mut progress),
    )?;
    if status >= 400 {
        return Err(lan_status_error(status, text));
    }
    Ok(status)
}

pub fn https_post_empty(
    host: &str,
    port: u16,
    path: &str,
    fingerprint: &str,
    extra_headers: &[(&str, &str)],
) -> Result<Value, String> {
    https_post_empty_with_timeout(host, port, path, fingerprint, extra_headers, None)
}

/// C# `TryCancelRemoteSessionAsync` uses a 2s timeout and swallows errors.
pub fn https_post_empty_with_timeout(
    host: &str,
    port: u16,
    path: &str,
    fingerprint: &str,
    extra_headers: &[(&str, &str)],
    timeout: Option<Duration>,
) -> Result<Value, String> {
    let (_status, text) = https_request(
        host,
        port,
        "POST",
        path,
        fingerprint,
        HttpBody::Bytes(&[]),
        "application/octet-stream",
        extra_headers,
        timeout,
    )?;
    if text.is_empty() {
        return Ok(json!({ "ok": true }));
    }
    serde_json::from_str(&text).or_else(|_| Ok(json!({ "raw": text })))
}

fn https_request(
    host: &str,
    port: u16,
    method: &str,
    path: &str,
    fingerprint: &str,
    body: HttpBody<'_>,
    content_type: &str,
    extra_headers: &[(&str, &str)],
    timeout: Option<Duration>,
) -> Result<(u16, String), String> {
    let (status, text) = https_exchange(
        host,
        port,
        method,
        path,
        fingerprint,
        body,
        content_type,
        extra_headers,
        timeout,
        None,
    )?;
    if status >= 400 {
        return Err(lan_status_error(status, text));
    }
    Ok((status, text))
}

fn https_exchange(
    host: &str,
    port: u16,
    method: &str,
    path: &str,
    fingerprint: &str,
    body: HttpBody<'_>,
    content_type: &str,
    extra_headers: &[(&str, &str)],
    timeout: Option<Duration>,
    mut progress: Option<&mut dyn FnMut(u64)>,
) -> Result<(u16, String), String> {
    throw_if_canceled()?;
    ensure_crypto();
    use rustls::pki_types::ServerName;
    use std::io::{Read, Write};
    use std::sync::Arc;

    let content_length = match body {
        HttpBody::Bytes(bytes) => bytes.len() as u64,
        HttpBody::File(path) => std::fs::metadata(path).map_err(|e| e.to_string())?.len(),
    };
    let config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(FingerprintPin(fingerprint.to_string())))
        .with_no_client_auth();
    let server_name = if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        ServerName::IpAddress(ip.into())
    } else {
        ServerName::try_from(host.to_string()).map_err(|e| e.to_string())?
    };
    let conn = rustls::ClientConnection::new(Arc::new(config), server_name).map_err(|e| e.to_string())?;
    let sock = connect_peer(host, port, timeout)?;
    let mut tls = rustls::StreamOwned::new(conn, sock);
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}:{port}\r\nContent-Type: {content_type}\r\nContent-Length: {content_length}\r\nConnection: close\r\n"
    );
    for (k, v) in extra_headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    tls.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    match body {
        HttpBody::Bytes(bytes) if !bytes.is_empty() => {
            tls.write_all(bytes).map_err(|e| e.to_string())?;
        }
        HttpBody::Bytes(_) => {}
        HttpBody::File(path) => {
            let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
            let mut buf = vec![0u8; 1024 * 1024];
            let mut sent = 0u64;
            loop {
                throw_if_canceled()?;
                let n = file.read(&mut buf).map_err(|e| e.to_string())?;
                if n == 0 {
                    break;
                }
                tls.write_all(&buf[..n]).map_err(|e| e.to_string())?;
                sent += n as u64;
                if let Some(cb) = progress.as_mut() {
                    cb(sent);
                }
            }
        }
    }
    let mut raw = Vec::new();
    loop {
        let mut chunk = [0u8; 8192];
        match tls.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => raw.extend_from_slice(&chunk[..n]),
            Err(err) => {
                if http_message_complete(&raw) {
                    break;
                }
                return Err(err.to_string());
            }
        }
        if http_message_complete(&raw) {
            break;
        }
    }
    let header_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| "incomplete HTTP response".to_string())?;
    let head = String::from_utf8_lossy(&raw[..header_end]);
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = if header_end + 4 <= raw.len() {
        String::from_utf8_lossy(&raw[header_end + 4..]).into_owned()
    } else {
        String::new()
    };
    Ok((status, body))
}

fn http_message_complete(raw: &[u8]) -> bool {
    let Some(header_end) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
        return false;
    };
    let head = String::from_utf8_lossy(&raw[..header_end]);
    let need = head.lines().find_map(|line| {
        line.split_once(':').and_then(|(k, v)| {
            k.eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse::<usize>().ok())
                .flatten()
        })
    });
    match need {
        Some(n) => raw.len() >= header_end + 4 + n,
        None => true,
    }
}

fn connect_peer(host: &str, port: u16, timeout: Option<Duration>) -> Result<std::net::TcpStream, String> {
    use std::net::{TcpStream, ToSocketAddrs};
    let addrs = (host, port).to_socket_addrs().map_err(|e| e.to_string())?;
    let mut last = None;
    for addr in addrs {
        let result = match timeout {
            Some(limit) => TcpStream::connect_timeout(&addr, limit),
            None => TcpStream::connect(addr),
        };
        match result {
            Ok(sock) => {
                if let Some(limit) = timeout {
                    sock.set_read_timeout(Some(limit)).ok();
                    sock.set_write_timeout(Some(limit)).ok();
                }
                return Ok(sock);
            }
            Err(err) => last = Some(err),
        }
    }
    Err(last
        .map(|e| e.to_string())
        .unwrap_or_else(|| "无法连接局域网设备。".into()))
}

fn ensure_crypto() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[derive(Debug)]
struct FingerprintPin(String);

impl rustls::client::danger::ServerCertVerifier for FingerprintPin {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if fixed_time_eq_hex(&sha256_hex(end_entity.as_ref()), &self.0) {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_broadcast_uses_the_adapters_actual_prefix() {
        use std::net::Ipv4Addr;
        let target = |address: &str, prefix_length| LanDiscoveryInterface {
            address: address.parse().unwrap(), prefix_length,
        }.broadcast_address();
        assert_eq!(target("192.168.31.100", 24), Some(Ipv4Addr::new(192, 168, 31, 255)));
        assert_eq!(target("10.12.34.56", 8), Some(Ipv4Addr::new(10, 255, 255, 255)));
        assert_eq!(target("10.12.34.56", 1), None); // Its calculated broadcast falls in loopback.
        assert_eq!(target("128.12.34.56", 1), None); // Its calculated broadcast is the global target.
        assert_eq!(target("10.12.34.56", 16), Some(Ipv4Addr::new(10, 12, 255, 255)));
        assert_eq!(target("172.24.96.1", 20), Some(Ipv4Addr::new(172, 24, 111, 255)));
        assert_eq!(target("192.168.31.100", 25), Some(Ipv4Addr::new(192, 168, 31, 127)));
        assert_eq!(target("192.168.31.100", 30), None);
        assert_eq!(target("192.168.31.101", 30), Some(Ipv4Addr::new(192, 168, 31, 103)));
        for prefix in [0, 31, 32, 33, 255] { assert_eq!(target("192.168.31.100", prefix), None); }
        for address in ["0.0.0.0", "0.1.2.3", "127.0.0.1", "224.0.0.1", "240.0.0.1", "255.255.255.255",
                        "192.168.31.0", "192.168.31.255"] {
            assert_eq!(target(address, 24), None, "{address}");
        }
    }

    #[test]
    fn discovery_packet_roundtrip_rejects_bad_fingerprint() {
        let id = LanIdentity::generate();
        assert_eq!(id.fingerprint.len(), 64);
        assert_eq!(id.fingerprint, sha256_hex(&id.cert_der));
        assert_eq!(id.device_id.len(), 32);
        assert!(id.device_id.chars().all(|c| c.is_ascii_hexdigit()));
        let bytes = LanDiscoveryPacket::query(&id, "desk", 29766, true).to_bytes();
        let parsed = LanDiscoveryPacket::parse(&bytes).unwrap();
        assert_eq!(parsed.protocol, PROTOCOL);
        assert_eq!(parsed.kind, "query");
        let mut bad = parsed.clone();
        bad.certificate_fingerprint = "zz".into();
        assert!(LanDiscoveryPacket::parse(&serde_json::to_vec(&bad).unwrap()).is_none());
    }

    #[test]
    fn lan_identity_uses_csharp_rsa_3072() {
        use rsa::pkcs8::DecodePrivateKey;
        use rsa::traits::PublicKeyParts;
        use rsa::RsaPrivateKey;
        let key = rsa_3072_key_pair();
        let der = key.serialize_der();
        let parsed = RsaPrivateKey::from_pkcs8_der(&der).expect("pkcs8");
        assert_eq!(parsed.size() * 8, 3072);
    }

    #[test]
    fn parse_https_host_port_accepts_ipv4_and_default() {
        assert_eq!(
            parse_https_host_port("https://10.0.0.2:29766/cliplink/v1/info"),
            Some(("10.0.0.2".into(), 29766))
        );
        assert_eq!(
            parse_https_host_port("https://example.com"),
            Some(("example.com".into(), 443))
        );
    }

    #[test]
    fn probe_dead_peer_returns_none() {
        let peer = LanDiscoveredPeer {
            device_id: "dead".into(),
            alias: "desk".into(),
            base_url: "https://127.0.0.1:1".into(),
            certificate_fingerprint: "a".repeat(64),
            last_seen_at_utc: Utc::now(),
            pairing_available: false,
            paired: true,
            online: false,
            token: None,
            inbound_token_hash: None,
            paired_certificate_fingerprint: None,
            device_kind: Default::default(),
            clipboard_transport: ClipboardTransport::Push,
        };
        assert!(probe_paired_peer(&peer).is_none());
    }

    #[test]
    fn pairing_code_expires_attempts() {
        let mut code = PairingCode::fresh();
        assert_eq!(code.attempts_remaining, 5);
        assert!(!code.accept("000000"));
        assert_eq!(code.attempts_remaining, 4);
        let current = code.code.clone();
        assert!(code.accept(&current));
    }

    #[test]
    fn bearer_token_is_sha256_of_utf8() {
        let mut runtime = test_runtime();
        let token = "abc123";
        runtime.discovered.insert(
            "peer1".into(),
            LanDiscoveredPeer {
                device_id: "peer1".into(),
                alias: "desk".into(),
                base_url: "https://127.0.0.1:29766".into(),
                certificate_fingerprint: "a".repeat(64),
                last_seen_at_utc: Utc::now(),
                pairing_available: false,
                paired: true,
                online: true,
                token: Some("outbound".into()),
                inbound_token_hash: Some(sha256_utf8_hex(token)),
                paired_certificate_fingerprint: Some("a".repeat(64)),
                device_kind: Default::default(),
            clipboard_transport: ClipboardTransport::Push,
            },
        );
        assert!(runtime.authenticate("peer1", Some("Bearer abc123")).is_ok());
        assert!(runtime.authenticate("peer1", Some("Bearer wrong")).is_err());
        assert!(runtime.authenticate("missing", Some("Bearer abc123")).is_err());
    }

    #[test]
    fn mime_backoff_and_peer_endpoint_match_csharp() {
        assert_eq!(resolve_mime(r"C:\a.TXT"), "text/plain");
        assert_eq!(resolve_mime("note.PNG"), "image/png");
        assert_eq!(resolve_mime("a.jpeg"), "image/jpeg");
        assert_eq!(resolve_mime("a.bin"), "application/octet-stream");
        assert_eq!(
            lan_status_error(403, r#"{"status":403,"title":"Forbidden"}"#),
            r#"局域网设备返回 403 Forbidden: {"status":403,"title":"Forbidden"}"#
        );
        assert_eq!(delivery_backoff_secs(0), 5);
        assert_eq!(delivery_backoff_secs(1), 10);
        assert_eq!(delivery_backoff_secs(6), 300);
        assert_eq!(delivery_backoff_secs(7), 300);
        assert_eq!(encode_path_segment("ab cd"), "ab%20cd");
        let fp = "a".repeat(64);
        assert!(validate_peer_endpoint("https://10.0.0.2:29766", &fp).is_ok());
        assert!(validate_peer_endpoint("https://10.0.0.2", &fp).is_err());
        assert!(validate_peer_endpoint("", &fp).is_err());
        assert!(validate_peer_endpoint("https://10.0.0.2:29766", "zz").is_err());
        let token = create_token();
        assert!(validate_token(&token).is_ok());
        use base64::Engine;
        let raw = base64::engine::general_purpose::STANDARD.decode(&token).unwrap();
        assert_eq!(raw.len(), 32);
        assert_eq!(validate_token("recv-token").unwrap_err(), "配对令牌格式无效。");
        assert_eq!(validate_token("").unwrap_err(), "配对令牌不能为空。");
        let urls = local_https_urls(29766);
        assert!(urls.iter().any(|u| u == "https://127.0.0.1:29766"));
        assert_eq!(
            urls.iter().filter(|u| *u == "https://127.0.0.1:29766").count(),
            1
        );
        assert!(urls.iter().all(|u| !u.contains("/cliplink/")));
        assert!(urls.iter().all(|u| u.starts_with("https://") && u.ends_with(":29766")));
        let mut unique = urls.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), urls.len());
        assert_eq!(urls.last().map(String::as_str), Some("https://127.0.0.1:29766"));
        assert_eq!(
            resolve_remote_base_url("https://10.0.0.2:29766", Some("10.0.0.9")).unwrap(),
            "https://10.0.0.9:29766"
        );
        assert_eq!(pair_http_status("配对码无效或已过期。"), 403);
        assert_eq!(pair_http_status("配对令牌格式无效。"), 400);
        assert_eq!(inbound_http_status("io:disk full"), 507);
        assert_eq!(inbound_http_detail("io:disk full"), "disk full");
        assert_eq!(inbound_http_status("附件长度与声明不一致。"), 400);
        assert_eq!(safe_identifier("msg_1-ok").unwrap(), "msg_1-ok");
        assert_eq!(safe_identifier("bad.id").unwrap_err(), "标识符格式无效。");
        assert_eq!(safe_identifier("   ").unwrap_err(), "标识符格式无效。");
        let problem = problem_json(403, "配对码无效或已过期。");
        assert_eq!(problem["status"], 403);
        assert_eq!(problem["title"], "Forbidden");
        assert_eq!(problem["detail"], "配对码无效或已过期。");
    }

    #[test]
    fn prepare_text_then_commit_session() {
        let runtime = test_runtime();
        let mut receiver = runtime.receiver.lock();
        let request = LanPrepareMessageRequest {
            id: "msg1".into(),
            sender_device_id: "peer1".into(),
            sender_alias: "desk".into(),
            target_kind: "direct".into(),
            text: "hello".into(),
            created_at_utc: Utc::now(),
            attachments: Vec::new(),
        };
        let prepared = receiver.prepare_inbound(request, "peer1", false, 1024).unwrap();
        assert_eq!(prepared["alreadyCommitted"], false);
        let session = receiver.take_commit("peer1", "msg1").unwrap();
        assert_eq!(session.request.text, "hello");
    }

    #[test]
    fn list_peers_marks_identity_change_like_csharp() {
        let mut runtime = test_runtime();
        let saved_fp = "a".repeat(64);
        runtime.discovered.insert(
            "peer1".into(),
            LanDiscoveredPeer {
                device_id: "peer1".into(),
                alias: "desk".into(),
                base_url: "https://10.0.0.2:29766".into(),
                certificate_fingerprint: "b".repeat(64),
                last_seen_at_utc: Utc::now(),
                pairing_available: true,
                paired: true,
                online: true,
                token: None,
                inbound_token_hash: None,
                paired_certificate_fingerprint: Some(saved_fp),
                device_kind: Default::default(),
            clipboard_transport: ClipboardTransport::Push,
            },
        );
        let listed = runtime.peers_dto();
        assert_eq!(listed[0]["alias"], "desk（身份已更改）");
        assert_eq!(listed[0]["paired"], false);
        assert_eq!(listed[0]["online"], true);
    }

    #[test]
    fn expire_unpaired_discovered_after_35s_like_csharp() {
        let mut runtime = test_runtime();
        runtime.discovered.insert(
            "stale".into(),
            LanDiscoveredPeer {
                device_id: "stale".into(),
                alias: "gone".into(),
                base_url: "https://10.0.0.8:29766".into(),
                certificate_fingerprint: "c".repeat(64),
                last_seen_at_utc: Utc::now() - chrono::Duration::seconds(36),
                pairing_available: true,
                paired: false,
                online: false,
                token: None,
                inbound_token_hash: None,
                paired_certificate_fingerprint: None,
                device_kind: Default::default(),
            clipboard_transport: ClipboardTransport::Push,
            },
        );
        runtime.discovered.insert(
            "paired-old".into(),
            LanDiscoveredPeer {
                device_id: "paired-old".into(),
                alias: "keep".into(),
                base_url: "https://10.0.0.9:29766".into(),
                certificate_fingerprint: "d".repeat(64),
                last_seen_at_utc: Utc::now() - chrono::Duration::seconds(120),
                pairing_available: false,
                paired: true,
                online: false,
                token: None,
                inbound_token_hash: None,
                paired_certificate_fingerprint: Some("d".repeat(64)),
                device_kind: Default::default(),
            clipboard_transport: ClipboardTransport::Push,
            },
        );
        runtime.expire_unpaired_discovered();
        assert!(!runtime.discovered.contains_key("stale"));
        assert!(runtime.discovered.contains_key("paired-old"));
        let listed = runtime.peers_dto();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["deviceId"], "paired-old");
        assert_eq!(listed[0]["paired"], true);
        assert_eq!(listed[0]["online"], false);
    }

    #[test]
    fn discovery_device_kind_is_optional_and_does_not_replace_packet_kind() {
        use crate::lan_device::DeviceKind;
        let mut runtime = test_runtime();
        runtime.device_presentation.set_detected(DeviceKind::Laptop);
        runtime.apply_device_preferences(&json!({"deviceKindOverride":"phone"})).unwrap();
        let query = runtime.discovery_packet("query");
        assert_eq!(query.kind, "query");
        assert_eq!(query.device_kind, Some(DeviceKind::Laptop));
        assert_eq!(runtime.status(false)["deviceKind"], "phone");
        assert_eq!(runtime.status(false)["detectedDeviceKind"], "laptop");
        let mut legacy = serde_json::to_value(&query).unwrap();
        legacy.as_object_mut().unwrap().remove("deviceKind");
        let decoded = LanDiscoveryPacket::parse(&serde_json::to_vec(&legacy).unwrap()).unwrap();
        assert_eq!(decoded.device_kind, None);
        assert_eq!(decoded.kind, "query");
        for value in [json!("future-device"), json!(42), json!({})] {
            legacy["deviceKind"] = value;
            let decoded = LanDiscoveryPacket::parse(&serde_json::to_vec(&legacy).unwrap()).unwrap();
            assert_eq!(decoded.device_kind, Some(DeviceKind::Unknown));
            assert_eq!(decoded.kind, "query");
        }
    }

    #[test]
    fn device_preferences_survive_restart_and_auto_uses_latest_peer_report() {
        use crate::lan_device::DeviceKind;
        let mut runtime = test_runtime();
        runtime.device_presentation.set_detected(DeviceKind::Laptop);
        let mut packet = runtime.discovery_packet("announce");
        packet.device_id = "peer-device".into();
        // An alias is presentation text, never evidence of a device form factor.
        packet.alias = "LAPTOP-phone-desktop".into();
        packet.device_kind = Some(DeviceKind::Phone);
        runtime.ingest(packet.clone(), "127.0.0.1");
        runtime.mark_paired("peer-device", "outbound-token".into(), Some(sha256_utf8_hex("inbound")));
        runtime.persist_paired_peers().unwrap();
        runtime.apply_device_preferences(&json!({"deviceKindOverride":"desktop",
            "peerDeviceKindOverrides":{"peer-device":"laptop"}})).unwrap();
        runtime.save_settings().unwrap();
        let old_peer = runtime.discovered["peer-device"].clone();
        assert_eq!(runtime.peers_dto()[0]["deviceKind"], "laptop");
        assert_eq!(runtime.peers_dto()[0]["detectedDeviceKind"], "phone");

        let mut reopened = test_runtime();
        reopened.peers_path = runtime.peers_path.clone();
        reopened.load_paired_peers();
        reopened.load_settings();
        reopened.device_presentation.set_detected(DeviceKind::Laptop);
        assert_eq!(reopened.status(false)["deviceKind"], "desktop");
        assert_eq!(reopened.peers_dto()[0]["deviceKind"], "laptop");
        assert_eq!(reopened.discovered["peer-device"].device_kind, DeviceKind::Phone);
        assert_eq!(reopened.discovered["peer-device"].token, old_peer.token);
        assert_eq!(reopened.discovered["peer-device"].paired_certificate_fingerprint, old_peer.paired_certificate_fingerprint);

        packet.device_kind = Some(DeviceKind::Desktop);
        reopened.ingest(packet.clone(), "127.0.0.1");
        assert_eq!(reopened.peers_dto()[0]["deviceKind"], "laptop");
        assert_eq!(reopened.peers_dto()[0]["detectedDeviceKind"], "desktop");
        // Old peers do not erase a previously learned report by omitting metadata.
        packet.device_kind = None;
        reopened.ingest(packet, "127.0.0.1");
        assert_eq!(reopened.peers_dto()[0]["detectedDeviceKind"], "desktop");
        reopened.apply_device_preferences(&json!({"deviceKindOverride":"auto",
            "peerDeviceKindOverrides":{"peer-device":"auto"}})).unwrap();
        assert_eq!(reopened.status(false)["deviceKind"], "laptop");
        assert_eq!(reopened.peers_dto()[0]["deviceKind"], "desktop");
        assert_eq!(reopened.peers_dto()[0]["deviceKindOverride"], "auto");

        let mut stored = serde_json::to_value(old_peer).unwrap();
        stored.as_object_mut().unwrap().remove("deviceKind");
        let old: LanDiscoveredPeer = serde_json::from_value(stored).unwrap();
        assert_eq!(old.device_kind, DeviceKind::Unknown);
    }

    #[test]
    fn device_override_rejects_unpaired_and_changed_identity_without_partial_changes() {
        let mut runtime = test_runtime();
        let mut packet = runtime.discovery_packet("announce");
        packet.device_id = "peer-device".into();
        runtime.ingest(packet.clone(), "127.0.0.1");
        let request = json!({"deviceKindOverride":"phone", "peerDeviceKindOverrides":{"peer-device":"laptop"}});
        let before = runtime.device_presentation.clone();
        assert!(runtime.apply_device_preferences(&request).is_err());
        assert_eq!(runtime.device_presentation, before);
        runtime.mark_paired("peer-device", "token".into(), None);
        runtime.apply_device_preferences(&request).unwrap();
        packet.certificate_fingerprint = "a".repeat(64);
        runtime.ingest(packet, "127.0.0.1");
        let presentation = runtime.device_presentation.clone();
        assert!(runtime.apply_device_preferences(&json!({"deviceKindOverride":"desktop",
            "peerDeviceKindOverrides":{"peer-device":"phone"}})).is_err());
        assert_eq!(runtime.device_presentation, presentation);
    }

    #[test]
    fn device_preference_config_write_failure_rolls_back_and_retries() {
        crate::clipboard::force_headless(true);
        let fixture = test_runtime();
        let paths = crate::AppPaths::from_data_root(fixture.lan_root.join("host"));
        let host = crate::HostState::open(paths.clone(), 28965).unwrap();
        host.lan_accept_inbound_pair(&json!({"deviceId":"icon-peer", "alias":"peer",
            "baseUrl":"https://127.0.0.1:29766", "certificateFingerprint":"b".repeat(64),
            "pairingCode":host.lan_refresh_code()["code"], "tokenForReceiver":create_token(),
            "deviceKind":"phone"})).unwrap();
        host.lan_config_update(&json!({"enabled":false, "deviceAlias":"original", "deviceKindOverride":"laptop",
            "peerDeviceKindOverrides":{"icon-peer":"laptop"}})).unwrap();
        assert_eq!(host.lan_status()["enabled"], false);
        let settings = paths.configs.join("LanShare/settings.json");
        let backup = settings.with_file_name("settings.saved.json");
        let before = std::fs::read(&settings).unwrap();
        std::fs::rename(&settings, &backup).unwrap();
        std::fs::create_dir(&settings).unwrap();
        let request = json!({"deviceAlias":"changed", "enabled":true, "deviceKindOverride":"phone",
            "peerDeviceKindOverrides":{"icon-peer":"desktop"}});
        assert!(host.lan_config_update(&request).is_err());
        assert_eq!(host.lan_status()["enabled"], false);
        assert_eq!(host.lan_status()["deviceAlias"], "original");
        assert_eq!(host.lan_status()["deviceKindOverride"], "laptop");
        assert_eq!(host.lan_peers()[0]["deviceKindOverride"], "laptop");
        assert_eq!(host.lan_peers()[0]["detectedDeviceKind"], "phone");
        assert_eq!(std::fs::read(&backup).unwrap(), before);
        std::fs::remove_dir(&settings).unwrap();
        std::fs::rename(&backup, &settings).unwrap();
        assert!(host.lan_config_update(&json!({"deviceAlias":"invalid change", "deviceKindOverride":"invalid"})).is_err());
        assert_eq!(host.lan_status()["deviceAlias"], "original");
        host.lan_config_update(&request).unwrap();
        drop(host);
        let reopened = crate::HostState::open(paths, 28965).unwrap();
        assert_eq!(reopened.lan_status()["enabled"], true);
        assert_eq!(reopened.lan_status()["deviceAlias"], "changed");
        assert_eq!(reopened.lan_status()["deviceKindOverride"], "phone");
        assert_eq!(reopened.lan_peers()[0]["deviceKindOverride"], "desktop");
        assert_eq!(reopened.lan_peers()[0]["detectedDeviceKind"], "phone");
    }

    fn test_runtime() -> LanRuntime {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../.tmp/lan-core-tests")
            .join(Uuid::new_v4().simple().to_string());
        let _ = std::fs::create_dir_all(dir.join("Temp"));
        LanRuntime {
            identity: LanIdentity::generate(),
            enabled: true,
            alias: "local".into(),
            port: 29766,
            pairing: PairingCode::fresh(),
            discovered: HashMap::new(),
            conversations: Vec::new(),
            inbox: dir.join("Inbox").display().to_string(),
            pending_kind: None,
            receiver: Arc::new(parking_lot::Mutex::new(LanReceiver::new(dir.clone()))),
            lan_root: dir.clone(),
            peers_path: dir.join("peers.json"),
            max_incoming_file_size_bytes: DEFAULT_MAX_INCOMING_FILE_SIZE_BYTES,
            max_parallel_deliveries: DEFAULT_MAX_PARALLEL_DELIVERIES,
            received_files_to_history: false,
            device_presentation: Default::default(),
        }
    }
}
