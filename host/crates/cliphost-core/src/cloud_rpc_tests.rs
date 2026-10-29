use super::*;
use crate::{AppPaths, HostState};
use parking_lot::Mutex;
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

#[test]
fn cloud_product_gate_matches_csharp_and_frontend() {
    assert!(!PUBLICLY_AVAILABLE);
    assert!(include_str!("../../../../frontend/src/core/cloudSyncFeature.ts").contains("CLOUD_SYNC_UI_VISIBLE = false"));
    assert!(include_str!("../../../../src/IntelliTools.ClipHost/Hosting/CloudSyncFeatureFlags.cs").contains("IsPubliclyAvailable = false"));
}

#[test]
fn cloud_disabled_operations_preserve_favorites_and_existing_folder_files() {
    crate::clipboard::force_headless(true);
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("BetterClipTauri");
    let host = HostState::open(AppPaths::from_data_root(root.clone()), 28965).unwrap();
    host.favor_add_text_items(&json!({"text":"keep local favorite"})).unwrap();
    let before = host.favor_tree().unwrap();
    assert!(!root.join("cloud").exists());
    assert_eq!(host.cloud_status()["cloudSyncHostEnabled"], false);
    assert_eq!(host.cloud_run().unwrap_err(), DISABLED_REASON);
    assert!(!root.join("cloud").exists(), "status is not a folder transport operation");
    std::fs::create_dir_all(root.join("cloud/slots")).unwrap();
    std::fs::write(root.join("cloud/manifest.json"), "old fixture manifest").unwrap();
    std::fs::write(root.join("cloud/slots/cea.favor.json"), "old fixture data").unwrap();
    for result in [host.cloud_run(), host.cloud_cleanup(), host.cloud_purge()] {
        assert_eq!(result.unwrap_err(), DISABLED_REASON);
    }
    for command in ["sync list", "sync probe", "sync check --all", "sync pull cea.favor --force",
        "sync push cea.favor --force", "sync cleanup-favor", "sync purge-favor-cloud", "favor sync run"] {
        let result = host.execute_verb(command).unwrap();
        assert_eq!(result["cloud"]["success"], false, "{command}: {result}");
        assert_eq!(result["cloud"]["message"], DISABLED_REASON);
        assert_eq!(host.take_toast_push().unwrap()["message"], DISABLED_REASON);
    }
    assert_eq!(host.favor_tree().unwrap(), before);
    assert_eq!(std::fs::read_to_string(root.join("cloud/manifest.json")).unwrap(), "old fixture manifest");
    assert_eq!(std::fs::read_to_string(root.join("cloud/slots/cea.favor.json")).unwrap(), "old fixture data");
    assert!(!root.join("cloud/local.json").exists());
    assert_eq!(host.cloud_progress()["active"], false);
    assert!(host.cloud_logs(&json!({}))["entries"].as_array().unwrap().is_empty());
}

struct Peer {
    result: Mutex<Result<Value, String>>,
    calls: Mutex<Vec<String>>,
    wait: AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[tokio::test]
async fn cloud_readonly_probe_reports_real_rpc_errors_manifest_and_timeout() {
    crate::clipboard::force_headless(true);
    let dir = tempfile::tempdir().unwrap();
    let host = HostState::open(AppPaths::from_data_root(dir.path().join("BetterClipTauri")), 28965).unwrap();
    assert_eq!(host.health_cloud_sync_probe(true)["rpcConnected"], false);
    let registry = host.quicker_rpc();
    let control = Arc::new(Peer { result:Mutex::new(Ok(json!("{\"emoji\":\"😀\"}"))), calls:Mutex::new(Vec::new()),
        wait:AtomicBool::new(false), entered:tokio::sync::Notify::new(), release:tokio::sync::Notify::new() });
    let (server, mut client) = tokio::io::duplex(65536);
    let stop = Arc::new(crate::pipe_rpc::StopSignal::default());
    let serving = tokio::spawn(crate::quicker_rpc::serve(server, registry.clone(), "cloud-fixture",
        stop.clone(), Arc::new(|_| None)));
    let peer = control.clone();
    let responding = tokio::spawn(async move {
        while let Ok(Some(request)) = crate::quicker_rpc::read_message(&mut client).await {
            if request.type_name != "request" { continue; }
            let mut reply = request.clone(); reply.type_name = "response".into(); reply.method = None;
            reply.payload = Some(match request.method.as_deref().unwrap() {
                "quicker.ping" => json!("pong"),
                "quicker.getProtocolVersion" => json!(11),
                "quicker.readCloudState" => {
                    assert_eq!(request.payload.as_ref().unwrap()["CloudKey"], MANIFEST_KEY);
                    peer.calls.lock().push("read".into());
                    if peer.wait.load(Ordering::SeqCst) { peer.entered.notify_one(); peer.release.notified().await; }
                    match peer.result.lock().clone() {
                        Ok(value) => value,
                        Err(error) => { reply = crate::pipe_rpc::PipeRpcMessage::error(request.id, "fixture", &error); Value::Null }
                    }
                },
                method => panic!("diagnostic must not write cloud data: {method}"),
            });
            if client.write_all(&crate::pipe_rpc::encode_frame(&reply).unwrap()).await.is_err() { break; }
        }
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while registry.select().is_none() { tokio::time::sleep(Duration::from_millis(5)).await; }
    }).await.unwrap();
    assert_eq!(host.cloud_status()["rpcConnected"], true);
    assert_eq!(host.cloud_status()["cloudSyncHostEnabled"], false);
    assert_eq!(host.cloud_run().unwrap_err(), DISABLED_REASON);
    assert!(control.calls.lock().is_empty(), "online plugin must not bypass the product gate");
    let testing = host.clone(); let peer = control.clone();
    tokio::task::spawn_blocking(move || {
        let hidden = testing.health_cloud_sync_probe(false);
        assert_eq!(hidden["ok"], true); assert!(hidden["manifest"].is_null());
        assert_eq!(hidden["manifestFound"], true); assert_eq!(hidden["manifestLength"], 14);
        let shown = testing.health_cloud_sync_probe(true);
        assert_eq!(shown["manifest"], "{\"emoji\":\"😀\"}");
        assert_eq!(shown["cloudSyncSupported"], true); assert_eq!(shown["cloudSyncHostEnabled"], false);
        *peer.result.lock() = Ok(Value::Null);
        let empty = testing.health_cloud_sync_probe(false);
        assert_eq!(empty["ok"], true); assert_eq!(empty["manifestFound"], false);
        *peer.result.lock() = Err("cloud unavailable".into());
        let failed = testing.health_cloud_sync_probe(false);
        assert_eq!(failed["ok"], false); assert!(failed["cloudError"].as_str().unwrap().contains("cloud unavailable"));
        *peer.result.lock() = Ok(json!({"unexpected":"object"}));
        assert_eq!(testing.health_cloud_sync_probe(false)["ok"], false);
    }).await.unwrap();
    control.wait.store(true, Ordering::SeqCst);
    let testing = registry.clone();
    let pending = tokio::task::spawn_blocking(move || probe_with_timeout(&testing, false, Duration::from_secs(2), Duration::from_secs(2)));
    tokio::time::timeout(Duration::from_secs(2), control.entered.notified()).await.unwrap();
    let testing = host.clone();
    tokio::time::timeout(Duration::from_secs(1), tokio::task::spawn_blocking(move || {
        testing.apply_window_action("setPinned", true);
        assert_eq!(testing.cloud_status()["cloudSyncHostEnabled"], false);
    })).await.unwrap().unwrap();
    let timed_out = pending.await.unwrap();
    assert_eq!(timed_out["ok"], false); assert!(timed_out["cloudError"].as_str().unwrap().contains("超时"));
    // Finish the server before closing the client: the timeout queues a cancel frame,
    // so abruptly aborting the client can race the writer and cause a broken pipe.
    stop.request();
    tokio::time::timeout(Duration::from_secs(2), serving).await.unwrap().unwrap().unwrap();
    assert!(registry.select().is_none(), "shutdown must unregister the RPC session");
    assert_eq!(host.cloud_status()["rpcConnected"], false);
    responding.abort(); let _ = responding.await;
    assert!(!dir.path().join("BetterClipTauri/cloud").exists());
}
