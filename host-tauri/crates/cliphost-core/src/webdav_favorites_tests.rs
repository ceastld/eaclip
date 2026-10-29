use super::*;

fn fixture(document: bool) -> (tempfile::TempDir, AppPaths, AppStore) {
    let dir = tempfile::tempdir().unwrap();
    let paths = AppPaths::from_data_root(dir.path().join("data"));
    paths.ensure().unwrap();
    if document {
        let conn = rusqlite::Connection::open(&paths.db).unwrap();
        conn.execute_batch("CREATE TABLE clip_item(Id TEXT PRIMARY KEY,Data TEXT);CREATE TABLE favor_item(Id TEXT PRIMARY KEY,Data TEXT,RowToken TEXT);").unwrap();
    }
    let store = AppStore::open(&paths.db).unwrap();
    (dir, paths, store)
}
fn favorite(id: &str, text: &str) -> FavorRecord {
    let mut item = FavorRecord::from_clip(&ClipRecord::new_text(text, "fixture"), None);
    item.id = id.into();
    item.name = text.into();
    item
}
fn folder(id: &str, children: &[&str]) -> FavorRecord {
    let mut item = favorite(id, id);
    item.is_folder = true;
    item.kind = crate::model::ClipKind::FavoriteClipItem;
    item.children = children.iter().map(|s| s.to_string()).collect();
    item
}
fn config(paths: &AppPaths, roots: &[&str]) {
    crate::config_file::save(
        &config_path(paths),
        &json!({"RootChildren":roots,"UnrelatedFutureSetting":{"keep":true}}),
    )
    .unwrap();
}
fn plan(bundle: &ExportBundle, revision: String) -> ApplyPlan {
    ApplyPlan {
        expected_revision: revision,
        entries: bundle.entries.clone(),
        roots: bundle.roots.clone(),
        blobs: bundle.blobs.clone(),
        receipt: Some(("webdav.dev.applied-token".into(), "fixture-token".into())),
    }
}

#[test]
fn portable_favorites_shared_graph_roots_and_receipt_round_trip() {
    let (_a, paths, store) = fixture(false);
    let mut leaf = favorite("leaf", "共享正文");
    leaf.parent_id = Some("a".into());
    store
        .replace_favors(&[folder("a", &["leaf"]), folder("b", &["leaf"]), leaf])
        .unwrap();
    config(&paths, &["b", "a"]);
    let source = export_favorites(&store, &paths, &Limits::default()).unwrap();
    assert!(source.issues.is_empty());
    let (_b, target_paths, target) = fixture(false);
    let before = export_favorites(&target, &target_paths, &Limits::default()).unwrap();
    apply_favorites(
        &target,
        &target_paths,
        &plan(&source, before.revision),
        &Limits::default(),
    )
    .unwrap();
    let after = export_favorites(&target, &target_paths, &Limits::default()).unwrap();
    assert_eq!(source.entries, after.entries);
    assert_eq!(source.roots, after.roots);
    assert_eq!(target.get_favor("a").unwrap().children, vec!["leaf"]);
    assert_eq!(target.get_favor("b").unwrap().children, vec!["leaf"]);
    assert_eq!(
        target
            .kv_get("webdav.dev.applied-token")
            .unwrap()
            .as_deref(),
        Some("fixture-token")
    );
}

#[test]
fn portable_document_unknown_fields_image_and_rich_source_survive_relocation() {
    let (_a, paths, store) = fixture(true);
    let image = paths.image_library.join("source.png");
    fs::write(&image, b"original encoded image bytes").unwrap();
    let mut item = favorite("image", "图片");
    item.kind = crate::model::ClipKind::ImageClipItem;
    item.image_path = Some(image.to_string_lossy().into_owned());
    store
        .replace_favors(&[folder("root", &["image"]), {
            item.parent_id = Some("root".into());
            item.clone()
        }])
        .unwrap();
    let tx = store.connection().unchecked_transaction().unwrap();
    let mut raw: Value = serde_json::from_str(
        &tx.query_row(
            "SELECT Data FROM main.\"favor_item\" WHERE Id='image'",
            [],
            |r| r.get::<_, String>(0),
        )
        .unwrap(),
    )
    .unwrap();
    raw["UnknownFuture"] = json!({"nested":[1,"保留",{"a":true}]});
    raw["Item"]["UnknownImageMetadata"] = json!({"codec":"preserve-original"});
    restore_document(&tx, &item, &raw).unwrap();
    tx.commit().unwrap();
    config(&paths, &["root"]);
    let mut source = export_favorites(&store, &paths, &Limits::default()).unwrap();
    assert!(source.issues.is_empty());
    source.entries.get_mut("favor/image").unwrap()["record"]["futurePortableMetadata"] =
        json!({"keep":true});
    for document in [false, true] {
        let (_b, target_paths, target) = fixture(document);
        let before = export_favorites(&target, &target_paths, &Limits::default()).unwrap();
        apply_favorites(
            &target,
            &target_paths,
            &plan(&source, before.revision),
            &Limits::default(),
        )
        .unwrap();
        let after = export_favorites(&target, &target_paths, &Limits::default()).unwrap();
        assert_eq!(source.entries, after.entries);
        let local = target.get_favor("image").unwrap();
        let downloaded = Path::new(local.image_path.as_ref().unwrap());
        assert!(downloaded.starts_with(target_paths.data_root.join("WebDavSync")));
        assert_eq!(
            fs::read(downloaded).unwrap(),
            b"original encoded image bytes"
        );
        if document {
            let tx = target.connection().unchecked_transaction().unwrap();
            let raw: String = tx
                .query_row(
                    "SELECT Data FROM main.\"favor_item\" WHERE Id='image'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            let raw: Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(raw["UnknownFuture"]["nested"][1], "保留");
            assert_eq!(
                raw["Item"]["UnknownImageMetadata"]["codec"],
                "preserve-original"
            );
            tx.commit().unwrap();
        }
        let mut edited = target.get_favor("image").unwrap();
        edited.name = "仅改标题".into();
        target.save_favor(&edited).unwrap();
        let edited = export_favorites(&target, &target_paths, &Limits::default()).unwrap();
        let old: PortableFavorite =
            serde_json::from_value(source.entries["favor/image"].clone()).unwrap();
        let new: PortableFavorite =
            serde_json::from_value(edited.entries["favor/image"].clone()).unwrap();
        assert_eq!(new.record["imagePath"], old.record["imagePath"]);
        assert_eq!(
            new.record["futurePortableMetadata"],
            old.record["futurePortableMetadata"]
        );
        assert_eq!(
            new.document.as_ref().unwrap()["UnknownFuture"],
            old.document.as_ref().unwrap()["UnknownFuture"]
        );
    }
}

#[test]
fn portable_html_original_bytes_do_not_become_local_download_paths() {
    let (_a, paths, store) = fixture(false);
    let image = paths.image_library.join("embedded.png");
    fs::write(&image, b"embedded bytes").unwrap();
    let uri = url::Url::from_file_path(&image).unwrap().to_string();
    let html=crate::cf_html::build_clipboard_html(&format!("<html><head><style>b{{color:red}}</style></head><body><!--StartFragment--><b>原文</b><img src=\"{uri}\"><!--EndFragment--></body></html>"),Some("https://example.invalid/source"));
    let mut item = favorite("html", "rich");
    item.kind = crate::model::ClipKind::HtmlClipItem;
    item.html_data = Some(html.clone());
    store.replace_favors(&[item]).unwrap();
    let source = export_favorites(&store, &paths, &Limits::default()).unwrap();
    assert!(source.issues.is_empty(), "{:?}", source.issues);
    let (_b, target_paths, target) = fixture(false);
    let before = export_favorites(&target, &target_paths, &Limits::default()).unwrap();
    apply_favorites(
        &target,
        &target_paths,
        &plan(&source, before.revision),
        &Limits::default(),
    )
    .unwrap();
    let after = export_favorites(&target, &target_paths, &Limits::default()).unwrap();
    assert_eq!(source.entries, after.entries);
    let remote: PortableFavorite =
        serde_json::from_value(after.entries["favor/html"].clone()).unwrap();
    assert_eq!(remote.record["htmlData"], html);
    assert!(target
        .get_favor("html")
        .unwrap()
        .html_data
        .unwrap()
        .contains("WebDavSync"));
}

#[test]
fn portable_files_are_bounded_and_cloud_directories_and_folders_are_issues() {
    let (_a, paths, store) = fixture(false);
    let normal = paths.data_root.join("plain.txt");
    fs::write(&normal, b"file body").unwrap();
    let cloud = paths.data_root.join("OneDriveFixture");
    fs::create_dir(&cloud).unwrap();
    let placeholder = cloud.join("never-read.txt");
    fs::write(&placeholder, b"private").unwrap();
    let mut item = favorite("files", "files");
    item.kind = crate::model::ClipKind::FileClipItem;
    item.file_paths = vec![
        normal.to_string_lossy().into(),
        cloud.to_string_lossy().into(),
        placeholder.to_string_lossy().into(),
    ];
    store.replace_favors(&[item]).unwrap();
    let exported = export_favorites(&store, &paths, &Limits::default()).unwrap();
    assert_eq!(exported.issues.len(), 2);
    assert_eq!(exported.blobs.len(), 1);
    assert_eq!(exported.blobs.values().next().unwrap(), b"file body");
    let tiny = Limits {
        max_file_bytes: 2,
        ..Limits::default()
    };
    let exported = export_favorites(&store, &paths, &tiny).unwrap();
    assert_eq!(exported.issues.len(), 3);
    assert!(exported.blobs.is_empty());
}

#[test]
fn portable_attachment_integrity_and_stale_apply_leave_existing_data() {
    let (_a, paths, store) = fixture(false);
    let image = paths.image_library.join("a.png");
    fs::write(&image, b"payload").unwrap();
    let mut item = favorite("image", "image");
    item.image_path = Some(image.to_string_lossy().into());
    store.replace_favors(&[item]).unwrap();
    let source = export_favorites(&store, &paths, &Limits::default()).unwrap();
    let (_b, target_paths, target) = fixture(false);
    target.replace_favors(&[favorite("keep", "keep")]).unwrap();
    let before = export_favorites(&target, &target_paths, &Limits::default()).unwrap();
    let mut bad = plan(&source, before.revision.clone());
    bad.blobs.values_mut().next().unwrap().push(0);
    assert!(apply_favorites(&target, &target_paths, &bad, &Limits::default()).is_err());
    assert_eq!(
        export_favorites(&target, &target_paths, &Limits::default())
            .unwrap()
            .revision,
        before.revision
    );
    let mut invalid_wire = plan(&source, before.revision.clone());
    invalid_wire.entries.get_mut("favor/image").unwrap()["record"]["filePaths"] =
        json!("PRIVATE_BODY_MUST_NOT_ENTER_STATUS");
    let error =
        apply_favorites(&target, &target_paths, &invalid_wire, &Limits::default()).unwrap_err();
    assert!(!error.contains("PRIVATE_BODY_MUST_NOT_ENTER_STATUS"));
    let stale = plan(&source, before.revision);
    let mut edit = target.get_favor("keep").unwrap();
    edit.name = "changed locally".into();
    target.save_favor(&edit).unwrap();
    assert!(
        apply_favorites(&target, &target_paths, &stale, &Limits::default())
            .unwrap_err()
            .contains("WRITE_CONFLICT")
    );
    assert_eq!(target.get_favor("keep").unwrap().name, "changed locally");
}

#[test]
fn portable_apply_database_failure_rolls_back_and_recovers_config_journal() {
    let (_a, paths, store) = fixture(false);
    store.replace_favors(&[favorite("bad", "reject")]).unwrap();
    let source = export_favorites(&store, &paths, &Limits::default()).unwrap();
    let (_b, target_paths, target) = fixture(false);
    target.replace_favors(&[folder("keep", &[])]).unwrap();
    config(&target_paths, &["keep"]);
    let before = export_favorites(&target, &target_paths, &Limits::default()).unwrap();
    let config_before = read_config(&target_paths).unwrap();
    target.connection().execute_batch("CREATE TRIGGER reject_webdav BEFORE INSERT ON favor_item WHEN NEW.name='reject' BEGIN SELECT RAISE(ABORT,'fixture rejection');END;").unwrap();
    assert!(apply_favorites(
        &target,
        &target_paths,
        &plan(&source, before.revision.clone()),
        &Limits::default()
    )
    .is_err());
    assert_eq!(read_config(&target_paths).unwrap(), config_before);
    assert_eq!(
        export_favorites(&target, &target_paths, &Limits::default())
            .unwrap()
            .revision,
        before.revision
    );
    assert!(!target_paths.data_root.join(JOURNAL).exists());
}

#[test]
fn portable_interrupted_config_apply_uses_database_receipt_and_refuses_external_edits() {
    let (_a, paths, store) = fixture(false);
    config(&paths, &["old"]);
    let before = read_config(&paths).unwrap();
    let after = set_roots(before.clone().unwrap(), &["new".into()]).unwrap();
    let journal = ApplyJournal {
        operation_id: "operation".into(),
        before: before.clone(),
        after: after.clone(),
    };
    let journal_path = paths.data_root.join(JOURNAL);
    crate::config_file::save(&journal_path, &journal).unwrap();
    crate::config_file::save(&config_path(&paths), &after).unwrap();
    recover_apply(&store, &paths).unwrap();
    assert_eq!(read_config(&paths).unwrap(), before);
    crate::config_file::save(&journal_path, &journal).unwrap();
    store.kv_set(APPLY_RECEIPT, "operation").unwrap();
    recover_apply(&store, &paths).unwrap();
    assert_eq!(read_config(&paths).unwrap(), Some(after.clone()));
    crate::config_file::save(&journal_path, &journal).unwrap();
    config(&paths, &["external"]);
    assert!(recover_apply(&store, &paths).is_err());
    assert!(journal_path.exists());
    assert_eq!(
        roots_from(read_config(&paths).unwrap().as_ref()).unwrap(),
        vec!["external"]
    );
}

#[test]
fn portable_invalid_graph_never_changes_the_target() {
    let (_a, paths, store) = fixture(false);
    store
        .replace_favors(&[folder("a", &["b"]), folder("b", &["a"])])
        .unwrap();
    let source = export_favorites(&store, &paths, &Limits::default()).unwrap();
    let (_b, target_paths, target) = fixture(false);
    let before = export_favorites(&target, &target_paths, &Limits::default()).unwrap();
    assert!(apply_favorites(
        &target,
        &target_paths,
        &plan(&source, before.revision.clone()),
        &Limits::default()
    )
    .unwrap_err()
    .contains("cycle"));
    assert_eq!(
        export_favorites(&target, &target_paths, &Limits::default())
            .unwrap()
            .revision,
        before.revision
    );
}

#[test]
fn portable_file_names_survive_devices_and_remote_names_cannot_escape_managed_root() {
    let (_a, paths, store) = fixture(false);
    let first = paths.data_root.join("报告 original.txt");
    let second = paths.data_root.join("same bytes other name.txt");
    fs::write(&first, b"shared bytes").unwrap();
    fs::write(&second, b"shared bytes").unwrap();
    let mut file = favorite("files", "file names");
    file.kind = crate::model::ClipKind::FileClipItem;
    file.file_paths = vec![
        first.to_string_lossy().into(),
        second.to_string_lossy().into(),
    ];
    store.replace_favors(&[file]).unwrap();
    let source = export_favorites(&store, &paths, &Limits::default()).unwrap();
    assert!(source.issues.is_empty());
    assert_eq!(source.blobs.len(), 1);
    let (_b, target_paths, target) = fixture(false);
    let before = export_favorites(&target, &target_paths, &Limits::default()).unwrap();
    apply_favorites(
        &target,
        &target_paths,
        &plan(&source, before.revision),
        &Limits::default(),
    )
    .unwrap();
    let files = target.get_favor("files").unwrap().file_paths;
    assert_eq!(
        Path::new(&files[0]).file_name().unwrap(),
        "报告 original.txt"
    );
    assert_eq!(
        Path::new(&files[1]).file_name().unwrap(),
        "same bytes other name.txt"
    );
    assert_ne!(files[0], files[1]);
    let mut hostile: PortableFavorite =
        serde_json::from_value(source.entries["favor/files"].clone()).unwrap();
    let identity = hostile.resources.values().next().unwrap().clone();
    let bad_names = vec![
        "../../escape.txt".to_owned(),
        "C:/elsewhere/CON.txt".into(),
        "C:/elsewhere/ads.txt:stream".into(),
        "C:/elsewhere/trailing. ".into(),
        format!("C:/elsewhere/{}.txt", "x".repeat(500)),
    ];
    hostile.record["filePaths"] = json!(bad_names);
    hostile.resources = bad_names
        .iter()
        .map(|name| (name.clone(), identity.clone()))
        .collect();
    let mut incoming = source.clone();
    incoming
        .entries
        .insert("favor/files".into(), serde_json::to_value(hostile).unwrap());
    let (_c, safe_paths, safe_store) = fixture(false);
    let before = export_favorites(&safe_store, &safe_paths, &Limits::default()).unwrap();
    apply_favorites(
        &safe_store,
        &safe_paths,
        &plan(&incoming, before.revision),
        &Limits::default(),
    )
    .unwrap();
    let owned = safe_paths
        .data_root
        .join("WebDavSync")
        .join("attachments")
        .join(&identity.sha256);
    for path in safe_store.get_favor("files").unwrap().file_paths {
        let path = Path::new(&path);
        assert_eq!(path.parent(), Some(owned.as_path()));
        assert!(path.is_file());
        assert_eq!(fs::read(path).unwrap(), b"shared bytes");
        let name = path.file_name().unwrap().to_string_lossy();
        assert!(!name.contains(':'));
        assert!(!name.ends_with([' ', '.']));
        assert!(!name.eq_ignore_ascii_case("CON.txt"));
        assert!(name.len() <= 240);
    }
    assert!(!safe_paths.data_root.join("escape.txt").exists());
}

#[test]
fn portable_preflight_checks_resources_and_graph_without_opening_source_paths() {
    let bytes = b"original bytes without any on-disk source".to_vec();
    let hash = digest(&bytes);
    let reference = "C:/cliphost-validation-no-such-source/image.png";
    let mut image = favorite("image", "preflight");
    image.kind = crate::model::ClipKind::ImageClipItem;
    image.image_path = Some(reference.into());
    let portable = PortableFavorite {
        format: 1,
        record: serde_json::to_value(image).unwrap(),
        document: None,
        resources: BTreeMap::from([(
            reference.into(),
            Resource {
                sha256: hash.clone(),
                byte_len: bytes.len() as u64,
                extension: "png".into(),
            },
        )]),
    };
    let entries = BTreeMap::from([(
        "favor/image".into(),
        serde_json::to_value(&portable).unwrap(),
    )]);
    let blobs = BTreeMap::from([(hash.clone(), bytes)]);
    validate_portable(&entries, &[], &blobs, &Limits::default()).unwrap();
    let mut missing = entries.clone();
    missing.get_mut("favor/image").unwrap()["resources"] = json!({});
    assert!(validate_portable(&missing, &[], &blobs, &Limits::default()).is_err());
    let mut orphan = entries.clone();
    orphan.get_mut("favor/image").unwrap()["record"]["parentId"] = json!("missing-owner");
    assert!(validate_portable(&orphan, &[], &blobs, &Limits::default())
        .unwrap_err()
        .contains("owner"));
    let mut corrupt = blobs.clone();
    corrupt.get_mut(&hash).unwrap().push(0);
    assert!(
        validate_portable(&entries, &[], &corrupt, &Limits::default())
            .unwrap_err()
            .contains("SHA-256")
    );
    let mut length = entries;
    length.get_mut("favor/image").unwrap()["resources"][reference]["byte_len"] = json!(0);
    assert!(validate_portable(&length, &[], &blobs, &Limits::default())
        .unwrap_err()
        .contains("length"));
}

#[test]
fn portable_diff_apply_preserves_untouched_token_and_original_file_then_detects_new_bytes() {
    let (_dir, paths, store) = fixture(true);
    let path = paths.data_root.join("user-owned original.txt");
    fs::write(&path, b"first source bytes").unwrap();
    let mut file = favorite("file", "file");
    file.kind = crate::model::ClipKind::FileClipItem;
    file.file_paths = vec![path.to_string_lossy().into()];
    file.parent_id = Some("root".into());
    let mut other = favorite("other", "other");
    other.parent_id = Some("root".into());
    store
        .replace_favors(&[folder("root", &["file", "other"]), file, other])
        .unwrap();
    config(&paths, &["root"]);
    let original = export_favorites(&store, &paths, &Limits::default()).unwrap();
    let token = store.get_favor("file").unwrap().storage_revision;
    let other_token = store.get_favor("other").unwrap().storage_revision;
    apply_favorites(
        &store,
        &paths,
        &plan(&original, original.revision.clone()),
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(store.get_favor("file").unwrap().storage_revision, token);
    assert_eq!(
        store.get_favor("file").unwrap().file_paths,
        vec![path.to_string_lossy().into_owned()]
    );
    let current = export_favorites(&store, &paths, &Limits::default()).unwrap();
    assert!(current.issues.is_empty());
    assert_eq!(current.entries, original.entries);
    let mut incoming = current.clone();
    incoming.entries.get_mut("favor/other").unwrap()["record"]["name"] =
        json!("remote changed another item");
    apply_favorites(
        &store,
        &paths,
        &plan(&incoming, current.revision),
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(store.get_favor("file").unwrap().storage_revision, token);
    assert_eq!(
        store.get_favor("file").unwrap().file_paths,
        vec![path.to_string_lossy().into_owned()]
    );
    assert_ne!(
        store.get_favor("other").unwrap().storage_revision,
        other_token
    );
    fs::write(&path, b"second source bytes").unwrap();
    let changed = export_favorites(&store, &paths, &Limits::default()).unwrap();
    assert!(changed.issues.is_empty(), "{:?}", changed.issues);
    assert_ne!(
        changed.entries["favor/file"],
        original.entries["favor/file"]
    );
    let portable: PortableFavorite =
        serde_json::from_value(changed.entries["favor/file"].clone()).unwrap();
    assert_eq!(
        portable.resources[&path.to_string_lossy().into_owned()].sha256,
        digest(b"second source bytes")
    );
    apply_favorites(
        &store,
        &paths,
        &plan(&changed, changed.revision.clone()),
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(
        store.get_favor("file").unwrap().file_paths,
        vec![path.to_string_lossy().into_owned()]
    );
    fs::write(&path, b"third source bytes").unwrap();
    let again = export_favorites(&store, &paths, &Limits::default()).unwrap();
    assert!(again.issues.is_empty());
    assert_ne!(again.entries["favor/file"], changed.entries["favor/file"]);
}
