use super::*;
use std::cell::RefCell;

thread_local! {
    static BUSY_GATE: RefCell<Option<(Sender<()>, Receiver<()>)>> = const { RefCell::new(None) };
}

// SQLite calls this only after the real query has encountered the other
// connection's exclusive transaction. No scheduling guesses or sleeps.
fn wait_for_database_release(_: i32) -> bool {
    BUSY_GATE.with(|slot| {
        let Some((entered, release)) = slot.borrow_mut().take() else {
            return false;
        };
        let _ = entered.send(());
        release.recv_timeout(Duration::from_secs(10)).is_ok()
    })
}

fn fixture() -> (tempfile::TempDir, HostState) {
    crate::clipboard::force_headless(true);
    let directory = tempfile::tempdir().unwrap();
    let host = HostState::open(
        AppPaths::from_data_root(directory.path().join("BetterClipTauri")),
        28965,
    ).unwrap();
    host.set_clipboard_ui_runner(|_| Err("test must not write clipboard".into()));
    host.set_input_runner(|_| Err("test must not send input".into()));
    let mut clip = ClipRecord::new_text("first\nsecond", "contention fixture");
    clip.id = "text".into();
    let mut favor = FavorRecord::from_clip(&clip, None);
    favor.id = "favorite".into();
    let inner = host.inner.lock();
    let db = inner.store.lock();
    db.insert_clip(&clip, true).unwrap();
    db.insert_favor(&favor).unwrap();
    db.set_busy_handler(wait_for_database_release);
    drop(db);
    drop(inner);
    (directory, host)
}

// Always unblock and join both workers before reporting a failure; a failed
// regression must not strand a background thread holding the host mutex.
fn while_database_busy(
    host: &HostState,
    query: impl FnOnce(HostState) + Send + 'static,
    action: impl FnOnce(HostState) + Send + 'static,
) -> bool {
    let blocker = rusqlite::Connection::open(&host.inner.lock().paths.db).unwrap();
    blocker.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let (entered, waiting) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let querying_host = host.clone();
    let query_worker = thread::spawn(move || {
        BUSY_GATE.with(|slot| *slot.borrow_mut() = Some((entered, released)));
        query(querying_host);
        BUSY_GATE.with(|slot| slot.borrow_mut().take());
    });
    let entered = waiting.recv_timeout(Duration::from_secs(5));
    let (done, completion) = mpsc::channel();
    let action_host = host.clone();
    let action_worker = thread::spawn(move || {
        action(action_host);
        let _ = done.send(());
    });
    let responsive = completion.recv_timeout(Duration::from_millis(500)).is_ok();
    blocker.execute_batch("ROLLBACK").unwrap();
    let _ = release.send(());
    query_worker.join().unwrap();
    action_worker.join().unwrap();
    entered.expect("operation must actually reach SQLite contention");
    responsive
}

#[test]
fn starting_escape_during_database_loading_retires_old_work_and_keeps_new_chord() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (_directory, host) = fixture();
    host.set_window_runtime_ready(true);
    let writes = Arc::new(AtomicUsize::new(0));
    let write_count = writes.clone();
    host.set_clipboard_ui_runner(move |_| {
        write_count.fetch_add(1, Ordering::SeqCst);
        Err("canceled loading must not attempt a clipboard write".into())
    });
    host.set_input_runner(|_| panic!("canceled loading must not send input"));
    let event = |vk, down, alt, now_ms| KeyEvent {
        vk, down, alt, ctrl: false, shift: false, win: false, now_ms,
    };
    assert!(host.dispatch_low_level_key(event(0xC0, true, true, 1)));
    let old_generation = host.inner.lock().qp.hook_generation;
    assert!(while_database_busy(&host,
        |host| host.poll_quick_paste_with_modifiers(|| false),
        move |host| {
            assert_eq!(host.inner.lock().qp.machine.phase, QuickPastePhase::SessionStarting);
            assert!(host.dispatch_low_level_key(event(0x1B, true, true, 2)));
            {
                let inner = host.inner.lock();
                assert_eq!(inner.qp.machine.phase, QuickPastePhase::Idle);
                assert!(!inner.qp.overlay_visible);
                assert!(inner.qp.pending_commands.is_empty());
                assert!(inner.qp.escape_down);
            }
            assert!(host.dispatch_low_level_key(event(0x1B, false, true, 3)));
            assert!(!host.dispatch_low_level_key(event(0x12, false, false, 4)));
            // End the canceled press, then queue a fresh chord while its old
            // candidate read is still blocked inside the actual SQLite call.
            assert!(host.dispatch_low_level_key(event(0xC0, false, false, 5)));
            assert!(host.dispatch_low_level_key(event(0xC0, true, true, 6)));
            let inner = host.inner.lock();
            assert_eq!(inner.qp.machine.phase, QuickPastePhase::SessionStarting);
            assert!(!inner.qp.overlay_visible);
            assert!(!inner.qp.escape_down);
            assert_eq!(inner.qp.hook_generation, old_generation.wrapping_add(1));
            assert_eq!(inner.qp.pending_commands.len(), 1);
            let pending = inner.qp.pending_commands.front().unwrap();
            assert_eq!(pending.command, QuickPasteCommand::BeginSession);
            assert_eq!(pending.generation, inner.qp.hook_generation);
            assert_eq!(pending.cancel_generation, inner.qp.cancel_generation);
        },
    ), "Escape must return without waiting for SQLite");
    assert_eq!(writes.load(Ordering::SeqCst), 0);
    // The worker also drains the new Begin queued during the old blocked read.
    // Its valid overlay may already be open when the first poll returns.
    host.poll_quick_paste_with_modifiers(|| false);
    {
        let inner = host.inner.lock();
        assert_eq!(inner.qp.machine.phase, QuickPastePhase::SessionActive);
        assert!(inner.qp.overlay_visible, "canceling the old read lost the next chord");
        assert_eq!(inner.qp.session_generation, inner.qp.hook_generation);
        assert_eq!(inner.qp.session_generation, old_generation.wrapping_add(1));
        assert!(inner.qp.pending_commands.is_empty());
        assert_eq!(inner.chrome.iter().filter(|command|
            **command == ChromeCommand::ShowQuickPaste).count(), 1,
            "only the new chord may publish an overlay");
    }
    assert_eq!(writes.load(Ordering::SeqCst), 0);
    assert_eq!(host.store().lock().get_clip("text").unwrap().usage_count, 0);
    host.qp_debug_end();
}

#[test]
fn execution_alignment_fifo_loading_rearm_retires_old_candidates() {
    let (_directory, host) = fixture();
    host.arm_fifo_queue(vec!["text".into()]);
    while host.take_chrome_command().is_some() {}
    // The old candidate read reaches a real exclusive SQLite transaction.
    // Rearm updates/retire state before its HUD waits for that same database.
    let _ = while_database_busy(&host, |host| host.qp_begin_session(), |host| {
        host.arm_fifo_queue(vec!["text".into()]);
    });
    {
        let inner = host.inner.lock();
        assert!(!inner.qp.overlay_visible, "the old FIFO read must not reopen its overlay");
        assert!(inner.qp.items.is_empty());
        assert!(inner.qp.fifo_generation.is_none());
        assert!(!inner.chrome.iter().any(|command| *command == ChromeCommand::ShowQuickPaste));
    }
    let generation = host.inner.lock().fifo_generation;
    host.qp_debug_begin();
    assert_eq!(host.inner.lock().qp.fifo_generation, Some(generation));
    assert_eq!(host.inner.lock().qp.items[0].id, "text");
    host.fifo_cancel();
    assert!(!host.inner.lock().qp.overlay_visible, "cancel must retire an already loaded FIFO session too");
    // An ordinary QP session keeps its established behavior when FIFO changes.
    host.qp_debug_begin();
    assert!(host.inner.lock().qp.fifo_generation.is_none());
    host.arm_fifo_queue(vec!["text".into()]);
    host.fifo_cancel();
    assert!(host.inner.lock().qp.overlay_visible);
    assert_eq!(host.inner.lock().qp.items[0].id, "text");
    assert_eq!(host.store().lock().get_clip("text").unwrap().usage_count, 0);
    host.qp_debug_end();
}

fn check_window_commands(host: HostState) {
    assert_eq!(host.health()["service"], HEALTH_SERVICE_ID);
    host.apply_window_action("setPinned", true);
    host.apply_window_action("setCollapsed", true);
    host.apply_window_action("hide", true);
    let inner = host.inner.lock();
    assert!(inner.window_pinned);
    assert!(!inner.window_visible);
}

#[cfg(windows)]
#[test]
fn process_icon_lookup_keeps_search_and_window_commands_responsive() {
    let (directory, host) = fixture();
    let exe = directory.path().join("slow-shell-fixture.exe");
    std::fs::write(&exe, b"MZ").unwrap();
    host.inner.lock().store.lock()
        .remember_process_icon("slow-shell-fixture.exe", exe.to_str().unwrap()).unwrap();
    let session = host.create_session();
    let (entered, entering) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let lookup_host = host.clone();
    let lookup = thread::spawn(move || {
        crate::file_icon::tests::set_lookup_hook(move || {
            entered.send(()).unwrap();
            released.recv_timeout(Duration::from_secs(10)).unwrap();
        });
        lookup_host.file_icon_png(None, Some("slow-shell-fixture"), None)
    });
    let reached_shell = entering.recv_timeout(Duration::from_secs(5));
    let (done, completed) = mpsc::channel();
    let reader = thread::spawn(move || {
        check_window_commands(host.clone());
        let items = host.search_items(&json!({"webSessionId":session["webSessionId"], "query":"first"})).unwrap();
        assert_eq!(items["total"], 1);
        done.send(()).unwrap();
    });
    let responsive = completed.recv_timeout(Duration::from_millis(500)).is_ok();
    // Always release and join both workers before asserting, including on failure.
    let _ = release.send(());
    let png = lookup.join().unwrap().unwrap();
    reader.join().unwrap();
    reached_shell.expect("stored process icon must reach the real Shell lookup");
    assert!(png.starts_with(b"\x89PNG"));
    assert!(responsive, "slow process icon lookup blocked search/window commands");
}

#[test]
fn store_contention_keeps_window_commands_responsive() {
    let mut blocked = Vec::new();
    for operation in [
        "history-detail", "history-delete", "history-copy", "history-paste",
        "history-paste-batch", "history-paste-plain", "history-files", "history-split",
        "favorite-tree", "favorite-search", "favorite-detail", "favorite-batch",
        "favorite-copy", "favorite-paste", "favorite-from-clip", "config",
        "panel-catalog", "image", "favorite-image", "lan-messages", "quick-paste-items",
        "fifo-status", "fifo-hud",
    ] {
        let (_directory, host) = fixture();
        if operation.starts_with("fifo-") {
            let mut inner = host.inner.lock();
            inner.fifo = Some(vec!["text".into()]);
            inner.fifo_total = 1;
        }
        let responsive = while_database_busy(&host, move |host| {
            let item = json!({"id":"text"});
            let missing = json!({"id":"missing", "ids":["missing"]});
            let favorite = json!({"id":"favorite"});
            match operation {
                "history-detail" => { assert_eq!(host.detail(&item).unwrap()["bodyText"], "first\nsecond"); }
                "history-delete" => { assert_eq!(host.delete_item(&item).unwrap()["ok"], true); }
                "history-copy" => { assert!(host.copy_items(&missing).is_err()); }
                "history-paste" => { assert!(host.paste_item(&missing).is_err()); }
                "history-paste-batch" => { assert!(host.paste_items(&missing).is_err()); }
                "history-paste-plain" => { assert!(host.paste_plain(&missing).is_err()); }
                "history-files" => { assert_eq!(host.files_list(&item).unwrap(), json!([])); }
                "history-split" => { assert!(host.split_lines(&item).is_ok()); }
                "favorite-tree" => { assert!(host.favor_tree().is_ok()); }
                "favorite-search" => { assert!(host.favor_search(&json!({})).is_ok()); }
                "favorite-detail" => { assert!(host.favor_get(&favorite).is_ok()); }
                "favorite-batch" => { assert_eq!(host.favor_get_by_ids(&json!({"ids":["favorite"]})).unwrap().as_array().unwrap().len(), 1); }
                "favorite-copy" => { assert!(host.favor_copy(&missing).is_err()); }
                "favorite-paste" => { assert!(host.favor_paste(&missing).is_err()); }
                "favorite-from-clip" => { assert_eq!(host.favorite_from_clip(&item).unwrap()["ok"], true); }
                "config" => { assert!(host.config_get().is_object()); }
                "panel-catalog" => { assert!(host.panel_catalog().is_ok()); }
                "image" => { assert!(host.image_bytes("text").is_err()); }
                "favorite-image" => { assert!(host.favor_image_bytes("favorite").is_err()); }
                "lan-messages" => { assert!(host.lan_messages(&json!({"conversationId":"all"})).is_ok()); }
                "quick-paste-items" => { assert_eq!(host.qp_session_items().unwrap().0.len(), 1); }
                "fifo-status" => { assert_eq!(host.fifo_status()["remainingCount"], 1); }
                "fifo-hud" => host.fifo_show_armed_hud(),
                _ => unreachable!(),
            }
        }, check_window_commands);
        if !responsive { blocked.push(operation); }
    }
    assert!(blocked.is_empty(), "database wait blocked window commands: {blocked:?}");
}

#[test]
fn store_contention_fifo_cancel_is_not_undone_by_late_compaction() {
    let (_directory, host) = fixture();
    {
        let mut inner = host.inner.lock();
        inner.fifo = Some(vec!["text".into()]);
        inner.fifo_total = 1;
    }
    let responsive = while_database_busy(&host, |host| host.compact_fifo(), |host| {
        assert_eq!(host.fifo_cancel()["armed"], false);
    });
    assert!(host.inner.lock().fifo.is_none());
    assert!(responsive, "cancel must finish before the database is released");
}

#[test]
fn store_contention_fifo_consumed_head_is_not_restored() {
    let (_directory, host) = fixture();
    {
        let mut inner = host.inner.lock();
        inner.fifo = Some(vec!["text".into()]);
        inner.fifo_total = 1;
    }
    let responsive = while_database_busy(&host, |host| host.compact_fifo(), |host| {
        assert!(host.fifo_mark_pasted("text", 0));
    });
    assert!(host.inner.lock().fifo.is_none());
    assert!(responsive);
}

#[test]
fn store_contention_fifo_database_error_keeps_queue() {
    let (_directory, host) = fixture();
    let path = {
        let mut inner = host.inner.lock();
        inner.fifo = Some(vec!["text".into()]);
        inner.fifo_total = 1;
        inner.store.lock().set_busy_handler(|_| false);
        inner.paths.db.clone()
    };
    let blocker = rusqlite::Connection::open(path).unwrap();
    blocker.execute_batch("BEGIN EXCLUSIVE").unwrap();
    host.compact_fifo();
    blocker.execute_batch("ROLLBACK").unwrap();
    assert_eq!(host.inner.lock().fifo, Some(vec!["text".into()]));
    assert_eq!(host.fifo_status()["totalCount"], 1);
}

#[test]
fn store_contention_fifo_cancel_discards_late_hud() {
    let (_directory, host) = fixture();
    {
        let mut inner = host.inner.lock();
        inner.fifo = Some(vec!["text".into()]);
        inner.fifo_total = 1;
    }
    let responsive = while_database_busy(&host, |host| host.fifo_show_armed_hud(), |host| {
        host.fifo_cancel();
    });
    assert!(responsive);
    assert_eq!(host.take_toast_push().unwrap()["message"], "连续粘贴已取消");
}

#[test]
fn quick_paste_database_load_failure_reports_error_and_retry_recovers() {
    let (_directory, host) = fixture();
    let blocker = rusqlite::Connection::open(&host.inner.lock().paths.db).unwrap();
    blocker.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let status = host.qp_debug_begin();
    blocker.execute_batch("ROLLBACK").unwrap();
    assert_eq!(status["sessionActive"], false, "database failure was mistaken for an empty session");
    assert_eq!(status["overlayVisible"], false);
    assert_eq!(host.take_toast_push().unwrap()["kind"], "error");
    assert_eq!(host.qp_debug_begin()["itemCount"], 1);
    assert_eq!(host.qp_debug_status()["sessionActive"], true);
    host.qp_debug_end();
}

#[test]
fn quick_paste_database_fifo_load_failure_keeps_queue_and_reports_error() {
    let (_directory, host) = fixture();
    host.arm_fifo_queue(vec!["text".into()]);
    host.take_toast_push();
    let blocker = rusqlite::Connection::open(&host.inner.lock().paths.db).unwrap();
    blocker.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let status = host.qp_debug_begin();
    blocker.execute_batch("ROLLBACK").unwrap();
    assert_eq!(status["sessionActive"], false);
    assert_eq!(host.fifo_status()["remainingCount"], 1);
    assert_eq!(host.take_toast_push().unwrap()["kind"], "error");
    assert_eq!(host.qp_debug_begin()["itemCount"], 1);
    host.qp_debug_end();
}

#[test]
fn quick_paste_database_reload_failure_preserves_visible_items_and_selection() {
    let (_directory, host) = fixture();
    host.qp_debug_begin();
    let previous = host.inner.lock().qp.items.clone();
    let blocker = rusqlite::Connection::open(&host.inner.lock().paths.db).unwrap();
    blocker.execute_batch("BEGIN EXCLUSIVE").unwrap();
    host.qp_reload_session_items();
    blocker.execute_batch("ROLLBACK").unwrap();
    assert_eq!(host.inner.lock().qp.items.iter().map(|item| &item.id).collect::<Vec<_>>(),
        previous.iter().map(|item| &item.id).collect::<Vec<_>>());
    assert_eq!(host.qp_debug_status()["overlayVisible"], true);
    assert_eq!(host.take_toast_push().unwrap()["kind"], "error");
    host.qp_debug_end();
}

#[test]
fn quick_paste_database_delete_failure_is_visible_and_keeps_the_item() {
    let (_directory, host) = fixture();
    host.qp_debug_begin();
    let blocker = rusqlite::Connection::open(&host.inner.lock().paths.db).unwrap();
    blocker.execute_batch("BEGIN EXCLUSIVE").unwrap();
    host.qp_apply_command(QuickPasteCommand::DeleteActive);
    blocker.execute_batch("ROLLBACK").unwrap();
    assert_eq!(host.qp_debug_status()["itemCount"], 1);
    assert!(host.store().lock().get_clip("text").is_ok());
    assert_eq!(host.take_toast_push().unwrap()["kind"], "error");
    host.qp_debug_end();
}

#[test]
fn quick_paste_database_debug_open_canceled_during_load_does_not_reclaim_keys() {
    let (_directory, host) = fixture();
    let responsive = while_database_busy(&host, |host| { host.qp_debug_begin(); }, |host| {
        host.dismiss_native_window("quick-paste");
    });
    assert!(responsive);
    assert_eq!(host.qp_debug_status()["sessionActive"], false);
    assert_eq!(host.qp_debug_status()["overlayVisible"], false);
    assert!(host.take_toast_push().is_none());
}

#[test]
fn store_contention_quick_paste_native_cancel_discards_late_open() {
    let (_directory, host) = fixture();
    let responsive = while_database_busy(&host, |host| host.qp_begin_session(), |host| {
        host.dismiss_native_window("quick-paste");
    });
    assert!(responsive, "closing must not wait for the blocked query");
    assert!(!host.qp_state_detail()["visible"].as_bool().unwrap(),
        "a canceled load reopened the quick-paste overlay");
    assert!(!host.inner.lock().qp.awaiting_reveal);
    assert!(host.inner.lock().qp.items.is_empty());
    while let Some(command) = host.take_chrome_command() {
        assert!(!matches!(command, ChromeCommand::ShowQuickPaste));
    }
}

#[test]
fn store_contention_quick_paste_end_discards_late_reload() {
    let (_directory, host) = fixture();
    {
        let mut inner = host.inner.lock();
        inner.qp.session_started = Some(Instant::now());
        inner.qp.overlay_visible = true;
    }
    let responsive = while_database_busy(&host, |host| host.qp_reload_session_items(), |host| {
        host.qp_end_session(false, false);
    });
    assert!(responsive);
    let inner = host.inner.lock();
    assert!(inner.qp.items.is_empty());
    assert!(!inner.qp.overlay_visible);
}

#[test]
fn store_contention_quick_paste_arm_respects_concurrent_cancel() {
    let (_directory, host) = fixture();
    {
        let mut inner = host.inner.lock();
        inner.qp.session_started = Some(Instant::now());
        inner.qp.overlay_visible = true;
        let item = inner.store.lock().get_clip("text").unwrap();
        inner.qp.items = vec![item];
    }
    let responsive = while_database_busy(&host, |host| host.qp_arm_fifo_from_active(), |host| {
        host.fifo_cancel();
    });
    assert!(responsive);
    let inner = host.inner.lock();
    assert!(inner.fifo.is_none());
    assert!(!inner.qp.fifo_arm_only_this_session);
    assert_eq!(inner.qp.items.len(), 1);
}
