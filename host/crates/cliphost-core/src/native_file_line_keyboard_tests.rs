//! Exercise the real cached input router without OS hooks or clipboard access.
use super::*;

const POPUP: isize = 101;
const FOREGROUND: isize = 200;
const PRESENTATION: u64 = 10;
const REGIONS: &[(i32, i32, i32, i32)] = &[(0, 0, 800, 74), (50, 80, 264, 200)];

fn fixture() -> (tempfile::TempDir, HostState) {
    crate::clipboard::force_headless(true);
    let scratch = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
    std::fs::create_dir_all(&scratch).unwrap();
    let temp = tempfile::Builder::new().prefix("file-line-keyboard-").tempdir_in(scratch).unwrap();
    let host = HostState::open(AppPaths::from_data_root(temp.path().join("data")), 29065).unwrap();
    host.set_window_runtime_ready(true);
    host.set_clipboard_web_view_ready(true);
    {
        let mut inner = host.inner.lock();
        inner.extras.file_menu.top_edge_line = true;
        inner.last_fg = FOREGROUND;
        inner.last_non_host_fg = FOREGROUND;
        inner.window_visible = false;
        inner.no_focus_session_active = false;
        inner.has_host_fake_focus = false;
        inner.pointer_in_host = false;
        inner.ui.panel_visible = false;
        inner.ui.keyboard_focus_host = None;
    }
    (temp, host)
}

fn event(vk: u32, down: bool) -> KeyEvent {
    KeyEvent { vk, down, ctrl: false, alt: false, shift: false, win: false, now_ms: 1 }
}
fn arm(host: &HostState, active: bool) -> bool {
    host.register_native_file_line_keyboard(POPUP, PRESENTATION, active, REGIONS)
}
fn route(host: &HostState, vk: u32, down: bool) -> bool {
    host.dispatch_low_level_key_for_foreground(event(vk, down), FOREGROUND)
}
fn take(host: &HostState) -> Vec<NativeFileLineKey> {
    let keys = host.drain_native_file_line_keys(PRESENTATION);
    assert!(keys.iter().all(|key| host.native_file_line_key_is_current(key)));
    keys
}

#[test]
fn file_line_keyboard_only_arms_an_active_enabled_surface() {
    let (_temp, host) = fixture();
    assert!(!arm(&host, false));
    assert!(!route(&host, 0x28, true), "hidden line never steals ArrowDown");
    assert!(!route(&host, 0x28, false));
    host.inner.lock().extras.file_menu.top_edge_line = false;
    assert!(!arm(&host, true));
    assert!(!route(&host, 0x1B, true));
    host.inner.lock().extras.file_menu.top_edge_line = true;
    assert!(!arm(&host, true), "enabling does not revive a retired hover");
    assert!(!arm(&host, false));
    assert!(arm(&host, true));
}

#[test]
fn file_line_keyboard_preserves_fast_navigation_and_executes_actions_once() {
    let (_temp, host) = fixture();
    assert!(arm(&host, true));
    assert!(route(&host, 0x28, true));
    assert!(route(&host, 0x28, true));
    assert!(route(&host, 0x28, false));
    for vk in [0x0D, 0x20, 0x2E, 0x08, 0x1B] {
        assert!(route(&host, vk, true));
        assert!(route(&host, vk, true));
        assert!(route(&host, vk, false));
    }
    assert!(arm(&host, true), "polling must not drop accepted keys");
    assert_eq!(take(&host).iter().map(NativeFileLineKey::key).collect::<Vec<_>>(),
        ["ArrowDown", "ArrowDown", "Enter", "Space", "Delete", "Backspace", "Escape"]);
}

#[test]
fn file_line_keyboard_ctrl_enter_and_undo_keep_their_original_modifiers() {
    let (_temp, host) = fixture();
    assert!(arm(&host, true));
    for (vk, name) in [(0x0D, "Enter"), (0x5A, "Undo")] {
        let down = KeyEvent { ctrl: true, ..event(vk, true) };
        assert!(host.dispatch_low_level_key_for_foreground(down, FOREGROUND));
        assert!(host.dispatch_low_level_key_for_foreground(down, FOREGROUND));
        assert!(route(&host, vk, false), "release is swallowed even when Ctrl released first");
        let keys = take(&host);
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].key(), name);
        assert!(keys[0].ctrl());
    }
}

#[test]
fn file_line_keyboard_escape_release_cannot_reach_the_target_after_dismissal() {
    let (_temp, host) = fixture();
    assert!(arm(&host, true));
    assert!(route(&host, 0x1B, true));
    let accepted = take(&host).pop().unwrap();
    host.clear_native_file_line_keyboard(PRESENTATION);
    assert!(!host.native_file_line_key_is_current(&accepted));
    assert!(route(&host, 0x1B, true), "repeat stays a drain");
    assert!(route(&host, 0x1B, false), "paired release belongs to the dismissed line");
    assert!(!route(&host, 0x1B, true), "next physical Escape belongs to target");
}

#[test]
fn file_line_keyboard_external_typing_ime_and_modified_keys_retire_without_rearming() {
    for (vk, ctrl, shift) in [(0x41, false, false), (0xE5, false, false), (0x46, true, false), (0x1B, false, true)] {
        let (_temp, host) = fixture();
        assert!(arm(&host, true));
        let input = KeyEvent { ctrl, shift, ..event(vk, true) };
        assert!(!host.dispatch_low_level_key_for_foreground(input, FOREGROUND));
        assert!(!arm(&host, true), "polling must not steal a foreground edit");
        assert!(!route(&host, 0x28, true));
        assert!(take(&host).is_empty());
    }
}

#[test]
fn file_line_keyboard_outside_click_foreground_change_and_new_surface_retire_old_events() {
    let (_temp, host) = fixture();
    assert!(arm(&host, true));
    assert!(route(&host, 0x28, true));
    assert!(route(&host, 0x28, false));
    let old = take(&host).pop().unwrap();
    host.inner.lock().native_file_line_keyboard.outside_click(0x0201, 900, 100);
    assert!(!host.native_file_line_key_is_current(&old));
    assert!(!arm(&host, true));
    arm(&host, false);
    assert!(arm(&host, true));
    host.notify_native_super_menu_foreground(FOREGROUND + 1);
    host.notify_native_super_menu_foreground(FOREGROUND);
    assert!(!arm(&host, true), "foreground roundtrip cannot revive keys");
    assert!(host.register_native_file_line_keyboard(POPUP, PRESENTATION + 1, true, REGIONS));
    assert!(!host.native_file_line_key_is_current(&old));
}

#[test]
fn file_line_keyboard_yields_to_native_inputs_drag_and_other_menus() {
    for blocker in ["input", "drag", "menu", "modal"] {
        let (_temp, host) = fixture();
        assert!(arm(&host, true));
        {
            let mut inner = host.inner.lock();
            match blocker {
                "input" => inner.ui.keyboard_focus_host = Some("clipboard"),
                "drag" => inner.ole_drag_active = true,
                "menu" => inner.super_menu.visible = true,
                "modal" => inner.spa_modal_open = true,
                _ => unreachable!(),
            }
        }
        // Call the file-line branch directly: another surface may legitimately
        // consume this key later, but the line must never enqueue it.
        let mut inner = host.inner.lock();
        assert_eq!(host.input_route_native_file_line_key(&mut inner, event(0x28, true), FOREGROUND, false), None);
        assert!(inner.native_file_line_keyboard.inbox.is_empty(), "{blocker}");
    }
}

#[test]
fn file_line_keyboard_does_not_acquire_a_preexisting_press_and_bounds_its_queue() {
    let (_temp, host) = fixture();
    assert!(!route(&host, 0x28, true));
    assert!(arm(&host, true));
    assert!(!route(&host, 0x28, true), "preexisting held Down remains external");
    assert!(!route(&host, 0x28, false));
    assert!(route(&host, 0x28, true));
    for _ in 0..MAX_KEYS { assert!(route(&host, 0x28, true)); }
    assert!(take(&host).is_empty(), "overflow retires instead of replaying a partial sequence");
    assert!(!arm(&host, true));
    assert!(route(&host, 0x28, false));
}

#[test]
fn file_line_keyboard_input_gap_discards_keys_and_does_not_rebind_on_poll() {
    let (_temp, host) = fixture();
    assert!(arm(&host, true));
    assert!(route(&host, 0x0D, true));
    host.mark_input_gap();
    assert!(take(&host).is_empty());
    assert!(!arm(&host, true));
    assert!(!route(&host, 0x0D, false));
    arm(&host, false);
    assert!(arm(&host, true));
}
