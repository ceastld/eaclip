//! Keep root tool windows out of the taskbar and make visibility changes immediate.

use windows::Win32::Foundation::{BOOL, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Threading::{GetCurrentProcessId, GetCurrentThreadId};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_TRANSITIONS_FORCEDISABLED};
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    GetAncestor, GetForegroundWindow, GetGUIThreadInfo, GetWindowLongPtrW, GetWindowThreadProcessId,
    IsChild, IsWindowVisible, SetForegroundWindow, SetWindowLongPtrW, SetWindowPos,
    GA_ROOTOWNER, GWL_EXSTYLE, GUITHREADINFO, HWND_TOP, HWND_TOPMOST, STYLESTRUCT,
    SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE, SWP_NOZORDER,
    WM_NCDESTROY, WM_STYLECHANGING, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
};

const VISIBILITY_SUBCLASS: usize = 172;

/// Reconcile native state after Tao's cached always-on-top setter. Call on the
/// owning UI thread, only for a window whose current policy requires topmost.
pub fn ensure_topmost(hwnd: HWND) {
    unsafe {
        if GetWindowLongPtrW(hwnd, GWL_EXSTYLE) & WS_EX_TOPMOST.0 as isize != 0 { return; }
        if let Err(error) = SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER) {
            tracing::warn!(%error, hwnd = hwnd.0 as usize, "Cannot restore native topmost state");
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShowLayer { Normal, Topmost }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ShowPlan { insert_after: HWND, activate: bool }

fn explicit_show_plan(layer: ShowLayer, activate: bool) -> ShowPlan {
    ShowPlan { insert_after: match layer { ShowLayer::Normal => HWND_TOP, ShowLayer::Topmost => HWND_TOPMOST }, activate }
}

unsafe fn belongs_to_root(root: HWND, window: HWND) -> bool {
    !window.is_invalid() && (window == root || IsChild(root, window).as_bool()
        || GetAncestor(window, GA_ROOTOWNER) == root)
}

/// An explicit user Show always reorders the current native window, even when
/// Tao and WS_EX_TOPMOST already agree. No-focus presentation never activates,
/// changes the saved paste target, or changes the configured topmost policy.
/// Background/recovery paths must not call this helper.
pub fn present_explicit(hwnd: HWND, label: &str, layer: ShowLayer, activate: bool) {
    let plan = explicit_show_plan(layer, activate);
    unsafe {
        let mut pid = 0;
        let tid = GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid != GetCurrentProcessId() || tid != GetCurrentThreadId() {
            tracing::warn!(label, stage = "explicit-show-invalid-owner", error = true, "Host window presentation");
            return;
        }
        // Always apply HWND_TOP(MOST); a cached topmost bit does not say which
        // window is currently at the front of that Z-order band.
        let positioned = SetWindowPos(hwnd, plan.insert_after, 0, 0, 0, 0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER);
        let activation_returned = plan.activate && (belongs_to_root(hwnd, GetForegroundWindow()) || SetForegroundWindow(hwnd).as_bool());
        let foreground_window = GetForegroundWindow();
        let foreground = belongs_to_root(hwnd, foreground_window);
        let observed_tid = if foreground { GetWindowThreadProcessId(foreground_window, None) } else { tid };
        let mut gui = GUITHREADINFO { cbSize: std::mem::size_of::<GUITHREADINFO>() as u32, ..Default::default() };
        let gui_read = GetGUIThreadInfo(observed_tid, &mut gui).is_ok();
        let active = gui_read && belongs_to_root(hwnd, gui.hwndActive);
        let focus_in_root = gui_read && belongs_to_root(hwnd, gui.hwndFocus);
        let visible = IsWindowVisible(hwnd).as_bool();
        let activation_verified = foreground && active && focus_in_root;
        let position_error = positioned.as_ref().err().map(|error| error.code().0);
        if positioned.is_err() || !visible || (plan.activate && !activation_verified) {
            tracing::warn!(label, stage = "explicit-show-verified", topmost = layer == ShowLayer::Topmost,
                position_error, activate_requested = plan.activate, activation_returned,
                foreground, active, focus_in_root, gui_read, visible,
                "Host window presentation incomplete");
        } else {
            tracing::info!(label, stage = "explicit-show-verified", topmost = layer == ShowLayer::Topmost,
                activate_requested = plan.activate, activation_returned,
                foreground, active, focus_in_root, gui_read, visible, "Host window presentation");
        }
    }
}

#[cfg(test)]
mod explicit_show_tests {
    use super::*;

    #[test]
    fn clipboard_no_focus_show_reorders_topmost_without_activation() {
        let plan = explicit_show_plan(ShowLayer::Topmost, false);
        assert_eq!(plan.insert_after, HWND_TOPMOST);
        assert!(!plan.activate);
    }

    #[test]
    fn ordinary_clipboard_show_preserves_layer_and_requests_activation() {
        let plan = explicit_show_plan(ShowLayer::Topmost, true);
        assert_eq!(plan.insert_after, HWND_TOPMOST);
        assert!(plan.activate);
    }

    #[test]
    fn main_show_uses_normal_layer_without_changing_topmost_policy() {
        let plan = explicit_show_plan(ShowLayer::Normal, true);
        assert_eq!(plan.insert_after, HWND_TOP);
        assert!(plan.activate);
    }
}

pub fn configure(hwnd: HWND, label: &str) {
    unsafe {
        // Tao rewrites extended styles from cached flags on show/resize/focus
        // changes. Correct the proposed style before Windows can expose a tab.
        // Only the owned root HWND is subclassed; preserve all other style bits.
        let persistent = SetWindowSubclass(hwnd, Some(visibility_proc), VISIBILITY_SUBCLASS, 0).as_bool();
        if !persistent {
            tracing::warn!(label, "Cannot retain native tool window style");
        }
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let updated = (ex | WS_EX_TOOLWINDOW.0 as isize) & !(WS_EX_APPWINDOW.0 as isize);
        if updated != ex {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, updated);
            let _ = SetWindowPos(hwnd, HWND::default(), 0, 0, 0, 0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED);
        }
        let disabled = BOOL(1);
        let transitions = DwmSetWindowAttribute(hwnd, DWMWA_TRANSITIONS_FORCEDISABLED,
            &disabled as *const _ as *const _, std::mem::size_of_val(&disabled) as u32);
        if let Err(error) = &transitions {
            tracing::warn!(%error, label, "Native window transition suppression unavailable");
        }
        // This DWM attribute supports setting only; record the actual result
        // rather than trying to read it back with DwmGetWindowAttribute.
        tracing::info!(label, hwnd = hwnd.0 as usize, persistent_tool_style = persistent,
            transitions_disabled = transitions.is_ok(), "Tool window visibility configured");
    }
}

unsafe extern "system" fn visibility_proc(
    hwnd: HWND, message: u32, wp: WPARAM, lp: LPARAM, id: usize, _data: usize,
) -> LRESULT {
    if message == WM_STYLECHANGING && wp.0 as isize == GWL_EXSTYLE.0 as isize && lp.0 != 0 {
        let styles = &mut *(lp.0 as *mut STYLESTRUCT);
        styles.styleNew = (styles.styleNew | WS_EX_TOOLWINDOW.0) & !WS_EX_APPWINDOW.0;
        return LRESULT(0);
    }
    if message == WM_NCDESTROY {
        let _ = RemoveWindowSubclass(hwnd, Some(visibility_proc), id);
    }
    DefSubclassProc(hwnd, message, wp, lp)
}
