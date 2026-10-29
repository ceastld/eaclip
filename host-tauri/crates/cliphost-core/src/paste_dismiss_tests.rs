use super::*;

fn fixture(behavior: &str, pinned: bool, can_tuck: bool) -> (tempfile::TempDir, HostState, String) {
    crate::clipboard::force_headless(true);
    let root = tempfile::tempdir().unwrap();
    let host = HostState::open(AppPaths::from_data_root(root.path().to_path_buf()), 28965).unwrap();
    host.add_text(&json!({ "text": "paste-dismiss" })).unwrap();
    let id = host.store().lock().list_clips().unwrap().remove(0).id;
    {
        let mut inner = host.inner.lock();
        // A sentinel root HWND supplies the default paste sender without a
        // native window. host_can_tuck therefore uses its existing config
        // fallback; can_tuck controls that policy input without desktop I/O.
        inner.host_hwnd = isize::MAX;
        inner.config.dismiss_behavior = behavior.into();
        inner.config.enable_edge_hide = can_tuck;
        inner.window_pinned = pinned;
        inner.window_visible = true;
        inner.is_collapsed = false;
        inner.edge_tucked = false;
    }
    // Window dismissal is under test, not native activation or SendInput.
    host.set_input_runner(|_| Ok(()));
    host.set_clipboard_ui_runner(|_| Ok(()));
    while host.take_chrome_command().is_some() {}
    (root, host, id)
}

#[test]
fn paste_dismiss_edge_without_tuck_target_hides_normal_and_quick_paste_host() {
    for quick_paste in [false, true] {
        let (_root, host, id) = fixture("EdgeHide", false, false);
        assert!(!host.host_can_tuck());
        if quick_paste {
            host.qp_tuck_host_for_paste();
        } else {
            // Omitting sourceHandle must resolve to this clipboard root HWND.
            assert_eq!(host.paste_item(&json!({ "id": id })).unwrap()["ok"], true);
        }
        let inner = host.inner.lock();
        assert!(!inner.window_visible, "an unpinned paste must hide when edge tuck is unavailable");
        assert!(!inner.edge_tucked);
        assert!(!inner.is_collapsed);
        assert!(inner.chrome.iter().any(|cmd| matches!(cmd, ChromeCommand::Hide)));
        assert!(!inner.chrome.iter().any(|cmd| matches!(cmd, ChromeCommand::EdgeHide)),
            "unavailable geometry must not leave a no-op tuck queued");
    }
}

#[test]
fn paste_dismiss_preserves_pinned_fold_and_available_edge_tuck() {
    for (behavior, pinned, can_tuck, collapsed, tucked, expected_command) in [
        ("EdgeHide", true, false, false, false, None),
        ("Fold", false, false, true, false, Some(ChromeCommand::Fold)),
        ("EdgeHide", false, true, false, true, Some(ChromeCommand::EdgeHide)),
    ] {
        for quick_paste in [false, true] {
            let (_root, host, id) = fixture(behavior, pinned, can_tuck);
            assert_eq!(host.host_can_tuck(), can_tuck);
            if quick_paste {
                host.qp_tuck_host_for_paste();
            } else {
                assert_eq!(host.paste_item(&json!({ "id": id })).unwrap()["ok"], true);
            }
            let inner = host.inner.lock();
            assert!(inner.window_visible, "pin/fold/available tuck must retain the HWND");
            assert_eq!(inner.is_collapsed, collapsed);
            assert_eq!(inner.edge_tucked, tucked);
            assert!(!inner.chrome.iter().any(|cmd| matches!(cmd, ChromeCommand::Hide)));
            if let Some(expected) = expected_command {
                assert!(inner.chrome.iter().any(|cmd| *cmd == expected));
            } else {
                assert!(!inner.chrome.iter().any(|cmd| matches!(cmd, ChromeCommand::Fold | ChromeCommand::EdgeHide)),
                    "pin must prevent paste dismissal even without an edge target");
            }
        }
    }
}
