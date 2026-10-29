use super::*;
use axum::{body::Body, http::Request};
use tower::ServiceExt;
use serde_json::json;

#[tokio::test]
async fn public_build_exposes_windows_lan_but_no_private_management_routes_or_bridge_methods() {
    cliphost_core::clipboard::force_headless(true);
    let parent = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp/private-sync-boundary");
    std::fs::create_dir_all(&parent).unwrap();
    let root = tempfile::tempdir_in(parent).unwrap();
    let host = HostState::open(cliphost_core::AppPaths::from_data_root(root.path().into()), 28965).unwrap();
    let app = router(HttpState { host:host.clone(), events:event_bus() });
    for path in ["/api/clip/mobile-sync", "/api/clip/mobile-sync/invitation", "/api/clip/mobile-sync/clients/test"] {
        let response = app.clone().oneshot(Request::builder().uri(path).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }
    for method in [
        cliphost_bridge::HostBridgeMethods::MOBILE_SYNC_STATUS_GET,
        cliphost_bridge::HostBridgeMethods::MOBILE_SYNC_CONFIG_UPDATE,
        cliphost_bridge::HostBridgeMethods::MOBILE_SYNC_INVITATION_GET,
        cliphost_bridge::HostBridgeMethods::MOBILE_SYNC_CLIENT_FORGET,
    ] {
        let reply = cliphost_bridge::handle_request(&host, &json!({
            "id":"public-boundary", "method":method,
            "params":{"enabled":true,"deviceId":"private-client"},
        }).to_string());
        assert!(!reply.ok, "{method}");
    }
    let reply = cliphost_bridge::handle_request(&host, &json!({
        "id":"windows-lan", "method":cliphost_bridge::HostBridgeMethods::DEVICE_SYNC_STATUS_GET,
    }).to_string());
    assert!(reply.ok);
    let catalog = cliphost_bridge::handle_request(&host, &json!({
        "id":"public-catalog", "method":cliphost_bridge::HostBridgeMethods::SETTINGS_CATALOG_GET,
    }).to_string());
    assert!(catalog.ok);
    assert!(!catalog.result.unwrap()["configs"].as_array().unwrap().iter().any(|row| row["id"] == "mobileSync"));
    let response = app.oneshot(Request::builder().uri("/api/health/app-info").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let info: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(info.get("relaySync").is_none());
}
