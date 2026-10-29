use super::*;

fn fixture() -> (tempfile::TempDir, HostState, String, PathBuf) {
    crate::clipboard::force_headless(true);
    let root = tempfile::tempdir().unwrap();
    let host = HostState::open(AppPaths::from_data_root(root.path().join("BetterClipTauri")), 28965).unwrap();
    let source = host.inner.lock().paths.image_temp.join("original.png");
    image::RgbaImage::from_pixel(3, 2, image::Rgba([0, 255, 0, 255])).save(&source).unwrap();
    let mut record = ClipRecord::new_text("image", "gif fixture");
    record.kind = ClipKind::ImageClipItem;
    record.image_path = Some(source.to_string_lossy().into_owned());
    let id = record.id.clone();
    host.inner.lock().store.lock().insert_clip(&record, true).unwrap();
    (root, host, id, source)
}

#[test]
fn gif_copy_host_preserves_source_and_propagates_prepare_and_write_errors() {
    let (_root, host, id, source) = fixture();
    let params = json!({"id":id});
    let original = std::fs::read(&source).unwrap();
    let before = host.detail(&params).unwrap();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = calls.clone();
    let inner = host.inner.clone();
    host.set_clipboard_ui_runner(move |job| {
        let host_guard = inner.try_lock().expect("host lock must be released");
        let db_guard = host_guard.store.try_lock().expect("store lock must be released");
        drop(db_guard); drop(host_guard);
        seen.fetch_add(1, Ordering::Relaxed);
        job()
    });
    assert_eq!(host.copy_gif(&params).unwrap(), json!({"ok":true}));
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(std::fs::read(&source).unwrap(), original);
    assert_eq!(host.detail(&params).unwrap(), before);
    let exports = host.inner.lock().paths.image_library.join("ClipboardExports");
    let gif = std::fs::read_dir(&exports).unwrap().next().unwrap().unwrap().path();
    assert_eq!(image::open(&gif).unwrap().to_rgba8().dimensions(), (3, 2));
    host.copy_gif(&params).unwrap();
    assert_eq!(std::fs::read_dir(&exports).unwrap().count(), 1);
    host.set_clipboard_ui_runner(|_| Err("clipboard busy".into()));
    assert_eq!(host.copy_gif(&params).unwrap_err(), "clipboard busy");
    host.set_clipboard_ui_runner(|_| panic!("bad input must not reach the clipboard"));
    std::fs::write(&source, b"broken PNG").unwrap();
    assert!(host.copy_gif(&params).is_err());
    std::fs::remove_file(&source).unwrap();
    assert!(host.copy_gif(&params).is_err());
}

#[cfg(windows)]
#[test]
#[ignore = "writes synthetic GIF/file/text formats to Windows clipboard; run alone"]
fn native_gif_copy_roundtrip_and_failed_conversion_preserves_clipboard() {
    let (_root, host, id, source) = fixture();
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            crate::clipboard::force_headless(true);
            std::env::set_var("CLIPHOST_TAURI_HEADLESS", "1");
        }
    }
    let _restore = Restore;
    crate::clipboard::force_headless(false);
    std::env::set_var("CLIPHOST_TAURI_HEADLESS", "0");
    let params = json!({"id":id});
    host.copy_gif(&params).unwrap();
    let snapshot = crate::clipboard::peek_snapshot().unwrap();
    assert_eq!(snapshot.files.len(), 1);
    assert!(snapshot.files[0].ends_with(".gif"));
    assert!(snapshot.qq_rich.unwrap().contains(&snapshot.files[0]));
    assert!(snapshot.png.is_none(), "do not flatten the GIF to a bitmap");
    assert!(std::fs::read(&snapshot.files[0]).unwrap().starts_with(b"GIF8"));
    assert_eq!(image::open(&snapshot.files[0]).unwrap().to_rgba8(),
        image::RgbaImage::from_pixel(3, 2, image::Rgba([0, 255, 0, 255])));
    // A conversion error must leave existing clipboard contents intact.
    crate::clipboard::write_text("GIF failure sentinel 中文").unwrap();
    std::fs::write(&source, b"not an image").unwrap();
    assert!(host.copy_gif(&params).is_err());
    assert_eq!(crate::clipboard::peek_snapshot().unwrap().text.as_deref(), Some("GIF failure sentinel 中文"));
}
