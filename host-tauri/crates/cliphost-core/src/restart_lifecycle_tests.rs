use super::*;

fn fixture() -> (tempfile::TempDir, HostState) {
    crate::clipboard::force_headless(true);
    let root = tempfile::tempdir().unwrap();
    let host = HostState::open(AppPaths::from_data_root(root.path().to_path_buf()), 28965).unwrap();
    while host.take_chrome_command().is_some() {}
    (root, host)
}

#[test]
fn pipe_shutdown_queues_native_exit_cleanup() {
    let (_root, host) = fixture();
    host.request_graceful_shutdown();
    let commands: Vec<_> = std::iter::from_fn(|| host.take_chrome_command()).collect();
    assert!(commands.iter().any(|command| matches!(command, ChromeCommand::Exit)),
        "Pipe shutdown must reach Tauri's RunEvent::Exit and native OLE cleanup");
    assert!(host.drain_push_events().iter().any(|(name, _)| name == "hostShutdown"));
}

#[test]
fn restart_persists_window_state_before_launch_and_queues_native_cleanup() {
    let (_root, host) = fixture();
    host.remember_host_window_rect("main", 10.0, 20.0, 500.0, 550.0);
    let result = host.restart_with_launch(true, |prefer_medium| {
        assert!(prefer_medium, "Forward the requested privilege mode");
        assert!(!host.window_states.has_due(Instant::now() + Duration::from_secs(2)),
            "Pending window state must be flushed before the replacement starts");
        let latest = host.window_states.latest(crate::window_state::MAIN_WINDOW).unwrap();
        assert_eq!((latest.left, latest.top, latest.width, latest.height), (10.0, 20.0, 500.0, 550.0),
            "Keep the last geometry for later property updates after flushing");
        let saved = crate::window_state::load(&host.data_root(), crate::window_state::MAIN_WINDOW).unwrap();
        assert_eq!((saved.left, saved.top, saved.width, saved.height), (10.0, 20.0, 500.0, 550.0));
        assert!(!std::iter::from_fn(|| host.take_chrome_command())
            .any(|command| matches!(command, ChromeCommand::Exit)),
            "Current process must remain alive until launch succeeds");
        Ok(())
    });
    assert_eq!(result["ok"], true);
    assert!(std::iter::from_fn(|| host.take_chrome_command())
        .any(|command| matches!(command, ChromeCommand::Exit)));
}

#[test]
fn rejected_restart_retains_current_host_and_allows_retry() {
    let (_root, host) = fixture();
    let result = host.restart_with_launch(false, |_| Err("launch rejected".into()));
    assert_eq!(result["ok"], false);
    assert_eq!(result["error"], "launch rejected");
    assert!(!std::iter::from_fn(|| host.take_chrome_command())
        .any(|command| matches!(command, ChromeCommand::Exit)));
    host.request_show();
    assert_eq!(host.health()["service"], HEALTH_SERVICE_ID);
    let result = host.restart_with_launch(false, |_| Ok(()));
    assert_eq!(result["ok"], true);
    assert!(std::iter::from_fn(|| host.take_chrome_command())
        .any(|command| matches!(command, ChromeCommand::Exit)));
}
