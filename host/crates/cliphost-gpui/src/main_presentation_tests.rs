//! Native layer regression on off-screen, self-owned HWNDs. These tests never
//! request foreground focus, send keyboard/mouse input, or open a product store.
use super::*;
use windows::{
    core::w,
    Win32::{
        Foundation::{HWND, LPARAM, WPARAM},
        UI::{
            Shell::GetWindowSubclass,
            WindowsAndMessaging::{
                CreateWindowExW, DestroyWindow, GetWindow, GetWindowLongPtrW, IsIconic,
                IsWindowVisible, SendMessageW, SetWindowPos, ShowWindow, GWL_EXSTYLE, GW_HWNDNEXT,
                GW_OWNER,
                HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE,
                SWP_SHOWWINDOW, SW_HIDE, SW_SHOWMINNOACTIVE, SW_SHOWNOACTIVATE, WA_INACTIVE,
                WINDOW_EX_STYLE, WM_ACTIVATE, WM_TIMER, WS_CAPTION, WS_EX_NOACTIVATE,
                WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_MINIMIZEBOX, WS_POPUP, WS_SYSMENU,
            },
        },
    },
};

struct OwnedWindow(HWND);

impl OwnedWindow {
    fn new(ex_style: WINDOW_EX_STYLE) -> Self {
        Self::with_owner(ex_style, None)
    }

    fn with_owner(ex_style: WINDOW_EX_STYLE, owner: Option<HWND>) -> Self {
        let hwnd = unsafe {
            CreateWindowExW(
                ex_style | WS_EX_TOOLWINDOW,
                w!("STATIC"),
                w!("ClipHost self-owned main presentation regression"),
                WS_POPUP | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX,
                -32000,
                -32000,
                64,
                64,
                owner,
                None,
                None,
                None,
            )
        }
        .unwrap();
        Self(hwnd)
    }

    fn ex_style(&self) -> isize {
        unsafe { GetWindowLongPtrW(self.0, GWL_EXSTYLE) }
    }

    fn topmost(&self) -> bool {
        self.ex_style() & WS_EX_TOPMOST.0 as isize != 0
    }

    fn visible(&self) -> bool {
        unsafe { IsWindowVisible(self.0).as_bool() }
    }

    fn temporary_raise_registered(&self) -> bool {
        unsafe {
            let mut policy = 0;
            GetWindowSubclass(
                self.0,
                Some(main_foreground_proc),
                MAIN_FOREGROUND_SUBCLASS,
                Some(&mut policy),
            )
            .as_bool()
        }
    }

    fn raise_without_activation(&self) {
        unsafe {
            SetWindowPos(
                self.0,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_SHOWWINDOW,
            )
        }
        .unwrap();
    }

    fn send_deactivation(&self) {
        unsafe {
            SendMessageW(
                self.0,
                WM_ACTIVATE,
                Some(WPARAM(WA_INACTIVE as usize)),
                Some(LPARAM(0)),
            );
        }
    }
}

impl Drop for OwnedWindow {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.0);
        }
    }
}

/// Compare only the two owned handles in native Z-order. Intervening handles
/// are never inspected or operated on, so another topmost app cannot make this
/// assertion depend on the windows being immediate neighbours.
fn above(upper: &OwnedWindow, lower: &OwnedWindow) -> bool {
    let mut next = upper.0;
    for _ in 0..4096 {
        let Ok(hwnd) = (unsafe { GetWindow(next, GW_HWNDNEXT) }) else {
            return false;
        };
        if hwnd.is_invalid() {
            return false;
        }
        if hwnd == lower.0 {
            return true;
        }
        next = hwnd;
    }
    false
}

#[test]
fn ordinary_main_first_and_repeated_show_raise_above_no_focus_topmost_clipboard() {
    let clipboard = OwnedWindow::new(WS_EX_NOACTIVATE);
    let settings = OwnedWindow::new(WS_EX_NOACTIVATE);
    clipboard.raise_without_activation();
    assert!(!settings.visible());
    assert!(!settings.topmost());

    raw_prepare_main_show(settings.0, false).unwrap();
    assert!(settings.visible());
    assert!(settings.topmost());
    assert_eq!(settings.ex_style() & WS_EX_NOACTIVATE.0 as isize, 0);
    assert!(settings.temporary_raise_registered());
    assert!(above(&settings, &clipboard));
    assert_ne!(clipboard.ex_style() & WS_EX_NOACTIVATE.0 as isize, 0);

    clipboard.raise_without_activation();
    assert!(above(&clipboard, &settings));
    raw_prepare_main_show(settings.0, false).unwrap();
    assert!(above(&settings, &clipboard));
    assert!(settings.temporary_raise_registered());

    settings.send_deactivation();
    assert!(settings.visible());
    assert!(!settings.topmost());
    assert!(!settings.temporary_raise_registered());
    assert!(clipboard.topmost());
}

#[test]
fn hiding_main_restores_ordinary_layer_and_next_show_can_raise_again() {
    let settings = OwnedWindow::new(WS_EX_NOACTIVATE);
    raw_prepare_main_show(settings.0, false).unwrap();
    unsafe {
        let _ = ShowWindow(settings.0, SW_HIDE);
    }
    assert!(!settings.visible());
    assert!(!settings.topmost());
    assert!(!settings.temporary_raise_registered());

    raw_prepare_main_show(settings.0, false).unwrap();
    assert!(settings.visible());
    assert!(settings.topmost());
    assert!(settings.temporary_raise_registered());
    settings.send_deactivation();
    assert!(!settings.topmost());
}

#[test]
fn owned_dialog_preserves_main_raise_until_foreground_leaves_the_owner_family() {
    let settings = OwnedWindow::new(WS_EX_NOACTIVATE);
    raw_prepare_main_show(settings.0, false).unwrap();
    let dialog = OwnedWindow::with_owner(WS_EX_NOACTIVATE, Some(settings.0));
    dialog.raise_without_activation();
    unsafe {
        SendMessageW(
            settings.0,
            WM_ACTIVATE,
            Some(WPARAM(WA_INACTIVE as usize)),
            Some(LPARAM(dialog.0 .0 as isize)),
        );
    }
    assert!(settings.topmost());
    assert!(dialog.topmost());
    assert!(settings.temporary_raise_registered());

    // No foreground request was made by either fixture. Exercise the actual
    // timer path that notices an owned picker has lost the family's foreground.
    unsafe {
        SendMessageW(
            settings.0,
            WM_TIMER,
            Some(WPARAM(MAIN_FOREGROUND_SUBCLASS)),
            Some(LPARAM(0)),
        );
    }
    assert!(!settings.topmost());
    assert!(!dialog.topmost());
    assert!(!settings.temporary_raise_registered());
}

#[test]
fn originally_topmost_main_keeps_its_layer_after_deactivation_and_hide() {
    let settings = OwnedWindow::new(WS_EX_NOACTIVATE | WS_EX_TOPMOST);
    raw_prepare_main_show(settings.0, false).unwrap();
    assert!(!settings.temporary_raise_registered());
    settings.send_deactivation();
    assert!(settings.topmost());
    unsafe {
        let _ = ShowWindow(settings.0, SW_HIDE);
    }
    assert!(settings.topmost());
    raw_restore_main_layer(settings.0).unwrap();
    assert!(settings.topmost());
}

#[test]
fn explicit_pin_supersedes_temporary_main_raise() {
    let settings = OwnedWindow::new(WS_EX_NOACTIVATE);
    raw_prepare_main_show(settings.0, false).unwrap();
    assert!(settings.temporary_raise_registered());

    raw_prepare_main_show(settings.0, true).unwrap();
    assert!(!settings.temporary_raise_registered());
    settings.send_deactivation();
    assert!(settings.topmost());
    unsafe {
        let _ = ShowWindow(settings.0, SW_HIDE);
    }
    assert!(settings.topmost());
    raw_restore_main_layer(settings.0).unwrap();
    assert!(settings.topmost());
}

#[test]
fn minimized_main_is_restored_before_display() {
    let settings = OwnedWindow::new(WS_EX_NOACTIVATE);
    unsafe {
        let _ = ShowWindow(settings.0, SW_SHOWMINNOACTIVE);
        assert!(IsIconic(settings.0).as_bool());
    }
    raw_prepare_main_show(settings.0, false).unwrap();
    assert!(settings.visible());
    assert!(!unsafe { IsIconic(settings.0).as_bool() });
    assert!(settings.topmost());
    settings.send_deactivation();
    assert!(!settings.topmost());
}

#[test]
fn hidden_clipboard_leaves_topmost_band_and_explicit_show_reenters_it() {
    let clipboard = OwnedWindow::new(WS_EX_NOACTIVATE);
    let other = OwnedWindow::new(WINDOW_EX_STYLE::default());
    unsafe {
        let _ = ShowWindow(other.0, SW_SHOWNOACTIVATE);
    }
    clipboard.raise_without_activation();
    assert!(clipboard.visible());
    assert!(clipboard.topmost());
    assert!(above(&clipboard, &other));

    raw_hide(clipboard.0).unwrap();
    assert!(!clipboard.visible());
    assert!(!clipboard.topmost());
    assert!(!other.topmost());

    raw_show(clipboard.0, true, false).unwrap();
    assert!(clipboard.visible());
    assert!(clipboard.topmost());
    assert!(above(&clipboard, &other));
    assert_ne!(clipboard.ex_style() & WS_EX_NOACTIVATE.0 as isize, 0);
}

#[test]
fn hide_drops_topmost_on_the_owner_and_its_owned_window() {
    let owner = OwnedWindow::new(WS_EX_NOACTIVATE);
    let owned = OwnedWindow::with_owner(WS_EX_NOACTIVATE, Some(owner.0));
    owner.raise_without_activation();
    owned.raise_without_activation();
    assert!(owner.topmost());
    assert!(owned.topmost());

    raw_hide(owner.0).unwrap();
    assert!(!owner.visible());
    assert!(!owner.topmost());
    assert!(
        !owned.topmost(),
        "owned window must leave the topmost band with its owner"
    );
}

#[test]
fn hiding_owned_overlay_preserves_visible_topmost_owner_and_sibling() {
    let owner = OwnedWindow::new(WS_EX_NOACTIVATE);
    let popup = OwnedWindow::new(WS_EX_NOACTIVATE);
    let sibling = OwnedWindow::with_owner(WS_EX_NOACTIVATE, Some(owner.0));
    owner.raise_without_activation();
    sibling.raise_without_activation();
    // The number-paste hint and context menus use this native show/hide pair.
    raw_overlay_show(popup.0, owner.0 .0 as isize, (-32000, -32000, 64, 64), 0).unwrap();
    assert_eq!(unsafe { GetWindow(popup.0, GW_OWNER) }.unwrap(), owner.0);
    assert!(popup.visible() && popup.topmost());

    raw_hide(popup.0).unwrap();

    assert!(!popup.visible());
    assert!(!popup.topmost());
    assert!(owner.visible());
    assert!(owner.topmost(), "closing a popup must preserve its visible owner's layer");
    assert!(sibling.visible());
    assert!(sibling.topmost(), "closing a popup must preserve other visible companions");
}

#[test]
fn hidden_overlay_stays_non_topmost_across_owner_reopen_then_rejoins_on_show() {
    let owner = OwnedWindow::new(WS_EX_NOACTIVATE);
    let popup = OwnedWindow::new(WS_EX_NOACTIVATE);
    owner.raise_without_activation();
    raw_overlay_show(popup.0, owner.0 .0 as isize, (-32000, -32000, 64, 64), 0).unwrap();
    raw_hide(popup.0).unwrap();

    raw_hide(owner.0).unwrap();
    raw_show(owner.0, true, false).unwrap();
    assert!(owner.visible() && owner.topmost());
    assert!(!popup.visible());
    assert!(
        !popup.topmost(),
        "reopening the owner must not restore TOPMOST on its retained hidden popup"
    );

    // A later explicit popup show must restore the native ownership contract.
    raw_overlay_show(popup.0, owner.0 .0 as isize, (-32000, -32000, 64, 64), 0).unwrap();
    assert!(popup.visible() && popup.topmost());
    assert!(owner.topmost());
    assert_eq!(unsafe { GetWindow(popup.0, GW_OWNER) }.unwrap(), owner.0);
    assert!(above(&popup, &owner));
}

#[test]
fn hide_of_temporary_main_raise_clears_topmost_and_subclass() {
    let settings = OwnedWindow::new(WS_EX_NOACTIVATE);
    raw_prepare_main_show(settings.0, false).unwrap();
    assert!(settings.temporary_raise_registered());
    raw_hide(settings.0).unwrap();
    assert!(!settings.visible());
    assert!(!settings.topmost());
    assert!(!settings.temporary_raise_registered());
}

#[test]
fn hiding_nested_overlay_clears_its_family_but_preserves_its_visible_owner() {
    let owner = OwnedWindow::new(WS_EX_NOACTIVATE);
    let popup = OwnedWindow::new(WS_EX_NOACTIVATE);
    let nested = OwnedWindow::new(WS_EX_NOACTIVATE);
    owner.raise_without_activation();
    raw_overlay_show(popup.0, owner.0 .0 as isize, (-32000, -32000, 64, 64), 0).unwrap();
    raw_overlay_show(nested.0, popup.0 .0 as isize, (-32000, -32000, 64, 64), 0).unwrap();
    raw_hide(popup.0).unwrap();
    assert!(owner.visible() && owner.topmost());
    assert!(!popup.visible() && !popup.topmost());
    assert!(!nested.topmost(), "nested retained previews must leave the hidden family layer");
    assert_eq!(unsafe { GetWindow(popup.0, GW_OWNER) }.unwrap(), owner.0);
    assert_eq!(unsafe { GetWindow(nested.0, GW_OWNER) }.unwrap(), popup.0);
}

#[test]
fn retained_popup_cleanup_is_deferred_and_outlives_the_presentation_guard() {
    let owner = OwnedWindow::new(WS_EX_NOACTIVATE);
    let popup = OwnedWindow::new(WS_EX_NOACTIVATE);
    owner.raise_without_activation();
    raw_overlay_show(popup.0, owner.0 .0 as isize, (-32000, -32000, 64, 64), 0).unwrap();
    let _expired = guard_condition(|| false);
    hide_retained_overlay(popup.0 .0 as isize).unwrap();
    assert!(popup.visible(), "cleanup must not reenter GPUI from a view destructor");
    unsafe { dispatch_native(popup.0); }
    assert!(!popup.visible());
    assert!(!popup.topmost());
    assert!(owner.visible() && owner.topmost());
}

#[test]
fn pinned_main_drops_topmost_while_hidden_and_reapplies_it_on_show() {
    let settings = OwnedWindow::new(WS_EX_NOACTIVATE);
    raw_prepare_main_show(settings.0, true).unwrap();
    assert!(settings.topmost());
    assert!(!settings.temporary_raise_registered());

    raw_hide(settings.0).unwrap();
    assert!(!settings.visible());
    assert!(!settings.topmost());
    assert!(!settings.temporary_raise_registered());

    raw_prepare_main_show(settings.0, true).unwrap();
    assert!(settings.visible());
    assert!(settings.topmost());
    assert!(!settings.temporary_raise_registered());
}

#[test]
fn failed_activation_rollback_restores_only_temporary_layer() {
    let settings = OwnedWindow::new(WS_EX_NOACTIVATE);
    raw_prepare_main_show(settings.0, false).unwrap();
    // Exercise the exact rollback used by raw_present_main without relying on
    // whether Windows grants a test worker foreground permission.
    raw_restore_main_layer(settings.0).unwrap();
    assert!(settings.visible());
    assert!(!settings.topmost());
    assert!(!settings.temporary_raise_registered());
    raw_restore_main_layer(settings.0).unwrap();
    assert!(!settings.topmost());

    let pinned = OwnedWindow::new(WS_EX_NOACTIVATE);
    raw_prepare_main_show(pinned.0, true).unwrap();
    raw_restore_main_layer(pinned.0).unwrap();
    assert!(pinned.topmost());
}
