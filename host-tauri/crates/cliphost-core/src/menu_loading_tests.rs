use super::*;

#[test]
fn independent_menu_roots_do_not_wait_for_the_favorites_database() {
    crate::clipboard::force_headless(true);
    let dir = tempfile::tempdir().unwrap();
    let host = HostState::open(AppPaths::from_data_root(dir.path().join("BetterClipTauri")), 28965).unwrap();
    host.text_action_upsert(&json!({"type":"Send", "title":"独立文本动作", "data":{"Text":"fixture"}})).unwrap();
    let store = host.store();
    let mut blocked = Vec::new();
    for action in ["text", "window", "clipboard"] {
        let database = store.lock();
        let worker_host = host.clone();
        let (send, receive) = mpsc::channel();
        let worker = thread::spawn(move || {
            let menu = worker_host.menu_create_session(action);
            let _ = send.send(());
            menu
        });
        let responsive = receive.recv_timeout(Duration::from_millis(500)).is_ok();
        // Even a failing regression must release and join its worker.
        drop(database);
        let menu = worker.join().unwrap();
        if action == "text" {
            assert!(menu["root"]["items"].as_array().unwrap().iter().any(|n| n["title"] == "独立文本动作"));
        }
        if action == "clipboard" { assert_eq!(menu["root"]["items"], json!([])); }
        assert!(host.menu_close_session(menu["sessionId"].as_str().unwrap()));
        if !responsive { blocked.push(action); }
    }
    assert!(blocked.is_empty(), "unrelated database blocked menus: {blocked:?}");
}

fn super_menu_fixture() -> (tempfile::TempDir, HostState) {
    crate::clipboard::force_headless(true);
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
    std::fs::create_dir_all(&root).unwrap();
    let dir = tempfile::Builder::new().prefix("super-menu-trigger-").tempdir_in(root).unwrap();
    let host = HostState::open(AppPaths::from_data_root(dir.path().join("data")), 28965).unwrap();
    host.set_window_runtime_ready(true);
    host.inner.lock().last_non_host_fg = 42;
    (dir, host)
}

#[test]
fn super_menu_trigger_returns_before_loading_and_keeps_the_captured_target() {
    let (_dir, host) = super_menu_fixture();
    let (started, ready) = mpsc::channel();
    let (release, wait) = mpsc::channel();
    let worker = host.request_super_menu_loading(Some(43), move |_, _| {
        started.send(()).unwrap();
        wait.recv_timeout(Duration::from_secs(5)).unwrap();
        crate::menu_api::MenuBuildContext::default()
    }).unwrap();
    let responsive = ready.recv_timeout(Duration::from_secs(2)).is_ok();
    host.inner.lock().last_non_host_fg = 44;
    release.send(()).unwrap();
    let created = worker.join().unwrap();
    assert!(responsive, "root loading must not block the hotkey or settings thread");
    let session = created["sessionId"].as_str().unwrap();
    assert_eq!(host.menu_session_target_handle(session), 43);
    assert_eq!(host.super_menu_state_detail()["visible"], true);
    assert_eq!(host.super_menu_state_detail()["sessionId"], session);
}

#[test]
fn super_menu_trigger_close_or_new_request_cancels_a_loading_root() {
    for replace in [false, true] {
        let (_dir, host) = super_menu_fixture();
        let (started, ready) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        let worker = host.request_super_menu_loading(None, move |_, _| {
            started.send(()).unwrap();
            wait.recv_timeout(Duration::from_secs(5)).unwrap();
            crate::menu_api::MenuBuildContext::default()
        }).unwrap();
        let started = ready.recv_timeout(Duration::from_secs(2)).is_ok();
        if replace {
            host.menu_create_session_loading("text", "", 0, 43, |_, _| Default::default());
        } else {
            assert!(host.try_dispatch_input_key_for_foreground(KeyEvent {
                vk: 0x1B, down: true, ctrl: false, alt: false, shift: false, win: false, now_ms: 1,
            }, 42), "plain Escape must cancel the pending root through the real input router");
        }
        let current = host.super_menu_state_detail();
        release.send(()).unwrap();
        let stale = worker.join().unwrap();
        assert!(started);
        assert_eq!(host.super_menu_state_detail()["sessionId"], current["sessionId"]);
        assert_eq!(host.super_menu_state_detail()["visible"], replace);
        assert_eq!(host.menu_session_target_handle(stale["sessionId"].as_str().unwrap()), 0);
    }
}

#[test]
fn super_menu_trigger_from_host_keeps_the_previous_external_target() {
    let (_dir, host) = super_menu_fixture();
    host.inner.lock().host_hwnd = 200;
    for foreground in [Some(200), None, Some(0)] {
        let created = host.request_super_menu_loading(foreground, |_, _| Default::default()).unwrap().join().unwrap();
        assert_eq!(host.menu_session_target_handle(created["sessionId"].as_str().unwrap()), 42);
    }
    host.inner.lock().last_non_host_fg = 0;
    let created = host.request_super_menu_loading(None, |_, _| Default::default()).unwrap().join().unwrap();
    assert_eq!(host.menu_session_target_handle(created["sessionId"].as_str().unwrap()), 0);
}

#[test]
fn super_menu_trigger_reopens_the_last_left_category() {
    let (_dir, host) = super_menu_fixture();
    assert_eq!(host.super_menu_reopen_action(), "super");
    let created = host.request_super_menu_loading(None, |_, _| Default::default()).unwrap().join().unwrap();
    assert_eq!(host.super_menu_reopen_action(), "favor");
    let session = created["sessionId"].as_str().unwrap();
    let generation = host.super_menu_state_detail()["generation"].as_i64().unwrap() as i32;
    host.menu_create_session_for_current(session, generation, "text", 0).unwrap();
    assert_eq!(host.super_menu_reopen_action(), "text");
    // The file menu's recent-folder entry is not a Hub category to return to.
    host.menu_create_session_loading("reopen", "", 0, 43, |_, _| Default::default());
    assert_eq!(host.super_menu_reopen_action(), "text");
}

#[test]
fn super_menu_category_switch_keeps_the_opening_anchor() {
    let (_dir, host) = super_menu_fixture();
    let created = host.request_super_menu_loading(None, |_, _| Default::default()).unwrap().join().unwrap();
    // The click on the strip must not become the new anchor.
    let opened = (12_345, 678);
    host.inner.lock().super_menu.captured_anchor = Some(opened);
    let before = host.super_menu_state_detail();
    let generation = before["generation"].as_i64().unwrap() as i32;
    host.menu_create_session_for_current(created["sessionId"].as_str().unwrap(), generation, "window", 0).unwrap();
    let after = host.super_menu_state_detail();
    assert_eq!((after["anchorScreenX"].as_i64(), after["anchorScreenY"].as_i64()), (Some(12_345), Some(678)));
    for key in ["overlayLeftPx", "overlayTopPx", "overlayWidthPx", "overlayHeightPx", "hostDpiScaleX"] {
        assert_eq!(after[key], before[key], "{key}");
    }
}

#[test]
fn bare_menu_verb_reopens_the_category_shared_with_the_hotkey() {
    let (dir, host) = super_menu_fixture();
    let first = host.execute_verb("menu").unwrap();
    assert_eq!(first["action"], "super");
    assert_eq!(host.super_menu_reopen_action(), "favor");
    host.super_menu_hide_session();

    for category in ["window", "file", "favor", "bookmarks", "text", "clipboard"] {
        let current = host.execute_verb("menu").unwrap();
        let session = current["sessionId"].as_str().unwrap();
        let generation = host.super_menu_state_detail()["generation"].as_i64().unwrap() as i32;
        host.menu_create_session_for_current(session, generation, category, 0).unwrap();
        host.super_menu_hide_session();
        assert_eq!(host.super_menu_reopen_action(), category);

        let reopened = host.execute_verb("menu").unwrap();
        assert_eq!(reopened["action"], category);
        assert_eq!(host.menu_session_target_handle(reopened["sessionId"].as_str().unwrap()), 42);
        host.super_menu_hide_session();
    }

    let with_options = host.execute_verb("menu --show").unwrap();
    assert_eq!(with_options["action"], "clipboard");
    let explicit = host.execute_verb("menu file").unwrap();
    assert_eq!(explicit["action"], "file");
    host.execute_verb("menu reopen --show").unwrap();
    assert_eq!(host.execute_verb("menu").unwrap()["action"], "file");

    drop(host);
    let restarted = HostState::open(AppPaths::from_data_root(dir.path().join("data")), 28965).unwrap();
    assert_eq!(restarted.execute_verb("menu").unwrap()["action"], "super");
}
