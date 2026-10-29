//! End-to-end core storage acceptance through the actual HostState services.
//! All roots are explicit repository .tmp copies; no production root discovery.
use super::*;
use rusqlite::{params, Connection};
use std::collections::BTreeMap;

const STAMP: &str = "2026-10-17T03:12:34.123456789Z";

fn test_parent() -> PathBuf {
    let parent = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .canonicalize()
        .unwrap()
        .join(".tmp/storage-core-host-tests");
    std::fs::create_dir_all(&parent).unwrap();
    parent
}

fn insert_document(connection: &Connection, table: &str, id: &str, value: Value) {
    let raw = serde_json::to_string(&value)
        .unwrap()
        .replace(
            "\"RAW_BIG_NUMBER\"",
            "1234567890123456789012345678901234567890",
        )
        .replace("\"RAW_LEXICAL_NUMBER\"", "1.23400e+30");
    connection.execute(
        &format!("INSERT INTO {table}(Id,Data,ObjectType,CreatedTime,UpdateTime,RowToken) VALUES(?1,?2,'fixture',?3,?3,?4)"),
        params![id, raw, STAMP, format!("old-{id}")],
    ).unwrap();
}

fn document(id: &str, title: Value, payload: Value, parent: Option<&str>) -> Value {
    json!({"Id":id,"Title":title,"Icon":null,"Description":"来源说明",
        "SourceProcessName":"fixture.exe","CreateTime":STAMP,"UpdateTime":STAMP,
        "SubIndex":0,"IsTop":false,"ParentId":parent,"Children":[],
        "SourceClipId":null,"Item":payload,"FutureNumber":"RAW_BIG_NUMBER",
        "FutureLexical":"RAW_LEXICAL_NUMBER"})
}

pub(super) fn seed_documents(root: &Path) {
    std::fs::create_dir_all(root.join("Configs")).unwrap();
    std::fs::create_dir_all(root.join("Image/Temp")).unwrap();
    image::RgbaImage::from_pixel(2, 2, image::Rgba([30, 100, 170, 255]))
        .save(root.join("Image/Temp/shared.png"))
        .unwrap();
    std::fs::write(
        root.join("Configs/FavorConfig.json"),
        br#"{"RootChildren":["target","home"],"FuturePreference":{"keep":true}}"#,
    )
    .unwrap();
    std::fs::write(
        root.join("Configs/ClipConfig.json"),
        br#"{"MaxItemSize":10000}"#,
    )
    .unwrap();
    let sql = Connection::open(root.join("userdata.db")).unwrap();
    sql.execute_batch("CREATE TABLE clip_item(Id TEXT PRIMARY KEY,Data TEXT NOT NULL,ObjectType TEXT,CreatedTime TEXT,UpdateTime TEXT,RowToken TEXT);
        CREATE TABLE favor_item(Id TEXT PRIMARY KEY,Data TEXT NOT NULL,ObjectType TEXT,CreatedTime TEXT,UpdateTime TEXT,RowToken TEXT);
        CREATE TABLE UsageInfo(Id TEXT PRIMARY KEY,Data TEXT NOT NULL,ObjectType TEXT,CreatedTime TEXT,UpdateTime TEXT,RowToken TEXT);
        CREATE TABLE cliphost_kv(key TEXT PRIMARY KEY,value TEXT NOT NULL);").unwrap();
    sql.execute(
        "INSERT INTO cliphost_kv(key,value) VALUES(?1,?2)",
        params![
            "opaque-fixture",
            r#"{"future":123456789012345678901234567890}"#
        ],
    )
    .unwrap();
    insert_document(
        &sql,
        "clip_item",
        "same",
        document(
            "same",
            Value::Null,
            json!({"ObjType":1,"Text":"原文\r\n🙂","FuturePayload":"RAW_BIG_NUMBER"}),
            None,
        ),
    );
    let image_payload = json!({"ObjType":3,"RSource":{"IsInImageDir":true,"RPath":"Temp/shared.png"},
        "ImageHash":"legacy-original-hash","ImageMD5":"different-original-md5","PerceptualHash":"010101"});
    insert_document(
        &sql,
        "clip_item",
        "history-image",
        document("history-image", Value::Null, image_payload.clone(), None),
    );
    let mut folder = document("home", json!("Home"), json!({"ObjType":100}), None);
    folder["Children"] = json!(["same", "favorite-image", "rich", "link"]);
    insert_document(&sql, "favor_item", "home", folder);
    insert_document(
        &sql,
        "favor_item",
        "target",
        document("target", json!("Target"), json!({"ObjType":100}), None),
    );
    let mut favorite = document(
        "same",
        json!("收藏标题"),
        json!({"ObjType":1,"Text":"收藏正文","FuturePayload":"RAW_BIG_NUMBER"}),
        Some("home"),
    );
    favorite["SourceClipId"] = json!("same");
    insert_document(&sql, "favor_item", "same", favorite);
    let mut picture = document("favorite-image", Value::Null, image_payload, Some("home"));
    picture["SourceClipId"] = json!("history-image");
    insert_document(&sql, "favor_item", "favorite-image", picture);
    let html = crate::cf_html::build_clipboard_html(
        "<b>原始富文本 🙂</b>\r\n",
        Some("https://example.test/source"),
    );
    insert_document(
        &sql,
        "favor_item",
        "rich",
        document(
            "rich",
            Value::Null,
            json!({"ObjType":10,"Text":"原始富文本 🙂","HtmlData":html,"HtmlSourceUrl":"https://example.test/source","FuturePayload":"RAW_BIG_NUMBER"}),
            Some("home"),
        ),
    );
    insert_document(
        &sql,
        "favor_item",
        "link",
        document(
            "link",
            Value::Null,
            json!({"ObjType":20,"Url":"https://example.test/","Title":"来源网页标题","Icon":"字符:原","FuturePayload":"RAW_BIG_NUMBER"}),
            Some("home"),
        ),
    );
    for (id, count, time) in [
        ("clip_item_same", 3, STAMP),
        ("favor_item_same", 7, STAMP),
        ("clip_item_orphan", 0, "0001-01-01T00:00:00"),
        ("future-key", 0, "0001-01-01T00:00:00"),
    ] {
        insert_document(
            &sql,
            "UsageInfo",
            id,
            json!({"Id":id,"UsageCount":count,"LastUseTime":time}),
        );
    }
}

pub(super) fn tree_hashes(root: &Path) -> BTreeMap<PathBuf, String> {
    fn visit(root: &Path, path: &Path, hashes: &mut BTreeMap<PathBuf, String>) {
        let metadata = std::fs::symlink_metadata(path).unwrap();
        assert!(!metadata.file_type().is_symlink());
        if metadata.is_dir() {
            for entry in std::fs::read_dir(path).unwrap() {
                visit(root, &entry.unwrap().path(), hashes);
            }
        } else {
            hashes.insert(
                path.strip_prefix(root).unwrap().to_owned(),
                crate::model::sha256_hex(&std::fs::read(path).unwrap()),
            );
        }
    }
    let mut hashes = BTreeMap::new();
    visit(root, root, &mut hashes);
    hashes
}

fn fixture() -> (tempfile::TempDir, HostState) {
    crate::clipboard::force_headless(true);
    let dir = tempfile::tempdir_in(test_parent()).unwrap();
    let source = dir.path().join("source");
    let candidate = dir.path().join("candidate");
    seed_documents(&source);
    let before = tree_hashes(&source);
    crate::storage_core::migrate::build_candidate(
        &crate::storage_v2::MigrationOptions {
            source_root: source.clone(),
            candidate_root: candidate.clone(),
        },
        &mut |_| Ok(()),
    )
    .unwrap();
    assert_eq!(before, tree_hashes(&source));
    assert!(
        AppStore::open(&candidate.join("userdata.db")).is_err(),
        "normal startup must refuse an isolated candidate"
    );
    let host = HostState::open_isolated_core(AppPaths::from_data_root(candidate), 29065).unwrap();
    (dir, host)
}

fn sql(host: &HostState) -> Connection {
    Connection::open(host.inner.lock().paths.db.clone()).unwrap()
}

fn preserved_columns(
    connection: &Connection,
    id: &str,
) -> (
    String,
    String,
    String,
    Option<String>,
    String,
    String,
    String,
) {
    connection.query_row("SELECT payload_json,source_extensions_json,source_process,source_history_id,created_at,description,coalesce(icon_override,'<NULL>') FROM items WHERE scope='favorite' AND id=?1", [id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?))).unwrap()
}

#[test]
fn core_host_metadata_edit_keeps_source_tokens_and_usage_does_not_conflict() {
    let (_dir, host) = fixture();
    let connection = sql(&host);
    let before = preserved_columns(&connection, "rich");
    assert!(before
        .1
        .contains("1234567890123456789012345678901234567890"));
    assert!(before.1.contains("1.23400e+30"));
    let old = host.favor_get(&json!({"id":"rich"})).unwrap();
    assert!(old["revision"].as_str().unwrap().starts_with("core2:"));
    host.store()
        .lock()
        .update_usage(&crate::store::favor_usage_key("rich"))
        .unwrap();
    let edited = host
        .favor_update(&json!({"id":"rich","title":"只改标题","expectedRevision":old["revision"]}))
        .unwrap();
    assert_ne!(edited["revision"], old["revision"]);
    assert_eq!(before, preserved_columns(&connection, "rich"));
    assert!(host
        .favor_update(&json!({"id":"rich","title":"旧窗口","expectedRevision":old["revision"]}))
        .unwrap_err()
        .contains("WRITE_CONFLICT"));
    assert!(host
        .favor_update(&json!({"id":"rich","title":"没有修订"}))
        .unwrap_err()
        .contains("WRITE_CONFLICT"));
    assert_eq!(
        host.store()
            .lock()
            .get_usage(&crate::store::favor_usage_key("rich"))
            .unwrap()
            .usage_count,
        1
    );
    assert_eq!(
        host.store()
            .lock()
            .get_clip("same")
            .unwrap()
            .create_time
            .timestamp_subsec_nanos(),
        123456789
    );
}

#[test]
fn core_host_title_intent_distinguishes_override_and_fallback() {
    let (_dir, host) = fixture();
    let connection = sql(&host);
    let before = preserved_columns(&connection, "link");
    let detail = host.favor_get(&json!({"id":"link"})).unwrap();
    assert_eq!(detail["title"], "来源网页标题");
    let edited = host
        .favor_update(
            &json!({"id":"link","title":"来源网页标题","expectedRevision":detail["revision"]}),
        )
        .unwrap();
    let title: Option<String> = connection
        .query_row(
            "SELECT title FROM items WHERE scope='favorite' AND id='link'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(title.as_deref(), Some("来源网页标题"));
    let cleared = host
        .favor_update(&json!({"id":"link","title":null,"expectedRevision":edited["revision"]}))
        .unwrap();
    let title: Option<String> = connection
        .query_row(
            "SELECT title FROM items WHERE scope='favorite' AND id='link'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(title.is_none());
    assert_eq!(cleared["title"], "来源网页标题");
    assert_eq!(before, preserved_columns(&connection, "link"));
}

#[test]
fn core_host_clear_history_keeps_favorites_images_and_restarts() {
    let (_dir, host) = fixture();
    let connection = sql(&host);
    let before = preserved_columns(&connection, "favorite-image");
    let tree_before = host.favor_tree().unwrap();
    let bytes = host.image_bytes("history-image").unwrap().0;
    assert_eq!(bytes, host.favor_image_bytes("favorite-image").unwrap().0);
    host.clear_all(false).unwrap();
    host.inner.lock().pending_clear.as_mut().unwrap().expires_at = Instant::now();
    host.commit_pending_clear();
    assert_eq!(host.store().lock().counts().unwrap(), (0, 0));
    assert_eq!(before, preserved_columns(&connection, "favorite-image"));
    assert_eq!(bytes, host.favor_image_bytes("favorite-image").unwrap().0);
    assert_eq!(
        tree_before["childrenById"],
        host.favor_tree().unwrap()["childrenById"]
    );
    assert_eq!(
        host.store()
            .lock()
            .get_favor("same")
            .unwrap()
            .source_clip_id
            .as_deref(),
        Some("same")
    );
    let paths = host.inner.lock().paths.clone();
    drop(connection);
    drop(host);
    let reopened = HostState::open_isolated_core(paths, 29065).unwrap();
    assert_eq!(reopened.store().lock().counts().unwrap(), (0, 0));
    assert_eq!(
        bytes,
        reopened.favor_image_bytes("favorite-image").unwrap().0
    );
    assert_eq!(
        tree_before["childrenById"],
        reopened.favor_tree().unwrap()["childrenById"]
    );
}

fn drag(host: &HostState, source: &str, id: &str, target: &str, copy: bool) -> Value {
    json!({"sourceKind":"favorite","sourceParentId":source,"itemIds":[id],"targetParentId":target,
        "placement":"into","copy":copy,"expectedRevision":host.favorite_drag_revision().unwrap(),
        "operationId":Uuid::new_v4().simple().to_string()})
}

#[test]
fn core_host_favorite_move_copy_retry_undo_preserve_complete_content() {
    let (_dir, host) = fixture();
    let connection = sql(&host);
    let before = preserved_columns(&connection, "rich");
    let params = drag(&host, "home", "rich", "target", false);
    let moved = host.favorite_drag_apply(&params).unwrap();
    assert_eq!(host.favorite_drag_apply(&params).unwrap(), moved);
    assert_eq!(before, preserved_columns(&connection, "rich"));
    let undo = json!({"undoId":moved["undoId"]});
    let undone = host.favorite_drag_undo(&undo).unwrap();
    assert_eq!(host.favorite_drag_undo(&undo).unwrap(), undone);
    assert_eq!(before, preserved_columns(&connection, "rich"));
    let copied = host
        .favorite_drag_apply(&drag(&host, "home", "rich", "target", true))
        .unwrap();
    let id = copied["selectedIds"][0].as_str().unwrap();
    let copy = preserved_columns(&connection, id);
    assert_eq!(copy.0, before.0);
    assert_eq!(copy.1, before.1);
    assert_eq!(copy.2, before.2);
    assert_eq!(copy.5, before.5);
    host.favorite_drag_undo(&json!({"undoId":copied["undoId"]}))
        .unwrap();
    assert!(host.store().lock().get_favor(id).is_err());
    assert_eq!(before, preserved_columns(&connection, "rich"));
}

#[test]
fn core_host_copy_image_and_delete_favorite_tree_keep_other_references() {
    let (_dir, host) = fixture();
    let original = host.favor_image_bytes("favorite-image").unwrap().0;
    let copied = host
        .favorite_drag_apply(&drag(&host, "home", "favorite-image", "target", true))
        .unwrap();
    let copy_id = copied["selectedIds"][0].as_str().unwrap();
    assert_eq!(host.favor_image_bytes(copy_id).unwrap().0, original);
    let connection = sql(&host);
    let payload = |id: &str| -> Value {
        let raw: String = connection
            .query_row(
                "SELECT payload_json FROM items WHERE scope='favorite' AND id=?1",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        serde_json::from_str(&raw).unwrap()
    };
    let source_payload = payload("favorite-image");
    let copied_payload = payload(copy_id);
    assert_eq!(
        source_payload["sourceContentHash"],
        copied_payload["sourceContentHash"]
    );
    assert_eq!(
        source_payload["perceptualHash"],
        copied_payload["perceptualHash"]
    );
    let folder = host
        .favor_folder_create(&json!({"name":"deletable","parentId":"home"}))
        .unwrap();
    let folder_id = folder["id"].as_str().unwrap();
    host.favorite_drag_apply(&drag(&host, "home", "favorite-image", folder_id, false))
        .unwrap();
    host.favor_delete(&json!({"id":folder_id})).unwrap();
    assert_eq!(host.favor_image_bytes(copy_id).unwrap().0, original);
    assert_eq!(host.image_bytes("history-image").unwrap().0, original);
    assert!(host.store().lock().get_favor("favorite-image").is_err());
    assert!(host.store().lock().get_favor(copy_id).is_ok());
}

#[test]
fn core_host_saved_undo_survives_candidate_relocation() {
    let (dir, host) = fixture();
    let before = host.favor_tree().unwrap();
    let request = drag(&host, "home", "favorite-image", "target", false);
    let moved = host.favorite_drag_apply(&request).unwrap();
    let revision = host.favorite_drag_revision().unwrap();
    let source = host.inner.lock().paths.data_root.clone();
    drop(host);
    fn copy_tree(source: &Path, target: &Path) {
        std::fs::create_dir(target).unwrap();
        for entry in std::fs::read_dir(source).unwrap() {
            let entry = entry.unwrap();
            let meta = entry.file_type().unwrap();
            assert!(!meta.is_symlink());
            if meta.is_dir() {
                copy_tree(&entry.path(), &target.join(entry.file_name()));
            } else {
                std::fs::copy(entry.path(), target.join(entry.file_name())).unwrap();
            }
        }
    }
    let original_hashes = tree_hashes(&source);
    let relocated = dir.path().join("relocated");
    copy_tree(&source, &relocated);
    let reopened =
        HostState::open_isolated_core(AppPaths::from_data_root(relocated), 29065).unwrap();
    assert_eq!(reopened.favorite_drag_revision().unwrap(), revision);
    reopened
        .favorite_drag_undo(&json!({"undoId":moved["undoId"]}))
        .unwrap();
    assert_eq!(
        before["childrenById"],
        reopened.favor_tree().unwrap()["childrenById"]
    );
    assert!(!reopened
        .favor_image_bytes("favorite-image")
        .unwrap()
        .0
        .is_empty());
    assert_eq!(original_hashes, tree_hashes(&source));
}

#[test]
fn core_host_isolation_disables_outbox_and_maps_generic_image_preview() {
    let (_dir, host) = fixture();
    let paths = host.inner.lock().paths.clone();
    let source = paths
        .data_root
        .parent()
        .unwrap()
        .join("source/Image/Temp/shared.png");
    let expected = host.image_bytes("history-image").unwrap().0;
    assert_eq!(
        host.local_image_bytes(&source.to_string_lossy()).unwrap().0,
        expected
    );
    assert!(host
        .local_image_bytes(r"Q:\unmapped-core-fixture\image.png")
        .is_none());
    drop(host);
    let settings = paths.configs.join("LanShare/settings.json");
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    std::fs::write(
        &settings,
        br#"{"enabled":true,"inboxFolder":"Q:\\unmapped-core-fixture"}"#,
    )
    .unwrap();
    let state = paths.configs.join("ClipboardSync/state.json");
    std::fs::create_dir_all(state.parent().unwrap()).unwrap();
    std::fs::write(&state, br#"{"enabled":true,"localSequence":87}"#).unwrap();
    let before = std::fs::read(&settings).unwrap();
    let sync_before = std::fs::read(&state).unwrap();
    let mut spoofed = paths;
    spoofed.configs = source.parent().unwrap().join("unselected-configs");
    let host = HostState::open_isolated_core(spoofed, 29065).unwrap();
    assert!(!host.lan.lock().enabled);
    assert!(!host.clipboard_sync.raw_enabled.load(Ordering::Relaxed));
    assert_eq!(host.clipboard_sync.state.lock().durable.local_sequence, 87);
    assert!(host.lan_outbox.send("must-not-be-sent".into()).is_err());
    assert_eq!(std::fs::read(settings).unwrap(), before);
    assert_eq!(std::fs::read(state).unwrap(), sync_before);
}

#[test]
fn core_host_isolation_rejects_external_commands_before_any_data_change() {
    let (_dir, host) = fixture();
    host.set_shell_open_runner(|_| panic!("isolated command reached an external launcher"));
    let paths = host.inner.lock().paths.clone();
    let (launch, admin, win_v) = {
        let inner = host.inner.lock();
        assert!(inner.monitor.is_none());
        assert!(inner.explorer_monitor.is_none());
        assert!(inner.data_location.require_explicit_root);
        assert_eq!(inner.data_location.default_root.as_ref(), Some(&paths.data_root));
        assert!(inner.data_location.program_directory.is_none());
        (inner.extras.global.launch_at_startup, inner.extras.global.run_as_administrator,
            inner.config.use_win_v_shortcut)
    };
    assert!(host.hotkey_listener.lock().is_none());
    let before = tree_hashes(&paths.data_root);
    let counts = host.store().lock().counts().unwrap();
    let sync_before = host.sync_status();
    assert_eq!(
        host.favor_clipboard_options_with_snapshot(|| {
            panic!("isolated paste-options menu reached the system clipboard reader")
        }),
        json!({"options":[]}),
        "the core gate must return before even invoking the clipboard reader",
    );
    let results = [
        ("change data location", host.settings_form_update(&json!({"configId":"dataLocation","values":{"dataFolder":"Q:/unselected-fixture"}}))),
        ("change startup integration", host.settings_form_update(&json!({"configId":"global","values":{"LaunchAtStartup":!launch}}))),
        ("change elevation integration", host.settings_form_update(&json!({"configId":"homeGlobal","values":{"RunAsAdministrator":!admin}}))),
        ("change WinV through trigger settings", host.settings_form_update(&json!({"configId":"trigger","values":{"UseWinVShortcut":!win_v}}))),
        ("open candidate path externally", host.shell_open_path(&json!({"path":paths.image_library.join("Temp/shared.png")}))),
        ("open external URI", host.shell_open_uri(&json!({"uri":"https://example.invalid/isolated-fixture"}))),
        ("reveal candidate path externally", host.shell_reveal(&json!({"path":paths.image_library.join("Temp/shared.png")}))),
        ("change LAN configuration", host.lan_config_update(&json!({"enabled":true}))),
        ("pair LAN", host.lan_pair(&json!({}))),
        ("send LAN content", host.lan_send(&json!({}))),
        ("send LAN items", host.lan_send_clips(&json!({"ids":["same"]}))),
        ("retry LAN delivery", host.lan_retry(&json!({}))),
        ("forget LAN identity", host.lan_forget(&json!({}))),
        ("prepare LAN input", host.lan_prepare_inbound("fixture", &json!({}))),
        ("change sync configuration", host.sync_config_update(&json!({"enabled":false}))),
        ("pair sync client", host.sync_pair_client(&json!({}))),
        ("create sync invitation", host.sync_invitation()),
        ("accept sync content", host.sync_receive_accept("fixture", None).map(|v| json!(v))),
        ("apply sync content", host.sync_receive_apply("fixture").map(|v| json!(v))),
        ("capture clipboard", host.handle_clipboard()),
        ("collect clipboard favorite", host.favor_from_clipboard(&json!({}))),
        ("pick external files", host.pick_files(&json!({}))),
        ("start external OLE", host.begin_ole_drag(&json!({"ids":["same"]}))),
        ("run external Quicker action", host.run_quicker_action("fixture", "").map(|_| Value::Null)),
        ("start shared RPC", host.start_verb_rpc().map(|_| Value::Null)),
    ];
    for (operation, result) in results {
        match result {
            Err(error) => assert!(error.starts_with("CORE_ISOLATED_SIDE_EFFECT:"), "{operation}: wrong rejection: {error}"),
            Ok(value) => panic!("{operation}: unexpectedly allowed: {value}"),
        }
    }
    for role in ["file-recycle", "file-move-to", "file-open-in-process", "window-kill", "window-close-class", "future-unsafe-role"] {
        let node = crate::menu_api::command("fixture", role, json!({}));
        match host.menu_invoke_node("fixture", "fixture", node, 0, 0, None) {
            Err(error) => assert!(error.starts_with("CORE_ISOLATED_SIDE_EFFECT:"), "menu role {role}: {error}"),
            Ok(value) => panic!("menu role {role}: unexpectedly allowed: {value}"),
        }
    }
    assert_eq!(host.restart(false)["restart"], false, "restart must be rejected");
    assert!(host.restart(false)["error"].as_str().unwrap().starts_with("CORE_ISOLATED_SIDE_EFFECT:"), "restart rejection reason");
    assert_eq!(host.lan_refresh_code()["ok"], false, "LAN pairing refresh must be rejected");
    assert!(host.sync_forget_client("fixture").unwrap_err().starts_with("CORE_ISOLATED_SIDE_EFFECT:"), "forget sync client rejection reason");
    assert!(host.lan_upload_inbound("fixture", "fixture", "fixture", b"", 0)
        .unwrap_err().starts_with("CORE_ISOLATED_SIDE_EFFECT:"), "LAN inbound upload rejection reason");
    assert_eq!(host.sync_status(), sync_before);
    assert_eq!(host.store().lock().counts().unwrap(), counts);
    assert_eq!(tree_hashes(&paths.data_root), before);
    assert!(host.data_paths()["legacyDataRoot"].is_null());
    assert_eq!(host.data_paths()["sidecarDirectory"], json!(paths.data_root));
    // The guard does not block candidate business editing, image preview or exit.
    assert!(!host.image_bytes("history-image").unwrap().0.is_empty());
    host.favor_folder_create(&json!({"name":"isolated local edit"})).unwrap();
    host.execute_verb("clip show").unwrap();
    host.enqueue_chrome(ChromeCommand::Exit);
}

#[test]
fn ordinary_host_paste_options_invoke_lazy_reader_and_preserve_shape() {
    crate::clipboard::force_headless(true);
    let dir = tempfile::Builder::new()
        .prefix("legacy-paste-options-")
        .tempdir_in(test_parent())
        .unwrap();
    let host = HostState::open(
        AppPaths::from_data_root(dir.path().join("legacy")), 29065,
    ).unwrap();
    let reads = std::cell::Cell::new(0);
    let options = host.favor_clipboard_options_with_snapshot(|| {
        reads.set(reads.get() + 1);
        Some(ClipboardSnapshot {
            quicker_action: Some(r#"{"Title":"Synthetic Action"}"#.into()),
            files: vec!["Q:/synthetic-a.txt".into(), "Q:/synthetic-b.txt".into()],
            text: Some("synthetic".into()),
            ..ClipboardSnapshot::default()
        })
    });
    assert_eq!(reads.get(), 1);
    assert_eq!(options, json!({"options":[
        {"key":"action","label":"粘贴动作: Synthetic Action"},
        {"key":"files","label":"粘贴 2 个文件"},
        {"key":"text","label":"粘贴文本: synthetic"},
    ]}));
}

#[test]
fn core_host_list_ui_config_ignores_system_fields_without_changing_authority() {
    let (_dir, host) = fixture();
    let (paths, launch, admin, win_v, global_before) = {
        let inner = host.inner.lock();
        (inner.paths.clone(), inner.extras.global.launch_at_startup,
            inner.config.run_as_administrator, inner.config.use_win_v_shortcut,
            serde_json::to_value(&inner.extras.global).unwrap())
    };
    let mut before = tree_hashes(&paths.data_root);
    before.remove(Path::new("Configs/ClipConfig.json"));
    // This legacy list/UI endpoint accepts a DTO subset and ignores system
    // settings. Its normal candidate-local ClipConfig persistence is allowed.
    assert_eq!(host.config_update(&json!({"useWinVShortcut":!win_v,
        "runAsAdministrator":!admin,"LaunchAtStartup":!launch})).unwrap(), json!({"ok":true}));
    {
        let inner = host.inner.lock();
        assert_eq!(inner.config.use_win_v_shortcut, win_v);
        assert_eq!(inner.config.run_as_administrator, admin);
        assert_eq!(serde_json::to_value(&inner.extras.global).unwrap(), global_before);
    }
    let mut after = tree_hashes(&paths.data_root);
    after.remove(Path::new("Configs/ClipConfig.json"));
    assert_eq!(after, before, "ignored system fields may only rewrite the candidate-local UI config");
}

#[test]
fn core_copy_preflight_rejects_all_unsafe_attribute_classes() {
    for bit in [0x400, 0x1000, 0x40000, 0x400000] {
        assert!(validate_core_copy_attributes(bit, false).unwrap_err().starts_with("CORE_COPY_UNSAFE_PATH:"));
        assert!(validate_core_copy_attributes(bit | 0x10 | 0x20, false).is_err());
    }
    assert!(validate_core_copy_attributes(0, true).is_err());
    assert!(validate_core_copy_attributes(0x10 | 0x20 | 0x80, false).is_ok());
}

#[test]
fn core_copy_preflight_is_repeatable_and_confined_to_existing_tmp_copies() {
    let (_dir, host) = fixture();
    let paths = host.inner.lock().paths.clone();
    let before = tree_hashes(&paths.data_root);
    validate_isolated_core_copy_tree(&paths).unwrap();
    validate_isolated_core_copy_tree(&paths).unwrap();
    assert_eq!(tree_hashes(&paths.data_root), before);
    // String construction retains the actual spelling at the API boundary;
    // PathBuf::join may already normalize a verbatim parent's '..' on Windows.
    for suffix in ["/../traversal", r"\..\traversal", r"/child\../traversal", r"\child/..\traversal"] {
        let raw = PathBuf::from(format!("{}{suffix}", test_parent().display()));
        let error = validate_isolated_core_copy_tree(&AppPaths::from_data_root(raw))
            .err().expect("raw traversal must be rejected");
        assert!(error.starts_with("CORE_COPY_UNSAFE_PATH:"), "raw suffix {suffix}: {error}");
    }
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(3).unwrap();
    assert!(validate_isolated_core_copy_tree(&AppPaths::from_data_root(workspace.join(".tmp")))
        .unwrap_err().starts_with("CORE_COPY_UNSAFE_PATH:"));
    assert!(validate_isolated_core_copy_tree(&AppPaths::from_data_root(workspace.join("unselected")))
        .unwrap_err().starts_with("CORE_COPY_UNSAFE_PATH:"));
}

#[cfg(windows)]
#[test]
fn core_copy_preflight_rejects_internal_and_ancestor_junction_without_touching_target() {
    crate::clipboard::force_headless(true);
    let dir = tempfile::tempdir_in(test_parent()).unwrap();
    let outside = dir.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("sentinel"), b"untouched outside candidate").unwrap();
    std::fs::create_dir(outside.join("candidate")).unwrap();
    let outside_before = tree_hashes(&outside);
    for ancestor_link in [false, true] {
        let root = dir.path().join(if ancestor_link { "ancestor-case" } else { "internal-case" });
        std::fs::create_dir(&root).unwrap();
        let (link, candidate) = if ancestor_link {
            let link = root.join("redirect");
            (link.clone(), link.join("candidate"))
        } else {
            (root.join("Configs"), root.clone())
        };
        let output = std::process::Command::new("cmd")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&link).arg(&outside).output().unwrap();
        assert!(output.status.success(), "test fixture junction creation failed");
        let paths = AppPaths::from_data_root(candidate.clone());
        assert!(validate_isolated_core_copy_tree(&paths).unwrap_err()
            .starts_with("CORE_COPY_UNSAFE_PATH:"));
        #[cfg(feature = "native-acceptance")]
        assert!(crate::native_acceptance::validate_core_copy_tree(&paths).unwrap_err()
            .starts_with("CORE_COPY_UNSAFE_PATH:"));
        assert!(HostState::open_isolated_core(paths, 29065).err().unwrap()
            .starts_with("CORE_COPY_UNSAFE_PATH:"));
        assert_eq!(tree_hashes(&outside), outside_before);
        if !ancestor_link { assert!(!candidate.join("userdata.db").exists()); }
        std::fs::remove_dir(&link).unwrap();
    }
    assert_eq!(tree_hashes(&outside), outside_before);
}

#[cfg(feature = "native-acceptance")]
#[test]
fn core_host_native_acceptance_requires_context_before_opening_storage() {
    let dir = tempfile::tempdir_in(test_parent()).unwrap();
    let missing_root = dir.path().join("not-created");
    let result = HostState::open_isolated_core_acceptance(
        AppPaths::from_data_root(missing_root.clone()), 29065);
    assert!(result.err().unwrap().contains("Native acceptance was not initialized"));
    assert!(!missing_root.exists(), "context rejection must precede any storage initialization");
}

#[test]
fn ordinary_startup_rejects_core_before_filter_compatibility_can_write() {
    let (_dir, host) = fixture();
    let paths = host.inner.lock().paths.clone();
    drop(host);
    // This valid old configuration would normally receive a URL tab plus the
    // RustUrlFilterDefaultAdded marker from config_compat::load_filters.
    std::fs::write(
        paths.configs.join("ClipFilterConfig.json"),
        br#"{"FilterItems":[{"Id":"original-text","DisplayName":"original","Type":"text","ItemType":2,"StarType":0}],"FutureNumber":123456789012345678901234567890}"#,
    ).unwrap();
    let before = tree_hashes(&paths.data_root);
    let error = match HostState::open(paths.clone(), 29065) {
        Ok(_) => panic!("ordinary startup accepted an isolated core candidate"),
        Err(error) => error,
    };
    assert!(error.contains("STORAGE_V2_RUNTIME_NOT_READY"), "{error}");
    assert_eq!(tree_hashes(&paths.data_root), before);
}

/// Run only with an explicit already verified historical candidate COPY. This
/// test exercises mutations, so callers must clone the baseline beforehand.
#[test]
#[ignore = "requires an explicit repository .tmp historical candidate copy"]
fn core_historical_copy_host_services_and_restart() {
    crate::clipboard::force_headless(true);
    let root = PathBuf::from(
        std::env::var_os("CLIPHOST_CORE_REPLAY_ROOT").expect("explicit historical COPY root"),
    );
    let paths = AppPaths::from_data_root(root.clone());
    let host = HostState::open_isolated_core(paths.clone(), 29065).unwrap();
    let store = host.store();
    let favorites = store.lock().list_favors().unwrap();
    let history = store.lock().list_clips().unwrap();
    let before = host.favor_tree().unwrap();
    let before_count = favorites.len();
    let mut ready_images = 0usize;
    let mut unavailable_images = 0usize;
    for row in &favorites {
        let detail = host.favor_get(&json!({"id":row.id})).unwrap();
        assert!(detail["revision"].as_str().unwrap().starts_with("core2:"));
    }
    let images: Vec<Value> = serde_json::from_slice(
        &std::fs::read(root.join("runtime-image-expectations.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        images.len(),
        favorites
            .iter()
            .filter(|r| r.kind == ClipKind::ImageClipItem)
            .count()
            + history
                .iter()
                .filter(|r| r.kind == ClipKind::ImageClipItem)
                .count()
    );
    for expected in &images {
        let id = expected["id"].as_str().unwrap();
        let actual = match expected["scope"].as_str().unwrap() {
            "favorite" => host.favor_image_bytes(id),
            "history" => host.image_bytes(id),
            _ => panic!("invalid image oracle scope"),
        };
        if let Some(hash) = expected["sha256"].as_str() {
            assert_eq!(crate::model::sha256_hex(&actual.unwrap().0), hash);
            ready_images += 1;
        } else {
            assert!(
                actual.is_err(),
                "unmapped or missing images cannot be read through another root"
            );
            unavailable_images += 1;
        }
    }
    // Change only a title, then restore it through the same opaque CAS contract.
    {
        let row = favorites
            .iter()
            .find(|row| !row.is_folder)
            .expect("historical acceptance requires a favorite leaf");
        let connection = sql(&host);
        let preserved = preserved_columns(&connection, &row.id);
        let prior = host.favor_get(&json!({"id":row.id})).unwrap();
        let original_title: Option<String> = connection
            .query_row(
                "SELECT title FROM items WHERE scope='favorite' AND id=?1",
                [&row.id],
                |r| r.get(0),
            )
            .unwrap();
        let edited = host.favor_update(&json!({"id":row.id,"title":"isolated migration acceptance","expectedRevision":prior["revision"]})).unwrap();
        assert_eq!(preserved, preserved_columns(&connection, &row.id));
        host.favor_update(
            &json!({"id":row.id,"title":original_title,"expectedRevision":edited["revision"]}),
        )
        .unwrap();
        assert_eq!(preserved, preserved_columns(&connection, &row.id));
    }
    // Actual favorite creation and structure undo on the migrated runtime.
    let folder = host
        .favor_folder_create(&json!({"name":"isolated acceptance folder"}))
        .unwrap();
    let folder_id = folder["id"].as_str().unwrap();
    {
        let row = history
            .iter()
            .find(|r| r.kind == ClipKind::TextClipItem)
            .expect("historical acceptance requires a history text item");
        let request = json!({"sourceKind":"history","itemIds":[row.id],"targetParentId":folder_id,
            "placement":"into","copy":true,"expectedRevision":host.favorite_drag_revision().unwrap(),
            "operationId":Uuid::new_v4().simple().to_string()});
        let result = host.favorite_drag_apply(&request).unwrap();
        assert_eq!(host.favorite_drag_apply(&request).unwrap(), result);
        let undo = json!({"undoId":result["undoId"]});
        host.favorite_drag_undo(&undo).unwrap();
    }
    host.favor_delete(&json!({"id":folder_id})).unwrap();
    assert_eq!(store.lock().list_favors().unwrap().len(), before_count);
    assert_eq!(store.lock().list_clips().unwrap().len(), history.len());
    assert_eq!(
        before["childrenById"],
        host.favor_tree().unwrap()["childrenById"]
    );
    let count = store.lock().counts().unwrap();
    drop(store);
    drop(host);
    let reopened = HostState::open_isolated_core(paths, 29065).unwrap();
    assert_eq!(reopened.store().lock().counts().unwrap(), count);
    assert_eq!(
        reopened.store().lock().list_favors().unwrap().len(),
        before_count
    );
    let report = json!({"history":history.len(),"favorites":before_count,"imagesReadable":ready_images,
        "imagesUnavailable":unavailable_images,"contentEdit":true,"dragCopyUndo":true,"restart":true,
        "productionActivated":false});
    std::fs::write(
        root.join("host-acceptance.json"),
        serde_json::to_vec_pretty(&report).unwrap(),
    )
    .unwrap();
    println!("{}", report);
}
