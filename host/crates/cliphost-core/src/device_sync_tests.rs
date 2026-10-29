use super::*;
use crate::clipboard_sync::{SyncEvent, SyncKind};

#[test]
fn device_sync_metadata_rewrite_does_not_republish_but_intervening_copy_preserves_return_to_image() {
    fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut bytes = (data.len() as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(data);
        let mut crc = u32::MAX;
        for byte in &bytes[4..] {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb88320 & 0u32.wrapping_sub(crc & 1));
            }
        }
        bytes.extend_from_slice(&(!crc).to_be_bytes());
        bytes
    }
    let (_root, host, _auth) = fixture();
    let original = image();
    let mut rewritten = original[..33].to_vec();
    rewritten.extend(chunk(b"sRGB", &[0]));
    rewritten.extend(chunk(b"gAMA", &45455u32.to_be_bytes()));
    rewritten.extend(chunk(b"pHYs", &[0, 0, 14, 196, 0, 0, 14, 196, 1]));
    rewritten.extend_from_slice(&original[33..]);
    assert_ne!(original, rewritten);
    let image_snapshot = |png| crate::clipboard::ClipboardSnapshot {
        png: Some(png),
        ..Default::default()
    };
    let runtime = &host.clipboard_sync;
    runtime.observe(1, &image_snapshot(original)).unwrap();
    let (sequence, event_id) = {
        let state = runtime.devices.state.lock();
        (
            state.durable.sequence,
            state.outgoing["windows-peer"].event.event_id.clone(),
        )
    };
    runtime.observe(2, &image_snapshot(rewritten.clone())).unwrap();
    {
        let state = runtime.devices.state.lock();
        assert_eq!(state.durable.sequence, sequence);
        assert_eq!(state.outgoing["windows-peer"].event.event_id, event_id);
    }
    runtime
        .observe(
            3,
            &crate::clipboard::ClipboardSnapshot {
                text: Some("a genuinely different copy".into()),
                ..Default::default()
            },
        )
        .unwrap();
    runtime.observe(4, &image_snapshot(rewritten)).unwrap();
    let state = runtime.devices.state.lock();
    assert_eq!(state.durable.sequence, sequence + 2);
    assert_ne!(state.outgoing["windows-peer"].event.event_id, event_id);
}

fn temporary_root() -> tempfile::TempDir {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp/device-sync-tests");
    std::fs::create_dir_all(&root).unwrap();
    tempfile::tempdir_in(root).unwrap()
}
fn fixture() -> (tempfile::TempDir, HostState, String) {
    crate::clipboard::force_headless(true);
    let root = temporary_root();
    let host = HostState::open(AppPaths::from_data_root(root.path().into()), 28965).unwrap();
    let token = crate::lan::create_token();
    host.lan.lock().discovered.insert(
        "windows-peer".into(),
        crate::lan::LanDiscoveredPeer {
            device_id: "windows-peer".into(),
            alias: "Windows peer".into(),
            base_url: "https://127.0.0.1:29999".into(),
            certificate_fingerprint: "a".repeat(64),
            paired_certificate_fingerprint: Some("a".repeat(64)),
            last_seen_at_utc: Utc::now(),
            pairing_available: false,
            paired: true,
            online: true,
            token: Some(token.clone()),
            inbound_token_hash: Some(crate::model::sha256_hex(token.as_bytes())),
            device_kind: Default::default(),
            clipboard_transport: crate::lan::ClipboardTransport::Push,
        },
    );
    host.device_sync_pair_peer("windows-peer").unwrap();
    (root, host, format!("Bearer {token}"))
}
fn enable(host: &HostState, apply: bool, history: bool) {
    host.device_sync_config_update(
        &json!({"enabled":true,"deviceId":"windows-peer","sendEnabled":true,"receiveEnabled":true,
        "textEnabled":true,"imageEnabled":true,"applyClipboard":apply,"saveHistory":history}),
    )
    .unwrap();
}
fn text(id: &str, seq: u64, value: &str) -> SyncEvent {
    SyncEvent::text(id.into(), "windows-peer".into(), seq, value.into())
}
fn image() -> Vec<u8> {
    let image =
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(2, 2, image::Rgb([1, 2, 3])));
    let mut bytes = std::io::Cursor::new(Vec::new());
    image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
    bytes.into_inner()
}
fn image_event(id: &str, seq: u64, bytes: &[u8]) -> SyncEvent {
    SyncEvent {
        message_type: "clipboard".into(),
        version: 1,
        event_id: id.into(),
        origin_device_id: "windows-peer".into(),
        origin_seq: seq,
        kind: SyncKind::Image,
        sha256: crate::model::sha256_hex(bytes),
        text: None,
        image: Some(crate::clipboard_sync::validate_image(bytes).unwrap()),
    }
}

#[test]
fn device_sync_paired_defaults_send_and_receive_text_and_images() {
    let (_root, host, auth) = fixture();
    let status = host.device_sync_status();
    assert_eq!(status["enabled"], true);
    for key in [
        "sendEnabled",
        "receiveEnabled",
        "textEnabled",
        "imageEnabled",
        "applyClipboard",
        "saveHistory",
    ] {
        assert_eq!(status["peers"][0][key], true, "{key}");
    }
    assert!(host
        .device_sync_authenticate(Some("windows-peer"), Some(&auth))
        .is_ok());
    host.clipboard_sync
        .observe(
            1,
            &crate::clipboard::ClipboardSnapshot {
                text: Some("default outgoing".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(host
        .clipboard_sync
        .devices
        .state
        .lock()
        .outgoing
        .contains_key("windows-peer"));
    host.device_sync_receive_begin(
        "windows-peer",
        &auth,
        text("default-text", 1, "incoming"),
        false,
    )
    .unwrap();
    let bytes = image();
    host.device_sync_receive_begin(
        "windows-peer",
        &auth,
        image_event("default-image", 2, &bytes),
        false,
    )
    .unwrap();
    host.device_sync_receive_media("windows-peer", &auth, "default-image", Some(&bytes))
        .unwrap();
    assert_eq!(host.store().lock().list_clips().unwrap().len(), 2);
    assert!(host
        .device_sync_authenticate(Some("unknown"), Some(&auth))
        .is_err());
    assert!(host
        .device_sync_config_update(&json!({"deviceId":"unknown","receiveEnabled":true}))
        .is_err());
}
#[test]
fn device_sync_ready_default_starts_transport_only_after_trusted_pairing() {
    crate::clipboard::force_headless(true);
    let root = temporary_root();
    let paths = AppPaths::from_data_root(root.path().into());
    let host = HostState::open(paths.clone(), 28965).unwrap();
    assert_eq!(host.device_sync_status()["enabled"], true);
    assert!(!host.lan.lock().enabled);
    assert!(!host.clipboard_sync.raw_enabled.load(Ordering::Relaxed));
    host.lan_accept_inbound_pair(&json!({
        "deviceId":"new-peer", "alias":"New Windows", "baseUrl":"https://127.0.0.1:29999",
        "certificateFingerprint":"b".repeat(64), "tokenForReceiver":crate::lan::create_token(),
        "pairingCode":host.lan_refresh_code()["code"]
    }))
    .unwrap();
    assert!(host.lan.lock().enabled);
    assert!(host.clipboard_sync.raw_enabled.load(Ordering::Relaxed));
    assert_eq!(host.device_sync_status()["peers"][0]["imageEnabled"], true);
    // The persisted trust restores a missing default policy as well as an
    // explicitly stored one, without another setup step.
    std::fs::remove_file(&host.clipboard_sync.devices.path).unwrap();
    drop(host);
    let reopened = HostState::open(paths, 28965).unwrap();
    assert!(reopened.lan.lock().enabled);
    reopened
        .clipboard_sync
        .observe(
            1,
            &crate::clipboard::ClipboardSnapshot {
                text: Some("after restart".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(reopened
        .clipboard_sync
        .devices
        .state
        .lock()
        .outgoing
        .contains_key("new-peer"));
}
#[test]
fn device_sync_old_peer_restrictions_are_removed_but_master_pause_is_preserved() {
    let root = temporary_root();
    let path = root.path().join("devices.json");
    std::fs::write(
        &path,
        serde_json::to_vec(&json!({"enabled":false,"peers":{"old-peer":{
            "sendEnabled":false,"receiveEnabled":false,"textEnabled":true,"imageEnabled":false,
            "applyClipboard":false,"saveHistory":true
        }}}))
        .unwrap(),
    )
    .unwrap();
    let runtime = crate::device_sync::DeviceSyncRuntime::open(path);
    runtime.register_paired_peers(["old-peer", "new-peer"].into_iter());
    let state = runtime.state.lock();
    assert!(!state.durable.enabled);
    let old = &state.durable.peers["old-peer"];
    assert_eq!(*old, crate::device_sync::DeviceSyncPolicy::default());
    assert!(state.durable.text_enabled && state.durable.image_enabled);
    assert!(state.durable.peers["new-peer"].send_enabled);
}
#[test]
fn device_sync_master_pause_revokes_work_and_preserves_type_preferences() {
    let (_root, host, auth) = fixture();
    host.device_sync_config_update(&json!({"textEnabled":false,"imageEnabled":true}))
        .unwrap();
    let bytes = image();
    host.clipboard_sync
        .observe(
            1,
            &crate::clipboard::ClipboardSnapshot {
                png: Some(bytes.clone()),
                ..Default::default()
            },
        )
        .unwrap();
    host.device_sync_receive_begin(
        "windows-peer",
        &auth,
        image_event("before-pause", 1, &bytes),
        false,
    )
    .unwrap();
    let status = host
        .device_sync_config_update(&json!({"enabled":false}))
        .unwrap();
    assert_eq!(status["enabled"], false);
    assert_eq!(status["textEnabled"], false);
    assert!(host.clipboard_sync.devices.state.lock().pending.is_empty());
    assert!(host.clipboard_sync.devices.state.lock().outgoing.is_empty());
    assert!(host
        .device_sync_receive_media("windows-peer", &auth, "before-pause", Some(&bytes))
        .is_err());
    assert!(host
        .device_sync_receive_begin(
            "windows-peer",
            &auth,
            image_event("while-paused", 2, &bytes),
            false
        )
        .is_err());
    host.clipboard_sync
        .observe(
            2,
            &crate::clipboard::ClipboardSnapshot {
                png: Some(bytes),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(host.clipboard_sync.devices.state.lock().outgoing.is_empty());
    let reopened =
        crate::device_sync::DeviceSyncRuntime::open(host.clipboard_sync.devices.path.clone());
    assert!(!reopened.state.lock().durable.enabled);
    assert!(!reopened.state.lock().durable.text_enabled);
    host.device_sync_config_update(&json!({"enabled":true}))
        .unwrap();
    assert_eq!(host.device_sync_status()["textEnabled"], false);
}
#[test]
fn device_sync_global_types_control_both_directions_and_require_one_type() {
    let (_root, host, auth) = fixture();
    host.device_sync_config_update(&json!({"imageEnabled":false}))
        .unwrap();
    let bytes = image();
    host.clipboard_sync
        .observe(
            1,
            &crate::clipboard::ClipboardSnapshot {
                png: Some(bytes.clone()),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(host.clipboard_sync.devices.state.lock().outgoing.is_empty());
    assert!(host
        .device_sync_receive_begin(
            "windows-peer",
            &auth,
            image_event("blocked-image", 1, &bytes),
            false
        )
        .is_err());
    assert!(host
        .device_sync_config_update(&json!({"textEnabled":false}))
        .is_err());
    assert_eq!(host.device_sync_status()["textEnabled"], true);
    host.device_sync_receive_begin(
        "windows-peer",
        &auth,
        text("allowed-text", 1, "text allowed"),
        false,
    )
    .unwrap();
    host.device_sync_config_update(&json!({"textEnabled":false,"imageEnabled":true}))
        .unwrap();
    assert!(host
        .device_sync_receive_begin(
            "windows-peer",
            &auth,
            text("blocked-text", 2, "disabled"),
            false
        )
        .is_err());
    host.clipboard_sync
        .observe(
            2,
            &crate::clipboard::ClipboardSnapshot {
                png: Some(bytes.clone()),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(host
        .clipboard_sync
        .devices
        .state
        .lock()
        .outgoing
        .contains_key("windows-peer"));
    host.device_sync_receive_begin(
        "windows-peer",
        &auth,
        image_event("allowed-image", 2, &bytes),
        false,
    )
    .unwrap();
    host.device_sync_receive_media("windows-peer", &auth, "allowed-image", Some(&bytes))
        .unwrap();
    // Old per-device type switches never become hidden global restrictions.
    host.device_sync_config_update(&json!({"deviceId":"windows-peer","sendEnabled":false,
        "receiveEnabled":false,"textEnabled":false,"imageEnabled":false,
        "applyClipboard":false,"saveHistory":false}))
        .unwrap();
    let state = host.clipboard_sync.devices.state.lock();
    assert_eq!(
        state.durable.peers["windows-peer"],
        crate::device_sync::DeviceSyncPolicy::default()
    );
    assert!(!state.durable.text_enabled && state.durable.image_enabled);
}
#[test]
fn device_sync_legacy_action_fields_cannot_block_effects_and_retry_is_idempotent() {
    for (apply, history) in [(false, true), (true, false), (true, true)] {
        let (_root, host, auth) = fixture();
        enable(&host, apply, history);
        let writes = Arc::new(AtomicU64::new(0));
        let observed = writes.clone();
        host.set_clipboard_ui_runner(move |job| {
            observed.fetch_add(1, Ordering::Relaxed);
            job()
        });
        let event = text("one-event", 1, "  raw\r\n text  ");
        let ack = host
            .device_sync_receive_begin("windows-peer", &auth, event.clone(), false)
            .unwrap();
        assert_eq!(writes.load(Ordering::Relaxed), 1);
        assert_eq!(host.store().lock().list_clips().unwrap().len(), 1);
        assert_eq!(ack["history"], "saved");
        assert_eq!(
            host.device_sync_receive_begin("windows-peer", &auth, event, false)
                .unwrap(),
            ack
        );
        assert_eq!(writes.load(Ordering::Relaxed), 1);
        assert!(host
            .device_sync_receive_begin(
                "windows-peer",
                &auth,
                text("one-event", 1, "changed"),
                false
            )
            .is_err());
        let state =
            crate::device_sync::DeviceSyncRuntime::open(host.clipboard_sync.devices.path.clone());
        assert_eq!(state.state.lock().durable.receipts.len(), 1);
    }
}
#[test]
fn device_sync_third_party_same_content_converges_but_a_b_a_is_a_new_copy() {
    let (_root, host, auth) = fixture();
    enable(&host, true, true);
    let runtime = &host.clipboard_sync;
    let observe = |sequence, value: &str| {
        runtime
            .observe(
                sequence,
                &crate::clipboard::ClipboardSnapshot {
                    text: Some(value.into()),
                    ..Default::default()
                },
            )
            .unwrap()
    };
    observe(1, "A");
    let first = runtime.devices.state.lock().durable.sequence;
    observe(2, "A");
    assert_eq!(runtime.devices.state.lock().durable.sequence, first);
    observe(3, "B");
    observe(4, "A");
    assert_eq!(runtime.devices.state.lock().durable.sequence, first + 2);
    // Sequence-less headless fixtures model the same currently observed value.
    runtime.devices.state.lock().native_sequence = None;
    let writes = Arc::new(AtomicU64::new(0));
    let count = writes.clone();
    host.set_clipboard_ui_runner(move |job| {
        count.fetch_add(1, Ordering::Relaxed);
        job()
    });
    let ack = host
        .device_sync_receive_begin(
            "windows-peer",
            &auth,
            text("alternate-route", 1, "A"),
            false,
        )
        .unwrap();
    assert_eq!(ack["clipboard"], "duplicate");
    assert_eq!(writes.load(Ordering::Relaxed), 0);
    let tagged = crate::clipboard::ClipboardSnapshot {
        sync_origin: Some("remote".into()),
        ..Default::default()
    };
    observe(5, "B");
    runtime.observe(6, &tagged).unwrap();
    assert!(runtime.devices.state.lock().outgoing.is_empty());
    let before = runtime.devices.state.lock().durable.sequence;
    observe(7, "B");
    assert_eq!(
        runtime.devices.state.lock().durable.sequence,
        before + 1,
        "unknown mobile origin must clear the old content baseline"
    );
}
#[test]
fn device_sync_slow_image_keeps_history_without_overwriting_a_newer_file_copy() {
    let (_root, host, auth) = fixture();
    enable(&host, true, true);
    let bytes = image();
    let event = image_event("slow-image", 1, &bytes);
    assert_eq!(
        host.device_sync_receive_begin("windows-peer", &auth, event, false)
            .unwrap()["status"],
        "pendingMedia"
    );
    host.clipboard_sync
        .observe(
            7,
            &crate::clipboard::ClipboardSnapshot {
                files: vec!["C:/synthetic.txt".into()],
                ..Default::default()
            },
        )
        .unwrap();
    let writes = Arc::new(AtomicU64::new(0));
    let count = writes.clone();
    host.set_clipboard_ui_runner(move |job| {
        count.fetch_add(1, Ordering::Relaxed);
        job()
    });
    let ack = host
        .device_sync_receive_media("windows-peer", &auth, "slow-image", Some(&bytes))
        .unwrap();
    assert_eq!(ack["clipboard"], "superseded");
    assert_eq!(ack["history"], "saved");
    assert_eq!(writes.load(Ordering::Relaxed), 0);
}
#[test]
fn device_sync_revoke_on_owner_thread_and_replay_never_replace_clipboard() {
    let (_root, host, auth) = fixture();
    enable(&host, true, true);
    let other = host.clone();
    host.set_clipboard_ui_runner(move |job| {
        other.device_sync_config_update(&json!({"enabled":false}))?;
        job()
    });
    let ack = host
        .device_sync_receive_begin("windows-peer", &auth, text("revoke", 1, "first"), false)
        .unwrap();
    assert_eq!(ack["clipboard"], "superseded");
    enable(&host, true, true);
    host.set_clipboard_ui_runner(|_| panic!("Replay cannot write clipboard"));
    let ack = host
        .device_sync_receive_begin("windows-peer", &auth, text("replay", 2, "second"), true)
        .unwrap();
    assert_eq!(ack["clipboard"], "replaySkipped");
}

#[test]
fn device_sync_disabled_global_image_type_rejects_before_reserving_capacity() {
    let (_root, host, auth) = fixture();
    host.device_sync_config_update(&json!({"imageEnabled":false}))
        .unwrap();
    let bytes = image();
    assert!(host
        .device_sync_receive_begin(
            "windows-peer",
            &auth,
            image_event("unused", 1, &bytes),
            false
        )
        .is_err());
    assert!(host.clipboard_sync.devices.state.lock().pending.is_empty());
    assert!(!host
        .clipboard_sync
        .media_path(&crate::model::sha256_hex(&bytes), "image/png")
        .unwrap()
        .exists());
}

#[test]
fn device_sync_content_marker_is_checked_and_excluded_copy_resets_dedup() {
    let (_root, host, _auth) = fixture();
    enable(&host, true, true);
    let runtime = &host.clipboard_sync;
    let snap = |text: &str| crate::clipboard::ClipboardSnapshot {
        text: Some(text.into()),
        ..Default::default()
    };
    runtime.observe(1, &snap("X")).unwrap();
    let first = runtime.devices.state.lock().durable.sequence;
    runtime
        .observe(
            2,
            &crate::clipboard::ClipboardSnapshot {
                exclude_monitor: true,
                ..Default::default()
            },
        )
        .unwrap();
    runtime.observe(3, &snap("X")).unwrap();
    assert_eq!(runtime.devices.state.lock().durable.sequence, first + 1);
    let event = text("marked", 1, "X");
    let key = crate::device_sync::content_key(&snap("X"))
        .unwrap()
        .unwrap();
    let marker = crate::device_sync::device_origin(&event, &key);
    let mut tagged = snap("X");
    tagged.sync_origin = Some(marker);
    runtime.observe(4, &tagged).unwrap();
    let before = runtime.devices.state.lock().durable.sequence;
    tagged.text = Some("Changed but marker retained".into());
    runtime.observe(5, &tagged).unwrap();
    assert_eq!(runtime.devices.state.lock().durable.sequence, before + 1);
    runtime
        .observe(6, &snap(" Changed but marker retained "))
        .unwrap();
    assert_eq!(
        runtime.devices.state.lock().durable.sequence,
        before + 2,
        "raw spaces remain significant for transport dedup"
    );
}

#[test]
fn device_sync_same_pixels_different_png_encoding_share_only_exact_pixel_identity() {
    let original = image();
    let rgba = image::load_from_memory(&original).unwrap().to_rgba8();
    let mut encoded = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(rgba)
        .write_to(&mut encoded, image::ImageFormat::Png)
        .unwrap();
    let encoded = encoded.into_inner();
    assert_ne!(
        crate::model::sha256_hex(&original),
        crate::model::sha256_hex(&encoded)
    );
    assert_eq!(
        crate::device_sync::image_key(&original).unwrap(),
        crate::device_sync::image_key(&encoded).unwrap()
    );
    let (_root, host, auth) = fixture();
    enable(&host, false, true);
    host.device_sync_receive_begin(
        "windows-peer",
        &auth,
        image_event("original-image", 1, &original),
        false,
    )
    .unwrap();
    host.device_sync_receive_media("windows-peer", &auth, "original-image", Some(&original))
        .unwrap();
    host.device_sync_receive_begin(
        "windows-peer",
        &auth,
        image_event("reencoded-image", 2, &encoded),
        false,
    )
    .unwrap();
    let ack = host
        .device_sync_receive_media("windows-peer", &auth, "reencoded-image", Some(&encoded))
        .unwrap();
    assert_eq!(ack["history"], "duplicate");
    assert_eq!(host.store().lock().list_clips().unwrap().len(), 1);
}

#[test]
fn device_sync_delayed_text_body_saves_history_without_replacing_new_local_copy() {
    let (_root, host, auth) = fixture();
    enable(&host, true, true);
    let observation = host.device_sync_observation();
    host.clipboard_sync
        .observe(
            2,
            &crate::clipboard::ClipboardSnapshot {
                text: Some("newer local".into()),
                ..Default::default()
            },
        )
        .unwrap();
    host.set_clipboard_ui_runner(|_| panic!("A slow POST body cannot replace a newer copy"));
    let ack = host
        .device_sync_receive_begin_observed(
            "windows-peer",
            &auth,
            text("slow-text", 1, "older remote"),
            false,
            observation,
        )
        .unwrap();
    assert_eq!(ack["clipboard"], "superseded");
    assert_eq!(ack["history"], "saved");
}

#[test]
fn device_sync_database_receipt_prevents_resurrection_if_state_file_save_fails() {
    let (_root, host, auth) = fixture();
    enable(&host, true, true);
    let path = host.clipboard_sync.devices.path.clone();
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    let event = text("state-failure", 1, "not resurrected");
    assert!(host
        .device_sync_receive_begin("windows-peer", &auth, event.clone(), false)
        .is_err());
    let saved = host.store().lock().list_clips().unwrap();
    assert_eq!(saved.len(), 1);
    host.store().lock().delete_clip(&saved[0].id).unwrap();
    host.set_clipboard_ui_runner(|_| panic!("Receipt replay cannot apply"));
    assert!(host
        .device_sync_receive_begin("windows-peer", &auth, event, false)
        .is_ok());
    assert!(host.store().lock().list_clips().unwrap().is_empty());
}

#[test]
fn device_sync_corrupt_settings_remain_disabled_and_cannot_be_overwritten() {
    crate::clipboard::force_headless(true);
    let root = temporary_root();
    let paths = AppPaths::from_data_root(root.path().into());
    let path = paths.configs.join("ClipboardSync/devices.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let invalid = b"{malformed device state";
    std::fs::write(&path, invalid).unwrap();
    let host = HostState::open(paths, 28965).unwrap();
    assert_eq!(host.device_sync_status()["enabled"], false);
    assert!(host
        .device_sync_config_update(&json!({"enabled":true}))
        .is_err());
    assert_eq!(std::fs::read(path).unwrap(), invalid);
}

#[test]
fn device_sync_jpeg_marker_binds_the_png_bitmap_actually_published() {
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(2, 2, image::Rgb([23, 45, 67])))
        .write_to(&mut bytes, image::ImageFormat::Jpeg)
        .unwrap();
    let bytes = bytes.into_inner();
    let png =
        crate::clipboard::dib_to_png(crate::clipboard::encode_dib_from_bytes(&bytes).unwrap())
            .unwrap();
    let platform_key = crate::clipboard_payload::sync_image_platform_key(&bytes).unwrap();
    assert_eq!(platform_key, crate::device_sync::image_key(&png).unwrap());
    assert_ne!(
        platform_key,
        crate::device_sync::image_key(&bytes).unwrap(),
        "the original JPEG bytes are a separate transport identity"
    );
    let event = image_event("jpeg-marker", 1, &bytes);
    let mut snapshot = crate::clipboard::ClipboardSnapshot {
        png: Some(png),
        sync_origin: Some(crate::device_sync::device_origin(&event, &platform_key)),
        ..Default::default()
    };
    crate::device_sync::validate_snapshot_origin(&mut snapshot);
    assert!(
        snapshot.sync_origin.is_some(),
        "reading our own applied bitmap must retain its no-echo marker"
    );
}

#[test]
fn device_sync_lan_shutdown_pauses_effects_and_peer_edits_do_not_reopen_it() {
    let (_root, host, auth) = fixture();
    enable(&host, true, true);
    let bytes = image();
    host.device_sync_receive_begin(
        "windows-peer",
        &auth,
        image_event("lan-paused", 1, &bytes),
        false,
    )
    .unwrap();
    host.lan_config_update(&json!({"enabled":false})).unwrap();
    assert!(host
        .device_sync_authenticate(Some("windows-peer"), Some(&auth))
        .is_err());
    assert!(host
        .device_sync_receive_media("windows-peer", &auth, "lan-paused", Some(&bytes))
        .is_err());
    assert!(!host
        .clipboard_sync
        .media_path(&crate::model::sha256_hex(&bytes), "image/png")
        .unwrap()
        .exists());
    host.device_sync_config_update(&json!({"deviceId":"windows-peer","textEnabled":false}))
        .unwrap();
    assert!(
        !host.lan.lock().enabled,
        "editing a peer policy must preserve an explicitly paused LAN service"
    );
    assert_eq!(
        host.device_sync_status()["enabled"],
        true,
        "the independent sync master remains configured"
    );
    host.device_sync_config_update(&json!({"enabled":true}))
        .unwrap();
    assert!(
        host.lan.lock().enabled,
        "explicitly enabling device sync also enables its transport"
    );
}


#[cfg(feature = "private-sync")]
fn pair_pull(host: &HostState, id: &str) -> String {
    host.lan_config_update(&json!({"enabled":true})).unwrap();
    let paired = host.lan_accept_inbound_pair(&json!({
        "deviceId":id,"alias":"Android test","deviceKind":"phone",
        "pairingCode":host.lan_status()["pairingCode"]["code"],"clipboardTransport":"pull"
    })).unwrap();
    assert_eq!(paired["clipboardTransport"], "pull");
    assert_eq!(paired["clipboardSyncPath"], crate::device_sync::PREFIX);
    assert_eq!(paired["cursor"], host.clipboard_sync.devices.state.lock().durable.sequence);
    format!("Bearer {}", paired["tokenForRequester"].as_str().unwrap())
}

#[cfg(feature = "private-sync")]
#[test]
fn device_sync_pull_pair_uses_shared_peer_without_callback_or_mobile_grant() {
    let (_root, host, _) = fixture();
    let auth = pair_pull(&host, "android-peer");
    assert!(!host.clipboard_sync.state.lock().durable.enabled);
    assert!(host.clipboard_sync.raw_enabled.load(Ordering::Relaxed));
    let peer = host.lan.lock().discovered["android-peer"].clone();
    assert!(peer.trusted() && peer.is_pull());
    assert!(peer.base_url.is_empty() && peer.certificate_fingerprint.is_empty());
    assert!(peer.token.is_none() && peer.paired_certificate_fingerprint.is_none());
    let dto = host.lan_peers().as_array().unwrap().iter().find(|row| row["deviceId"] == "android-peer").unwrap().clone();
    assert_eq!(dto["paired"], true);
    assert_eq!(dto["supportsManualTransfer"], false);
    assert_eq!(dto["clipboardTransport"], "pull");
    host.lan.lock().discovered.get_mut("android-peer").unwrap().last_seen_at_utc = Utc::now() - chrono::Duration::seconds(36);
    let online = || host.device_sync_status()["peers"].as_array().unwrap().iter().find(|row| row["deviceId"] == "android-peer").unwrap()["online"].clone();
    assert_eq!(online(), false);
    host.device_sync_poll("android-peer", &auth, 0, false).unwrap();
    assert_eq!(online(), true);
    assert!(host.lan_delivery_targets("dm:android-peer").is_err());
    assert!(host.lan_delivery_targets(ALL_CONVERSATION_ID).unwrap().iter().all(|p| !p.is_pull()));
    assert!(host.device_sync_poll("android-peer", &auth, 0, false).is_ok());
    let mut packet = crate::lan::LanDiscoveryPacket::announce(&crate::lan::LanIdentity::generate(), "Spoof", 29999, true);
    packet.device_id = "android-peer".into();
    assert!(!host.lan.lock().ingest(packet, "127.0.0.1"));
    assert!(host.lan.lock().discovered["android-peer"].is_pull());
}

#[cfg(feature = "private-sync")]
#[test]
fn device_sync_pull_pair_rejects_wrong_code_callback_and_transport() {
    let (_root, host, _) = fixture();
    for patch in [json!({"baseUrl":"https://127.0.0.1:2"}), json!({"certificateFingerprint":"a".repeat(64)}), json!({"tokenForReceiver":"a".repeat(64)}), json!({"clipboardTransport":"other"}), json!({"pairingCode":"invalid"})] {
        let mut request = json!({"deviceId":"android-peer","alias":"Phone","clipboardTransport":"pull","pairingCode":host.lan_status()["pairingCode"]["code"]});
        request.as_object_mut().unwrap().extend(patch.as_object().unwrap().clone());
        assert!(host.lan_accept_inbound_pair(&request).is_err());
        assert!(!host.lan.lock().discovered.contains_key("android-peer"));
    }
}

#[cfg(feature = "private-sync")]
#[test]
fn device_sync_pull_latest_copy_cursor_ack_and_media_scope() {
    let (_root, host, _) = fixture();
    let auth = pair_pull(&host, "android-peer");
    let other = pair_pull(&host, "other-phone");
    let mut changed = host.device_sync_poll_subscribe();
    let copy = |sequence, value: &str| host.clipboard_sync.observe(sequence, &crate::clipboard::ClipboardSnapshot {text:Some(value.into()),..Default::default()}).unwrap();
    copy(100, "desktop one");
    changed.try_recv().unwrap();
    let first = host.device_sync_poll("android-peer", &auth, 0, false).unwrap();
    assert_eq!(first["events"][0]["text"], "desktop one");
    assert_eq!(first["events"][0]["replay"], false);
    assert_eq!(host.device_sync_poll("android-peer", &auth, 0, false).unwrap()["events"][0]["replay"], true);
    copy(101, "desktop two");
    host.device_sync_pull_ack("android-peer", &auth, &json!({"eventId":first["events"][0]["eventId"],"status":"applied"})).unwrap();
    let latest = host.device_sync_poll("android-peer", &auth, first["cursor"].as_u64().unwrap(), true).unwrap();
    assert_eq!(latest["events"][0]["text"], "desktop two");
    assert_eq!(latest["events"][0]["replay"], true);
    host.device_sync_pull_ack("android-peer", &auth, &json!({"eventId":latest["events"][0]["eventId"],"status":"applied"})).unwrap();
    assert_eq!(host.device_sync_poll("android-peer", &auth, 0, false).unwrap()["events"],json!([]));
    assert_eq!(host.device_sync_poll("android-peer", &auth, u64::MAX, false).unwrap()["reset"],true);
    let bytes = image();
    let hash = crate::model::sha256_hex(&bytes);
    host.clipboard_sync.observe(102, &crate::clipboard::ClipboardSnapshot {png:Some(bytes),..Default::default()}).unwrap();
    assert!(host.device_sync_pull_blob("android-peer", &auth, &hash).unwrap().is_file());
    assert!(host.device_sync_pull_blob("android-peer", &auth, &"a".repeat(64)).is_err());
    let event = host.device_sync_poll("android-peer", &auth, 0, false).unwrap();
    host.device_sync_pull_ack("android-peer", &auth, &json!({"eventId":event["events"][0]["eventId"],"status":"accepted"})).unwrap();
    assert!(host.device_sync_pull_blob("android-peer", &auth, &hash).is_err());
    assert!(host.device_sync_pull_blob("other-phone", &other, &hash).is_ok());
}

#[cfg(feature = "private-sync")]
#[test]
fn device_sync_pull_shared_receive_switch_repair_and_forget_revoke() {
    let (_root, host, _) = fixture();
    let auth = pair_pull(&host, "android-peer");
    let event = SyncEvent::text("phone-text-one".into(), "android-peer".into(), 1, "Phone content".into());
    let ack = host.device_sync_receive_begin("android-peer", &auth, event.clone(), false).unwrap();
    assert!(matches!(ack["status"].as_str(), Some("accepted"|"applied")));
    let mut changes = host.device_sync_poll_subscribe();
    host.device_sync_config_update(&json!({"enabled":false})).unwrap();
    changes.try_recv().unwrap();
    assert!(host.device_sync_poll("android-peer", &auth, 0, false).is_err());
    assert!(host.device_sync_receive_begin("android-peer", &auth, event.clone(), false).is_err());
    let new_auth = pair_pull(&host, "android-peer");
    assert!(!host.clipboard_sync.devices.enabled());
    assert!(host.device_sync_poll("android-peer", &auth, 0, false).is_err());
    host.device_sync_config_update(&json!({"enabled":true})).unwrap();
    assert!(host.device_sync_poll("android-peer", &new_auth, 0, false).is_ok());
    host.lan_forget(&json!({"deviceId":"android-peer"})).unwrap();
    assert!(host.device_sync_poll("android-peer", &new_auth, 0, false).is_err());
    assert!(host.device_sync_receive_begin("android-peer", &new_auth, event, false).is_err());
}


#[cfg(feature = "private-sync")]
#[test]
fn device_sync_pull_pair_limit_does_not_change_existing_push_pairing() {
    let (_root, host, _) = fixture();
    pair_pull(&host, "existing-phone");
    {
        let mut lan = host.lan.lock();
        let template = lan.discovered["windows-peer"].clone();
        for index in 0..30 {
            let mut peer = template.clone();
            peer.device_id = format!("desktop-{index}");
            lan.discovered.insert(peer.device_id.clone(), peer);
        }
        assert_eq!(lan.discovered.len(), 32);
    }
    // Existing phone may rotate its credential at capacity.
    pair_pull(&host, "existing-phone");
    let code = host.lan_status()["pairingCode"]["code"].clone();
    assert!(host.lan_accept_inbound_pair(&json!({"deviceId":"extra-phone","alias":"Phone",
        "clipboardTransport":"pull","pairingCode":code})).unwrap_err().contains("Too many"));
    // Old desktop callers omit clipboardTransport, and retain their old limit.
    let response = host.lan_accept_inbound_pair(&json!({"deviceId":"extra-desktop","alias":"Desktop",
        "baseUrl":"https://127.0.0.1:29999","certificateFingerprint":"b".repeat(64),
        "tokenForReceiver":crate::lan::create_token(),"pairingCode":code})).unwrap();
    assert_eq!(response["clipboardTransport"], "push");
    assert_eq!(host.lan.lock().discovered.len(), 33);
}
