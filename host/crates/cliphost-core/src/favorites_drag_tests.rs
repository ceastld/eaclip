use super::*;

fn fixture() -> (tempfile::TempDir, HostState) {
    crate::clipboard::force_headless(true);
    let dir = tempfile::tempdir().unwrap();
    let host = HostState::open(
        AppPaths::from_data_root(dir.path().join("BetterClipTauri")),
        28965,
    )
    .unwrap();
    (dir, host)
}

fn folder(host: &HostState, title: &str, parent: Option<&str>) -> String {
    host.favor_folder_create(&json!({"name":title,"parentId":parent}))
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .into()
}

fn text(host: &HostState, title: &str, parent: &str) -> String {
    host.favor_add_text_items(&json!({"text":title,"parentId":parent}))
        .unwrap()["createdIds"][0]
        .as_str()
        .unwrap()
        .into()
}

fn request(
    host: &HostState,
    source: Option<&str>,
    ids: &[&str],
    target: Option<&str>,
    anchor: Option<&str>,
    placement: &str,
    copy: bool,
) -> Value {
    json!({"sourceParentId":source,"itemIds":ids,"targetParentId":target,"anchorId":anchor,
        "placement":placement,"copy":copy,"expectedRevision":host.favorite_drag_revision().unwrap(),
        "operationId":Uuid::new_v4().simple().to_string()})
}

fn item(host: &HostState, id: &str) -> FavorRecord {
    host.store().lock().get_favor(id).unwrap()
}
fn children(host: &HostState, id: &str) -> Vec<String> {
    item(host, id).children
}
fn set_children(host: &HostState, id: &str, children: &[&str]) {
    let mut row = item(host, id);
    row.children = children.iter().map(|id| (*id).into()).collect();
    host.store().lock().save_favor(&row).unwrap();
}

#[test]
fn favorite_drag_batch_order_revision_idempotency_and_undo() {
    let (_dir, host) = fixture();
    let home = folder(&host, "Home", None);
    let a = text(&host, "a", &home);
    let b = text(&host, "b", &home);
    let c = text(&host, "c", &home);
    let d = text(&host, "d", &home);
    let before = host.favor_tree().unwrap();
    assert_eq!(
        before["dragRevision"],
        host.favorite_drag_revision().unwrap()
    );
    let time = item(&host, &b).update_time;
    let params = request(
        &host,
        Some(&home),
        &[&d, &b],
        Some(&home),
        Some(&c),
        "after",
        false,
    );
    let result = host.favorite_drag_apply(&params).unwrap();
    assert_eq!(children(&host, &home), [a.clone(), c, b.clone(), d.clone()]);
    assert_eq!(result["selectedIds"], json!([b, d]));
    assert_eq!(item(&host, &b).update_time, time);
    assert_eq!(result["revision"], host.favorite_drag_revision().unwrap());
    assert_eq!(host.favorite_drag_apply(&params).unwrap(), result);
    let mut reused = params.clone();
    reused["copy"] = json!(true);
    assert!(
        host.favorite_drag_apply(&reused)
            .unwrap_err()
            .contains("编号")
    );
    let undo = json!({"undoId":result["undoId"]});
    let undone = host.favorite_drag_undo(&undo).unwrap();
    assert_eq!(
        host.favor_tree().unwrap()["childrenById"],
        before["childrenById"]
    );
    assert_eq!(host.favorite_drag_undo(&undo).unwrap(), undone);
}

#[test]
fn favorite_drag_shared_edge_moves_only_its_source_and_preserves_owner() {
    let (_dir, host) = fixture();
    let owner = folder(&host, "Owner", None);
    let source = folder(&host, "Source", None);
    let target = folder(&host, "Target", None);
    let shared = text(&host, "shared", &owner);
    set_children(&host, &source, &[&shared]);
    host.favorite_drag_apply(&request(
        &host,
        Some(&source),
        &[&shared],
        Some(&target),
        None,
        "into",
        false,
    ))
    .unwrap();
    assert!(children(&host, &source).is_empty());
    assert_eq!(children(&host, &owner), [shared.clone()]);
    assert_eq!(children(&host, &target), [shared.clone()]);
    assert_eq!(
        item(&host, &shared).parent_id.as_deref(),
        Some(owner.as_str())
    );
}

#[test]
fn favorite_drag_parent_only_edges_prevent_cycles_and_are_copied() {
    let (_dir, host) = fixture();
    let root = folder(&host, "Root", None);
    let child = folder(&host, "Child", Some(&root));
    let leaf = text(&host, "leaf", &child);
    set_children(&host, &root, &[]);
    let target = folder(&host, "Target", None);
    for copy in [false, true] {
        assert!(
            host.favorite_drag_apply(&request(
                &host,
                None,
                &[&root],
                Some(&child),
                None,
                "into",
                copy
            ))
            .unwrap_err()
            .contains("自身或子文件夹")
        );
    }
    let copied = host
        .favorite_drag_apply(&request(
            &host,
            None,
            &[&root],
            Some(&target),
            None,
            "into",
            true,
        ))
        .unwrap();
    let copy_root = copied["selectedIds"][0].as_str().unwrap();
    let copy_child = children(&host, copy_root)[0].clone();
    let copy_leaf = children(&host, &copy_child)[0].clone();
    assert_ne!(copy_child, child);
    assert_ne!(copy_leaf, leaf);
    assert_eq!(item(&host, &copy_leaf).body_text.as_deref(), Some("leaf"));
}

#[test]
fn favorite_drag_copy_isolated_subgraph_retains_internal_sharing_and_assets() {
    let (dir, host) = fixture();
    let root = folder(&host, "Root", None);
    let left = folder(&host, "Left", Some(&root));
    let right = folder(&host, "Right", Some(&root));
    let source_image = dir.path().join("source.png");
    std::fs::write(&source_image, b"exact-image-fixture").unwrap();
    let mut image = FavorRecord::from_clip(&ClipRecord::new_text("image", ""), Some(left.clone()));
    image.kind = ClipKind::ImageClipItem;
    image.image_path = Some(source_image.to_string_lossy().into_owned());
    host.store().lock().insert_favor_checked(&image).unwrap();
    set_children(&host, &right, &[&image.id]);
    let target = folder(&host, "Target", None);
    let result = host
        .favorite_drag_apply(&request(
            &host,
            None,
            &[&root],
            Some(&target),
            None,
            "into",
            true,
        ))
        .unwrap();
    let copy_root = result["selectedIds"][0].as_str().unwrap();
    let copy_dirs = children(&host, copy_root);
    let copy_image_id = children(&host, &copy_dirs[0])[0].clone();
    assert_eq!(children(&host, &copy_dirs[1]), [copy_image_id.clone()]);
    let path = item(&host, &copy_image_id).image_path.unwrap();
    assert_ne!(path, source_image.to_string_lossy());
    std::fs::remove_file(source_image).unwrap();
    assert_eq!(std::fs::read(path).unwrap(), b"exact-image-fixture");
    assert_eq!(children(&host, &left), [image.id.clone()]);
}

#[test]
fn favorite_drag_batch_failure_rolls_back_every_relationship_and_receipt() {
    let (_dir, host) = fixture();
    let source = folder(&host, "Source", None);
    let target = folder(&host, "Target", None);
    let a = text(&host, "a", &source);
    let b = text(&host, "b", &source);
    let before = host.favor_tree().unwrap();
    let params = request(
        &host,
        Some(&source),
        &[&a, &b],
        Some(&target),
        None,
        "into",
        false,
    );
    let sql = rusqlite::Connection::open(host.inner.lock().paths.db.clone()).unwrap();
    sql.execute_batch("CREATE TRIGGER drag_reject BEFORE UPDATE ON favor_item WHEN OLD.body_text='b' BEGIN SELECT RAISE(ABORT,'reject batch'); END;").unwrap();
    host.drain_push_events();
    assert!(
        host.favorite_drag_apply(&params)
            .unwrap_err()
            .contains("reject batch")
    );
    assert_eq!(host.favor_tree().unwrap(), before);
    assert!(
        !host
            .drain_push_events()
            .iter()
            .any(|(name, _)| name == "favoritesChanged")
    );
    sql.execute_batch("DROP TRIGGER drag_reject;").unwrap();
    host.favorite_drag_apply(&params).unwrap();
    assert!(children(&host, &source).is_empty());
    assert_eq!(children(&host, &target), [a, b]);
}

#[test]
fn favorite_drag_conflicts_do_not_overwrite_new_edits_or_undo_them() {
    let (_dir, host) = fixture();
    let root = folder(&host, "Root", None);
    let a = text(&host, "a", &root);
    let b = text(&host, "b", &root);
    let stale = request(
        &host,
        Some(&root),
        &[&a],
        Some(&root),
        Some(&b),
        "after",
        false,
    );
    host.favor_update(&json!({"id":b,"title":"new edit"}))
        .unwrap();
    assert!(
        host.favorite_drag_apply(&stale)
            .unwrap_err()
            .starts_with("WRITE_CONFLICT:")
    );
    let result = host
        .favorite_drag_apply(&request(
            &host,
            Some(&root),
            &[&a],
            Some(&root),
            Some(&b),
            "after",
            false,
        ))
        .unwrap();
    host.favor_update(&json!({"id":b,"title":"later edit"}))
        .unwrap();
    assert!(
        host.favorite_drag_undo(&json!({"undoId":result["undoId"]}))
            .unwrap_err()
            .starts_with("WRITE_CONFLICT:")
    );
    assert_eq!(item(&host, &b).name, "later edit");
}

#[test]
fn favorite_drag_root_move_reorder_and_undo_persist_legacy_config() {
    let (_dir, host) = fixture();
    let home = folder(&host, "Home", None);
    let nested = folder(&host, "Nested", Some(&home));
    let other = folder(&host, "Other", None);
    let result = host
        .favorite_drag_apply(&request(
            &host,
            Some(&home),
            &[&nested],
            None,
            Some(&other),
            "before",
            false,
        ))
        .unwrap();
    let tree = host.favor_tree().unwrap();
    let roots = tree["orderedRootItems"].as_array().unwrap();
    assert_eq!(
        roots.iter().position(|id| id == &nested).unwrap() + 1,
        roots.iter().position(|id| id == &other).unwrap()
    );
    assert_eq!(item(&host, &nested).parent_id, None);
    assert!(!has_pending_recovery(&host.inner.lock().paths));
    host.favorite_drag_undo(&json!({"undoId":result["undoId"]}))
        .unwrap();
    assert_eq!(
        item(&host, &nested).parent_id.as_deref(),
        Some(home.as_str())
    );
    assert_eq!(children(&host, &home), [nested]);
}

#[test]
fn favorite_drag_noop_anchor_keeps_root_membership_and_undo() {
    let (_dir, host) = fixture();
    let root = folder(&host, "Root", None);
    let other = folder(&host, "Other", None);
    let paths = host.inner.lock().paths.clone();
    let mut config: crate::extra_settings::FavorSettings =
        crate::config_compat::load(&paths.configs.join("FavorConfig.json")).unwrap();
    config.root_children = vec![other.clone(), root.clone(), "stale".into(), other.clone()];
    crate::config_file::save(&paths.configs.join("FavorConfig.json"), &config).unwrap();
    let before = host.favor_tree().unwrap();
    let result = host
        .favorite_drag_apply(&request(
            &host,
            None,
            &[&root],
            None,
            Some(&root),
            "before",
            false,
        ))
        .unwrap();
    assert_eq!(host.favor_tree().unwrap(), before);
    assert!(result.get("undoId").is_none());
    assert!(
        host.favor_tree().unwrap()["orderedRootItems"]
            .as_array()
            .unwrap()
            .contains(&json!(other))
    );
}

#[test]
fn favorite_drag_copy_before_or_after_itself_creates_adjacent_independent_item() {
    let (_dir, host) = fixture();
    let root = folder(&host, "Root", None);
    let a = text(&host, "a", &root);
    let b = text(&host, "b", &root);
    let first = host
        .favorite_drag_apply(&request(
            &host,
            Some(&root),
            &[&a],
            Some(&root),
            Some(&a),
            "before",
            true,
        ))
        .unwrap();
    let first_id = first["selectedIds"][0].as_str().unwrap().to_owned();
    assert_eq!(
        children(&host, &root),
        [first_id.clone(), a.clone(), b.clone()]
    );
    let second = host
        .favorite_drag_apply(&request(
            &host,
            Some(&root),
            &[&a],
            Some(&root),
            Some(&a),
            "after",
            true,
        ))
        .unwrap();
    let second_id = second["selectedIds"][0].as_str().unwrap().to_owned();
    assert_ne!(second_id, a);
    assert_eq!(
        children(&host, &root),
        [first_id, a.clone(), second_id.clone(), b]
    );
    host.favor_update(&json!({"id":second_id,"bodyText":"edited copy"}))
        .unwrap();
    assert_eq!(item(&host, &a).body_text.as_deref(), Some("a"));
}

#[test]
fn favorite_drag_history_preserves_original_html_and_history() {
    let (_dir, host) = fixture();
    let root = folder(&host, "Root", None);
    let mut clip = ClipRecord::new_text("history", "browser.exe");
    clip.kind = ClipKind::HtmlClipItem;
    clip.html_data = Some(crate::cf_html::build_clipboard_html(
        "<b>history</b>",
        Some("https://example.test/"),
    ));
    host.store().lock().insert_clip(&clip, true).unwrap();
    let mut params = request(&host, None, &[&clip.id], Some(&root), None, "into", false);
    params["sourceKind"] = json!("history");
    let result = host.favorite_drag_apply(&params).unwrap();
    let copied = item(&host, result["selectedIds"][0].as_str().unwrap());
    assert_ne!(copied.id, clip.id);
    assert_eq!(copied.html_data, clip.html_data);
    assert!(host.store().lock().get_clip(&clip.id).is_ok());
    host.favorite_drag_undo(&json!({"undoId":result["undoId"]}))
        .unwrap();
    assert!(host.store().lock().get_clip(&clip.id).is_ok());
    assert!(children(&host, &root).is_empty());
}

#[test]
fn favorite_drag_recovery_chooses_committed_receipt_and_preserves_preferences() {
    let (_dir, host) = fixture();
    let a = folder(&host, "A", None);
    let b = folder(&host, "B", None);
    let paths = host.inner.lock().paths.clone();
    let mut favor: crate::extra_settings::FavorSettings =
        crate::config_compat::load(&paths.configs.join("FavorConfig.json")).unwrap();
    let before = vec![a.clone(), b.clone()];
    let after = vec![b.clone(), a.clone()];
    favor.root_children = after.clone();
    favor.font_size = 19.0;
    crate::config_file::save(&paths.configs.join("FavorConfig.json"), &favor).unwrap();
    let journal = RootJournal {
        operation_id: "aborted".into(),
        before: before.clone(),
        after: after.clone(),
    };
    crate::config_file::save(&paths.data_root.join(JOURNAL_FILE), &journal).unwrap();
    recover_pending(&host.store().lock(), &paths, &mut favor).unwrap();
    assert_eq!(favor.root_children, before);
    assert_eq!(favor.font_size, 19.0);
    let mut metadata = DragState::default();
    remember(&mut metadata, "committed", "hash", json!({"ok":true}));
    save_state(&host.store().lock(), &metadata).unwrap();
    crate::config_file::save(
        &paths.data_root.join(JOURNAL_FILE),
        &RootJournal {
            operation_id: "committed".into(),
            before,
            after: after.clone(),
        },
    )
    .unwrap();
    recover_pending(&host.store().lock(), &paths, &mut favor).unwrap();
    assert_eq!(favor.root_children, after);
    assert_eq!(favor.font_size, 19.0);
}

#[test]
fn favorite_drag_copied_cycle_and_missing_member_fail_without_changes() {
    let (_dir, host) = fixture();
    let root = folder(&host, "Root", None);
    let child = folder(&host, "Child", Some(&root));
    let target = folder(&host, "Target", None);
    set_children(&host, &child, &[&root]);
    let before = host.favorite_drag_revision().unwrap();
    assert!(
        host.favorite_drag_apply(&request(
            &host,
            Some(&root),
            &[&child],
            Some(&target),
            None,
            "into",
            true
        ))
        .unwrap_err()
        .contains("循环")
    );
    assert_eq!(host.favorite_drag_revision().unwrap(), before);
    set_children(&host, &child, &["missing"]);
    let before = host.favorite_drag_revision().unwrap();
    assert!(
        host.favorite_drag_apply(&request(
            &host,
            Some(&root),
            &[&child],
            Some(&target),
            None,
            "into",
            true
        ))
        .unwrap_err()
        .contains("缺失")
    );
    assert_eq!(host.favorite_drag_revision().unwrap(), before);
}

#[test]
fn favorite_drag_failed_commit_restores_root_file_and_all_database_rows() {
    let (_dir, host) = fixture();
    let home = folder(&host, "Home", None);
    let nested = folder(&host, "Nested", Some(&home));
    let other = folder(&host, "Other", None);
    let before = host.favor_tree().unwrap();
    let params = request(
        &host,
        Some(&home),
        &[&nested],
        None,
        Some(&other),
        "before",
        false,
    );
    let store = host.store();
    store.lock().connection().execute_batch(
        "PRAGMA foreign_keys=ON;
         CREATE TABLE drag_parent(id INTEGER PRIMARY KEY);
         CREATE TABLE drag_deferred(id INTEGER REFERENCES drag_parent(id) DEFERRABLE INITIALLY DEFERRED);
         CREATE TRIGGER drag_fail_commit AFTER UPDATE ON favor_item BEGIN INSERT INTO drag_deferred VALUES(17); END;"
    ).unwrap();
    assert!(
        host.favorite_drag_apply(&params)
            .unwrap_err()
            .contains("FOREIGN KEY")
    );
    assert_eq!(host.favor_tree().unwrap(), before);
    assert_eq!(item(&host, &nested).parent_id, Some(home));
    assert!(!has_pending_recovery(&host.inner.lock().paths));
}

#[test]
fn favorite_drag_missing_image_rejects_entire_copy_and_removes_prepared_assets() {
    let (dir, host) = fixture();
    let root = folder(&host, "Root", None);
    let target = folder(&host, "Target", None);
    let a = text(&host, "a", &root);
    let b = text(&host, "b", &root);
    let source = dir.path().join("image.png");
    std::fs::write(&source, b"image").unwrap();
    for (id, path) in [(&a, source), (&b, dir.path().join("missing.png"))] {
        let mut row = item(&host, id);
        row.kind = ClipKind::ImageClipItem;
        row.image_path = Some(path.to_string_lossy().into_owned());
        host.store().lock().save_favor(&row).unwrap();
    }
    let before = host.favor_tree().unwrap();
    assert!(
        host.favorite_drag_apply(&request(
            &host,
            Some(&root),
            &[&a, &b],
            Some(&target),
            None,
            "into",
            true
        ))
        .is_err()
    );
    assert_eq!(host.favor_tree().unwrap(), before);
    assert!(
        !std::fs::read_dir(host.inner.lock().paths.image_library.clone())
            .unwrap()
            .any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("drag-copy-"))
    );
}

#[test]
fn favorite_drag_document_store_copy_preserves_extensions_and_conflicts() {
    crate::clipboard::force_headless(true);
    let dir = tempfile::tempdir().unwrap();
    let paths = AppPaths::from_data_root(dir.path().join("BetterClipTauri"));
    paths.ensure().unwrap();
    let sql = rusqlite::Connection::open(&paths.db).unwrap();
    sql.execute_batch(
        "CREATE TABLE clip_item (Id TEXT PRIMARY KEY, Data TEXT NOT NULL, ObjectType TEXT, CreatedTime TEXT, UpdateTime TEXT, RowToken TEXT);
         CREATE TABLE favor_item (Id TEXT PRIMARY KEY, Data TEXT NOT NULL, ObjectType TEXT, CreatedTime TEXT, UpdateTime TEXT, RowToken TEXT);
         CREATE TABLE UsageInfo (Id TEXT PRIMARY KEY, Data TEXT NOT NULL, ObjectType TEXT, CreatedTime TEXT, UpdateTime TEXT);"
    ).unwrap();
    let host = HostState::open(paths, 28965).unwrap();
    let root = folder(&host, "Root", None);
    let target = folder(&host, "Target", None);
    let a = text(&host, "a", &root);
    sql.execute("UPDATE favor_item SET Data=json_set(Data,'$.FutureSetting',json('{\"keep\":true}'),'$.Item.FuturePayload','preserved'),RowToken='external' WHERE Id=?1", [&a]).unwrap();
    let params = request(&host, Some(&root), &[&a], Some(&target), None, "into", true);
    let result = host.favorite_drag_apply(&params).unwrap();
    assert_eq!(result["revision"], host.favorite_drag_revision().unwrap());
    let new_id = result["selectedIds"][0].as_str().unwrap();
    let encoded: String = sql
        .query_row("SELECT Data FROM favor_item WHERE Id=?1", [new_id], |row| {
            row.get(0)
        })
        .unwrap();
    let document: Value = serde_json::from_str(&encoded).unwrap();
    assert_eq!(document["FutureSetting"], json!({"keep":true}));
    assert_eq!(document["Item"]["FuturePayload"], "preserved");
    assert_eq!(document["ParentId"], target);
    let stale = request(
        &host,
        Some(&root),
        &[&a],
        Some(&target),
        None,
        "into",
        false,
    );
    sql.execute("UPDATE favor_item SET Data=json_set(Data,'$.FutureSetting.keep',0),RowToken='next' WHERE Id=?1", [&a]).unwrap();
    assert!(
        host.favorite_drag_apply(&stale)
            .unwrap_err()
            .contains("WRITE_CONFLICT")
    );
}

/// Timing probe for the drop → write → reload path. Point
/// `CLIPHOST_FAVORITE_BENCH_ROOT` at a *copy* of a data root; synthetic
/// folders are used otherwise. Run with `--ignored --nocapture`.
#[test]
#[ignore]
fn favorite_drag_refresh_timing_probe() {
    crate::clipboard::force_headless(true);
    let (_dir, host) = match std::env::var_os("CLIPHOST_FAVORITE_BENCH_ROOT") {
        Some(root) => (
            tempfile::tempdir().unwrap(),
            HostState::open(AppPaths::from_data_root(root.into()), 28965).unwrap(),
        ),
        None => {
            let (dir, host) = fixture();
            for f in 0..20 {
                let parent = folder(&host, &format!("F{f}"), None);
                for i in 0..50 {
                    text(&host, &format!("item {f}-{i}"), &parent);
                }
            }
            (dir, host)
        }
    };
    let tree = host.favor_tree().unwrap();
    let count = tree["items"].as_object().map_or(0, |items| items.len());
    let (parent, children) = tree["childrenById"]
        .as_object()
        .unwrap()
        .iter()
        .filter_map(|(id, ids)| Some((id.clone(), ids.as_array()?.clone())))
        .filter(|(_, ids)| ids.len() >= 2)
        .max_by_key(|(_, ids)| ids.len())
        .expect("a folder with two children");
    let first = children[0].as_str().unwrap().to_string();
    let last = children[children.len() - 1].as_str().unwrap().to_string();
    let time = |label: &str, run: &mut dyn FnMut()| {
        let samples: Vec<_> = (0..5)
            .map(|_| {
                let start = std::time::Instant::now();
                run();
                start.elapsed().as_secs_f64() * 1000.0
            })
            .collect();
        let mut sorted = samples.clone();
        sorted.sort_by(f64::total_cmp);
        println!("{label}: median {:.1} ms, max {:.1} ms", sorted[2], sorted[4]);
    };
    println!("favorites: {count} items, folder children {}", children.len());
    time("favor_tree", &mut || {
        host.favor_tree().unwrap();
    });
    time("favor_tree json bytes", &mut || {
        let bytes = serde_json::to_vec(&host.favor_tree().unwrap()).unwrap().len();
        assert!(bytes > 0);
    });
    time("store list_favors", &mut || {
        host.store().lock().list_favors().unwrap();
    });
    let items = host.store().lock().list_favors().unwrap();
    time("snapshot_revision only", &mut || {
        snapshot_revision(&items, &[]).unwrap();
    });
    time("children counts only", &mut || {
        let total: usize = items.iter().map(|item| items.iter().filter(|other| other.parent_id.as_deref() == Some(item.id.as_str())).count()).sum();
        assert!(total < usize::MAX);
    });
    time("favorite_drag_revision", &mut || {
        host.favorite_drag_revision().unwrap();
    });
    let mut flip = false;
    time("favorite_drag_apply", &mut || {
        let (moved, anchor) = if flip { (&first, &last) } else { (&last, &first) };
        let placement = if flip { "after" } else { "before" };
        flip = !flip;
        let params = request(&host, Some(&parent), &[moved], Some(&parent), Some(anchor), placement, false);
        host.favorite_drag_apply(&params).unwrap();
    });
}
