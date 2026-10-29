use super::*;

#[test]
fn quicker_drop_captures_destination_and_appends_independent_favorites() {
    crate::clipboard::force_headless(true);
    let dir = tempfile::tempdir().unwrap();
    let host = HostState::open(
        AppPaths::from_data_root(dir.path().join("BetterClipTauri")),
        28965,
    )
    .unwrap();
    let first = host.favor_folder_create(&json!({"name":"first"})).unwrap();
    let second = host.favor_folder_create(&json!({"name":"second"})).unwrap();
    let nested = host
        .favor_folder_create(&json!({"name":"nested", "parentId":first["id"]}))
        .unwrap();
    host.favor_add_text_items(&json!({"parentId":nested["id"], "text":"existing"}))
        .unwrap();
    host.panel_show(&json!({"favorId":first["id"], "folderId":nested["id"]}))
        .unwrap();
    let destination = host.panel_quicker_drop_parent();
    host.panel_show(&json!({"favorId":second["id"]})).unwrap();
    let payload =
        crate::quicker_wire::decode(include_bytes!("../tests/fixtures/quicker-drag-v1-wpf.bin"))
            .unwrap();
    let one = host
        .panel_drop_quicker_action(&payload, destination.as_deref())
        .unwrap();
    let two = host
        .panel_drop_quicker_action(&payload, destination.as_deref())
        .unwrap();
    let store = host.inner.lock().store.clone();
    let db = store.lock();
    let folder = db.get_favor(nested["id"].as_str().unwrap()).unwrap();
    assert_eq!(&folder.children[1..], &[one.clone(), two]);
    assert!(db
        .get_favor(second["id"].as_str().unwrap())
        .unwrap()
        .children
        .is_empty());
    let item = db.get_favor(&one).unwrap();
    assert_eq!(item.kind, ClipKind::ActionClipItem);
    assert_eq!(item.name, "Rust 动作 🦀");
    assert_eq!(item.source_clip_id, None);
    drop(db);
    assert!(std::iter::from_fn(|| host.take_chrome_command())
        .any(|command| matches!(command, ChromeCommand::PanelRefresh)));
}

#[test]
fn quicker_drop_rejects_invalid_payload_and_disappeared_destination() {
    crate::clipboard::force_headless(true);
    let dir = tempfile::tempdir().unwrap();
    let host = HostState::open(
        AppPaths::from_data_root(dir.path().join("BetterClipTauri")),
        28965,
    )
    .unwrap();
    host.favor_folder_create(&json!({"name":"protected home"})).unwrap();
    let folder = host.favor_folder_create(&json!({"name":"gone"})).unwrap();
    let payload = crate::quicker_wire::decode(include_bytes!(
        "../tests/fixtures/quicker-action-v1-wpf.bin"
    ))
    .unwrap();
    host.favor_delete(&json!({"id":folder["id"]})).unwrap();
    let store = host.inner.lock().store.clone();
    let before = store.lock().list_favors().unwrap().len();
    assert!(host
        .panel_drop_quicker_action(&payload, folder["id"].as_str())
        .is_err());
    assert!(host
        .panel_drop_quicker_action("{\"Title\":\"missing ID\"}", None)
        .is_err());
    assert!(host
        .panel_drop_quicker_action(&payload, Some(crate::panel::ROOT_FOLDER_ID))
        .is_err());
    assert_eq!(store.lock().list_favors().unwrap().len(), before);
}
