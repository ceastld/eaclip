//! Real HostState keyboard admission in an explicit repository-local test root.
//! These tests call the hook router directly; they never send OS input, query
//! a real clipboard sequence, create windows or start a production host.
use super::*;

const OWNER_HWND: isize = 100;
const TARGET_HWND: isize = 200;
const LEASE: u64 = 7;
const VK_TAB: u32 = 0x09;
const VK_ENTER: u32 = 0x0D;
const VK_ESCAPE: u32 = 0x1B;
const VK_SPACE: u32 = 0x20;

fn fixture() -> (tempfile::TempDir, HostState, NativeFifoPreviewRequest) {
    crate::clipboard::force_headless(true);
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
    std::fs::create_dir_all(&root).unwrap();
    let temp = tempfile::Builder::new()
        .prefix("native-fifo-keyboard-")
        .tempdir_in(root)
        .unwrap();
    let host = HostState::open(AppPaths::from_data_root(temp.path().to_path_buf()), 29065).unwrap();
    host.set_window_runtime_ready(true);
    host.set_clipboard_web_view_ready(true);
    host.set_host_hwnd(OWNER_HWND);
    {
        // Headless foreground/ownership input for the real cached hook path.
        // Lifecycle changes below use HostState's production mutation APIs.
        let mut inner = host.inner.lock();
        inner.window_visible = true;
        inner.config.enable_no_focus_mode = true;
        inner.no_focus_session_active = true;
        inner.has_host_fake_focus = true;
        inner.keyboard_target_hwnd = TARGET_HWND;
        inner.keyboard_target_rebind_until_ms = 0;
        inner.last_fg = TARGET_HWND;
        inner.last_non_host_fg = TARGET_HWND;
        inner.ui.keyboard_focus_host = None;
    }
    assert!(host.acquire_native_context_menu(LEASE, "clipboard"));
    assert!(host.set_native_context_menu_hwnd(LEASE, OWNER_HWND));
    // The FIFO owns this overlay. Synchronize it before opting in to keys.
    host.apply_window_action("setSpaOverlayOpen", true);
    let request = host.capture_native_fifo_preview(LEASE, OWNER_HWND).unwrap();
    while host.take_chrome_command().is_some() {}
    (temp, host, request)
}

fn scope(focused: NativeFifoControl) -> NativeFifoKeyboardScope {
    NativeFifoKeyboardScope {
        generation: 41,
        focus_epoch: 3,
        enabled_order: vec![
            NativeFifoControl::Row(0),
            NativeFifoControl::Row(1),
            NativeFifoControl::Retry,
            NativeFifoControl::Start,
            NativeFifoControl::Cancel,
        ],
        focused,
        selected_row: 0,
        composing: false,
        starting: false,
    }
}

fn key(vk: u32, down: bool) -> KeyEvent {
    KeyEvent {
        vk,
        down,
        alt: false,
        ctrl: false,
        shift: false,
        win: false,
        now_ms: 1,
    }
}

fn route(host: &HostState, vk: u32, down: bool) -> bool {
    host.dispatch_low_level_key_for_foreground(key(vk, down), TARGET_HWND)
}

fn take(host: &HostState) -> NativeFifoKey {
    let event = host.take_native_fifo_key().expect("accepted FIFO key");
    assert!(host.native_fifo_key_is_current(&event));
    event
}

#[test]
fn fifo_host_router_keeps_fast_tab_space_pairs_ahead_of_the_ui_poll() {
    let (_temp, host, request) = fixture();
    let initial = scope(NativeFifoControl::Container);
    host.bind_native_fifo_keyboard(&request, initial.clone())
        .unwrap();
    assert!(route(&host, VK_TAB, true));
    assert!(route(&host, VK_SPACE, true));
    // The renderer still reports its old focus. A same-scope bind must not
    // rewind the keyboard cursor or orphan the already accepted Space press.
    host.bind_native_fifo_keyboard(&request, initial).unwrap();
    assert!(route(&host, VK_TAB, false));
    assert!(route(&host, VK_SPACE, false));

    let tab_down = take(&host);
    let space_down = take(&host);
    let tab_up = take(&host);
    let space_up = take(&host);
    assert_eq!((tab_down.key(), tab_down.down()), ("Tab", true));
    assert_eq!((space_down.key(), space_down.down()), ("Space", true));
    assert_eq!((tab_up.key(), tab_up.down()), ("Tab", false));
    assert_eq!((space_up.key(), space_up.down()), ("Space", false));
    for event in [&tab_down, &space_down, &tab_up, &space_up] {
        assert_eq!(event.control(), NativeFifoControl::Row(0));
        assert_eq!(event.focus_epoch(), 3);
        assert_eq!(event.generation(), 41);
    }
    assert_eq!(tab_down.press_id(), tab_up.press_id());
    assert_eq!(space_down.press_id(), space_up.press_id());
    assert_ne!(tab_down.press_id(), space_down.press_id());
    assert!(host.take_native_fifo_key().is_none());
    assert!(host.native_context_menu_is_current(LEASE));
}

#[test]
fn fifo_escape_queues_one_cancel_and_drains_release_without_generic_lease_close() {
    let (_temp, host, request) = fixture();
    let binding = host
        .bind_native_fifo_keyboard(&request, scope(NativeFifoControl::Start))
        .unwrap();
    assert!(route(&host, VK_ESCAPE, true));
    assert!(
        route(&host, VK_ESCAPE, true),
        "held Escape repeat is swallowed"
    );
    let cancel = take(&host);
    assert_eq!((cancel.key(), cancel.down()), ("Escape", true));
    assert_eq!(cancel.control(), NativeFifoControl::Cancel);
    assert!(!cancel.repeat());
    assert!(
        host.take_native_fifo_key().is_none(),
        "repeat cannot queue a second Cancel"
    );
    assert!(host.native_context_menu_is_current(LEASE));
    assert!(!std::iter::from_fn(|| host.take_chrome_command())
        .any(|command| matches!(command, ChromeCommand::DismissContextMenu(_))));

    // UI retirement between Cancel down and up keeps the physical release
    // owned by FIFO, rather than lending it to generic Escape or a new popup.
    host.retire_native_fifo_keyboard(&binding);
    assert!(route(&host, VK_ESCAPE, false));
    assert!(host.take_native_fifo_key().is_none());
    assert!(host.native_context_menu_is_current(LEASE));
}

#[test]
fn fifo_foreground_round_trip_cannot_rebind_the_original_presentation() {
    let (_temp, host, request) = fixture();
    let original_scope = scope(NativeFifoControl::Start);
    host.bind_native_fifo_keyboard(&request, original_scope.clone())
        .unwrap();
    assert!(route(&host, VK_SPACE, true));
    let original = take(&host);

    // The hook supplies the live foreground, independently of its cached
    // headless value. Returning to A must not revive the retired A binding.
    assert!(!host.dispatch_low_level_key_for_foreground(key(VK_TAB, true), 300));
    assert!(
        route(&host, VK_SPACE, false),
        "old Space up is drain-only on A"
    );
    assert!(!host.native_fifo_key_is_current(&original));
    assert!(host.take_native_fifo_key().is_none());
    assert!(host
        .bind_native_fifo_keyboard(&request, original_scope)
        .is_err());
    assert!(host.native_context_menu_is_current(LEASE));
}

#[test]
fn fifo_lifecycle_round_trips_retire_keys_without_a_poll_during_the_invalid_interval() {
    for mode in ["modal", "runtime", "hwnd"] {
        let (_temp, host, request) = fixture();
        let original_scope = scope(NativeFifoControl::Start);
        host.bind_native_fifo_keyboard(&request, original_scope.clone())
            .unwrap();
        assert!(route(&host, VK_ENTER, true));
        let original = take(&host);
        match mode {
            "modal" => {
                host.apply_window_action("setSpaModalOpen", true);
                host.apply_window_action("setSpaModalOpen", false);
            }
            "runtime" => {
                host.set_window_runtime_ready(false);
                host.set_window_runtime_ready(true);
            }
            _ => {
                host.set_host_hwnd(0);
                host.set_host_hwnd(OWNER_HWND);
            }
        }
        // No key/current/drain call observed the intermediate invalid state.
        assert!(host.native_fifo_preview_is_current(&request), "{mode}");
        assert!(!host.native_fifo_key_is_current(&original), "{mode}");
        assert!(
            host.bind_native_fifo_keyboard(&request, original_scope)
                .is_err(),
            "{mode}"
        );
        assert!(route(&host, VK_ENTER, false), "{mode}: release stays owned");
        assert!(host.take_native_fifo_key().is_none(), "{mode}");
        assert!(host.native_context_menu_is_current(LEASE), "{mode}");
    }
}

#[cfg(windows)]
#[test]
fn fifo_input_busy_raw_foreground_round_trip_cannot_replay_an_accepted_key() {
    use crate::active_window::{deliver_test_win_event, EVENT_SYSTEM_FOREGROUND};
    let (_temp, host, request) = fixture();
    host.bind_native_fifo_keyboard(&request, scope(NativeFifoControl::Start))
        .unwrap();
    assert!(route(&host, VK_ENTER, true));
    let accepted = take(&host);
    crate::hooks::with_win_event_test_host(&host, || {
        let owner = host.inner.lock();
        let started = Instant::now();
        deliver_test_win_event(EVENT_SYSTEM_FOREGROUND, 300);
        deliver_test_win_event(EVENT_SYSTEM_FOREGROUND, TARGET_HWND);
        assert!(started.elapsed() < Duration::from_millis(200));
        assert!(host.input_gap_pending());
        drop(owner);
        assert!(!host.native_fifo_key_is_current(&accepted));
        host.poll_keyboard_work();
        assert!(host.take_native_fifo_key().is_none());
        assert!(!route(&host, VK_ENTER, false));
        assert!(host.take_native_fifo_key().is_none());
    });
}

#[test]
fn fifo_replacement_lease_survives_old_event_validation_and_cleanup() {
    let (_temp, host, request) = fixture();
    let original_binding = host
        .bind_native_fifo_keyboard(&request, scope(NativeFifoControl::Start))
        .unwrap();
    assert!(route(&host, VK_ENTER, true));
    let old_event = take(&host);
    assert!(host.acquire_native_context_menu(8, "clipboard"));
    assert!(host.set_native_context_menu_hwnd(8, OWNER_HWND));
    let replacement = host.capture_native_fifo_preview(8, OWNER_HWND).unwrap();
    host.bind_native_fifo_keyboard(&replacement, scope(NativeFifoControl::Start))
        .unwrap();
    assert!(!host.native_fifo_key_is_current(&old_event));
    host.retire_native_fifo_keyboard(&original_binding);
    assert!(route(&host, VK_ENTER, false));
    assert!(
        host.take_native_fifo_key().is_none(),
        "old Enter up cannot become a new action"
    );
    assert!(route(&host, VK_SPACE, true));
    let current = take(&host);
    assert_eq!(current.control(), NativeFifoControl::Start);
    host.retire_native_fifo_keyboard(&original_binding);
    assert!(!host.native_fifo_key_is_current(&old_event));
    assert!(host.native_fifo_key_is_current(&current));
    assert!(route(&host, VK_SPACE, false));
    let release = take(&host);
    assert_eq!(release.press_id(), current.press_id());
    assert!(host.native_context_menu_is_current(8));
    assert!(!host.native_context_menu_is_current(LEASE));
}

#[test]
fn fifo_external_focus_epoch_drains_old_space_up_then_accepts_a_fresh_press() {
    let (_temp, host, request) = fixture();
    let initial = scope(NativeFifoControl::Row(0));
    let old_binding = host
        .bind_native_fifo_keyboard(&request, initial.clone())
        .unwrap();
    assert!(route(&host, VK_SPACE, true));
    let old_down = take(&host);
    let changed = NativeFifoKeyboardScope {
        focus_epoch: initial.focus_epoch + 1,
        focused: NativeFifoControl::Cancel,
        ..initial
    };
    host.bind_native_fifo_keyboard(&request, changed).unwrap();
    host.retire_native_fifo_keyboard(&old_binding);
    assert!(!host.native_fifo_key_is_current(&old_down));
    assert!(route(&host, VK_SPACE, false));
    assert!(
        host.take_native_fifo_key().is_none(),
        "old release cannot activate Cancel"
    );
    assert!(route(&host, VK_SPACE, true));
    let new_down = take(&host);
    assert_eq!(new_down.control(), NativeFifoControl::Cancel);
    assert_eq!(new_down.focus_epoch(), old_down.focus_epoch() + 1);
    assert_ne!(new_down.press_id(), old_down.press_id());
    assert!(route(&host, VK_SPACE, false));
    let new_up = take(&host);
    assert_eq!(new_up.press_id(), new_down.press_id());
    assert_eq!((new_up.key(), new_up.down()), ("Space", false));
    assert!(host.native_context_menu_is_current(LEASE));
}

#[test]
fn tauri_without_fifo_keyboard_opt_in_keeps_generic_menu_escape() {
    let (_temp, host, _request) = fixture();
    // Capturing the preview does not opt in. The existing WebView caller never
    // binds NativeFifoKeyboard and must retain its generic menu Escape route.
    assert!(route(&host, VK_ESCAPE, true));
    assert!(!host.native_context_menu_is_current(LEASE));
    assert!(route(&host, VK_ESCAPE, true));
    assert!(route(&host, VK_ESCAPE, false));
    let commands = std::iter::from_fn(|| host.take_chrome_command()).collect::<Vec<_>>();
    assert_eq!(
        commands
            .iter()
            .filter(|command| matches!(command, ChromeCommand::DismissContextMenu(LEASE)))
            .count(),
        1
    );
    assert!(host.take_native_fifo_key().is_none());
    assert!(!host.inner.lock().native_context_menu_escape_down);
    assert!(host.inner.lock().window_visible);
}
