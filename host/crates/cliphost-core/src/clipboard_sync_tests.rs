use super::*;
use crate::clipboard_sync::{self, SyncAckStatus, SyncEvent, SyncKind};

fn fixture() -> (tempfile::TempDir, HostState) {
    crate::clipboard::force_headless(true);
    let directory = tempfile::tempdir().unwrap();
    let host = HostState::open(
        AppPaths::from_data_root(directory.path().to_path_buf()),
        28965,
    )
    .unwrap();
    host.sync_set_enabled(true).unwrap();
    (directory, host)
}
fn text_event(id: &str, seq: u64, text: &str) -> SyncEvent {
    SyncEvent::text(id.into(), "android-test".into(), seq, text.into())
}
fn image_bytes(format: image::ImageFormat) -> Vec<u8> {
    let image =
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(3, 2, image::Rgb([10, 20, 30])));
    let mut bytes = std::io::Cursor::new(Vec::new());
    image.write_to(&mut bytes, format).unwrap();
    bytes.into_inner()
}
fn image_event(id: &str, seq: u64, bytes: &[u8]) -> SyncEvent {
    SyncEvent {
        message_type: "clipboard".into(),
        version: 1,
        event_id: id.into(),
        origin_device_id: "android-test".into(),
        origin_seq: seq,
        kind: SyncKind::Image,
        sha256: crate::model::sha256_hex(bytes),
        text: None,
        image: Some(clipboard_sync::validate_image(bytes).unwrap()),
    }
}

#[test]
fn clipboard_sync_default_windows_without_peers_does_not_enable_raw_capture() {
    crate::clipboard::force_headless(true);
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp/raw-capture-tests");
    std::fs::create_dir_all(&root).unwrap();
    let directory = tempfile::tempdir_in(root).unwrap();
    let paths = AppPaths::from_data_root(directory.path().into());
    let runtime = clipboard_sync::SyncRuntime::open(
        "raw-capture-default".into(),
        paths.configs.join("ClipboardSync/state.json"),
        paths.image_library.join("Sync"),
    );
    assert!(runtime.devices.enabled());
    assert!(!runtime.raw_enabled.load(Ordering::Relaxed));
    drop(runtime);
    let host = HostState::open(paths, 28965).unwrap();
    host.sync_set_enabled(true).unwrap();
    assert!(
        host.clipboard_sync.raw_enabled.load(Ordering::Relaxed),
        "Android sync keeps its own capture grant"
    );
    host.sync_set_enabled(false).unwrap();
    assert!(!host.clipboard_sync.raw_enabled.load(Ordering::Relaxed));
    #[cfg(feature = "private-sync")]
    host.relay_refresh_capture();
    assert!(
        !host.clipboard_sync.raw_enabled.load(Ordering::Relaxed),
        "an inactive Relay refresh must not activate the unpaired Windows default"
    );
}

#[test]
fn clipboard_sync_preserves_raw_text_and_new_equal_content_copy_identity() {
    let (_directory, host) = fixture();
    let mut subscriber = host.sync_subscribe();
    let text = "  一行\r\n😀\t尾行  \n";
    let snapshot = crate::clipboard::ClipboardSnapshot {
        text: Some(text.into()),
        ..Default::default()
    };
    let hub = host.inner.lock().capture.clone();
    hub.notify_raw_snapshot(100, &snapshot);
    let first = subscriber.try_recv().unwrap();
    assert_eq!(first.text.as_deref(), Some(text));
    assert_eq!(first.sha256, crate::model::sha256_hex(text.as_bytes()));
    hub.notify_raw_snapshot(100, &snapshot);
    assert!(subscriber.try_recv().is_err());
    hub.notify_raw_snapshot(101, &snapshot);
    let second = subscriber.try_recv().unwrap();
    assert_ne!(first.event_id, second.event_id);
    assert_eq!(second.origin_seq, first.origin_seq + 1);
    let tagged = crate::clipboard::ClipboardSnapshot {
        text: Some(text.into()),
        sync_origin: Some(first.origin_tag()),
        ..Default::default()
    };
    hub.notify_raw_snapshot(102, &tagged);
    assert!(subscriber.try_recv().is_err(), "remote event must not echo");
    hub.notify_raw_snapshot(103, &snapshot);
    assert_eq!(
        subscriber.try_recv().unwrap().origin_seq,
        second.origin_seq + 1,
        "a real copy immediately after apply remains eligible"
    );
}

#[test]
fn clipboard_sync_receive_is_idempotent_and_does_not_advance_paste_usage() {
    let (_directory, host) = fixture();
    let writes = Arc::new(AtomicU64::new(0));
    let observed = writes.clone();
    host.set_clipboard_ui_runner(move |job| {
        observed.fetch_add(1, Ordering::Relaxed);
        job()
    });
    let text = "\t 收到的原文 \r\n";
    let event = text_event("receive-1", 1, text);
    assert!(!host.sync_receive_begin(event.clone()).unwrap().needs_media);
    assert_eq!(
        host.sync_receive_accept(&event.event_id, None)
            .unwrap()
            .status,
        SyncAckStatus::Accepted
    );
    assert_eq!(
        host.sync_receive_apply(&event.event_id).unwrap().status,
        SyncAckStatus::Applied
    );
    assert_eq!(
        host.sync_receive_begin(event.clone())
            .unwrap()
            .duplicate_ack
            .unwrap()
            .status,
        SyncAckStatus::Applied
    );
    assert_eq!(
        host.sync_receive_accept(&event.event_id, None)
            .unwrap()
            .status,
        SyncAckStatus::Applied
    );
    assert_eq!(
        host.sync_receive_apply(&event.event_id).unwrap().status,
        SyncAckStatus::Applied
    );
    let records = host.store().lock().list_clips().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].body_text.as_deref(), Some(text));
    assert_eq!(records[0].usage_count, 0);
    assert_eq!(records[0].last_access, None);
    assert_eq!(writes.load(Ordering::Relaxed), 1);
    let replacement = text_event("receive-1", 1, "different payload");
    assert!(host.sync_receive_begin(replacement).is_err());
    let runtime = clipboard_sync::SyncRuntime::open(
        host.sync_device_id(),
        host.clipboard_sync.state_path.clone(),
        host.clipboard_sync.media_root.clone(),
    );
    assert_eq!(
        runtime
            .begin(event, None)
            .unwrap()
            .duplicate_ack
            .unwrap()
            .status,
        SyncAckStatus::Applied,
        "receipts survive process restart"
    );
}

#[test]
fn clipboard_sync_delayed_image_and_out_of_order_events_keep_newer_clipboard() {
    let (_directory, host) = fixture();
    let writes = Arc::new(AtomicU64::new(0));
    let observed = writes.clone();
    host.set_clipboard_ui_runner(move |job| {
        observed.fetch_add(1, Ordering::Relaxed);
        job()
    });
    let bytes = image_bytes(image::ImageFormat::Png);
    let delayed = image_event("delayed-image", 1, &bytes);
    assert!(
        host.sync_receive_begin(delayed.clone())
            .unwrap()
            .needs_media
    );
    host.inner.lock().capture.notify_raw_snapshot(
        12,
        &crate::clipboard::ClipboardSnapshot {
            text: Some("new local copy".into()),
            ..Default::default()
        },
    );
    assert_eq!(
        host.sync_receive_accept(&delayed.event_id, Some(&bytes))
            .unwrap()
            .status,
        SyncAckStatus::Accepted
    );
    assert_eq!(
        host.sync_receive_apply(&delayed.event_id).unwrap().status,
        SyncAckStatus::Superseded
    );
    assert_eq!(writes.load(Ordering::Relaxed), 0);
    assert_eq!(
        std::fs::read(host.sync_read_media(&delayed.sha256).unwrap()).unwrap(),
        bytes
    );
    assert_eq!(
        host.store().lock().list_clips().unwrap().len(),
        1,
        "a superseded current write is still useful history"
    );
    let older = text_event("older", 2, "older remote");
    let newer = text_event("newer", 3, "newer remote");
    host.sync_receive_begin(older.clone()).unwrap();
    host.sync_receive_begin(newer.clone()).unwrap();
    host.sync_receive_accept(&older.event_id, None).unwrap();
    assert_eq!(
        host.sync_receive_apply(&older.event_id).unwrap().status,
        SyncAckStatus::Superseded
    );
    host.sync_receive_accept(&newer.event_id, None).unwrap();
    assert_eq!(
        host.sync_receive_apply(&newer.event_id).unwrap().status,
        SyncAckStatus::Applied
    );
    assert_eq!(writes.load(Ordering::Relaxed), 1);
    assert_eq!(
        host.sync_receive_begin(text_event("replay-old-seq", 2, "replay"))
            .unwrap()
            .duplicate_ack
            .unwrap()
            .status,
        SyncAckStatus::Superseded
    );
}

#[test]
fn clipboard_sync_stop_and_native_dispatch_recheck_cancel_queued_write() {
    let (_directory, host) = fixture();
    let event = text_event("queued", 1, "must not write");
    host.sync_receive_begin(event.clone()).unwrap();
    host.sync_receive_accept(&event.event_id, None).unwrap();
    let other = host.clone();
    host.set_clipboard_ui_runner(move |job| {
        other.sync_set_enabled(false).unwrap();
        job()
    });
    assert_eq!(
        host.sync_receive_apply(&event.event_id).unwrap().status,
        SyncAckStatus::Superseded
    );
    host.sync_set_enabled(true).unwrap();
    assert_eq!(
        host.sync_receive_begin(event)
            .unwrap()
            .duplicate_ack
            .unwrap()
            .status,
        SyncAckStatus::Superseded
    );
}

#[test]
fn clipboard_sync_validates_owned_static_png_jpeg_before_native_write() {
    let (_directory, host) = fixture();
    for (index, format) in [image::ImageFormat::Png, image::ImageFormat::Jpeg]
        .into_iter()
        .enumerate()
    {
        let bytes = image_bytes(format);
        let event = image_event(&format!("image-{index}"), index as u64 + 1, &bytes);
        host.sync_receive_begin(event.clone()).unwrap();
        let mut corrupted = bytes.clone();
        corrupted[0] ^= 1;
        assert!(host
            .sync_receive_accept(&event.event_id, Some(&corrupted))
            .is_err());
        assert_eq!(host.store().lock().list_clips().unwrap().len(), index);
        host.sync_receive_accept(&event.event_id, Some(&bytes))
            .unwrap();
        host.sync_receive_apply(&event.event_id).unwrap();
        let path = host.sync_read_media(&event.sha256).unwrap();
        assert!(path.starts_with(host.clipboard_sync.media_root.clone()));
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
    assert!(clipboard_sync::validate_image(b"GIF89a unsupported").is_err());
    let animated = [
        b"\x89PNG\r\n\x1a\n".as_slice(),
        &[0, 0, 0, 0],
        b"acTL",
        &[0, 0, 0, 0],
    ]
    .concat();
    assert!(clipboard_sync::validate_image(&animated)
        .unwrap_err()
        .contains("Animated"));
    assert!(host.sync_read_media("../outside").is_err());
}

#[test]
fn clipboard_sync_write_failure_is_not_reported_as_applied_and_restart_does_not_replay_accepted() {
    let (_directory, host) = fixture();
    let event = text_event("write-failure", 1, "received but not applied");
    host.sync_receive_begin(event.clone()).unwrap();
    host.sync_receive_accept(&event.event_id, None).unwrap();
    let restarted = clipboard_sync::SyncRuntime::open(
        host.sync_device_id(),
        host.clipboard_sync.state_path.clone(),
        host.clipboard_sync.media_root.clone(),
    );
    assert_eq!(
        restarted
            .begin(event.clone(), None)
            .unwrap()
            .duplicate_ack
            .unwrap()
            .status,
        SyncAckStatus::Superseded
    );
    host.set_clipboard_ui_runner(|_| Err("Native clipboard is locked.".into()));
    let ack = host.sync_receive_apply(&event.event_id).unwrap();
    assert_eq!(ack.status, SyncAckStatus::Failed);
    assert_eq!(ack.error.as_deref(), Some("Native clipboard is locked."));
    assert_eq!(
        host.sync_receive_begin(event)
            .unwrap()
            .duplicate_ack
            .unwrap()
            .status,
        SyncAckStatus::Failed
    );
    assert_eq!(host.store().lock().list_clips().unwrap().len(), 1);
}

#[test]
fn clipboard_sync_pair_client_has_no_listener_url_and_revokes_old_tokens() {
    let (directory, host) = fixture();
    let code = host.sync_invitation().unwrap()["pairCode"]
        .as_str()
        .unwrap()
        .to_owned();
    let paired = host
        .sync_pair_client(&json!({"deviceId":"android-test","name":"Phone","pairCode":code}))
        .unwrap();
    let token = paired["token"].as_str().unwrap();
    let authorization = format!("Bearer {token}");
    assert_eq!(
        host.sync_authenticate(Some("android-test"), Some(&authorization))
            .unwrap(),
        "android-test"
    );
    assert!(
        !std::fs::read_to_string(&host.clipboard_sync.state_path)
            .unwrap()
            .contains(token),
        "persist only token hash"
    );
    let code = host.sync_invitation().unwrap()["pairCode"]
        .as_str()
        .unwrap()
        .to_owned();
    host.sync_pair_client(&json!({"deviceId":"android-test","name":"Phone","pairCode":code}))
        .unwrap();
    assert!(host
        .sync_authenticate(Some("android-test"), Some(&authorization))
        .is_err());
    assert_eq!(host.sync_status()["clients"].as_array().unwrap().len(), 1);
    host.sync_forget_client("android-test").unwrap();
    assert_eq!(host.sync_status()["clients"].as_array().unwrap().len(), 0);
    assert!(directory.path().join("Configs").is_dir() || host.clipboard_sync.state_path.exists());
}

#[test]
fn clipboard_sync_invitation_urls_are_directly_importable_without_changing_manual_lan() {
    let (_directory, host) = fixture();
    let manual = host.lan_status()["localUrls"].clone();
    let invitation = host.sync_invitation().unwrap();
    for url in invitation["httpsUrls"].as_array().unwrap() {
        let uri = url::Url::parse(url.as_str().unwrap()).unwrap();
        assert_eq!(uri.path(), "/cliplink/sync/v1");
    }
    assert!(invitation["httpsUrl"]
        .as_str()
        .unwrap()
        .ends_with("/cliplink/sync/v1"));
    assert_eq!(host.lan_status()["localUrls"], manual);
}

#[test]
fn clipboard_sync_offline_archive_releases_receive_capacity_and_never_dispatches_a_write() {
    let (_directory, host) = fixture();
    host.set_clipboard_ui_runner(|_| panic!("offline events must not write current clipboard"));
    for seq in 1..=40 {
        let event = text_event(
            &format!("replay-{seq}"),
            seq,
            &format!("offline copy {seq}"),
        );
        host.sync_receive_begin(event.clone()).unwrap();
        host.sync_receive_accept(&event.event_id, None).unwrap();
        assert_eq!(
            host.sync_receive_supersede(&event.event_id).unwrap().status,
            SyncAckStatus::Superseded
        );
        assert_eq!(
            host.sync_receive_begin(event)
                .unwrap()
                .duplicate_ack
                .unwrap()
                .status,
            SyncAckStatus::Superseded
        );
    }
    assert!(host.clipboard_sync.state.lock().pending.is_empty());
    assert_eq!(host.store().lock().list_clips().unwrap().len(), 40);
    let fresh = text_event("fresh-after-replays", 41, "current copy");
    host.sync_receive_begin(fresh.clone()).unwrap();
    host.sync_receive_accept(&fresh.event_id, None).unwrap();
    host.set_clipboard_ui_runner(|job| job());
    assert_eq!(
        host.sync_receive_apply(&fresh.event_id).unwrap().status,
        SyncAckStatus::Applied
    );
}

fn pair_authorization(host: &HostState) -> String {
    let invitation = host.sync_invitation().unwrap();
    let paired = host
        .sync_pair_client(
            &json!({"deviceId":"android-test","name":"Phone","pairCode":invitation["pairCode"]}),
        )
        .unwrap();
    format!("Bearer {}", paired["token"].as_str().unwrap())
}

#[test]
fn clipboard_sync_authenticated_forget_before_begin_cannot_reserve_an_old_request() {
    let (_directory, host) = fixture();
    let authorization = pair_authorization(&host);
    host.sync_authenticate(Some("android-test"), Some(&authorization))
        .unwrap();
    // A request can have passed HTTP authentication before the user forgets it.
    host.sync_forget_client("android-test").unwrap();
    host.set_clipboard_ui_runner(|_| panic!("forgotten client must never write"));
    assert!(host
        .sync_receive_begin_authenticated(
            text_event("forgotten-request", 1, "must not reserve"),
            Some("android-test"),
            Some(&authorization)
        )
        .is_err());
    assert!(host.clipboard_sync.state.lock().pending.is_empty());
    assert!(host.store().lock().list_clips().unwrap().is_empty());
}

#[test]
fn clipboard_sync_authenticated_repair_before_apply_invalidates_old_token_ticket() {
    let (_directory, host) = fixture();
    let authorization = pair_authorization(&host);
    let event = text_event("old-token-ticket", 1, "accepted using old token");
    host.sync_receive_begin_authenticated(
        event.clone(),
        Some("android-test"),
        Some(&authorization),
    )
    .unwrap();
    host.sync_receive_accept(&event.event_id, None).unwrap();
    let generation_before = host.clipboard_sync.state.lock().generation;
    let repaired = pair_authorization(&host);
    assert_eq!(
        host.clipboard_sync.state.lock().generation,
        generation_before,
        "re-pairing does not cancel other clients by a global epoch"
    );
    host.set_clipboard_ui_runner(|_| panic!("re-paired old token must never write"));
    assert_eq!(
        host.sync_receive_apply(&event.event_id).unwrap().status,
        SyncAckStatus::Superseded
    );
    assert!(host
        .sync_receive_begin_authenticated(
            text_event("late-old-token", 2, "old authenticated request"),
            Some("android-test"),
            Some(&authorization)
        )
        .is_err());
    let fresh = text_event("new-token-ticket", 2, "new authorized request");
    host.sync_receive_begin_authenticated(fresh.clone(), Some("android-test"), Some(&repaired))
        .unwrap();
    host.sync_receive_accept(&fresh.event_id, None).unwrap();
    host.set_clipboard_ui_runner(|job| job());
    assert_eq!(
        host.sync_receive_apply(&fresh.event_id).unwrap().status,
        SyncAckStatus::Applied
    );
    assert_eq!(
        host.store().lock().list_clips().unwrap().len(),
        2,
        "accepted old history remains available"
    );
}
