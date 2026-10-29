//! Public artifacts retain Windows LAN while excluding private transports.
use super::*;

fn fixture() -> (tempfile::TempDir, HostState) {
    crate::clipboard::force_headless(true);
    let parent = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp/private-sync-boundary");
    std::fs::create_dir_all(&parent).unwrap();
    let root = tempfile::tempdir_in(parent).unwrap();
    let host = HostState::open(AppPaths::from_data_root(root.path().into()), 28965).unwrap();
    (root, host)
}

#[test]
fn public_build_rejects_private_pairing_without_consuming_windows_pair_code() {
    let (_root, host) = fixture();
    host.lan_config_update(&json!({"enabled":true})).unwrap();
    let code = host.lan_status()["pairingCode"]["code"].clone();
    let rejected = host.lan_accept_inbound_pair(&json!({
        "deviceId":"private-client", "alias":"Private client",
        "clipboardTransport":"pull", "pairingCode":code,
    }));
    assert!(rejected.unwrap_err().contains("Unsupported clipboard transport"));
    assert!(!host.lan.lock().discovered.contains_key("private-client"));
    let response = host.lan_accept_inbound_pair(&json!({
        "deviceId":"windows-peer", "alias":"Windows peer", "pairingCode":code,
        "baseUrl":"https://127.0.0.1:29999", "certificateFingerprint":"a".repeat(64),
        "tokenForReceiver":crate::lan::create_token(),
    })).unwrap();
    assert_eq!(response["deviceId"], host.sync_device_id());
    assert!(host.lan.lock().discovered["windows-peer"].trusted());
    assert!(host.clipboard_sync.devices.enabled());
    assert!(!host.clipboard_sync.state.lock().durable.enabled);
    for info in [host.lan_info(), serde_json::from_slice(&host.lan_announce_bytes("announce")).unwrap()] {
        assert!(!info.get("capabilities").and_then(Value::as_array)
            .is_some_and(|caps| caps.iter().any(|cap| cap == "pull")));
    }
}

#[test]
fn public_build_does_not_activate_private_sync_verb_or_load_private_credentials() {
    let (root, host) = fixture();
    let reply = host.execute_verb("relay-sync start both").unwrap();
    assert_eq!(reply, crate::verb::verb_ok(json!({})));
    assert!(!root.path().join("Configs/ClipboardSync/relay.json").exists());
    assert!(!host.clipboard_sync.private_capture_enabled());
    assert!(!host.clipboard_sync.raw_enabled.load(Ordering::Relaxed));
}

#[test]
fn public_build_never_trusts_saved_client_only_peer_as_windows_lan_authority() {
    let (_root, host) = fixture();
    let token = crate::lan::create_token();
    let peer: crate::lan::LanDiscoveredPeer = serde_json::from_value(json!({
        "clipboardTransport":"pull", "deviceId":"saved-private-peer", "alias":"Private peer",
        "baseUrl":"", "certificateFingerprint":"", "lastSeenAtUtc":Utc::now(),
        "pairingAvailable":false, "paired":true, "online":false,
        "inboundTokenHash":crate::model::sha256_hex(token.as_bytes()), "deviceKind":"phone",
    })).unwrap();
    assert!(peer.is_pull());
    assert!(!peer.trusted());
    host.lan.lock().discovered.insert(peer.device_id.clone(), peer);
    assert!(host.lan.lock().authenticate("saved-private-peer", Some(&format!("Bearer {token}"))).is_err());
}
