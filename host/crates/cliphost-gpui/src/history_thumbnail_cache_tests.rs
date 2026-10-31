use super::*;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc, Barrier,
};

fn fixture() -> tempfile::TempDir {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .unwrap();
    let parent = repository.join(".tmp/history-thumbnail-cache-tests");
    // Inspect only the compile-time repository path. No product data discovery.
    for ancestor in repository.ancestors().collect::<Vec<_>>().into_iter().rev() {
        let metadata = fs::symlink_metadata(ancestor).unwrap();
        check_metadata(&metadata).unwrap();
        assert!(metadata.is_dir());
    }
    let temporary = repository.join(".tmp");
    match fs::create_dir(&temporary) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => panic!("cannot create repository fixture parent: {error}"),
    }
    check_metadata(&fs::symlink_metadata(&temporary).unwrap()).unwrap();
    match fs::create_dir(&parent) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => panic!("cannot create thumbnail fixture parent: {error}"),
    }
    check_metadata(&fs::symlink_metadata(&parent).unwrap()).unwrap();
    tempfile::Builder::new()
        .prefix("thumb-")
        .tempdir_in(parent)
        .unwrap()
}

fn png(value: u8) -> Vec<u8> {
    let image = image::RgbImage::from_pixel(32, 16, image::Rgb([value, 23, 45]));
    let mut output = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(image)
        .write_to(&mut output, image::ImageFormat::Png)
        .unwrap();
    output.into_inner()
}

fn display(value: u8) -> ContentImage {
    ContentImage {
        image: Arc::new(Image::from_bytes(ImageFormat::Png, png(value))),
        width: 3840.0,
        height: 1920.0,
        render: None,
    }
}

fn cached_files(root: &Path) -> Vec<PathBuf> {
    let directory = root.join("Image/Thumbnails").join(CACHE_DIRECTORY);
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    entries
        .map(Result::unwrap)
        .map(|entry| entry.path())
        .filter(|path| path.file_name().unwrap().to_str().and_then(owned_name) == Some(false))
        .collect()
}

fn file_for(root: &Path, version: &str) -> PathBuf {
    root.join("Image/Thumbnails")
        .join(CACHE_DIRECTORY)
        .join(format!("{}.thumb", cache_key(version)))
}

#[test]
fn disk_hit_retains_natural_dimensions_without_generating_or_touching_the_file() {
    let fixture = fixture();
    let root = fixture.path();
    let first = load_or_generate(root, "source-1", || Ok(display(51))).unwrap();
    let path = file_for(root, "source-1");
    assert_eq!(cached_files(root), vec![path.clone()]);
    let old_time = UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
    File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(old_time))
        .unwrap();
    let original = fs::read(&path).unwrap();
    let second =
        load_or_generate(root, "source-1", || panic!("disk hit must not regenerate")).unwrap();
    assert_eq!((second.width, second.height), (3840.0, 1920.0));
    assert_eq!(first.image.bytes(), second.image.bytes());
    assert_eq!(fs::read(&path).unwrap(), original);
    assert_eq!(fs::metadata(path).unwrap().modified().unwrap(), old_time);
}

fn display_file_for(root: &Path, version: &str, height: u32) -> PathBuf {
    root.join("Image/Thumbnails")
        .join(CACHE_DIRECTORY)
        .join(format!("{}.thumb", display_cache_key(version, height)))
}

fn stored_png_size(path: &Path) -> (u32, u32) {
    png_dimensions(&fs::read(path).unwrap()[HEADER_BYTES..]).unwrap()
}

#[test]
fn display_disk_hit_keeps_natural_dimensions_and_reuses_the_verified_pixels() {
    let fixture = fixture();
    let root = fixture.path();
    load_or_generate_display(root, "display-hit", 8, || Ok(display(51))).unwrap();
    let path = display_file_for(root, "display-hit", 8);
    // The disk entry is the painted size, not the generator's larger PNG.
    assert_eq!(stored_png_size(&path), (16, 8));
    let bytes = fs::read(&path).unwrap();
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    let prepared = load_or_generate_display(root, "display-hit", 8, || {
        panic!("display cache hit must not regenerate")
    }).unwrap();
    assert_eq!((prepared.width, prepared.height), (3840.0, 1920.0));
    let render = prepared.render.as_ref().unwrap().clone();
    assert_eq!((render.size(0).width.0, render.size(0).height.0), (16, 8));
    assert_eq!(&render.as_bytes(0).unwrap()[..4], &[45, 23, 51, 255]);
    // The normal display adapter is now a no-op: the fully validated PNG was
    // already converted, and the encoded identity token is never decoded.
    let again = crate::native_content::display_from_list_image(prepared, 8);
    assert!(Arc::ptr_eq(&render, again.render.as_ref().unwrap()));
    assert_eq!(fs::read(&path).unwrap(), bytes);
    assert_eq!(fs::metadata(path).unwrap().modified().unwrap(), modified);
}

#[test]
fn display_heights_and_encoded_thumbnails_keep_separate_disk_entries() {
    let fixture = fixture();
    let root = fixture.path();
    let encoded = load_or_generate(root, "display-heights", || Ok(display(52))).unwrap();
    assert!(encoded.render.is_none());
    let calls = AtomicUsize::new(0);
    let generate = || {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(display(52))
    };
    let small = load_or_generate_display(root, "display-heights", 8, generate).unwrap();
    let large = load_or_generate_display(root, "display-heights", 16, generate).unwrap();
    let minimum = load_or_generate_display(root, "display-heights", 0, generate).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_eq!(small.render.as_ref().unwrap().size(0).height.0, 8);
    assert_eq!(large.render.as_ref().unwrap().size(0).height.0, 16);
    assert_eq!(minimum.render.as_ref().unwrap().size(0).height.0, 1);
    assert_eq!(stored_png_size(&display_file_for(root, "display-heights", 8)), (16, 8));
    assert_eq!(stored_png_size(&display_file_for(root, "display-heights", 16)), (32, 16));
    // The encoded caller still owns its own full-size entry.
    assert_eq!(stored_png_size(&file_for(root, "display-heights")), (32, 16));
    let small_again = load_or_generate_display(root, "display-heights", 8, || panic!("disk hit")).unwrap();
    assert_eq!(small_again.render.as_ref().unwrap().size(0).height.0, 8);
    let encoded_again = load_or_generate(root, "display-heights", || panic!("disk hit")).unwrap();
    assert_eq!(encoded_again.image.bytes(), encoded.image.bytes());
    assert_eq!(cached_files(root).len(), 4);
}

#[test]
fn display_disk_hit_rejects_corrupt_pixels_even_with_a_valid_outer_digest() {
    let fixture = fixture();
    let root = fixture.path();
    load_or_generate_display(root, "display-corrupt", 8, || Ok(display(53))).unwrap();
    let path = display_file_for(root, "display-corrupt", 8);
    let mut bytes = fs::read(&path).unwrap();
    let data = bytes[HEADER_BYTES..].windows(4).position(|chunk| chunk == b"IDAT").unwrap();
    bytes[HEADER_BYTES + data + 4] ^= 0xff;
    let digest = blake3::hash(&bytes[HEADER_BYTES..]);
    bytes[24..56].copy_from_slice(digest.as_bytes());
    fs::write(&path, bytes).unwrap();
    assert!(read_entry_prepared(&path, &display_cache_key("display-corrupt", 8), Some(8)).is_err());
    let calls = AtomicUsize::new(0);
    let recovered = load_or_generate_display(root, "display-corrupt", 8, || {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(display(54))
    }).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(&recovered.render.as_ref().unwrap().as_bytes(0).unwrap()[..4], &[45, 23, 54, 255]);
}

#[test]
#[ignore = "manual disk-hit decode timing; no timing assertion"]
fn display_disk_hit_timing_comparison() {
    use std::time::Instant;

    let fixture = fixture();
    let root = fixture.path();
    let original = image::RgbImage::from_fn(3840, 2160, |x, y| image::Rgb([
        (x.wrapping_mul(17) ^ y.wrapping_mul(13)) as u8,
        (x.wrapping_mul(3) ^ y.wrapping_mul(7)) as u8,
        (x.wrapping_add(y).wrapping_mul(11)) as u8,
    ]));
    let mut encoded = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(original).write_to(&mut encoded, image::ImageFormat::Png).unwrap();
    let source = encoded.into_inner();
    let thumbnail = crate::native_content::list_image(source.clone()).unwrap();
    load_or_generate(root, "display-timing", || Ok(thumbnail)).unwrap();
    let small = crate::native_content::list_display_image(source, 160).unwrap();
    drop(load_or_generate_display(root, "display-timing", 160, || Ok(small)).unwrap());
    let mut old_times = Vec::new();
    let mut new_times = Vec::new();
    for round in 0..20 {
        // Alternate execution order and drop every result before the next
        // load, so the weak gate cannot substitute an in-memory hit.
        for prepared in [round % 2 == 0, round % 2 != 0] {
            let began = Instant::now();
            let image = if prepared {
                load_or_generate_display(root, "display-timing", 160, || panic!("disk hit")).unwrap()
            } else {
                let encoded = load_or_generate(root, "display-timing", || panic!("disk hit")).unwrap();
                crate::native_content::display_from_list_image(encoded, 160)
            };
            assert_eq!(image.render.as_ref().unwrap().size(0).height.0, 160);
            let elapsed = began.elapsed().as_secs_f64() * 1000.0;
            std::hint::black_box(&image);
            drop(image);
            if prepared { new_times.push(elapsed); } else { old_times.push(elapsed); }
        }
    }
    old_times.sort_by(f64::total_cmp);
    new_times.sort_by(f64::total_cmp);
    eprintln!("{}", serde_json::json!({
        "rounds":20,"naturalSize":[3840,2160],"encodedSize":[1024,576],"displaySize":[284,160],
        "encodedThenDisplayMedianMs":(old_times[9]+old_times[10])/2.0,
        "verifiedDisplayMedianMs":(new_times[9]+new_times[10])/2.0,
    }));
}

#[test]
fn concurrent_same_key_waiters_share_one_generation_even_when_disk_writes_fail() {
    let fixture = fixture();
    let root = fixture.path();
    fs::write(root.join("Image"), b"block directory creation").unwrap();
    // Keep the gate alive until every concurrent caller has observed it, even
    // when the OS schedules one caller only after the first has completed.
    let _gate = key_gate(root, &cache_key("same-version"));
    let calls = AtomicUsize::new(0);
    let barrier = Barrier::new(8);
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    load_or_generate(root, "same-version", || {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok(display(21))
                    })
                    .unwrap()
                })
            })
            .collect();
        for handle in handles {
            assert_eq!(handle.join().unwrap().image.bytes(), png(21));
        }
    });
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(cached_files(root).is_empty());
}

#[test]
fn different_keys_generate_without_waiting_for_the_other_image() {
    let fixture = fixture();
    let root = fixture.path();
    let (entered_send, entered_receive) = mpsc::channel();
    let (release_a, wait_a) = mpsc::channel();
    let (release_b, wait_b) = mpsc::channel();
    std::thread::scope(|scope| {
        let entered_a = entered_send.clone();
        let first = scope.spawn(move || {
            load_or_generate(root, "independent-a", || {
                entered_a.send(()).unwrap();
                wait_a
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .unwrap();
                Ok(display(11))
            })
        });
        let second = scope.spawn(move || {
            load_or_generate(root, "independent-b", || {
                entered_send.send(()).unwrap();
                wait_b
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .unwrap();
                Ok(display(12))
            })
        });
        let both_entered = entered_receive
            .recv_timeout(std::time::Duration::from_secs(5))
            .is_ok()
            && entered_receive
                .recv_timeout(std::time::Duration::from_secs(5))
                .is_ok();
        // Always release workers, including on the failure path.
        let _ = release_a.send(());
        let _ = release_b.send(());
        assert!(first.join().unwrap().is_ok());
        assert!(second.join().unwrap().is_ok());
        assert!(
            both_entered,
            "another source must not wait on a cold decode"
        );
    });
    assert_eq!(
        cached_files(root).len(),
        2,
        "both independently generated images must persist"
    );
}

#[test]
fn concurrent_cold_keys_all_publish_before_returning() {
    let fixture = fixture();
    let root = fixture.path();
    let ready = Barrier::new(4);
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..4)
            .map(|index| {
                let ready = &ready;
                scope.spawn(move || {
                    load_or_generate(root, &format!("cold-{index}"), || {
                        ready.wait();
                        Ok(display(index as u8 + 31))
                    })
                    .unwrap()
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
    });
    assert_eq!(cached_files(root).len(), 4);
    for index in 0..4 {
        assert_eq!(
            load_or_generate(root, &format!("cold-{index}"), || panic!(
                "cold thumbnail was not persisted"
            ))
            .unwrap()
            .image
            .bytes(),
            png(index as u8 + 31)
        );
    }
}

#[cfg(windows)]
#[test]
fn canonical_verbatim_disk_roots_share_cache_files_and_gates_with_dos_roots() {
    let fixture = fixture();
    let root = fixture.path();
    let canonical = fs::canonicalize(root).unwrap();
    assert!(canonical.to_string_lossy().starts_with(r"\\?\"));
    assert!(check_path(&canonical).is_ok());
    let first = load_or_generate(&canonical, "verbatim-root", || Ok(display(52))).unwrap();
    assert_eq!(cached_files(root).len(), 1);
    let second = load_or_generate(root, "verbatim-root", || {
        panic!("DOS spelling must hit the canonical root's cache")
    })
    .unwrap();
    assert_eq!(first.image.bytes(), second.image.bytes());
    assert!(Arc::ptr_eq(
        &key_gate(root, "same-gate"),
        &key_gate(&canonical, "same-gate")
    ));
    for unsafe_root in [
        r"\\server\share\private",
        r"\\?\UNC\server\share\private",
        r"\\.\C:\private",
    ] {
        assert_eq!(
            check_path(Path::new(unsafe_root)),
            Err("THUMB_CACHE_UNSAFE_PATH")
        );
    }
}

#[test]
fn changed_source_version_uses_an_independent_persisted_thumbnail() {
    let fixture = fixture();
    let root = fixture.path();
    load_or_generate(root, "version-1", || Ok(display(71))).unwrap();
    let replacement = load_or_generate(root, "version-2", || Ok(display(72))).unwrap();
    assert_eq!(cached_files(root).len(), 2);
    assert_eq!(replacement.image.bytes(), png(72));
    assert_eq!(
        load_or_generate(root, "version-1", || panic!("old version remains reusable"))
            .unwrap()
            .image
            .bytes(),
        png(71)
    );
}

#[test]
fn damaged_digest_and_oversized_entry_are_rebuilt_from_the_generator() {
    let fixture = fixture();
    let root = fixture.path();
    load_or_generate(root, "damaged", || Ok(display(81))).unwrap();
    let path = file_for(root, "damaged");
    let mut damaged = fs::read(&path).unwrap();
    damaged[HEADER_BYTES + 35] ^= 0x40;
    fs::write(&path, damaged).unwrap();
    let regenerated = load_or_generate(root, "damaged", || Ok(display(82))).unwrap();
    assert_eq!(regenerated.image.bytes(), png(82));
    assert_eq!(
        read_entry(&path, &cache_key("damaged"))
            .unwrap()
            .image
            .bytes(),
        png(82)
    );
    File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(MAX_ENTRY_BYTES + 1)
        .unwrap();
    assert_eq!(
        load_or_generate(root, "damaged", || Ok(display(83)))
            .unwrap()
            .image
            .bytes(),
        png(83)
    );
    assert!(fs::metadata(path).unwrap().len() < MAX_ENTRY_BYTES);
}

#[test]
fn cache_envelope_rejects_wrong_identity_and_invalid_natural_dimensions() {
    let fixture = fixture();
    let root = fixture.path();
    load_or_generate(root, "identity-a", || Ok(display(91))).unwrap();
    let first = file_for(root, "identity-a");
    let second = file_for(root, "identity-b");
    fs::copy(first, &second).unwrap();
    assert_eq!(
        load_or_generate(root, "identity-b", || Ok(display(92)))
            .unwrap()
            .image
            .bytes(),
        png(92)
    );
    let mut invalid = fs::read(&second).unwrap();
    invalid[8..12].copy_from_slice(&f32::NAN.to_le_bytes());
    fs::write(&second, invalid).unwrap();
    assert_eq!(
        load_or_generate(root, "identity-b", || Ok(display(93)))
            .unwrap()
            .image
            .bytes(),
        png(93)
    );
}

#[test]
fn item_and_byte_quotas_evict_only_module_owned_cache_files() {
    let fixture = fixture();
    let root = fixture.path();
    let entry_bytes = encode_entry(&display(1), &cache_key("sizing"))
        .unwrap()
        .len() as u64;
    let limits = Limits {
        bytes: entry_bytes * 2,
        items: 2,
    };
    let directory = DirectoryGuard::open(root, true).unwrap().path;
    fs::write(
        directory.join("keep-user-file.txt"),
        b"not generated by this cache",
    )
    .unwrap();
    for version in 0..8 {
        load_or_generate_with(root, &format!("quota-{version}"), || Ok(display(1)), limits)
            .unwrap();
        let files = cached_files(root);
        assert!(files.len() <= 2);
        assert!(
            files
                .iter()
                .map(|path| fs::metadata(path).unwrap().len())
                .sum::<u64>()
                <= limits.bytes
        );
        assert!(file_for(root, &format!("quota-{version}")).is_file());
    }
    assert_eq!(
        fs::read(directory.join("keep-user-file.txt")).unwrap(),
        b"not generated by this cache"
    );
    // A byte ceiling below one entry never rejects the live preview.
    let too_small = Limits {
        bytes: entry_bytes - 1,
        items: 10,
    };
    let result =
        load_or_generate_with(root, "too-large-for-quota", || Ok(display(1)), too_small).unwrap();
    assert_eq!(result.image.bytes(), png(1));
    assert!(!file_for(root, "too-large-for-quota").exists());
}

#[test]
fn a_recent_atomic_stage_counts_toward_quota_and_is_not_deleted() {
    let fixture = fixture();
    let root = fixture.path();
    let directory = DirectoryGuard::open(root, true).unwrap().path;
    let stage = directory.join(".clip-image-abc123.stage");
    fs::write(&stage, vec![0; 2048]).unwrap();
    let result = load_or_generate_with(
        root,
        "stage-quota",
        || Ok(display(7)),
        Limits {
            bytes: 1024,
            items: 1,
        },
    )
    .unwrap();
    assert_eq!(result.image.bytes(), png(7));
    assert_eq!(fs::metadata(stage).unwrap().len(), 2048);
    assert!(cached_files(root).is_empty());
}

#[test]
fn gif_and_webp_payloads_bypass_persistence_without_changing_bytes() {
    let fixture = fixture();
    let root = fixture.path();
    for (version, format, payload) in [
        ("animated-gif", ImageFormat::Gif, b"GIF89a".as_slice()),
        (
            "animated-webp",
            ImageFormat::Webp,
            b"RIFFfakeWEBP".as_slice(),
        ),
    ] {
        let expected = ContentImage {
            image: Arc::new(Image::from_bytes(format, payload.to_vec())),
            width: 320.0,
            height: 180.0,
            render: None,
        };
        let result = load_or_generate(root, version, || Ok(expected.clone())).unwrap();
        assert_eq!(result.image.format(), format);
        assert_eq!(result.image.bytes(), payload);
        let prepared = load_or_generate_display(root, version, 160, || Ok(expected.clone())).unwrap();
        assert_eq!(prepared.image.format(), format);
        assert_eq!(prepared.image.bytes(), payload);
        assert!(prepared.render.is_none());
    }
    assert!(cached_files(root).is_empty());
    assert!(!root.join("Image").exists());
}

#[test]
fn failed_or_unsafe_cache_io_keeps_the_generated_preview_and_source_untouched() {
    let fixture = fixture();
    let root = fixture.path();
    let original = root.join("original.png");
    fs::write(&original, png(14)).unwrap();
    fs::write(root.join("Image"), b"readonly blocker").unwrap();
    assert_eq!(
        load_or_generate(root, "unwritable-cache", || Ok(display(15)))
            .unwrap()
            .image
            .bytes(),
        png(15)
    );
    assert_eq!(fs::read(original).unwrap(), png(14));
    let unsafe_root = root.join("OneDrive-offline-must-not-be-opened");
    // Lexical admission rejects before even trying to create/open this root.
    assert_eq!(
        load_or_generate(&unsafe_root, "unsafe-root", || Ok(display(16)))
            .unwrap()
            .image
            .bytes(),
        png(16)
    );
    assert!(!unsafe_root.exists());
    assert_eq!(
        load_or_generate(root, "decode-failed", || Err(
            "synthetic-decode-failed".into()
        ))
        .err()
        .unwrap(),
        "synthetic-decode-failed"
    );
}

#[test]
fn cloud_placeholder_and_reparse_flags_are_rejected_before_any_data_open() {
    assert!(check_attributes(0).is_ok());
    for flag in [0x400, 0x1000, 0x40000, 0x400000] {
        assert_eq!(check_attributes(flag), Err("THUMB_CACHE_UNSAFE_PATH"));
    }
    for spelling in ["OneDrive", "OneDrive - Work", "Dropbox", "iCloud"] {
        let fixture = fixture();
        assert_eq!(
            check_path(&fixture.path().join(spelling)),
            Err("THUMB_CACHE_UNSAFE_PATH")
        );
    }
}
