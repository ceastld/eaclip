use super::*;
use std::sync::atomic::AtomicUsize;
use tokio::io::AsyncWriteExt;

struct Peer {
    mode: AtomicUsize,
    calls: Mutex<Vec<String>>,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    cancelled: tokio::sync::Notify,
}

async fn fixture() -> (tempfile::TempDir, HostState, Arc<Peer>, tokio::task::JoinHandle<()>, tokio::task::JoinHandle<Result<(), String>>) {
    crate::clipboard::force_headless(true);
    let dir = tempfile::tempdir().unwrap();
    let host = HostState::open(AppPaths::from_data_root(dir.path().join("BetterClipTauri")), 28965).unwrap();
    host.favor_folder_create(&json!({"name":"Protected home"})).unwrap();
    let peer = Arc::new(Peer { mode: AtomicUsize::new(0), calls: Mutex::new(Vec::new()),
        entered: tokio::sync::Notify::new(), release: tokio::sync::Notify::new(),
        cancelled: tokio::sync::Notify::new() });
    let (server, mut client) = tokio::io::duplex(65536);
    let serving = tokio::spawn(crate::quicker_rpc::serve(server, host.quicker_rpc(), "fixture",
        Arc::new(crate::pipe_rpc::StopSignal::default()), Arc::new(|_| None)));
    let control = peer.clone();
    let responding = tokio::spawn(async move {
        while let Ok(Some(request)) = crate::quicker_rpc::read_message(&mut client).await {
            if request.type_name == "cancel" { control.cancelled.notify_one(); continue; }
            if request.type_name != "request" { continue; }
            let method = request.method.as_deref().unwrap();
            let mut reply = request.clone();
            reply.type_name = "response".into();
            reply.method = None;
            if method == "quicker.ping" { reply.payload = Some(json!("pong")); }
            else {
                control.calls.lock().push(method.into());
                let mode = control.mode.load(Ordering::SeqCst);
                if mode == 3 { control.entered.notify_one(); control.release.notified().await; }
                // Withhold the response while still reading the caller's cancellation.
                if mode == 4 { continue; }
                if mode == 2 { reply = crate::pipe_rpc::PipeRpcMessage::error(request.id, "fixture", "browser failed"); }
                else { reply.payload = Some(match method {
                    "quicker.evaluateExpression" => json!("true"),
                    "quicker.getCurrentBrowserTab" if mode == 1 => Value::Null,
                    "quicker.getCurrentBrowserTab" => json!({"Title":" 页面中文 ", "Url":" https://example.test/?x=1&y=中文 ", "Icon":" icon-data "}),
                    "quicker.getAllBrowserTabs" if mode == 1 => json!([]),
                    "quicker.getAllBrowserTabs" => json!((0..12).map(|i| json!({"Title":format!("Tab {i}"),"Url":format!("https://example.test/{i}"),"Icon":"tab-icon"})).chain([json!({"Url":" "})]).collect::<Vec<_>>()),
                    "quicker.getBrowserBookmarksTree" if mode == 1 => json!(r#"[{"id":"0","children":[]}]"#),
                    "quicker.getBrowserBookmarksTree" => json!(r#"[{"id":"0","children":[{"id":"7","title":"插件书签","url":"https://example.test/bookmark"}]}]"#),
                    _ => panic!("unexpected callback: {method}"),
                }); }
            }
            if client.write_all(&crate::pipe_rpc::encode_frame(&reply).unwrap()).await.is_err() { break; }
        }
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while host.quicker_rpc.select().is_none() { tokio::time::sleep(Duration::from_millis(5)).await; }
    }).await.unwrap();
    (dir, host, peer, responding, serving)
}

fn tabs_menu(host: &HostState, parent: &str) -> (String, String) {
    {
        let mut inner = host.inner.lock();
        inner.extras.favor_menu.default_favorite1 = parent.into();
        inner.extras.menu_global.show_adder_menu_item = true;
        inner.extras.menu_global.quick_load_submenu = false;
        inner.extras.menu_global.max_single_load_count = 5;
    }
    let created = host.menu_create_session("favor");
    let session = created["sessionId"].as_str().unwrap().to_string();
    let adder = created["root"]["items"].as_array().unwrap().iter().find(|n| n["role"] == "favor-add").unwrap();
    assert_eq!(adder["payload"]["parentId"], parent);
    let children = host.menu_nodes(&session, adder["id"].as_str().unwrap()).unwrap();
    let node = children["items"].as_array().unwrap().iter().find(|n| n["role"] == "favor-add-open-tabs").unwrap();
    (session, node["id"].as_str().unwrap().to_string())
}

#[tokio::test]
async fn collect_link_uses_current_browser_page_and_rejects_unavailable_results() {
    let (_dir, host, peer, responding, serving) = fixture().await;
    // A slow browser must not prevent window-state changes.
    peer.mode.store(3, Ordering::SeqCst);
    let testing = host.clone();
    let collect = tokio::task::spawn_blocking(move || testing.execute_verb("collect link --title ignored --data ignored"));
    tokio::time::timeout(Duration::from_secs(2), peer.entered.notified()).await.unwrap();
    let testing = host.clone();
    let responsive = tokio::time::timeout(Duration::from_secs(1), tokio::task::spawn_blocking(move || {
        testing.apply_window_action("setPinned", true);
    })).await;
    peer.release.notify_one();
    responsive.unwrap().unwrap();
    let result = collect.await.unwrap().unwrap();
    let draft_id = result["draftId"].as_str().unwrap();
    let draft = host.collect_get(&json!({"draftId": draft_id})).unwrap();
    assert_eq!(draft["detail"]["kind"], "LinkClipItem");
    assert_eq!(draft["detail"]["title"], "页面中文");
    assert_eq!(draft["detail"]["linkUrl"], "https://example.test/?x=1&y=中文");
    let store = host.inner.lock().store.clone();
    let parent = store.lock().list_favors().unwrap()[0].id.clone();
    assert!(store.lock().list_clips().unwrap().is_empty());
    host.collect_commit(&json!({"draftId": draft_id, "parentId": parent, "title": "Saved page", "content": "must not replace URL"})).unwrap();
    let saved = store.lock().get_favor(draft["detail"]["id"].as_str().unwrap()).unwrap();
    assert_eq!(saved.body_text.as_deref(), Some("https://example.test/?x=1&y=中文"));
    assert_eq!(saved.icon, "icon-data");
    assert_eq!(saved.name, "Saved page");

    let navigation = host.inner.lock().ui.pending_nav.clone();
    for (mode, expected) in [(1, crate::browser::UNAVAILABLE), (2, "browser failed")] {
        peer.mode.store(mode, Ordering::SeqCst);
        let testing = host.clone();
        let error = tokio::task::spawn_blocking(move || testing.execute_verb("collect link")).await.unwrap().unwrap_err();
        assert!(error.contains(expected), "{error}");
        assert!(host.collect_drafts.lock().is_empty());
        assert_eq!(host.inner.lock().ui.pending_nav, navigation);
        assert_eq!(store.lock().list_favors().unwrap().len(), 2);
    }
    assert_eq!(*peer.calls.lock(), ["quicker.getCurrentBrowserTab"; 3]);
    responding.abort();
    let _ = responding.await;
    serving.await.unwrap().unwrap();
}

#[tokio::test]
async fn browser_callbacks_populate_favorites_menus_and_bookmarks() {
    let (_dir, host, peer, responding, serving) = fixture().await;
    let testing = host.clone();
    let control = peer.clone();
    tokio::task::spawn_blocking(move || {
        let folder = testing.favor_folder_create(&json!({"name":"Browser fixture"})).unwrap();
        let parent = folder["id"].as_str().unwrap();
        let item = testing.favor_add_link(&json!({"parentId":parent})).unwrap();
        assert_eq!(item["title"], "页面中文");
        assert_eq!(item["linkUrl"], "https://example.test/?x=1&y=中文");
        let store = testing.inner.lock().store.clone();
        assert_eq!(store.lock().get_favor(item["id"].as_str().unwrap()).unwrap().icon, "icon-data");
        let (session, node) = tabs_menu(&testing, parent);
        let first = testing.menu_nodes(&session, &node).unwrap();
        assert_eq!(first["items"].as_array().unwrap().len(), 5);
        assert_eq!(first["items"][0]["icon"], "tab-icon");
        assert_eq!(first["items"][0]["payload"]["parentId"], parent);
        let calls = control.calls.lock().len();
        let second = testing.menu_nodes_ex(&session, &node, "children", Some("more")).unwrap();
        assert_eq!(second["items"][0]["title"], "Tab 5");
        assert_eq!(control.calls.lock().len(), calls, "pagination reuses the same tab snapshot");
        testing.menu_invoke(&session, first["items"][0]["id"].as_str().unwrap(), &json!({})).unwrap();
        let saved = store.lock().get_favor(parent).unwrap();
        assert_eq!(saved.children.len(), 2);
        let links = saved.children.iter().map(|id| store.lock().get_favor(id).unwrap()).collect::<Vec<_>>();
        let saved_tab = links.iter().find(|item| item.body_text.as_deref()==Some("https://example.test/0")).unwrap();
        assert_eq!(saved_tab.icon, "tab-icon", "invoking a browser tab must persist its displayed favicon");
        let bookmarks = testing.menu_create_session("bookmarks");
        assert_eq!(bookmarks["root"]["items"][0]["title"], "插件书签");
        assert_eq!(bookmarks["root"]["items"][0]["payload"]["id"], "7");
        control.mode.store(1, Ordering::SeqCst);
        assert!(testing.favor_add_link(&json!({"parentId":parent})).is_err());
        assert_eq!(store.lock().get_favor(parent).unwrap().children.len(), 2);
        let empty = testing.menu_nodes(&session, &node).unwrap();
        assert_eq!(empty["items"][0]["role"], "favor-add-open-tabs-empty");
        assert!(testing.menu_create_session("bookmarks")["root"]["items"].as_array().unwrap().is_empty());
        control.mode.store(2, Ordering::SeqCst);
        let failed = testing.menu_nodes(&session, &node).unwrap();
        assert!(failed["items"][0]["description"].as_str().unwrap().contains("browser failed"));
        assert!(testing.favor_add_link(&json!({"parentId":parent})).unwrap_err().contains("browser failed"));
        let calls = control.calls.lock().len();
        testing.favor_add_link(&json!({"parentId":parent,"url":"https://manual.test"})).unwrap();
        assert_eq!(control.calls.lock().len(), calls, "explicit link needs no extension");
    }).await.unwrap();
    responding.abort();
    let _ = responding.await;
    serving.await.unwrap().unwrap();
}

#[tokio::test]
async fn browser_wait_does_not_lock_host_and_closed_menu_discards_response() {
    let (_dir, host, peer, responding, serving) = fixture().await;
    let testing = host.clone();
    let (session, node, parent) = tokio::task::spawn_blocking(move || {
        let folder = testing.favor_folder_create(&json!({"name":"Waiting browser"})).unwrap();
        let parent = folder["id"].as_str().unwrap().to_string();
        let (session, node) = tabs_menu(&testing, &parent);
        (session, node, parent)
    }).await.unwrap();
    peer.mode.store(3, Ordering::SeqCst);
    let testing = host.clone();
    let menu_id = session.clone();
    let load = tokio::task::spawn_blocking(move || testing.menu_nodes(&menu_id, &node));
    peer.entered.notified().await;
    let testing = host.clone();
    let responsive = tokio::time::timeout(Duration::from_secs(1), tokio::task::spawn_blocking(move || {
        testing.apply_window_action("setPinned", true);
        assert!(testing.menu_nodes(&session, "root").is_ok());
        testing.menu_close_session(&session)
    })).await;
    peer.release.notify_one();
    assert!(responsive.unwrap().unwrap());
    assert_eq!(load.await.unwrap().unwrap_err(), "Session not found");
    let testing = host.clone();
    let old_parent = parent.clone();
    let add = tokio::task::spawn_blocking(move || testing.favor_add_link(&json!({"parentId":old_parent})));
    peer.entered.notified().await;
    host.favor_delete(&json!({"id":parent})).unwrap();
    peer.release.notify_one();
    assert!(add.await.unwrap().unwrap_err().contains("Favorite folder not found"));
    peer.mode.store(4, Ordering::SeqCst);
    let testing = host.clone();
    let failure = tokio::task::spawn_blocking(move || testing.favor_add_link(&json!({}))).await.unwrap().unwrap_err();
    assert!(failure.contains("超时"));
    // Do not drop the peer while the timeout's cancellation is still queued:
    // that races read EOF against a broken-pipe write during fixture teardown.
    tokio::time::timeout(Duration::from_secs(2), peer.cancelled.notified()).await.unwrap();
    responding.abort();
    let _ = responding.await;
    serving.await.unwrap().unwrap();
}

#[tokio::test]
async fn independent_menus_skip_bookmarks_and_favorite_expression_callbacks() {
    let (_dir, host, peer, responding, serving) = fixture().await;
    {
        let mut inner = host.inner.lock();
        inner.extras.menu_global.show_book_marks_menu = true;
        inner.extras.favor_menu.config_items = vec![crate::favor_menu::FavorMenuConfigItem {
            match_expression: "$=1 == 1".into(),
            ..Default::default()
        }];
    }
    let testing = host.clone();
    tokio::task::spawn_blocking(move || {
        for action in ["text", "window", "clipboard"] {
            let menu = testing.menu_create_session(action);
            assert!(testing.menu_close_session(menu["sessionId"].as_str().unwrap()));
        }
    }).await.unwrap();
    let unrelated_calls = peer.calls.lock().clone();
    // A requested bookmark root must still query the connected plugin.
    let testing = host.clone();
    let bookmarks = tokio::task::spawn_blocking(move || testing.menu_create_session("bookmarks")).await.unwrap();
    host.menu_close_session(bookmarks["sessionId"].as_str().unwrap());
    let all_calls = peer.calls.lock().clone();
    responding.abort();
    let _ = responding.await;
    serving.await.unwrap().unwrap();
    assert!(unrelated_calls.is_empty(), "unrelated menu callbacks: {unrelated_calls:?}");
    assert_eq!(all_calls, ["quicker.getBrowserBookmarksTree"]);
    assert_eq!(bookmarks["root"]["items"][0]["title"], "插件书签");
}
