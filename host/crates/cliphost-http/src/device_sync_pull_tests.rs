//! Real loopback TLS for shared pairing, pull routing and authorization changes.
use crate::{event_bus, lan_tls, HttpState};
use cliphost_core::{lan, AppPaths, HostState};
use serde_json::{json, Value};

#[derive(Clone)]
struct Client { port: u16, pin: String, id: String, auth: String }
impl Client {
    async fn get(&self, suffix: &str) -> Result<Value, String> {
        let client = self.clone();
        let path = format!("/cliplink/device-sync/v1{suffix}");
        tokio::task::spawn_blocking(move || lan::https_get("127.0.0.1", client.port,
            &path, &client.pin, &[("X-ClipLink-Device", &client.id), ("Authorization", &client.auth)]))
            .await.unwrap()
    }
    async fn post(&self, path: &str, value: Value) -> (u16, Value) {
        let client = self.clone();
        let path = path.to_owned();
        tokio::task::spawn_blocking(move || lan::https_post_json_status("127.0.0.1", client.port,
            &path, &client.pin, &value, &[("X-ClipLink-Device", &client.id), ("Authorization", &client.auth)]))
            .await.unwrap().unwrap()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn device_sync_pull_tls_pair_poll_revocation_and_body_guards() {
    cliphost_core::force_headless(true);
    let parent = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp/device-sync-http-tests");
    std::fs::create_dir_all(&parent).unwrap();
    let root = tempfile::tempdir_in(parent).unwrap();
    let host = HostState::open(AppPaths::from_data_root(root.path().to_owned()), 29065).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    assert!(address.ip().is_loopback());
    host.lan_config_update(&json!({"enabled":true,"port":address.port()})).unwrap();
    assert_eq!(host.sync_status()["enabled"], false, "Old mobile authorization stays off");
    let mut client = Client { port:address.port(), pin:host.lan_info()["certificateFingerprint"].as_str().unwrap().into(),
        id:"http-android".into(), auth:String::new() };
    let server = tokio::spawn(lan_tls::serve_lan_https_listener(HttpState {host:host.clone(),events:event_bus()}, listener));
    struct Guard(tokio::task::JoinHandle<std::io::Result<()>>);
    impl Drop for Guard { fn drop(&mut self) { self.0.abort(); } }
    let _server = Guard(server);
    let (status, pair) = client.post("/cliplink/v1/pair", json!({
        "deviceId":client.id,"alias":"Android test","deviceKind":"phone","clipboardTransport":"pull",
        "pairingCode":host.lan_status()["pairingCode"]["code"]
    })).await;
    assert_eq!(status, 200, "{pair}");
    assert_eq!(pair["clipboardSyncPath"], "/cliplink/device-sync/v1");
    client.auth = format!("Bearer {}", pair["tokenForRequester"].as_str().unwrap());
    let empty = client.get("/events?after=0&waitMs=0").await.unwrap();
    assert_eq!(empty["events"], json!([]));
    assert_eq!(empty["reset"], false);
    let reset = client.get("/events?after=18446744073709551615&waitMs=0").await.unwrap();
    assert_eq!(reset["reset"], true);
    // Exercise an idle long poll through real TLS, including its timeout. The
    // listener remains observable by the network-isolation acceptance sampler.
    {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut roots = rustls::RootCertStore::empty();
        roots.add(rustls_pki_types::CertificateDer::from(host.lan_tls_der().unwrap().0)).unwrap();
        let config = rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::new(config));
        let socket = tokio::net::TcpStream::connect(address).await.unwrap();
        let name = rustls_pki_types::ServerName::IpAddress(address.ip().into());
        let mut tls = connector.connect(name, socket).await.unwrap();
        let request = format!("GET /cliplink/device-sync/v1/events?after=0&waitMs=5000 HTTP/1.1\r\nHost: 127.0.0.1\r\nX-ClipLink-Device: {}\r\nAuthorization: {}\r\nConnection: close\r\n\r\n",client.id,client.auth);
        let started = std::time::Instant::now();
        tls.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(std::time::Duration::from_secs(8), tls.take(64*1024).read_to_end(&mut response)).await.unwrap().unwrap();
        assert!(started.elapsed() >= std::time::Duration::from_millis(4500));
        let response = String::from_utf8(response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 "));
        let body: Value = serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body["events"],json!([]));
    }
    assert!(client.get("/events?after=bad&waitMs=0").await.unwrap_err().contains("400"));
    assert!(client.get(&format!("/blobs/{}", "a".repeat(64))).await.unwrap_err().contains("403"));
    assert_eq!(client.post("/cliplink/device-sync/v1/acks", json!({"eventId":"missing","status":"not-a-status"})).await.0,409);
    assert_eq!(client.post("/cliplink/device-sync/v1/acks", json!({"eventId":"missing","status":"failed","error":"x".repeat(5000)})).await.0,413);

    // Forget while a request is waiting; the response must authenticate again.
    let waiting = client.clone();
    let poll = tokio::spawn(async move { waiting.get("/events?after=0&waitMs=1500").await });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    host.lan_forget(&json!({"deviceId":client.id})).unwrap();
    let error = tokio::time::timeout(std::time::Duration::from_secs(2),poll).await.unwrap().unwrap().unwrap_err();
    assert!(error.contains("401"), "{error}");
    assert_eq!(client.post("/cliplink/device-sync/v1/acks",json!({"eventId":"old","status":"applied"})).await.0,401);
    assert!(client.get(&format!("/blobs/{}","a".repeat(64))).await.unwrap_err().contains("401"));
}
