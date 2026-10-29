use super::*;

fn fixture() -> (tempfile::TempDir, Mutex<AppStore>) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .unwrap()
        .join(".tmp/image-identity-preparation-tests");
    std::fs::create_dir_all(&root).unwrap();
    let directory = tempfile::tempdir_in(root).unwrap();
    let store = AppStore::open_unprojected(&directory.path().join("userdata.db")).unwrap();
    (directory, Mutex::new(store))
}

fn pixels() -> image::DynamicImage {
    image::DynamicImage::ImageRgb8(image::ImageBuffer::from_fn(5, 3, |x, y| {
        image::Rgb([
            (x * 23 + 4) as u8,
            (y * 51 + 19) as u8,
            ((x + y) * 31) as u8,
        ])
    }))
}

fn encode(image: image::DynamicImage) -> Vec<u8> {
    let mut output = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut output, image::ImageFormat::Png)
        .unwrap();
    output.into_inner()
}

fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut output = (data.len() as u32).to_be_bytes().to_vec();
    output.extend_from_slice(kind);
    output.extend_from_slice(data);
    let mut crc = u32::MAX;
    for byte in &output[4..] {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb88320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    output.extend_from_slice(&(!crc).to_be_bytes());
    output
}

fn windows_encoding() -> Vec<u8> {
    let bytes = encode(image::DynamicImage::ImageRgba8(pixels().to_rgba8()));
    let mut output = bytes[..33].to_vec();
    output.extend(chunk(b"sRGB", &[0]));
    output.extend(chunk(b"gAMA", &45455u32.to_be_bytes()));
    output.extend(chunk(b"pHYs", &[0, 0, 14, 196, 0, 0, 14, 196, 1]));
    output.extend_from_slice(&bytes[33..]);
    output
}

fn image(path: &Path, id: &str, bytes: &[u8]) -> ClipRecord {
    std::fs::write(path, bytes).unwrap();
    let mut item = ClipRecord::new_text("", "preparation fixture");
    item.kind = ClipKind::ImageClipItem;
    item.id = id.into();
    item.image_path = Some(path.to_string_lossy().into_owned());
    item.content_hash = crate::model::sha256_hex(bytes);
    item
}

fn receipt(item: &ClipRecord) -> String {
    serde_json::json!({"origin":"fixture-peer","sequence":1,"id":item.id,"hash":"fixture-digest",
        "ack":{"status":"accepted","history":"saved","historyId":item.id}})
    .to_string()
}

#[test]
fn real_png_preparation_unifies_capture_and_sync_in_both_arrival_orders() {
    for sync_first in [false, true] {
        let (directory, store) = fixture();
        let native = image(
            &directory.path().join("native.png"),
            "native",
            &encode(pixels()),
        );
        let synced = image(
            &directory.path().join("synced.png"),
            "synced",
            &windows_encoding(),
        );
        assert_ne!(native.content_hash, synced.content_hash);
        let key = "device-sync-receipt:preparation-fixture";
        if sync_first {
            let prepared = prepare(&store, &synced).unwrap();
            store
                .lock()
                .commit_device_sync_receipt_prepared(
                    key,
                    &receipt(&synced),
                    Some(&synced),
                    &prepared,
                )
                .unwrap();
            let prepared = prepare(&store, &native).unwrap();
            let merged = store
                .lock()
                .persist_capture_prepared(&native, false, |_| false, &prepared)
                .unwrap();
            assert_eq!(merged.id, synced.id);
        } else {
            let prepared = prepare(&store, &native).unwrap();
            store
                .lock()
                .persist_capture_prepared(&native, false, |_| false, &prepared)
                .unwrap();
            let prepared = prepare(&store, &synced).unwrap();
            store
                .lock()
                .commit_device_sync_receipt_prepared(
                    key,
                    &receipt(&synced),
                    Some(&synced),
                    &prepared,
                )
                .unwrap();
            let ack: serde_json::Value =
                serde_json::from_str(&store.lock().kv_get(key).unwrap().unwrap()).unwrap();
            assert_eq!(ack["ack"]["history"], "duplicate");
            assert_eq!(ack["ack"]["historyId"], native.id);
        }
        assert_eq!(store.lock().list_clips().unwrap().len(), 1);
        let native_identity = store
            .lock()
            .cached_image_identity(&native.content_hash)
            .unwrap();
        let sync_identity = store
            .lock()
            .cached_image_identity(&synced.content_hash)
            .unwrap();
        assert!(native_identity.is_some());
        assert_eq!(native_identity, sync_identity);
    }
}

#[test]
fn claimed_sha_must_match_actual_bytes_before_caching_or_merging() {
    let (directory, store) = fixture();
    let native = image(
        &directory.path().join("native.png"),
        "native",
        &encode(pixels()),
    );
    let prepared = prepare(&store, &native).unwrap();
    store
        .lock()
        .persist_capture_prepared(&native, false, |_| false, &prepared)
        .unwrap();
    let mut unverified = image(
        &directory.path().join("unverified.png"),
        "unverified",
        &windows_encoding(),
    );
    unverified.content_hash = "f".repeat(64);
    let prepared = prepare(&store, &unverified).unwrap();
    assert_eq!(
        store
            .lock()
            .cached_image_identity(&unverified.content_hash)
            .unwrap(),
        None
    );
    assert_eq!(
        store
            .lock()
            .persist_capture_prepared(&unverified, false, |_| false, &prepared)
            .unwrap()
            .id,
        unverified.id
    );
    assert_eq!(store.lock().list_clips().unwrap().len(), 2);
}

#[test]
fn changed_pixels_stay_separate_even_with_a_colliding_legacy_perceptual_hash() {
    let (directory, store) = fixture();
    let mut first = image(
        &directory.path().join("first.png"),
        "first",
        &encode(pixels()),
    );
    let mut changed_pixels = pixels().to_rgba8();
    changed_pixels.get_pixel_mut(2, 1)[0] ^= 1;
    let mut changed = image(
        &directory.path().join("changed.png"),
        "changed",
        &encode(image::DynamicImage::ImageRgba8(changed_pixels)),
    );
    first.perceptual_hash = "0".repeat(64);
    changed.perceptual_hash = first.perceptual_hash.clone();
    assert!(
        first.looks_equal(&changed),
        "legacy perceptual equality would lose this pixel edit"
    );
    let prepared = prepare(&store, &first).unwrap();
    store
        .lock()
        .persist_capture_prepared(&first, false, |_| false, &prepared)
        .unwrap();
    let prepared = prepare(&store, &changed).unwrap();
    assert_eq!(
        store
            .lock()
            .persist_capture_prepared(&changed, false, |_| false, &prepared)
            .unwrap()
            .id,
        changed.id
    );
    assert_eq!(store.lock().list_clips().unwrap().len(), 2);
}

#[test]
fn cloud_named_candidate_is_not_opened_or_used_as_an_exact_match() {
    let (directory, store) = fixture();
    // This is a disposable local fixture, not a real synchronized folder.
    let cloud_name = directory.path().join("OneDrive-fixture");
    std::fs::create_dir(&cloud_name).unwrap();
    let candidate = image(
        &cloud_name.join("candidate.png"),
        "candidate",
        &encode(pixels()),
    );
    store.lock().insert_clip(&candidate, true).unwrap();
    let incoming = image(
        &directory.path().join("incoming.png"),
        "incoming",
        &windows_encoding(),
    );
    let prepared = prepare(&store, &incoming).unwrap();
    assert!(store
        .lock()
        .cached_image_identity(&incoming.content_hash)
        .unwrap()
        .is_some());
    assert_eq!(
        store
            .lock()
            .cached_image_identity(&candidate.content_hash)
            .unwrap(),
        None
    );
    store
        .lock()
        .persist_capture_prepared(&incoming, false, |_| false, &prepared)
        .unwrap();
    assert_eq!(store.lock().list_clips().unwrap().len(), 2);
}

#[test]
fn verified_sha_cache_survives_an_unavailable_original_file_without_reopening_it() {
    let (directory, store) = fixture();
    let path = directory.path().join("source.png");
    let item = image(&path, "source", &windows_encoding());
    prepare(&store, &item).unwrap();
    let expected = store
        .lock()
        .cached_image_identity(&item.content_hash)
        .unwrap()
        .unwrap();
    std::fs::remove_file(&path).unwrap();
    prepare(&store, &item).unwrap();
    assert_eq!(
        store
            .lock()
            .cached_image_identity(&item.content_hash)
            .unwrap(),
        Some(expected)
    );
}
