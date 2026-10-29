use super::*;

fn fixture() -> (tempfile::TempDir, HostState) {
    crate::clipboard::force_headless(true);
    let root =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp/lan-received-history-tests");
    std::fs::create_dir_all(&root).unwrap();
    let directory = tempfile::tempdir_in(root).unwrap();
    let host = HostState::open(
        AppPaths::from_data_root(directory.path().join("host")),
        28965,
    )
    .unwrap();
    (directory, host)
}

fn prepare(host: &HostState, id: &str, files: &[(&str, &[u8])]) {
    let attachments: Vec<_> = files.iter().enumerate().map(|(index, (name, bytes))| json!({
        "id":format!("attachment{index}"), "name":name, "mimeType":"application/octet-stream",
        "sizeBytes":bytes.len(), "sha256":crate::lan::sha256_hex(bytes),
    })).collect();
    host.lan_prepare_inbound(
        "peer",
        &json!({
            "id":id, "senderDeviceId":"peer", "senderAlias":"Laptop", "targetKind":"direct",
            "text":"", "createdAtUtc":Utc::now(), "attachments":attachments,
        }),
    )
    .unwrap();
    for (index, (_, bytes)) in files.iter().enumerate() {
        host.lan_upload_inbound(
            "peer",
            id,
            &format!("attachment{index}"),
            bytes,
            bytes.len() as i64,
        )
        .unwrap();
    }
}

#[test]
fn lan_received_history_is_opt_in_and_persisted() {
    let (_directory, host) = fixture();
    assert_eq!(host.lan_status()["receivedFilesToHistory"], false);
    prepare(&host, "disabled", &[("one.txt", b"hello")]);
    let receipt = host.lan_commit_inbound("peer", "disabled").unwrap();
    assert_eq!(receipt["delivered"], true);
    assert_eq!(receipt["historyImportRequested"], false);
    assert!(host.store().lock().list_clips().unwrap().is_empty());
    host.lan_config_update(&json!({"receivedFilesToHistory":true}))
        .unwrap();
    let settings = host
        .lan
        .lock()
        .peers_path
        .parent()
        .unwrap()
        .join("settings.json");
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(settings).unwrap()).unwrap();
    assert_eq!(saved["receivedFilesToHistory"], true);
    let mut reloaded = host.lan.lock().clone();
    reloaded.received_files_to_history = false;
    reloaded.load_settings();
    assert!(reloaded.received_files_to_history);
    // Enabling later does not backfill old deliveries on a network retry.
    host.lan_commit_inbound("peer", "disabled").unwrap();
    assert!(host.store().lock().list_clips().unwrap().is_empty());
    let previous = host.lan_status();
    assert!(host
        .lan_config_update(
            &json!({"deviceAlias":"must not change", "receivedFilesToHistory":"yes"})
        )
        .is_err());
    assert_eq!(host.lan_status()["deviceAlias"], previous["deviceAlias"]);
}

#[test]
fn lan_received_history_groups_local_files_and_does_not_resurrect_deleted_items() {
    let (directory, host) = fixture();
    host.lan_config_update(&json!({"receivedFilesToHistory":true}))
        .unwrap();
    prepare(
        &host,
        "grouped",
        &[("one.txt", b"hello"), ("two.bin", b"world")],
    );
    let receipt = host.lan_commit_inbound("peer", "grouped").unwrap();
    assert_eq!(receipt["historyImportCompleted"], true);
    let id = receipt["historyItemId"].as_str().unwrap();
    let clip = host.store().lock().get_clip(id).unwrap();
    assert_eq!(clip.kind, ClipKind::FileClipItem);
    assert_eq!(clip.file_paths.len(), 2);
    assert_eq!(std::fs::read(&clip.file_paths[0]).unwrap(), b"hello");
    assert_eq!(std::fs::read(&clip.file_paths[1]).unwrap(), b"world");
    assert_eq!(clip.description, "局域网接收 · Laptop");
    assert!(clip
        .file_paths
        .iter()
        .all(|path| Path::new(path).is_absolute()));
    host.lan_commit_inbound("peer", "grouped").unwrap();
    assert_eq!(host.store().lock().list_clips().unwrap().len(), 1);
    host.delete_item(&json!({"id":id})).unwrap();
    let retried = host.lan_commit_inbound("peer", "grouped").unwrap();
    assert_eq!(retried["historyImportCompleted"], true);
    assert_eq!(retried["historyItemId"], id);
    assert!(host.store().lock().list_clips().unwrap().is_empty());
    assert!(host
        .lan_commit_inbound("different-peer", "grouped")
        .is_err());
    drop(host);
    let reopened = HostState::open(
        AppPaths::from_data_root(directory.path().join("host")),
        28965,
    )
    .unwrap();
    assert_eq!(
        reopened.lan_commit_inbound("peer", "grouped").unwrap()["historyImportCompleted"],
        true
    );
    assert!(reopened.store().lock().list_clips().unwrap().is_empty());
}

#[test]
fn lan_received_history_import_is_atomic_and_failed_import_can_retry() {
    let (_directory, host) = fixture();
    host.lan_config_update(&json!({"receivedFilesToHistory":true}))
        .unwrap();
    prepare(&host, "retry", &[("one.txt", b"hello")]);
    let db_path = host.inner.lock().paths.db.clone();
    let db = rusqlite::Connection::open(db_path).unwrap();
    // Fail the receipt write inside the same transaction as history insertion.
    db.execute_batch("CREATE TRIGGER fail_history_receipt BEFORE UPDATE ON lan_message WHEN NEW.id='retry' AND json_extract(NEW.payload,'$.historyImportCompleted')=1 BEGIN SELECT RAISE(ABORT,'receipt fixture failure'); END;").unwrap();
    let failed = host.lan_commit_inbound("peer", "retry").unwrap();
    assert_eq!(failed["delivered"], true);
    assert_eq!(failed["historyImportCompleted"], false);
    assert!(failed["historyImportError"]
        .as_str()
        .unwrap()
        .contains("receipt fixture failure"));
    assert!(
        host.store().lock().list_clips().unwrap().is_empty(),
        "receipt failure must roll back history insertion"
    );
    db.execute_batch("DROP TRIGGER fail_history_receipt;")
        .unwrap();
    let retried = host.lan_commit_inbound("peer", "retry").unwrap();
    assert_eq!(retried["historyImportCompleted"], true);
    assert!(retried.get("historyImportError").is_none());
    assert_eq!(host.store().lock().list_clips().unwrap().len(), 1);
}

#[test]
fn lan_received_history_rejects_nonlocal_changed_and_incomplete_files() {
    let (_directory, host) = fixture();
    let inbox = host.inner.lock().paths.inbox.clone();
    std::fs::create_dir_all(&inbox).unwrap();
    let file = inbox.join("file.txt");
    std::fs::write(&file, b"hello").unwrap();
    let mut payload = json!({
        "id":"validation", "senderDeviceId":"peer", "senderAlias":"Laptop", "status":"received",
        "attachments":[{"localPath":file, "status":"received", "sizeBytes":5, "sha256":crate::lan::sha256_hex(b"hello")}],
    });
    assert!(received_file_clip(&payload, &inbox).is_ok());
    payload["attachments"][0]["status"] = json!("pending");
    assert!(received_file_clip(&payload, &inbox).is_err());
    payload["attachments"][0]["status"] = json!("received");
    std::fs::write(&file, b"other").unwrap();
    assert!(received_file_clip(&payload, &inbox)
        .unwrap_err()
        .contains("校验失败"));
    payload["attachments"][0]["localPath"] = json!("remote/file.txt");
    assert!(received_file_clip(&payload, &inbox)
        .unwrap_err()
        .contains("绝对路径"));
}
