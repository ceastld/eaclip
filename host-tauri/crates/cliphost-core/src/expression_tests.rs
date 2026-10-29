use super::*;
use crate::favor_menu::{FavorMenuConfigItem, MenuTargetWindow};
use crate::quicker_expression as expression;
use std::sync::atomic::AtomicUsize;
use tokio::io::AsyncWriteExt;

struct Peer {
    result: Mutex<Result<Value, String>>,
    calls: Mutex<Vec<Value>>,
    wait: AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

async fn fixture() -> (tempfile::TempDir, HostState, Arc<Peer>, tokio::task::JoinHandle<()>, tokio::task::JoinHandle<Result<(), String>>) {
    crate::clipboard::force_headless(true);
    let dir = tempfile::tempdir().unwrap();
    let host = HostState::open(AppPaths::from_data_root(dir.path().join("BetterClipTauri")), 28965).unwrap();
    let peer = Arc::new(Peer {
        result: Mutex::new(Ok(json!("\"processed\""))), calls: Mutex::new(Vec::new()),
        wait: AtomicBool::new(false), entered: tokio::sync::Notify::new(), release: tokio::sync::Notify::new(),
    });
    let (server, mut client) = tokio::io::duplex(65536);
    let serving = tokio::spawn(crate::quicker_rpc::serve(server, host.quicker_rpc(), "expression-fixture",
        Arc::new(crate::pipe_rpc::StopSignal::default()), Arc::new(|_| None)));
    let control = peer.clone();
    let responding = tokio::spawn(async move {
        while let Ok(Some(request)) = crate::quicker_rpc::read_message(&mut client).await {
            if request.type_name != "request" { continue; }
            let mut reply = request.clone();
            reply.type_name = "response".into();
            reply.method = None;
            if request.method.as_deref() == Some("quicker.ping") { reply.payload = Some(json!("pong")); }
            else {
                assert_eq!(request.method.as_deref(), Some("quicker.evaluateExpression"));
                control.calls.lock().push(request.payload.unwrap());
                if control.wait.load(Ordering::SeqCst) {
                    control.entered.notify_one();
                    control.release.notified().await;
                }
                match control.result.lock().clone() {
                    Ok(value) => reply.payload = Some(value),
                    Err(error) => reply = crate::pipe_rpc::PipeRpcMessage::error(reply.id, "expression-fixture", &error),
                }
            }
            if client.write_all(&crate::pipe_rpc::encode_frame(&reply).unwrap()).await.is_err() { break; }
        }
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while host.quicker_rpc.select().is_none() { tokio::time::sleep(Duration::from_millis(5)).await; }
    }).await.unwrap();
    (dir, host, peer, responding, serving)
}

async fn close(responding: tokio::task::JoinHandle<()>, serving: tokio::task::JoinHandle<Result<(), String>>) {
    responding.abort();
    let _ = responding.await;
    serving.await.unwrap().unwrap();
}

#[test]
fn expression_local_values_keep_offline_text_actions_usable() {
    let registry = crate::quicker_rpc::Registry::default();
    let params = expression::text_params("甲\r\n\n beta \r尾");
    assert_eq!(params["list"], json!(["甲", " beta ", "尾"]));
    for (expr, expected) in [("literal", "literal"), ("$={list}", "甲\r\n beta \r\n尾"),
        ("$=true", "True"), ("$=42", "42"), ("$$<{text}> {unknown}", "<甲\r\n\n beta \r尾> {unknown}")] {
        assert_eq!(expression::text_result(expression::evaluate(&registry, expr, &params).unwrap()).unwrap(), expected);
    }
    assert!(expression::evaluate(&registry, "$={text}.Trim()", &params).unwrap_err().contains("未连接"));
    assert!(!crate::text_actions::needs_input(&json!({"Type":"ZExpression","Data":{"Expresstion":"$=DateTime.Now.Year"}})));
    assert!(crate::text_actions::needs_input(&json!({"Type":"ZExpression","Data":{"Expresstion":"$={list}.Count"}})));
    assert!(FavorMenuConfigItem { match_expression: "$=\nfalse".into(), ..Default::default() }.matches(&MenuTargetWindow::default()));
}

#[tokio::test]
async fn expression_rpc_preserves_parameters_and_decodes_supported_results() {
    let (_dir, host, peer, responding, serving) = fixture().await;
    let testing = host.clone();
    tokio::task::spawn_blocking(move || {
        let params = expression::text_params("中文\r\n\"quoted\"\\path\n\n tail ");
        for (result, expected) in [(json!("text\n中文"), "text\n中文"), (json!(["甲", "乙"]), "甲\r\n乙"),
            (json!(false), "False"), (json!(12.5), "12.5")] {
            *peer.result.lock() = Ok(json!(result.to_string()));
            let value = expression::evaluate(&testing.quicker_rpc, "$=Fixture({text}, {list})", &params).unwrap();
            assert_eq!(expression::text_result(value).unwrap(), expected);
            let calls = peer.calls.lock();
            let request = calls.last().unwrap();
            assert_eq!(request["Expression"], "$=Fixture({text}, {list})");
            assert_eq!(serde_json::from_str::<Value>(request["ParametersJson"].as_str().unwrap()).unwrap(), params);
        }
    }).await.unwrap();
    close(responding, serving).await;
}

#[tokio::test]
async fn expression_failures_stop_text_action_before_output() {
    let (_dir, host, peer, responding, serving) = fixture().await;
    let testing = host.clone();
    tokio::task::spawn_blocking(move || {
        let _snapshot = crate::clipboard::install_test_snapshot(ClipboardSnapshot {
            text: Some("preserve selection".into()), ..ClipboardSnapshot::default()
        });
        testing.set_clipboard_ui_runner(|_| panic!("failed evaluation must not overwrite clipboard"));
        let preparations = Arc::new(AtomicUsize::new(0));
        let count = preparations.clone();
        testing.set_input_runner(move |job| {
            assert!(count.fetch_add(1, Ordering::SeqCst) < 2, "failed evaluation must not replace selection");
            job()
        });
        for result in [Err("evaluation failed".into()), Ok(Value::Null), Ok(json!("null")),
            Ok(json!("invalid JSON")), Ok(json!("{\"key\":1}")), Ok(json!("[1,2]"))] {
            *peer.result.lock() = result;
            for simulate in [false, true] {
                preparations.store(0, Ordering::SeqCst);
                testing.inner.lock().text_actions = vec![json!({"Type":"ZExpression", "Simulate":simulate,
                    "Data":{"Expresstion":"$={text}.Trim()"}})];
                assert!(testing.execute_text_action(&json!({"actionId":"0"}), 0).is_err());
                assert_eq!(preparations.load(Ordering::SeqCst), 2, "activate and copy before evaluation");
            }
        }
        let outputs = Arc::new(AtomicUsize::new(0));
        let count = outputs.clone();
        testing.set_input_runner(move |_| { count.fetch_add(1, Ordering::SeqCst); Ok(()) });
        *peer.result.lock() = Ok(json!("\"valid output\""));
        assert_eq!(testing.execute_text_action(&json!({"actionId":"0"}), 0).unwrap()["closeMenu"], true);
        assert_eq!(outputs.load(Ordering::SeqCst), 3);
        let _empty = crate::clipboard::install_test_snapshot(ClipboardSnapshot::default());
        testing.inner.lock().text_actions[0]["Data"]["Expresstion"] = json!("$=42");
        assert_eq!(testing.execute_text_action(&json!({"actionId":"0"}), 0).unwrap()["closeMenu"], true);
        assert_eq!(outputs.load(Ordering::SeqCst), 5, "constant expression activates and outputs without copying");
    }).await.unwrap();
    close(responding, serving).await;
}

#[tokio::test]
async fn expression_capture_preprocess_preserves_original_on_unusable_results() {
    let (_dir, host, peer, responding, serving) = fixture().await;
    let testing = host.clone();
    tokio::task::spawn_blocking(move || {
        testing.inner.lock().config.text_process_expression = "$={text}.ToUpper()".into();
        testing.config_update(&json!({"showCopyMessage":false})).unwrap();
        let _snapshot = crate::clipboard::install_test_snapshot(ClipboardSnapshot {
            text: Some("original".into()), ..ClipboardSnapshot::default()
        });
        for (result, expected) in [(Ok(json!("\"PROCESSED\"")), "PROCESSED"),
            (Ok(json!("\"\"")), "original"), (Ok(json!("42")), "original"),
            (Ok(Value::Null), "original"), (Err("failed".into()), "original")] {
            *peer.result.lock() = result;
            let captured = testing.handle_clipboard().unwrap();
            let item = testing.inner.lock().store.lock().get_clip(captured["id"].as_str().unwrap()).unwrap();
            assert_eq!(item.body_text.as_deref(), Some(expected));
            assert_eq!(item.content_hash, crate::model::hash_text(expected));
            let calls = peer.calls.lock();
            let params: Value = serde_json::from_str(calls.last().unwrap()["ParametersJson"].as_str().unwrap()).unwrap();
            assert_eq!(params, json!({"text":"original"}));
        }
    }).await.unwrap();
    close(responding, serving).await;
}

#[tokio::test]
async fn expression_menu_conditions_change_snapshot_only_and_cache_duplicates() {
    let (_dir, host, peer, responding, serving) = fixture().await;
    let testing = host.clone();
    tokio::task::spawn_blocking(move || {
        let home = testing.favor_folder_create(&json!({"name":"Default home"})).unwrap();
        let conditional = testing.favor_folder_create(&json!({"name":"Conditional folder"})).unwrap();
        testing.favor_add_text_items(&json!({"parentId":home["id"],"text":"default content"})).unwrap();
        testing.favor_add_text_items(&json!({"parentId":conditional["id"],"text":"conditional content"})).unwrap();
        testing.inner.lock().extras.favor_menu.default_favorite1 = home["id"].as_str().unwrap().into();
        let rule = FavorMenuConfigItem { match_expression: "$={title}.Contains(\"fixture\")".into(),
            favorite_id1: conditional["id"].as_str().unwrap().into(), ..Default::default() };
        testing.inner.lock().extras.favor_menu.config_items = vec![rule.clone(), rule.clone(),
            FavorMenuConfigItem { is_enable: false, ..rule.clone() },
            FavorMenuConfigItem { proc_names: "other-app".into(), ..rule.clone() }];
        *peer.result.lock() = Ok(json!("false"));
        let mut ctx = testing.menu_build_context_with("favor", "", |_| {});
        ctx.target = MenuTargetWindow { process_name:"notepad".into(), class_name:"Editor".into(), title:"fixture title".into(), ..Default::default() };
        testing.resolve_menu_conditions(&mut ctx);
        assert!(!ctx.favor_menu.config_items[0].matches(&ctx.target));
        assert!(!ctx.favor_menu.config_items[1].matches(&ctx.target));
        let (nodes, _, _) = crate::menu_api::favor_root(&ctx);
        assert!(nodes.iter().any(|node| node.title == "default content"));
        assert!(!nodes.iter().any(|node| node.title == "conditional content"));
        assert_eq!(ctx.favor_menu.config_items[2].match_expression, rule.match_expression);
        assert_eq!(ctx.favor_menu.config_items[3].match_expression, rule.match_expression);
        assert_eq!(testing.inner.lock().extras.favor_menu.config_items[0].match_expression, rule.match_expression);
        assert_eq!(peer.calls.lock().len(), 1);
        let params: Value = serde_json::from_str(peer.calls.lock()[0]["ParametersJson"].as_str().unwrap()).unwrap();
        assert_eq!(params, json!({"procName":"notepad","className":"Editor","title":"fixture title"}));
        for result in [Ok(json!("true")), Ok(json!("123")), Err("failed".into())] {
            *peer.result.lock() = result;
            ctx.favor_menu.config_items = vec![rule.clone()];
            testing.resolve_menu_conditions(&mut ctx);
            assert!(ctx.favor_menu.config_items[0].matches(&ctx.target));
            let (nodes, _, _) = crate::menu_api::favor_root(&ctx);
            assert!(nodes.iter().any(|node| node.title == "conditional content"));
        }
    }).await.unwrap();
    close(responding, serving).await;
}

#[tokio::test]
async fn expression_menu_wait_is_bounded_and_leaves_host_responsive() {
    let (_dir, host, peer, responding, serving) = fixture().await;
    peer.wait.store(true, Ordering::SeqCst);
    let testing = host.clone();
    let evaluating = tokio::task::spawn_blocking(move || {
        let mut ctx = crate::menu_api::MenuBuildContext::default();
        ctx.favor_menu.config_items = (0..4).map(|i| FavorMenuConfigItem {
            match_expression: format!("$=Slow({i})"), ..Default::default()
        }).collect();
        let start = Instant::now();
        testing.resolve_menu_conditions(&mut ctx);
        assert!(start.elapsed() < Duration::from_secs(4), "one deadline for all conditions");
    });
    tokio::time::timeout(Duration::from_secs(2), peer.entered.notified()).await.unwrap();
    let testing = host.clone();
    tokio::time::timeout(Duration::from_secs(1), tokio::task::spawn_blocking(move || {
        testing.apply_window_action("setPinned", true);
        testing.menu_build_context_with("favor", "", |_| {});
    })).await.unwrap().unwrap();
    evaluating.await.unwrap();
    assert_eq!(peer.calls.lock().len(), 1, "do not start further calls after the deadline");
    close(responding, serving).await;
}

#[tokio::test]
async fn expression_late_result_does_not_overwrite_a_newer_clipboard() {
    let (_dir, host, peer, responding, serving) = fixture().await;
    peer.wait.store(true, Ordering::SeqCst);
    for simulate in [false, true] {
        let sequence = Arc::new(std::sync::atomic::AtomicU32::new(41));
        let input_calls = Arc::new(AtomicUsize::new(0));
        let clipboard_writes = Arc::new(AtomicUsize::new(0));
        let seen = input_calls.clone();
        host.set_input_runner(move |job| { seen.fetch_add(1, Ordering::SeqCst); job() });
        let seen = clipboard_writes.clone();
        host.set_clipboard_ui_runner(move |job| { seen.fetch_add(1, Ordering::SeqCst); job() });
        host.inner.lock().text_actions = vec![json!({"Type":"ZExpression", "Simulate":simulate,
            "Data":{"Expresstion":"$={text}.ToUpper()"}})];
        let testing = host.clone();
        let reading_sequence = sequence.clone();
        let evaluating = tokio::task::spawn_blocking(move || {
            let _sequence = crate::selection_capture::install_test_sequence(reading_sequence);
            let _snapshot = crate::clipboard::install_test_snapshot(ClipboardSnapshot {
                text: Some("selection at invocation".into()), ..Default::default()
            });
            testing.execute_text_action(&json!({"actionId":"0"}), 0)
        });
        tokio::time::timeout(Duration::from_secs(2), peer.entered.notified()).await.unwrap();
        // Simulate a new copy while the callback is still processing the old text.
        sequence.store(42, Ordering::SeqCst);
        let responsive = host.clone();
        let progress = tokio::time::timeout(Duration::from_secs(1), tokio::task::spawn_blocking(move || {
            responsive.set_window_pinned(true);
            responsive.health()
        })).await;
        peer.release.notify_one();
        let result = evaluating.await.unwrap();
        assert_eq!(progress.unwrap().unwrap()["ok"], true);
        assert!(result.is_err(), "late result replaced the user's newer clipboard: {result:?}");
        assert_eq!(input_calls.load(Ordering::SeqCst), 2, "only activate and copy");
        assert_eq!(clipboard_writes.load(Ordering::SeqCst), 0);
        assert!(host.clipboard_operation.try_lock().is_some());
    }
    close(responding, serving).await;
}
