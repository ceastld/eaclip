//! Real adapter + encrypted engine round trips; no network or system clipboard.
use super::*;
use cliphost_webdav::{
    Engine, PreparedSync, RecoveryKey, RemoteFile, RemoteStore, Snapshot, SyncError,
};
use std::sync::Mutex;

#[derive(Default)]
struct MemoryRemoteStore {
    state: Mutex<(u64, BTreeMap<String, RemoteFile>)>,
}
impl RemoteStore for MemoryRemoteStore {
    fn get(&self, path: &str) -> Result<Option<RemoteFile>, SyncError> {
        Ok(self.state.lock().unwrap().1.get(path).cloned())
    }
    fn create(&self, path: &str, bytes: &[u8]) -> Result<bool, SyncError> {
        let mut state = self.state.lock().unwrap();
        if state.1.contains_key(path) {
            return Ok(false);
        }
        state.0 += 1;
        let etag = format!("\"{}\"", state.0);
        state.1.insert(
            path.into(),
            RemoteFile {
                bytes: bytes.to_vec(),
                etag,
            },
        );
        Ok(true)
    }
    fn replace(&self, path: &str, etag: &str, bytes: &[u8]) -> Result<bool, SyncError> {
        let mut state = self.state.lock().unwrap();
        if state.1.get(path).map(|f| f.etag.as_str()) != Some(etag) {
            return Ok(false);
        }
        state.0 += 1;
        let etag = format!("\"{}\"", state.0);
        state.1.insert(
            path.into(),
            RemoteFile {
                bytes: bytes.to_vec(),
                etag,
            },
        );
        Ok(true)
    }
}
fn fixture(document: bool) -> (tempfile::TempDir, AppPaths, AppStore) {
    let dir = tempfile::tempdir().unwrap();
    let paths = AppPaths::from_data_root(dir.path().join("data"));
    paths.ensure().unwrap();
    if document {
        let db = rusqlite::Connection::open(&paths.db).unwrap();
        db.execute_batch("CREATE TABLE clip_item(Id TEXT PRIMARY KEY,Data TEXT);CREATE TABLE favor_item(Id TEXT PRIMARY KEY,Data TEXT);").unwrap();
    }
    let store = AppStore::open(&paths.db).unwrap();
    (dir, paths, store)
}
fn favorite(id: &str) -> FavorRecord {
    let mut r = FavorRecord::from_clip(&ClipRecord::new_text("round trip 原文", "fixture"), None);
    r.id = id.into();
    r.name = id.into();
    r
}
fn folder(id: &str, children: &[&str]) -> FavorRecord {
    let mut r = favorite(id);
    r.is_folder = true;
    r.kind = crate::model::ClipKind::FavoriteClipItem;
    r.children = children.iter().map(|s| s.to_string()).collect();
    r
}
fn seed(paths: &AppPaths, store: &AppStore) {
    use base64::Engine as _;
    let png=base64::engine::general_purpose::STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+j5l0AAAAASUVORK5CYII=").unwrap();
    let path = paths.image_library.join("original.png");
    fs::write(&path, png).unwrap();
    let mut image = favorite("image");
    image.kind = crate::model::ClipKind::ImageClipItem;
    image.image_path = Some(path.to_string_lossy().into());
    image.parent_id = Some("a".into());
    let mut html = favorite("html");
    html.kind = crate::model::ClipKind::HtmlClipItem;
    html.parent_id = Some("a".into());
    let uri = url::Url::from_file_path(&path).unwrap();
    html.html_data=Some(crate::cf_html::build_clipboard_html(&format!("<html><body><!--StartFragment--><b>round trip 原文</b><img src='{uri}'><!--EndFragment--></body></html>"),Some("https://example.invalid/")));
    let ordinary = paths.data_root.join("ordinary.txt");
    fs::write(&ordinary, b"ordinary attachment\0original bytes").unwrap();
    let mut file = favorite("file");
    file.kind = crate::model::ClipKind::FileClipItem;
    file.file_paths = vec![ordinary.to_string_lossy().into()];
    file.parent_id = Some("a".into());
    store
        .replace_favors(&[
            folder("a", &["image", "html", "file"]),
            folder("b", &["image"]),
            image.clone(),
            html,
            file,
        ])
        .unwrap();
    let tx = store.connection().unchecked_transaction().unwrap();
    let raw: String = tx
        .query_row(
            "SELECT Data FROM main.\"favor_item\" WHERE Id='image'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let mut raw: Value = serde_json::from_str(&raw).unwrap();
    raw["FutureSchema"] = json!({"unknown":["keep exact",23]});
    raw["Item"]["FutureCodecField"] = json!({"preserve":true});
    restore_document(&tx, &image, &raw).unwrap();
    tx.commit().unwrap();
    crate::config_file::save(
        &config_path(paths),
        &json!({"RootChildren":["b","a"],"KeepLocalSetting":true}),
    )
    .unwrap();
}
fn snapshot(bundle: &ExportBundle, first_empty_join: bool) -> Snapshot {
    assert!(bundle.issues.is_empty(), "{:?}", bundle.issues);
    let mut entries = bundle.entries.clone();
    // No local root edit exists on a brand-new empty replica. Sending [] here
    // would fabricate a roots conflict against the existing remote collection.
    if !(first_empty_join && entries.is_empty() && bundle.roots.is_empty()) {
        entries.insert("roots".into(), json!(bundle.roots));
    }
    Snapshot {
        entries,
        blobs: bundle.blobs.clone(),
    }
}
fn validate_candidate(candidate: &Snapshot) -> Result<(), SyncError> {
    let roots: Vec<String> = serde_json::from_value(
        candidate
            .entries
            .get("roots")
            .cloned()
            .unwrap_or_else(|| json!([])),
    )
    .map_err(|_| SyncError::InvalidData("favorite_roots"))?;
    let entries = candidate
        .entries
        .iter()
        .filter(|(key, _)| key.starts_with("favor/"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    validate_portable(&entries, &roots, &candidate.blobs, &Limits::default())
        .map_err(|_| SyncError::InvalidData("favorite_graph_or_resources"))
}
fn sync(
    engine: &mut Engine,
    remote: &MemoryRemoteStore,
    store: &AppStore,
    paths: &AppPaths,
    first_empty_join: bool,
) -> PreparedSync {
    let before = export_favorites(store, paths, &Limits::default()).unwrap();
    let input = snapshot(&before, first_empty_join);
    let prepared = engine
        .prepare_scoped_validated(
            remote,
            &input,
            &["favor/".into(), "roots".into()],
            validate_candidate,
        )
        .unwrap();
    assert!(prepared.conflicts.is_empty(), "{:?}", prepared.conflicts);
    let plan = ApplyPlan {
        expected_revision: before.revision,
        entries: prepared
            .snapshot
            .entries
            .iter()
            .filter(|(k, _)| k.starts_with("favor/"))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        roots: serde_json::from_value(prepared.snapshot.entries["roots"].clone()).unwrap(),
        blobs: prepared.snapshot.blobs.clone(),
        receipt: Some(("webdav.dev.applied-token".into(), prepared.token.clone())),
    };
    apply_favorites(store, paths, &plan, &Limits::default()).unwrap();
    assert_eq!(
        store.kv_get("webdav.dev.applied-token").unwrap().as_deref(),
        Some(prepared.token.as_str())
    );
    engine.acknowledge(&prepared.token).unwrap();
    prepared
}
fn edit(store: &AppStore, id: &str, title: &str) {
    let mut item = store.get_favor(id).unwrap();
    item.name = title.into();
    store.save_favor(&item).unwrap();
}

#[test]
fn webdav_adapter_engine_native_document_round_trip_and_offline_conflicts() {
    let remote = MemoryRemoteStore::default();
    let key = RecoveryKey::generate().unwrap();
    let (_a, a_paths, a_store) = fixture(true);
    seed(&a_paths, &a_store);
    let (_b, b_paths, b_store) = fixture(false);
    let original = export_favorites(&a_store, &a_paths, &Limits::default()).unwrap();
    assert_eq!(original.blobs.len(), 2);
    let mut a = Engine::open(&a_paths.data_root.join("engine"), key.clone()).unwrap();
    a.create(&remote).unwrap();
    sync(&mut a, &remote, &a_store, &a_paths, false);
    let mut b = Engine::open(&b_paths.data_root.join("engine"), key.clone()).unwrap();
    b.join(&remote).unwrap();
    sync(&mut b, &remote, &b_store, &b_paths, true);
    let received = export_favorites(&b_store, &b_paths, &Limits::default()).unwrap();
    assert_eq!(original.entries, received.entries);
    assert_eq!(original.roots, received.roots);
    assert_eq!(original.blobs, received.blobs);
    assert_eq!(
        b_store.get_favor("a").unwrap().children,
        vec!["image", "html", "file"]
    );
    assert_eq!(b_store.get_favor("b").unwrap().children, vec!["image"]);
    let source_png = fs::read(a_store.get_favor("image").unwrap().image_path.unwrap()).unwrap();
    let local_png = fs::read(b_store.get_favor("image").unwrap().image_path.unwrap()).unwrap();
    assert_eq!(source_png, local_png);
    for (path, file) in &remote.state.lock().unwrap().1 {
        if path != "vault.json" && path != "head.json" {
            assert!(!String::from_utf8_lossy(&file.bytes).contains("round trip 原文"));
            assert!(!String::from_utf8_lossy(&file.bytes).contains("FutureSchema"));
        }
    }
    let second = sync(&mut b, &remote, &b_store, &b_paths, false);
    assert!(!second.report.published);
    assert_eq!(second.report.uploaded_objects, 0);
    drop(b);
    let mut b = Engine::open(&b_paths.data_root.join("engine"), key).unwrap();
    let restarted = sync(&mut b, &remote, &b_store, &b_paths, false);
    assert!(!restarted.report.published);
    edit(&a_store, "image", "A offline image title");
    edit(&b_store, "html", "B offline HTML title");
    sync(&mut a, &remote, &a_store, &a_paths, false);
    sync(&mut b, &remote, &b_store, &b_paths, false);
    sync(&mut a, &remote, &a_store, &a_paths, false);
    assert_eq!(
        a_store.get_favor("html").unwrap().name,
        "B offline HTML title"
    );
    assert_eq!(
        b_store.get_favor("image").unwrap().name,
        "A offline image title"
    );
    let current = export_favorites(&a_store, &a_paths, &Limits::default()).unwrap();
    let image: PortableFavorite =
        serde_json::from_value(current.entries["favor/image"].clone()).unwrap();
    assert_eq!(
        image.document.unwrap()["FutureSchema"]["unknown"][0],
        "keep exact"
    );
    edit(&a_store, "html", "left simultaneous title");
    edit(&b_store, "html", "right simultaneous title");
    sync(&mut a, &remote, &a_store, &a_paths, false);
    let before = export_favorites(&b_store, &b_paths, &Limits::default()).unwrap();
    let receipt_before = b_store.kv_get("webdav.dev.applied-token").unwrap();
    let conflict = b.prepare(&remote, &snapshot(&before, false)).unwrap();
    assert_eq!(conflict.conflicts.len(), 1);
    let record = &conflict.conflicts[0];
    assert_eq!(record.key, "favor/html");
    let titles: BTreeSet<_> = [
        record.left_value.as_ref().unwrap()["record"]["name"]
            .as_str()
            .unwrap(),
        record.right_value.as_ref().unwrap()["record"]["name"]
            .as_str()
            .unwrap(),
    ]
    .into();
    assert_eq!(
        titles,
        BTreeSet::from(["left simultaneous title", "right simultaneous title"])
    );
    assert_eq!(
        b_store.get_favor("html").unwrap().name,
        "right simultaneous title"
    );
    assert_eq!(
        b_store.kv_get("webdav.dev.applied-token").unwrap(),
        receipt_before
    );
    assert_eq!(b.pending().unwrap().unwrap().conflicts, conflict.conflicts);
}

#[test]
fn webdav_source_file_change_during_network_cannot_be_replaced_by_old_snapshot() {
    let (_dir, paths, store) = fixture(false);
    let file = paths.data_root.join("changing.txt");
    fs::write(&file, b"old byte snapshot").unwrap();
    let mut item = favorite("changing");
    item.kind = crate::model::ClipKind::FileClipItem;
    item.file_paths = vec![file.to_string_lossy().into()];
    store.replace_favors(&[item]).unwrap();
    let before = export_favorites(&store, &paths, &Limits::default()).unwrap();
    fs::write(&file, b"new byte snapshot").unwrap();
    let plan = ApplyPlan {
        expected_revision: before.revision,
        entries: before.entries,
        roots: before.roots,
        blobs: before.blobs,
        receipt: None,
    };
    assert!(apply_favorites(&store, &paths, &plan, &Limits::default())
        .unwrap_err()
        .contains("source attachment changed"));
    assert_eq!(fs::read(&file).unwrap(), b"new byte snapshot");
    assert_eq!(
        store.get_favor("changing").unwrap().file_paths,
        vec![file.to_string_lossy().into_owned()]
    );
}

#[test]
fn webdav_deleted_root_folder_ignores_stale_order_cache_and_propagates_tombstone() {
    let remote = MemoryRemoteStore::default();
    let key = RecoveryKey::generate().unwrap();
    let (_a, a_paths, a_store) = fixture(false);
    a_store
        .replace_favors(&[folder("keep", &[]), folder("removed", &[])])
        .unwrap();
    crate::config_file::save(
        &config_path(&a_paths),
        &json!({"RootChildren":["keep","removed"],"PreserveLocal":true}),
    )
    .unwrap();
    let (_b, b_paths, b_store) = fixture(true);
    let mut a = Engine::open(&a_paths.data_root.join("engine"), key.clone()).unwrap();
    a.create(&remote).unwrap();
    sync(&mut a, &remote, &a_store, &a_paths, false);
    let mut b = Engine::open(&b_paths.data_root.join("engine"), key).unwrap();
    b.join(&remote).unwrap();
    sync(&mut b, &remote, &b_store, &b_paths, true);
    a_store.delete_favors(&["removed".into()]).unwrap();
    let raw_config = read_config(&a_paths).unwrap();
    assert_eq!(
        roots_from(raw_config.as_ref()).unwrap(),
        vec!["keep", "removed"]
    );
    let exported = export_favorites(&a_store, &a_paths, &Limits::default()).unwrap();
    assert!(exported.issues.is_empty());
    assert_eq!(exported.roots, vec!["keep"]);
    assert_eq!(read_config(&a_paths).unwrap(), raw_config);
    let published = sync(&mut a, &remote, &a_store, &a_paths, false);
    assert!(published.report.published);
    sync(&mut b, &remote, &b_store, &b_paths, false);
    assert!(b_store.get_favor("removed").is_err());
    assert!(b_store.get_favor("keep").unwrap().is_folder);
    assert_eq!(
        roots_from(read_config(&b_paths).unwrap().as_ref()).unwrap(),
        vec!["keep"]
    );
}

#[test]
fn webdav_merged_orphan_is_rejected_before_publishing_head() {
    let remote = MemoryRemoteStore::default();
    let key = RecoveryKey::generate().unwrap();
    let (_a, a_paths, a_store) = fixture(false);
    a_store
        .replace_favors(&[folder("keep", &[]), folder("owner", &[])])
        .unwrap();
    crate::config_file::save(
        &config_path(&a_paths),
        &json!({"RootChildren":["keep","owner"]}),
    )
    .unwrap();
    let (_b, b_paths, b_store) = fixture(false);
    let mut a = Engine::open(&a_paths.data_root.join("engine"), key.clone()).unwrap();
    a.create(&remote).unwrap();
    sync(&mut a, &remote, &a_store, &a_paths, false);
    let mut b = Engine::open(&b_paths.data_root.join("engine"), key).unwrap();
    b.join(&remote).unwrap();
    sync(&mut b, &remote, &b_store, &b_paths, true);
    a_store.delete_favors(&["owner".into()]).unwrap();
    sync(&mut a, &remote, &a_store, &a_paths, false);
    // ParentId-only edges are a supported legacy compatibility representation.
    // The owner document itself is unchanged on B, so the generic per-key merge
    // cannot detect the resulting orphan without the business validator.
    let mut rows = b_store.list_favors().unwrap();
    let mut child = favorite("new-child");
    child.parent_id = Some("owner".into());
    rows.push(child);
    b_store.replace_favors(&rows).unwrap();
    let local = export_favorites(&b_store, &b_paths, &Limits::default()).unwrap();
    assert!(local.issues.is_empty());
    let before = remote.get("head.json").unwrap().unwrap().bytes;
    let error = b
        .prepare_scoped_validated(
            &remote,
            &snapshot(&local, false),
            &["favor/".into(), "roots".into()],
            validate_candidate,
        )
        .unwrap_err();
    assert!(matches!(
        error,
        SyncError::InvalidData("favorite_graph_or_resources")
    ));
    assert_eq!(remote.get("head.json").unwrap().unwrap().bytes, before);
    assert_eq!(
        b_store.get_favor("new-child").unwrap().parent_id.as_deref(),
        Some("owner")
    );
    assert!(b.pending().unwrap().is_none());
}
