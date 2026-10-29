use super::*;

fn fixture() -> (tempfile::TempDir, HostState) {
    crate::clipboard::force_headless(true);
    let root = tempfile::tempdir().unwrap();
    let host = HostState::open(AppPaths::from_data_root(root.path().to_path_buf()), 28965).unwrap();
    while host.take_chrome_command().is_some() {}
    (root, host)
}

fn pending(host: &HostState) -> Vec<ChromeCommand> {
    let mut commands = Vec::new();
    while let Some(command) = host.take_chrome_command() {
        commands.push(command);
    }
    commands
}

#[test]
fn chrome_queue_wakes_dispatch_without_bypassing_lifecycle() {
    use std::future::Future;
    use std::task::{Context, Poll, Waker};

    let (_root, host) = fixture();
    let wakeup = host.chrome_command_notifier();
    let mut context = Context::from_waker(Waker::noop());
    // Discard the one coalesced startup permit, if startup queued any work.
    let mut startup = Box::pin(wakeup.notified());
    let _ = startup.as_mut().poll(&mut context);
    drop(startup);

    // A command that arrives before the waiter is registered retains a permit.
    host.request_show();
    host.window_hide();
    let mut before_wait = Box::pin(wakeup.notified());
    assert_eq!(before_wait.as_mut().poll(&mut context), Poll::Ready(()));
    drop(before_wait);
    assert_eq!(pending(&host), vec![ChromeCommand::Hide], "notification bypassed obsolete-show filtering");

    // Repeated enqueues coalesce, and work arriving at an idle waiter wakes it.
    let mut waiting = Box::pin(wakeup.notified());
    assert_eq!(waiting.as_mut().poll(&mut context), Poll::Pending);
    host.request_show();
    assert_eq!(waiting.as_mut().poll(&mut context), Poll::Ready(()));
    assert_eq!(pending(&host), vec![ChromeCommand::Show]);
}

fn assert_overlay_close_cancels_show(label: &str, show: ChromeCommand, hide: ChromeCommand) {
    let (_root, host) = fixture();
    if label == "copy-toast" {
        host.toast_show();
    } else {
        host.menu_create_session("text");
    }
    let old_show = host.take_chrome_command().unwrap();
    assert_eq!(old_show, show);
    host.enqueue_chrome(show);
    host.dismiss_native_window(label);
    assert!(!host.prepare_chrome_command(old_show), "a canceled overlay must not reopen");
    assert_eq!(pending(&host), vec![hide]);
}

fn assert_overlay_reopen_cancels_hide(label: &str, show: ChromeCommand, hide: ChromeCommand) {
    let (_root, host) = fixture();
    if label == "copy-toast" {
        host.toast_show();
    } else {
        host.menu_create_session("text");
    }
    pending(&host);
    host.dismiss_native_window(label);
    let old_hide = host.take_chrome_command().unwrap();
    assert_eq!(old_hide, hide);
    host.enqueue_chrome(hide);
    if label == "copy-toast" {
        host.toast_show();
    } else {
        host.menu_create_session("text");
    }
    assert!(!host.prepare_chrome_command(old_hide), "a new overlay must remain open");
    assert_eq!(pending(&host), vec![show]);
}

#[test]
fn toast_close_cancels_queued_and_dequeued_show() {
    assert_overlay_close_cancels_show("copy-toast", ChromeCommand::ShowToast, ChromeCommand::HideToast);
}

#[test]
fn toast_reopen_cancels_queued_and_dequeued_hide() {
    assert_overlay_reopen_cancels_hide("copy-toast", ChromeCommand::ShowToast, ChromeCommand::HideToast);
}

#[test]
fn super_menu_close_cancels_queued_and_dequeued_show() {
    assert_overlay_close_cancels_show("super-menu", ChromeCommand::ShowSuperMenu, ChromeCommand::HideSuperMenu);
}

#[test]
fn super_menu_reopen_cancels_queued_and_dequeued_hide() {
    assert_overlay_reopen_cancels_hide("super-menu", ChromeCommand::ShowSuperMenu, ChromeCommand::HideSuperMenu);
}

#[test]
fn last_hide_cancels_queued_show_and_collapse() {
    for collapse in [false, true] {
        let (_root, host) = fixture();
        host.request_show();
        if collapse {
            host.apply_window_action("setCollapsed", true);
        }
        host.window_hide();
        assert_eq!(pending(&host), vec![ChromeCommand::Hide]);
        assert!(!host.hide_snapshot().visible);
        assert!(!host.hide_snapshot().has_fake_focus);
    }
}

#[test]
fn reopen_discards_obsolete_policy_hide() {
    let (_root, host) = fixture();
    host.apply_window_action("hide", false);
    host.request_show();
    assert_eq!(pending(&host), vec![ChromeCommand::Show]);
    assert!(host.hide_snapshot().visible);
    assert!(host.hide_snapshot().has_fake_focus);
}

#[test]
fn poll_decision_cannot_apply_after_a_concurrent_reopen() {
    use crate::hide_policy::HostDismissAction;
    use std::sync::{mpsc, Arc, Barrier};

    for (behavior, expected) in [
        ("Hide", HostDismissAction::Hide),
        ("Fold", HostDismissAction::Fold),
        ("EdgeHide", HostDismissAction::EdgeHide),
    ] {
        let (_root, host) = fixture();
        host.request_show();
        pending(&host);
        {
            let mut inner = host.inner.lock();
            inner.config.dismiss_behavior = behavior.into();
            inner.config.enable_edge_hide = true;
            inner.has_host_fake_focus = false;
            inner.pointer_in_host = false;
            inner.expand_grace_until_ms = 0;
            inner.no_focus_show_grace_until_ms = 0;
        }
        let decision_ready = Arc::new(Barrier::new(2));
        let resume_poll = Arc::new(Barrier::new(2));
        let poll_host = host.clone();
        let observed = decision_ready.clone();
        let resume = resume_poll.clone();
        let poll = std::thread::spawn(move || {
            poll_host.poll_hide_current_policy(Some(&|decision| {
                assert_eq!(decision.window_action, expected, "{behavior} stopped dismissing normally");
                observed.wait();
                resume.wait();
            }))
        });
        decision_ready.wait();
        let (attempting, attempted) = mpsc::channel();
        let show_host = host.clone();
        let show = std::thread::spawn(move || {
            attempting.send(()).unwrap();
            show_host.request_show();
        });
        attempted.recv().unwrap();
        // The poll is paused after computing its real decision. New state must
        // be excluded until that decision is applied, not merely until read.
        assert!(host.inner.try_lock().is_none(), "poll released its snapshot before applying the decision");
        resume_poll.wait();
        assert_eq!(poll.join().unwrap().window_action, expected);
        show.join().unwrap();
        let state = host.hide_snapshot();
        assert!(state.visible && state.has_fake_focus, "{behavior} dismissed the new show");
        assert!(!state.is_collapsed && !host.inner.lock().edge_tucked);
        assert_eq!(pending(&host), vec![ChromeCommand::Show], "{behavior} retained old dismissal work");
    }
}

#[test]
fn edge_hover_restores_without_starting_a_new_fake_focus_session() {
    let (_root, host) = fixture();
    {
        let mut inner = host.inner.lock();
        inner.config.dismiss_behavior = "EdgeHide".into();
        inner.config.enable_edge_hide = true;
        inner.has_host_fake_focus = false;
        inner.expand_grace_until_ms = 0;
    }
    host.set_pointer_in_host(false);
    host.poll_hide();
    assert!(host.inner.lock().edge_tucked);
    pending(&host);
    let no_focus = host.clipboard_no_activate();
    host.set_pointer_in_host(true);
    host.poll_hide();
    assert!(
        !host.inner.lock().edge_tucked,
        "hover must bring the tucked window back"
    );
    assert!(
        !host.hide_snapshot().has_fake_focus,
        "hover must not retain input after the pointer leaves"
    );
    assert_eq!(host.clipboard_no_activate(), no_focus);
    assert_eq!(pending(&host), vec![ChromeCommand::RestoreEdge]);
    host.set_pointer_in_host(false);
    host.poll_hide();
    assert!(host.inner.lock().edge_tucked, "leaving can tuck again");
}

#[test]
fn queued_lifecycle_commands_recheck_each_windows_current_intent() {
    let (_root, host) = fixture();
    host.request_show();
    let dequeued = host.take_chrome_command().unwrap();
    host.window_hide();
    assert!(!host.chrome_command_lifecycle_allowed(dequeued));
    let old_hide = host.take_chrome_command().unwrap();
    host.request_show();
    assert!(!host.chrome_command_lifecycle_allowed(old_hide));
    pending(&host);
    host.apply_window_action("setCollapsed", true);
    let old_fold = host.take_chrome_command().unwrap();
    host.apply_window_action("setCollapsed", false);
    assert!(!host.chrome_command_lifecycle_allowed(old_fold));
    assert_eq!(pending(&host), vec![ChromeCommand::Unfold]);

    host.favor_folder_create(&json!({"title": "Lifecycle"}))
        .unwrap();
    host.panel_show(&json!({})).unwrap();
    host.apply_panel_hide();
    assert_eq!(pending(&host), vec![ChromeCommand::HidePanel]);
    host.panel_show(&json!({})).unwrap();
    assert!(!host.chrome_command_lifecycle_allowed(ChromeCommand::HidePanel));
    pending(&host);
    host.show_web_route("/settings/clip");
    host.apply_main_hide();
    assert_eq!(pending(&host), vec![ChromeCommand::HideMain]);
    host.show_web_route("/settings/clip");
    assert!(!host.chrome_command_lifecycle_allowed(ChromeCommand::HideMain));
}

#[test]
fn old_animation_frames_stay_canceled_after_hide_and_reopen() {
    let (_root, host) = fixture();
    for tucked in [false, true] {
        host.inner.lock().edge_tucked = tucked;
        let generation = host.begin_clipboard_animation(tucked).unwrap();
        assert!(host.clipboard_animation_is_current(generation, tucked));
        host.window_hide();
        assert!(!host.clipboard_animation_is_current(generation, tucked));
        host.request_show();
        if tucked {
            apply_edge_tuck_locked(&mut host.inner.lock());
        }
        assert!(
            !host.clipboard_animation_is_current(generation, tucked),
            "returning to the same state must not revive the old frame"
        );
        let current = host.begin_clipboard_animation(tucked).unwrap();
        assert!(host.clipboard_animation_is_current(current, tucked));
    }
}

#[test]
fn new_animation_and_manual_move_permanently_cancel_previous_frames() {
    let (_root, host) = fixture();
    // Both commands may be pending before the first animation starts. A later
    // Show can place the window directly instead of starting another animation.
    host.enqueue_chrome(ChromeCommand::RestoreEdge);
    host.request_show();
    assert!(host.prepare_chrome_command(host.take_chrome_command().unwrap()));
    let queued_restore = host.begin_clipboard_animation(false).unwrap();
    assert!(host.prepare_chrome_command(host.take_chrome_command().unwrap()));
    assert!(!host.clipboard_animation_is_current(queued_restore, false));
    let first = host.begin_clipboard_animation(false).unwrap();
    let second = host.begin_clipboard_animation(false).unwrap();
    assert!(!host.clipboard_animation_is_current(first, false));
    assert!(host.clipboard_animation_is_current(second, false));
    host.arm_caption_drag_interaction(false);
    assert!(!host.clipboard_animation_is_current(second, false));
    host.end_window_move();
    assert!(!host.clipboard_animation_is_current(second, false));

    // A native size loop/OLE drag can begin without the title-bar command.
    host.inner.lock().edge_tucked = true;
    let tucked = host.begin_clipboard_animation(true).unwrap();
    host.inner.lock().ole_drag_active = true;
    assert!(!host.clipboard_animation_is_current(tucked, true));
    host.inner.lock().ole_drag_active = false;
    assert!(!host.clipboard_animation_is_current(tucked, true));
    assert!(!host.inner.lock().edge_tucked);
}

#[test]
fn hover_and_collapse_cancel_motion_without_reopening_hidden_window() {
    let (_root, host) = fixture();
    host.inner.lock().edge_tucked = true;
    let tucked = host.begin_clipboard_animation(true).unwrap();
    host.set_pointer_in_host(true);
    assert!(!host.clipboard_animation_is_current(tucked, true));
    assert_eq!(pending(&host), vec![ChromeCommand::RestoreEdge]);
    let restored = host.begin_clipboard_animation(false).unwrap();
    host.apply_window_action("setCollapsed", true);
    assert!(!host.clipboard_animation_is_current(restored, false));
    assert!(host.begin_clipboard_animation(false).is_none());
    host.window_hide();
    host.set_pointer_in_host(true);
    assert!(!host.hide_snapshot().visible);
    assert_eq!(pending(&host), vec![ChromeCommand::Hide]);
    assert!(host.begin_clipboard_animation(false).is_none());
}

#[test]
fn disappeared_edge_target_cancels_tuck_without_full_hide() {
    let (_root, host) = fixture();
    host.inner.lock().has_host_fake_focus = false;
    apply_edge_tuck_locked(&mut host.inner.lock());
    let animation = host.begin_clipboard_animation(true).unwrap();
    host.cancel_edge_tuck();
    let state = host.hide_snapshot();
    assert!(
        state.visible,
        "moving away from an edge must not hide the whole window"
    );
    assert!(!state.has_fake_focus);
    assert!(
        pending(&host).is_empty(),
        "the canceled tuck must not run or create a Hide"
    );
    assert!(!host.clipboard_animation_is_current(animation, true));
    assert!(!host.chrome_command_lifecycle_allowed(ChromeCommand::EdgeHide));
}

#[test]
fn webview_recovery_retires_input_and_preserves_window_and_navigation_state() {
    let (_root, host) = fixture();
    let inputs = [
        ChromeCommand::ClipboardKeyboardFocus,
        ChromeCommand::HostKeyboardFocus("panel"),
        ChromeCommand::HostKeyboardFocus("main"),
    ];
    {
        let mut inner = host.inner.lock();
        inner.ui.panel_visible = true;
        inner.ui.main_visible = true;
        inner.ui.keyboard_focus_host = Some("main");
        inner.is_collapsed = true;
        inner.window_pinned = true;
        inner.ui.panel_pinned = true;
        inner.ui.main_pinned = true;
        inner.clipboard_normal_size = (438.0, 317.0);
        inner.no_focus_session_active = true;
        inner.has_host_fake_focus = true;
        inner.ui.panel_no_focus_session = true;
        inner.ui.panel_fake_focus = true;
        inner.expand_grace_until_ms = 1;
        inner.ui.pending_nav.insert("clipboard", "/favorites?folderId=pending".into());
        inner.ui.pending_nav.insert("panel", "/panel?favorId=pending&embed=1".into());
        inner.ui.pending_nav.insert("main", "/main/settings/clip".into());
    }
    for input in inputs { host.enqueue_chrome(input); }
    let old_tasks: Vec<_> = std::iter::from_fn(|| host.take_chrome_task()).collect();
    assert_eq!(old_tasks.len(), inputs.len());
    for input in inputs { host.enqueue_chrome(input); }
    let (navigation, config, escape_session) = {
        let mut inner = host.inner.lock();
        inner.spa_modal_open = true;
        inner.spa_overlay_open = true;
        inner.has_open_host_overlay = true;
        inner.ui.clipboard_editing = true;
        inner.ui.panel_spa_overlay_open = true;
        (inner.ui.pending_nav.clone(), serde_json::to_value(&inner.config).unwrap(),
            inner.clipboard_escape_session)
    };
    // Main recovery only retires that document's input, not another window's layer.
    host.begin_webview_recovery("main");
    assert!(host.inner.lock().spa_modal_open);
    host.begin_webview_recovery("clipboard");
    host.begin_webview_recovery("panel");
    {
        let inner = host.inner.lock();
        assert!(!inner.clipboard_web_view_ready && !inner.panel_web_view_ready);
        assert!(!inner.spa_modal_open && !inner.spa_overlay_open && !inner.has_open_host_overlay);
        assert!(!inner.ui.clipboard_editing && !inner.ui.panel_spa_overlay_open);
        assert_ne!(inner.clipboard_escape_session, escape_session);
    }
    assert!(pending(&host).is_empty(), "old queued input must also be retired");
    host.set_clipboard_web_view_ready(true);
    host.set_panel_web_view_ready(true);
    for task in old_tasks {
        assert!(host.chrome_command_input_allowed(task.command()));
        assert!(!host.prepare_chrome_task(task), "ready cannot revive old-document input");
    }
    let before_retry = chrono::Utc::now().timestamp_millis();
    for label in ["clipboard", "panel", "main"] { assert!(host.retry_webview_show(label)); }
    assert_eq!(pending(&host), vec![ChromeCommand::Show, ChromeCommand::ShowPanel, ChromeCommand::ShowMain]);
    {
        let inner = host.inner.lock();
        assert!(inner.window_visible && inner.ui.panel_visible && inner.ui.main_visible);
        assert!(inner.is_collapsed && inner.window_pinned && inner.ui.panel_pinned && inner.ui.main_pinned);
        assert_eq!(inner.clipboard_normal_size, (438.0, 317.0));
        assert!(inner.no_focus_session_active && inner.has_host_fake_focus);
        assert!(inner.ui.panel_no_focus_session && inner.ui.panel_fake_focus);
        assert_eq!(inner.ui.keyboard_focus_host, Some("main"));
        assert_eq!(inner.ui.pending_nav, navigation);
        assert_eq!(serde_json::to_value(&inner.config).unwrap(), config);
        assert!(inner.expand_grace_until_ms >= before_retry + crate::hide_policy::EXPAND_GRACE_MS);
    }
}

#[test]
fn webview_recovery_does_not_revive_canceled_sessions_or_hidden_windows() {
    let (_root, host) = fixture();
    let shows = [
        ("clipboard", ChromeCommand::Show),
        ("main", ChromeCommand::ShowMain),
        ("panel", ChromeCommand::ShowPanel),
        ("quick-paste", ChromeCommand::ShowQuickPaste),
        ("copy-toast", ChromeCommand::ShowToast),
        ("super-menu", ChromeCommand::ShowSuperMenu),
    ];
    let old_opening = {
        let mut inner = host.inner.lock();
        inner.ui.main_visible = true;
        inner.ui.panel_visible = true;
        inner.qp.overlay_visible = true;
        inner.qp.overlay_ready = true;
        inner.toast.visible = true;
        inner.toast.generation = 17;
        inner.toast.pending_route = Some("/favorites".into());
        inner.super_menu.visible = true;
        PendingQuickPasteCommand {
            command: QuickPasteCommand::BeginSession,
            source: QuickPasteInputSource::Keyboard,
            cancel_generation: inner.qp.cancel_generation,
            generation: inner.qp.hook_generation,
            press_index: 0,
            origin: None,
            disarm_shortcut_menu: false,
        }
    };
    for (label, _) in shows { assert!(host.retry_webview_show(label)); }
    let old_shows: Vec<_> = std::iter::from_fn(|| host.take_chrome_task()).collect();
    assert_eq!(old_shows.iter().map(|task| task.command()).collect::<Vec<_>>(),
        shows.iter().map(|(_, command)| *command).collect::<Vec<_>>());
    {
        let inner = host.inner.lock();
        assert_eq!(inner.toast.generation, 17, "retry must retain the active toast session");
        assert_eq!(inner.toast.pending_route.as_deref(), Some("/favorites"));
    }
    host.begin_webview_recovery("quick-paste");
    assert!(!host.inner.lock().qp.overlay_ready);
    assert!(!host.qp_begin_session_from_press(Some(old_opening)),
        "a loading request from the lost quick-paste page must stay canceled");
    assert!(!host.retry_webview_show("quick-paste"));
    for (label, _) in shows { host.dismiss_native_window(label); }
    pending(&host);
    for task in old_shows {
        assert!(!host.prepare_chrome_task(task), "a close during recovery must retire its show");
    }
    for (label, _) in shows { assert!(!host.retry_webview_show(label)); }
    assert!(!host.retry_webview_show("unknown"));
    assert!(pending(&host).is_empty(), "retry must not queue a show for a canceled owner");
}
