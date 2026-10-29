//! Real TLS on loopback, with isolated stores and a simulated clipboard writer.
//! No discovery broadcasts, firewall changes, production data or desktop writes.
use crate::{event_bus, lan_tls, HttpState};
use cliphost_core::{
    clipboard_sync::{SyncEvent, SyncKind},
    paths::AppPaths,
    HostState,
};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

#[derive(Clone)]
struct Wire {
    port: u16,
    fingerprint: String,
    origin: String,
    authorization: String,
}
impl Wire {
    async fn post(&self, body: Value) -> (u16, Value) {
        let wire = self.clone();
        tokio::task::spawn_blocking(move || {
            cliphost_core::lan::https_post_json_status(
                "127.0.0.1",
                wire.port,
                "/cliplink/device-sync/v1/events",
                &wire.fingerprint,
                &body,
                &[
                    ("X-ClipLink-Device", &wire.origin),
                    ("Authorization", &wire.authorization),
                ],
            )
        })
        .await
        .unwrap()
        .unwrap()
    }
    async fn media(&self, id: &str, bytes: Vec<u8>) -> Result<u16, String> {
        let wire = self.clone();
        let path = format!("/cliplink/device-sync/v1/events/{id}/media");
        tokio::task::spawn_blocking(move || {
            cliphost_core::lan::https_put_bytes(
                "127.0.0.1",
                wire.port,
                &path,
                &wire.fingerprint,
                &bytes,
                "application/octet-stream",
                &[
                    ("X-ClipLink-Device", &wire.origin),
                    ("Authorization", &wire.authorization),
                ],
            )
        })
        .await
        .unwrap()
    }
    fn text(&self, id: &str, seq: u64, text: &str) -> Value {
        json!(SyncEvent::text(
            id.into(),
            self.origin.clone(),
            seq,
            text.into()
        ))
    }
    fn image(&self, id: &str, seq: u64, bytes: &[u8]) -> Value {
        json!(SyncEvent {
            message_type: "clipboard".into(),
            version: 1,
            event_id: id.into(),
            origin_device_id: self.origin.clone(),
            origin_seq: seq,
            kind: SyncKind::Image,
            sha256: cliphost_core::model::sha256_hex(bytes),
            text: None,
            image: Some(cliphost_core::clipboard_sync::validate_image(bytes).unwrap()),
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn device_sync_loopback_tls_types_actions_retry_and_revocation() {
    cliphost_core::force_headless(true);
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../.tmp/device-sync-http-tests");
    std::fs::create_dir_all(&root).unwrap();
    let directory = tempfile::tempdir_in(root).unwrap();
    let a = HostState::open(AppPaths::from_data_root(directory.path().join("a")), 29065).unwrap();
    let b = HostState::open(AppPaths::from_data_root(directory.path().join("b")), 29066).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    assert!(address.ip().is_loopback());
    let port = address.port();
    b.lan_config_update(&json!({"enabled":true, "port":port}))
        .unwrap();
    let a_info = a.lan_info();
    let pairing = b
        .lan_accept_inbound_pair_from(
            &json!({
                "deviceId":a.sync_device_id(), "alias":"Windows A",
                "baseUrl":"https://127.0.0.1:29999",
                "certificateFingerprint":a_info["certificateFingerprint"],
                "tokenForReceiver":cliphost_core::lan::create_token(),
                "pairingCode":b.lan_status()["pairingCode"]["code"],
            }),
            Some("127.0.0.1"),
        )
        .unwrap();
    let wire = Wire {
        port,
        origin: a.sync_device_id(),
        fingerprint: b.lan_info()["certificateFingerprint"]
            .as_str()
            .unwrap()
            .into(),
        authorization: format!("Bearer {}", pairing["tokenForRequester"].as_str().unwrap()),
    };
    b.device_sync_config_update(&json!({"enabled":true,"textEnabled":true,"imageEnabled":false}))
        .unwrap();
    let writes = Arc::new(AtomicUsize::new(0));
    let counter = writes.clone();
    b.set_clipboard_ui_runner(move |job| {
        counter.fetch_add(1, Ordering::SeqCst);
        job()
    });
    let server = tokio::spawn(lan_tls::serve_lan_https_listener(
        HttpState {
            host: b.clone(),
            events: event_bus(),
        },
        listener,
    ));
    struct AbortServer(tokio::task::JoinHandle<std::io::Result<()>>);
    impl Drop for AbortServer {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _server = AbortServer(server);

    let event = wire.text("tls-text", 1, "  原文\r\n😀  ");
    let mut unauthorized = wire.clone();
    unauthorized.authorization = format!("Bearer {}", cliphost_core::lan::create_token());
    assert_eq!(unauthorized.post(event.clone()).await.0, 401);
    assert_eq!(writes.load(Ordering::SeqCst), 0);
    let (code, ack) = wire.post(event.clone()).await;
    assert_eq!(code, 200, "{ack}");
    assert_eq!(ack["clipboard"], "applied");
    assert_eq!(writes.load(Ordering::SeqCst), 1);
    assert_eq!(wire.post(event).await.1, ack);
    assert_eq!(
        writes.load(Ordering::SeqCst),
        1,
        "retry must not rewrite the clipboard"
    );
    let detail = b.detail(&json!({"id":ack["historyId"]})).unwrap();
    assert_eq!(detail["bodyText"], "  原文\r\n😀  ");

    let image = include_bytes!("../../../../ui-design/assets/clip-main-thumbnail.png").to_vec();
    let event = wire.image("tls-image", 2, &image);
    let (code, rejected) = wire.post(event.clone()).await;
    assert_eq!(
        code, 409,
        "images can be disabled independently: {rejected}"
    );
    assert!(rejected["detail"].as_str().unwrap().contains("disabled"));
    assert_eq!(writes.load(Ordering::SeqCst), 1);
    b.device_sync_config_update(&json!({"imageEnabled":true}))
        .unwrap();
    assert_eq!(wire.post(event.clone()).await.1["status"], "pendingMedia");
    assert_eq!(wire.media("tls-image", image.clone()).await.unwrap(), 200);
    let (_, image_ack) = wire.post(event).await;
    assert_eq!(image_ack["clipboard"], "applied");
    assert_eq!(writes.load(Ordering::SeqCst), 2);
    assert_eq!(
        b.detail(&json!({"id":image_ack["historyId"]})).unwrap()["kind"],
        "ImageClipItem"
    );

    b.device_sync_config_update(&json!({"deviceId":wire.origin, "applyClipboard":false}))
        .unwrap();
    let (_, both_effects) = wire
        .post(wire.text("legacy-action", 3, "keep history and clipboard"))
        .await;
    assert_eq!(both_effects["history"], "saved");
    assert_eq!(both_effects["clipboard"], "applied");
    assert_eq!(writes.load(Ordering::SeqCst), 3);

    assert_eq!(
        wire.post(wire.image("revoked-image", 4, &image)).await.1["status"],
        "pendingMedia"
    );
    b.device_sync_config_update(&json!({"enabled":false}))
        .unwrap();
    let rejected = wire.media("revoked-image", image).await.unwrap_err();
    assert!(rejected.contains("401 Unauthorized") && rejected.contains("disabled"));
    assert_eq!(
        writes.load(Ordering::SeqCst),
        3,
        "disabling sync invalidates an in-flight image"
    );
}
