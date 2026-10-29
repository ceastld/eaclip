//! Outbound-only HTTPS ClipRelay worker. No desktop listener or LAN grant is used.
use cliphost_core::{
    clipboard_sync::{SyncAck, SyncAckStatus, SyncEvent, SyncKind},
    relay_sync::{Invitation, MAX_PACKET},
    HostState,
};
use rustls::{
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, ServerName, UnixTime},
    DigitallySignedStruct, SignatureScheme,
};
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::time::{sleep, timeout};

#[derive(Debug)]
struct PinnedVerifier {
    inner: Arc<dyn ServerCertVerifier>,
    pin: String,
}
impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        cert: &CertificateDer<'_>,
        chain: &[CertificateDer<'_>],
        name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.inner
            .verify_server_cert(cert, chain, name, ocsp, now)?;
        if !cliphost_core::lan::fixed_time_eq_hex(
            &cliphost_core::model::sha256_hex(cert.as_ref()),
            &self.pin,
        ) {
            return Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ));
        }
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(m, c, d)
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(m, c, d)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}
struct Response {
    status: u16,
    headers: HashMap<String, String>,
    body: Vec<u8>,
    observed: cliphost_core::host::RelayObservation,
}
#[derive(Clone, Copy)]
enum Purpose {
    Poll,
    Event,
    Ack,
}
fn permitted(host: &HostState, generation: u64, purpose: Purpose) -> bool {
    host.relay_session()
        .is_some_and(|(_, current, send, _, _)| {
            current == generation && (!matches!(purpose, Purpose::Event) || send)
        })
}
async fn request(
    host: &HostState,
    i: &Invitation,
    generation: u64,
    purpose: Purpose,
    after: Option<u64>,
    method: &str,
    suffix: &str,
    body: &[u8],
) -> Result<Response, String> {
    timeout(
        Duration::from_secs(55),
        request_inner(
            host, i, generation, purpose, after, method, suffix, body, None,
        ),
    )
    .await
    .map_err(|_| "Relay request timed out.".to_string())?
}
async fn request_inner(
    host: &HostState,
    i: &Invitation,
    generation: u64,
    purpose: Purpose,
    after: Option<u64>,
    method: &str,
    suffix: &str,
    body: &[u8],
    roots_override: Option<rustls::RootCertStore>,
) -> Result<Response, String> {
    if !permitted(host, generation, purpose) {
        return Err("Relay permission changed.".into());
    }
    let url = url_parse(&i.base_url)?;
    let name = url.host_str().ok_or("Invalid Relay host.")?.to_owned();
    let port = url.port_or_known_default().ok_or("Invalid Relay port.")?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let roots = roots_override.unwrap_or_else(|| {
        rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned())
    });
    let normal = rustls::client::WebPkiServerVerifier::builder_with_provider(
        Arc::new(roots),
        provider.clone(),
    )
    .build()
    .map_err(|_| "Cannot initialize Relay certificate validation.")?;
    let verifier = Arc::new(PinnedVerifier {
        inner: normal,
        pin: i.certificate_sha256.clone(),
    });
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|_| "Cannot initialize Relay TLS.")?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    tls.alpn_protocols = vec![b"http/1.1".to_vec()];
    tls.enable_sni = i.send_sni;
    let tcp = tokio::net::TcpStream::connect((name.as_str(), port))
        .await
        .map_err(|_| "Cannot connect to Relay.")?;
    let connection = tokio_rustls::TlsConnector::from(Arc::new(tls))
        .connect(
            ServerName::try_from(name.clone()).map_err(|_| "Invalid Relay TLS host.")?,
            tcp,
        )
        .await
        .map_err(|_| "Relay certificate, pin or TLS validation failed.")?;
    if !permitted(host, generation, purpose) {
        return Err("Relay permission changed.".into());
    }
    // Bearer and payload are sent only after the CA + hostname + pin checks.
    let mut stream = BufReader::new(connection);
    let authority = if name.contains(':') {
        format!("[{name}]:{port}")
    } else {
        format!("{name}:{port}")
    };
    let path = format!("{}{suffix}", url.path().trim_end_matches('/'));
    let confirmed = after
        .map(|n| format!("X-Relay-After: {n}\r\n"))
        .unwrap_or_default();
    let header=format!("{method} {path} HTTP/1.1\r\nHost: {authority}\r\nAuthorization: Bearer {}\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\n{confirmed}Connection: close\r\n\r\n",i.token,body.len());
    stream
        .write_all(header.as_bytes())
        .await
        .map_err(|_| "Relay request write failed.")?;
    stream
        .write_all(body)
        .await
        .map_err(|_| "Relay upload failed.")?;
    stream
        .flush()
        .await
        .map_err(|_| "Relay request flush failed.")?;
    let mut total = 0usize;
    async fn line<R: tokio::io::AsyncBufRead + Unpin>(
        stream: &mut R,
        total: &mut usize,
    ) -> Result<String, String> {
        let mut bytes = Vec::new();
        let read = stream
            .take((65537usize.saturating_sub(*total)) as u64)
            .read_until(b'\n', &mut bytes)
            .await
            .map_err(|_| "Relay response headers failed.")?;
        *total += read;
        if read == 0 || *total > 65536 || !bytes.ends_with(b"\r\n") {
            return Err("Invalid Relay response headers.".into());
        }
        String::from_utf8(bytes).map_err(|_| "Invalid Relay response headers.".into())
    }
    let status_line = line(&mut stream, &mut total).await?;
    let mut pieces = status_line.split_whitespace();
    if !matches!(pieces.next(), Some("HTTP/1.1" | "HTTP/1.0")) {
        return Err("Invalid Relay HTTP version.".into());
    }
    let status = pieces
        .next()
        .ok_or("Missing Relay status.")?
        .parse::<u16>()
        .map_err(|_| "Invalid Relay status.")?;
    if (300..400).contains(&status) {
        return Err("Relay redirects are refused.".into());
    }
    let mut headers = HashMap::new();
    loop {
        let row = line(&mut stream, &mut total).await?;
        if row == "\r\n" {
            break;
        }
        let (name, value) = row
            .trim_end_matches("\r\n")
            .split_once(':')
            .ok_or("Invalid Relay response header.")?;
        let name = name.to_ascii_lowercase();
        if headers.insert(name, value.trim().to_owned()).is_some() {
            return Err("Duplicate Relay response header.".into());
        }
    }
    if headers.contains_key("transfer-encoding") {
        return Err("Relay response needs an unambiguous Content-Length.".into());
    }
    let len = if status == 204 {
        0
    } else {
        headers
            .get("content-length")
            .ok_or("Relay response length missing.")?
            .parse::<usize>()
            .map_err(|_| "Invalid Relay response length.")?
    };
    if len > MAX_PACKET {
        return Err("Relay response exceeds packet limit.".into());
    }
    let observed = host.relay_observation();
    let mut bytes = vec![0; len];
    stream
        .read_exact(&mut bytes)
        .await
        .map_err(|_| "Relay download was interrupted.")?;
    Ok(Response {
        status,
        headers,
        body: bytes,
        observed,
    })
}
fn url_parse(s: &str) -> Result<url::Url, String> {
    url::Url::parse(s).map_err(|_| "Invalid Relay URL.".into())
}
fn events(i: &Invitation) -> String {
    format!("/v1/rooms/{}/events", i.room_id)
}
async fn post(
    host: &HostState,
    i: &Invitation,
    id: &str,
    packet: &[u8],
    generation: u64,
    after: Option<u64>,
) -> Result<(), String> {
    let response = request(
        host,
        i,
        generation,
        if after.is_some() {
            Purpose::Ack
        } else {
            Purpose::Event
        },
        after,
        "POST",
        &format!("{}/{}?to={}", events(i), id, i.peer_device_id),
        packet,
    )
    .await?;
    if !matches!(response.status, 200 | 201) {
        return Err(format!("Relay rejected upload (HTTP {}).", response.status));
    }
    let value: Value =
        serde_json::from_slice(&response.body).map_err(|_| "Invalid Relay upload receipt.")?;
    if value["cursor"].as_u64().is_none() {
        return Err("Relay upload cursor missing.".into());
    }
    Ok(())
}

pub fn start(host: HostState) {
    if cliphost_core::is_headless() {
        return;
    }
    start_worker(host);
}
#[cfg(debug_assertions)]
pub fn start_fixture(host: HostState) -> Result<(), String> {
    if !cliphost_core::is_headless()
        || !host
            .data_root()
            .components()
            .any(|c| c.as_os_str() == ".tmp")
    {
        return Err("Relay fixture requires headless mode and an explicit .tmp data root.".into());
    }
    start_worker(host);
    Ok(())
}
struct Recovery {
    draining: bool,
}
impl Recovery {
    fn new() -> Self {
        Self { draining: true }
    }
    fn disconnected(&mut self) {
        self.draining = true;
    }
    fn completed(&mut self, caught_up: bool) {
        if caught_up {
            self.draining = false;
        }
    }
    fn archive(&self, replay: bool) -> bool {
        self.draining || replay
    }
}
fn start_worker(host: HostState) {
    if !host.relay_claim_worker() {
        return;
    }
    let sender = host.clone();
    let mut outgoing = sender.relay_subscribe();
    tokio::spawn(async move {
        loop {
            let mut captured = match outgoing.recv().await {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            };
            loop {
                while let Ok(newer) = outgoing.try_recv() {
                    captured = newer;
                }
                let Some((inv, generation, send, _, _)) = sender.relay_session() else {
                    break;
                };
                if !send || generation != captured.generation {
                    break;
                }
                let event = &captured.event;
                let sending = async {
                    let work_host = sender.clone();
                    let work_event = (*event).clone();
                    let work_inv = inv.clone();
                    let packet = tokio::task::spawn_blocking(move || {
                        let media = if work_event.kind == SyncKind::Image {
                            std::fs::read(work_host.sync_read_media(&work_event.sha256)?)
                                .map_err(|_| "Cannot read Relay image.")?
                        } else {
                            Vec::new()
                        };
                        work_inv.seal(
                            &work_event.event_id,
                            &json!({"type":"event","event":work_event,"replay":false}),
                            &media,
                        )
                    })
                    .await
                    .map_err(|_| "Relay encryption task failed.")??;
                    if !sender.relay_session_current(generation) {
                        return Err("Relay permission changed.".into());
                    }
                    post(&sender, &inv, &event.event_id, &packet, generation, None).await
                };
                tokio::select! {
                    result=sending=>{sender.relay_error(result.as_ref().err().map(String::as_str));break;},
                    newer=outgoing.recv()=>{match newer {Ok(newer)=>captured=newer,Err(_)=>break}}
                }
            }
        }
    });
    tokio::spawn(async move {
        let mut generation_seen = 0;
        let mut recovery = Recovery::new();
        loop {
            let Some((inv, generation, _, receive, cursor)) = host.relay_session() else {
                generation_seen = 0;
                sleep(Duration::from_millis(500)).await;
                continue;
            };
            if generation_seen != generation {
                generation_seen = generation;
                recovery.disconnected();
            }
            let result = async {
                if let Some(pending) = host.relay_pending_ack(generation) {
                    post(
                        &host,
                        &inv,
                        &pending.id,
                        &pending.packet,
                        generation,
                        Some(pending.cursor),
                    )
                    .await?;
                    host.relay_cursor_commit(generation, pending.cursor)?;
                    return Ok(false);
                }
                let response = request(
                    &host,
                    &inv,
                    generation,
                    Purpose::Poll,
                    None,
                    "GET",
                    &format!(
                        "{}?after={cursor}&waitMs={}",
                        events(&inv),
                        if recovery.draining { 0 } else { 25000 }
                    ),
                    &[],
                )
                .await?;
                if !host.relay_session_current(generation) {
                    return Err("Relay permission changed.".into());
                }
                if !matches!(response.status, 200 | 204) {
                    return Err(format!("Relay poll failed (HTTP {}).", response.status));
                }
                let next = response
                    .headers
                    .get("x-relay-cursor")
                    .ok_or("Relay cursor missing.")?
                    .parse::<u64>()
                    .map_err(|_| "Invalid Relay cursor.")?;
                if next < cursor {
                    return Err("Relay cursor regressed.".into());
                }
                if response.status == 204 {
                    host.relay_cursor_commit(generation, next)?;
                    return Ok(true);
                }
                let sender = response
                    .headers
                    .get("x-relay-sender")
                    .ok_or("Relay sender missing.")?;
                let id = response
                    .headers
                    .get("x-relay-event-id")
                    .ok_or("Relay event identity missing.")?;
                let work_inv = inv.clone();
                let work_sender = sender.clone();
                let work_id = id.clone();
                let work_packet = response.body;
                let (header, media) = tokio::task::spawn_blocking(move || {
                    work_inv.open(&work_sender, &work_id, &work_packet)
                })
                .await
                .map_err(|_| "Relay decryption task failed.")??;
                if header["type"] == "ack" {
                    let ack: SyncAck = serde_json::from_value(header["ack"].clone())
                        .map_err(|_| "Invalid Relay acknowledgement.")?;
                    host.relay_record_ack(generation, &ack);
                } else {
                    let event: SyncEvent = serde_json::from_value(header["event"].clone())
                        .map_err(|_| "Invalid Relay event.")?;
                    let mut ack = if receive {
                        let copy = host.clone();
                        let replay = recovery.archive(header["replay"] == true);
                        tokio::task::spawn_blocking(move || {
                            copy.relay_receive(event, &media, generation, response.observed, replay)
                        })
                        .await
                        .map_err(|_| "Relay receive task failed.")??
                    } else {
                        SyncAck {
                            message_type: "ack".into(),
                            version: 1,
                            event_id: event.event_id,
                            status: SyncAckStatus::Failed,
                            error: Some("receivingDisabled".into()),
                        }
                    };
                    if receive && ack.error.is_some() {
                        ack.error = Some("clipboardWriteFailed".into());
                    }
                    let ack_id = cliphost_core::relay_sync::new_packet_id();
                    let packet = inv.seal(&ack_id, &json!({"type":"ack","ack":ack}), &[])?;
                    host.relay_save_pending_ack(generation, &ack_id, &packet, next)?;
                    loop {
                        if !host.relay_session_current(generation) {
                            return Err("Relay permission changed.".into());
                        }
                        match post(&host, &inv, &ack_id, &packet, generation, Some(next)).await {
                            Ok(()) => break,
                            Err(_) => {
                                recovery.disconnected();
                                host.relay_error(Some("Relay acknowledgement delivery failed."));
                                sleep(Duration::from_secs(2)).await
                            }
                        }
                    }
                }
                host.relay_cursor_commit(generation, next)?;
                Ok::<bool, String>(false)
            }
            .await;
            match result {
                Ok(caught_up) => {
                    recovery.completed(caught_up);
                    host.relay_error(None);
                }
                Err(error) => {
                    recovery.disconnected();
                    host.relay_error(Some(&error));
                    sleep(Duration::from_secs(2)).await;
                }
            }
        }
    });
}
#[cfg(test)]
mod tests {
    use super::*;
    use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
    async fn fixture() -> (
        tempfile::TempDir,
        HostState,
        Invitation,
        u64,
        tokio::net::TcpListener,
        tokio_rustls::TlsAcceptor,
        rustls::RootCertStore,
    ) {
        cliphost_core::force_headless(true);
        let root = tempfile::tempdir().unwrap();
        let host = HostState::open(
            cliphost_core::AppPaths::from_data_root(root.path().into()),
            29065,
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (cert, key) = host.lan_tls_der().unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let server = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(cert.clone())],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)),
            )
            .unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(CertificateDer::from(cert.clone())).unwrap();
        let params = json!({"type":"cliprelay-invitation","version":1,"baseUrl":format!("https://localhost:{port}/cliprelay"),"certificateSha256":cliphost_core::model::sha256_hex(&cert),"roomId":"fixture","deviceId":host.sync_device_id(),"peerDeviceId":"phone","token":"11".repeat(32),"keyHex":"22".repeat(32)});
        host.relay_configure(&json!({"invitation":params})).unwrap();
        host.relay_configure(&json!({"enabled":true,"sendEnabled":true,"receiveEnabled":true}))
            .unwrap();
        let (inv, generation, _, _, _) = host.relay_session().unwrap();
        (
            root,
            host,
            inv,
            generation,
            listener,
            tokio_rustls::TlsAcceptor::from(Arc::new(server)),
            roots,
        )
    }
    #[test]
    fn relay_ack_reconnection_archives_backlog_until_a_later_empty_poll() {
        let mut recovery = Recovery::new();
        assert!(recovery.archive(false));
        recovery.completed(true);
        assert!(!recovery.archive(false));
        recovery.disconnected(); // ACK POST fails after local acceptance.
        recovery.completed(false); // Retrying that ACK succeeds; backlog is still unsafe.
        assert!(
            recovery.archive(false),
            "copies queued while disconnected must not replace the receiver's clipboard"
        );
        recovery.completed(true); // Only a verified 204 establishes the live boundary.
        assert!(!recovery.archive(false));
        assert!(recovery.archive(true));
    }
    #[tokio::test]
    async fn relay_request_rechecks_original_generation_after_blocked_tls_handshake() {
        let (_root, host, inv, generation, listener, acceptor, roots) = fixture().await;
        let (connected_tx, connected_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            connected_tx.send(()).unwrap();
            release_rx.await.unwrap();
            let mut stream = acceptor.accept(tcp).await.unwrap();
            let mut bytes = [0u8; 4096];
            match timeout(Duration::from_secs(2), stream.read(&mut bytes)).await {
                Ok(Ok(n)) => n,
                _ => 0,
            }
        });
        let caller = host.clone();
        let client = tokio::spawn(async move {
            request_inner(
                &caller,
                &inv,
                generation,
                Purpose::Event,
                None,
                "POST",
                "/v1/rooms/fixture/events/old?to=phone",
                b"synthetic-payload",
                Some(roots),
            )
            .await
        });
        timeout(Duration::from_secs(5), connected_rx)
            .await
            .unwrap()
            .unwrap();
        host.relay_configure(&json!({"enabled":false})).unwrap();
        host.relay_configure(&json!({"enabled":true,"sendEnabled":false,"receiveEnabled":true}))
            .unwrap();
        release_tx.send(()).unwrap();
        assert!(client.await.unwrap().is_err());
        assert_eq!(
            server.await.unwrap(),
            0,
            "neither bearer nor clipboard bytes may be sent after revocation"
        );
    }
    #[tokio::test]
    async fn relay_business_ack_sends_after_header_before_body_for_full_queue_release() {
        let (_root, host, inv, generation, listener, acceptor, roots) = fixture().await;
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let stream = acceptor.accept(tcp).await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut headers = String::new();
            loop {
                let mut row = String::new();
                reader.read_line(&mut row).await.unwrap();
                if row == "\r\n" {
                    break;
                }
                headers.push_str(&row);
            }
            assert!(
                headers.contains("X-Relay-After: 64\r\n"),
                "ACK must release locally committed input before the server quota check"
            );
            reader
                .write_all(b"HTTP/1.1 201 Created\r\nContent-Length: 13\r\n\r\n{\"cursor\":65}")
                .await
                .unwrap();
            reader.flush().await.unwrap();
        });
        let response = request_inner(
            &host,
            &inv,
            generation,
            Purpose::Ack,
            Some(64),
            "POST",
            "/v1/rooms/fixture/events/ack?to=phone",
            b"encrypted-ack",
            Some(roots),
        )
        .await
        .unwrap();
        assert_eq!(response.status, 201);
        server.await.unwrap();
    }
}
