//! Model checks without starting native windows, hooks, or a pointer poll.
use super::*;

fn node(id: &str, path: &str) -> Value {
    json!({"id":id, "kind":"Folder", "role":"file", "title":id,
        "isEnabled":true, "hasChildren":true, "invokeMode":"InvokeAndExpand", "payload":{"path":path}})
}

fn fixture() -> (tempfile::TempDir, FileLine) {
    cliphost_core::force_headless(true);
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
    std::fs::create_dir_all(&root).unwrap();
    let dir = tempfile::Builder::new().prefix("file-line-interaction-").tempdir_in(root).unwrap();
    let host = Arc::new(HostState::open(cliphost_core::AppPaths::from_data_root(dir.path().join("data")), 29065).unwrap());
    let rect = cliphost_core::edge_hide::Rect { left:0.0, top:0.0, width:1200.0, height:700.0 };
    let (slots, add) = line_slots(&[node("one", "fixture-one"), node("two", "fixture-two")]);
    let line = FileLine {
        host, tokens:DesignTokens::from_json(crate::preview::TOKENS_JSON).unwrap(), dark:true,
        trigger:EdgeTrigger::default(), edge:Some(ScreenEdge { monitor:rect, work:rect, dpi_scale:1.0 }),
        session:None, root_task:None, action_task:None, hover_task:None, slots, add, sheets:Vec::new(),
        status:None, epoch:1, mouse_down:false, interaction:Interaction::default(),
        native_drop:native_drop::FileLineDrop::default(), target:0, suspended:false, shown_edge:None,
        line_window:None, thread_window:None, line_shown:false, thread_shown:false, _poll:Task::ready(()),
    };
    (dir, line)
}

#[test]
fn root_refresh_matches_paths_across_session_ids_but_keeps_window_identity() {
    let first = node("old-session-id", r"C:\Work");
    let mut next = node("new-session-id", r"c:\work");
    assert!(same_root(&first, &next), "menu session ids are regenerated during refresh");
    next["role"] = json!("opened-folder");
    assert!(!same_root(&first, &next), "the same path can be both custom and already open");
    let mut opened = next.clone();
    opened["payload"]["windowHandle"] = json!(101);
    next["payload"]["windowHandle"] = json!(202);
    assert!(!same_root(&opened, &next), "restoring a pinned open-folder tag must not retarget another Explorer window");
    next["payload"]["windowHandle"] = json!(101);
    assert!(same_root(&opened, &next));
}

#[test]
fn sheet_keyboard_focus_avoids_disabled_rows_and_handles_empty_folders() {
    let (_dir, mut line) = fixture();
    let mut level = Level::new(node("one", "fixture-one"));
    let mut disabled = node("unavailable", "gone");
    disabled["isEnabled"] = json!(false);
    level.items = vec![json!({"kind":"Separator","isEnabled":true}), disabled, node("child", "child")];
    level.loading = false;
    line.sheets.push(Sheet { tag:"one".into(), pinned:true, levels:vec![level], task:None });
    line.focus_sheet("one");
    assert_eq!(line.interaction.focus, Some(interaction::Focus::Sheet("one".into(), Some(2))));
    line.sheets[0].levels[0].items.clear();
    line.focus_sheet("one");
    assert_eq!(line.interaction.focus, Some(interaction::Focus::Sheet("one".into(), None)));
}

#[test]
fn folded_line_keeps_only_pinned_controls_clickable_without_covering_top_edge() {
    let (_dir, mut line) = fixture();
    let mut level = Level::new(node("one", "fixture-one"));
    level.items = vec![node("child", "child")];
    level.loading = false;
    line.sheets.push(Sheet { tag:"one".into(), pinned:true, levels:vec![level], task:None });
    line.host.file_menu_replace(vec!["removed".into()]).unwrap();
    let undo = line.host.file_line_take_down("removed").unwrap().unwrap();
    line.interaction.undo = Some((undo, Instant::now() + Duration::from_secs(6)));
    let regions = line.regions();
    let sheet = line.sheet_boxes()[0].1;
    let hit = |point| regions.iter().any(|rect| interaction::contains(*rect, point, 0.0));
    assert!(hit((sheet.left + 20.0, sheet.top + SHEET_HEADER + 4.0)), "pinned contents remain interactive");
    assert!(!hit((line.width() - 100.0, LINE_TOP + PEG_DROP + 10.0)), "folding releases the undo/button band too");
    assert!(!hit((line.width() - 20.0, 4.0)), "the hidden band must release input to the application below");
    assert!(!hit((line.width() / 2.0, line.height() - 20.0)));
}
