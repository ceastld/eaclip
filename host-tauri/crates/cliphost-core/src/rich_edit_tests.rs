use super::*;
use std::cell::RefCell;

thread_local! {
    static BEFORE_IMAGES: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
}

// Pause the real image-persistence path only on the explicitly armed test thread.
pub(super) fn before_image_persistence() {
    let callback = BEFORE_IMAGES.with(|slot| slot.borrow_mut().take());
    if let Some(callback) = callback { callback(); }
}

#[derive(Clone, Copy, Debug)]
enum Destination { History, Favorite }

impl Destination {
    fn save(self, host: &HostState, params: &Value) -> Result<Value, String> {
        match self {
            Self::History => host.patch_item(params),
            Self::Favorite => host.favor_update(params),
        }
    }

    fn detail(self, host: &HostState) -> Value {
        match self {
            Self::History => host.detail(&json!({"id":"rich"})).unwrap(),
            Self::Favorite => favor_to_detail(&host.store().lock().get_favor("rich").unwrap()),
        }
    }
}

fn fixture(destination: Destination) -> (tempfile::TempDir, HostState, String, Vec<u8>) {
    crate::clipboard::force_headless(true);
    let dir = tempfile::tempdir().unwrap();
    let paths = AppPaths::from_data_root(dir.path().join("host"));
    let host = HostState::open(paths, 28965).unwrap();
    let source = dir.path().join("source.png");
    image::RgbaImage::from_pixel(2, 2, image::Rgba([17, 29, 43, 255])).save(&source).unwrap();
    let original = std::fs::read(&source).unwrap();
    let incoming = crate::im_rich::qq_image_envelope(source.to_str().unwrap());
    let mut item = ClipRecord::new_text("original body", "qq.exe");
    item.id = "rich".into();
    item.kind = ClipKind::QQRichTextItem;
    item.qq_rich_text = Some("<QQRichEditFormat/>".into());
    let store = host.store();
    match destination {
        Destination::History => { store.lock().insert_clip(&item, true).unwrap(); }
        Destination::Favorite => {
            let mut favor = FavorRecord::from_clip(&item, None);
            favor.id = item.id;
            store.lock().insert_favor(&favor).unwrap();
        }
    }
    let mut other = ClipRecord::new_text("unrelated searchable text", "fixture");
    other.id = "other".into();
    store.lock().insert_clip(&other, true).unwrap();
    (dir, host, incoming, original)
}

// A failed assertion must never leave the save worker holding a database lock.
fn during_image_preparation(
    host: &HostState, destination: Destination, params: Value,
    action: impl FnOnce(HostState) -> Result<Value, String> + Send + 'static,
) -> (bool, Result<Value, String>, Result<Value, String>) {
    during_image_preparation_with_store_lock(host, destination, params, false, action)
}

fn during_image_preparation_with_store_lock(
    host: &HostState, destination: Destination, params: Value, hold_database: bool,
    action: impl FnOnce(HostState) -> Result<Value, String> + Send + 'static,
) -> (bool, Result<Value, String>, Result<Value, String>) {
    let store = host.store();
    let hook_store = store.clone();
    let (entered, entering) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let saving_host = host.clone();
    let saving = thread::spawn(move || {
        BEFORE_IMAGES.with(|slot| *slot.borrow_mut() = Some(Box::new(move || {
            // Negative control simulates the original database-lock regression.
            let _held = hold_database.then(|| hook_store.lock());
            entered.send(()).unwrap();
            released.recv_timeout(Duration::from_secs(10)).unwrap();
        })));
        destination.save(&saving_host, &params)
    });
    entering.recv_timeout(Duration::from_secs(5)).expect("must reach real image preparation");
    // Check the cause directly; wall-clock IO latency under a debugger must
    // not be mistaken for a lock held by the paused image worker.
    let database_available = store.try_lock().is_some();
    let window_state_available = host.inner.try_lock().is_some();
    let (done, completed) = mpsc::channel();
    let other_host = host.clone();
    let other = thread::spawn(move || {
        let result = action(other_host);
        let _ = done.send(result);
    });
    let first = if database_available && window_state_available {
        // Both actual search/edit and persistence must finish before release.
        completed.recv_timeout(Duration::from_secs(5))
    } else {
        Err(mpsc::RecvTimeoutError::Timeout)
    };
    let responsive = database_available && window_state_available && first.is_ok();
    eprintln!("rich edit: destination={destination:?}, held_control={hold_database}, database_available={database_available}, window_state_available={window_state_available}, completed_before_release={responsive}");
    release.send(()).unwrap();
    let saved = saving.join().unwrap();
    other.join().unwrap();
    let concurrent = first.unwrap_or_else(|_| completed.recv().unwrap());
    (responsive, saved, concurrent)
}

#[test]
fn rich_image_save_keeps_other_history_operations_responsive() {
    let mut blocked = Vec::new();
    for destination in [Destination::History, Destination::Favorite] {
        let (_dir, host, incoming, original) = fixture(destination);
        let revision = destination.detail(&host)["revision"].clone();
        let (responsive, saved, concurrent) = during_image_preparation(&host, destination,
            json!({"id":"rich", "qqRichText":incoming, "expectedRevision":revision}), |host| {
                let page = host.search_items(&json!({"webSessionId":"rich-edit", "query":"unrelated"}))?;
                assert_eq!(page["total"], 1);
                host.patch_item(&json!({"id":"other", "bodyText":"edited while images pending"}))
            });
        if !responsive { blocked.push(format!("{destination:?}")); }
        concurrent.unwrap();
        let saved = saved.unwrap();
        let raw = saved["qqRichText"].as_str().unwrap();
        let segments = crate::im_rich::parse_segments(raw, crate::im_rich::ImRichOrigin::Qq);
        let path = Path::new(&segments[0].path);
        assert!(path.starts_with(&host.inner.lock().paths.image_library));
        assert_eq!(std::fs::read(path).unwrap(), original);
        assert_eq!(saved["revision"], destination.detail(&host)["revision"]);
    }
    assert!(blocked.is_empty(), "image preparation blocked other operations: {blocked:?}");
}

#[test]
fn rich_image_save_rechecks_revision_after_preparation() {
    for destination in [Destination::History, Destination::Favorite] {
        let (_dir, host, incoming, _) = fixture(destination);
        let revision = destination.detail(&host)["revision"].clone();
        let concurrent_revision = revision.clone();
        let (responsive, saved, concurrent) = during_image_preparation(&host, destination,
            json!({"id":"rich", "qqRichText":incoming, "expectedRevision":revision}), move |host| {
                destination.save(&host, &json!({"id":"rich", "title":"concurrent title", "expectedRevision":concurrent_revision}))
            });
        assert!(responsive, "{destination:?}: another edit must complete while copying images");
        concurrent.unwrap();
        assert!(saved.unwrap_err().starts_with("WRITE_CONFLICT:"));
        let final_item = destination.detail(&host);
        assert_eq!(final_item["title"], "concurrent title");
        assert_eq!(final_item["qqRichText"], "<QQRichEditFormat/>");
    }
}

#[test]
fn rich_image_legacy_save_merges_into_current_item_without_losing_other_fields() {
    for destination in [Destination::History, Destination::Favorite] {
        let (_dir, host, incoming, _) = fixture(destination);
        let (responsive, saved, concurrent) = during_image_preparation(&host, destination,
            json!({"id":"rich", "qqRichText":incoming}), move |host| {
                destination.save(&host, &json!({"id":"rich", "title":"concurrent title"}))
            });
        assert!(responsive, "{destination:?}: legacy editing also releases the database");
        concurrent.unwrap();
        let saved = saved.unwrap();
        assert_eq!(saved["title"], "concurrent title");
        assert_eq!(saved["revision"], destination.detail(&host)["revision"]);
    }
}

#[test]
fn rich_image_responsiveness_probe_rejects_an_actually_held_database_lock() {
    for destination in [Destination::History, Destination::Favorite] {
        let (_dir, host, incoming, _) = fixture(destination);
        let (responsive, saved, concurrent) = during_image_preparation_with_store_lock(
            &host, destination, json!({"id":"rich", "qqRichText":incoming}), true,
            |host| host.patch_item(&json!({"id":"other", "title":"after locked preparation"})),
        );
        assert!(!responsive, "{destination:?}: the probe accepted a held database lock");
        saved.unwrap();
        concurrent.unwrap();
    }
}
