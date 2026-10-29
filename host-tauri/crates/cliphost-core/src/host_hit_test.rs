//! C# `HostHitTestPolicy` + `HostHitTest` for no-focus dismiss hit-testing.

/// C# `HostHitTestPolicy.IsScreenPointInHostTree`.
pub fn is_screen_point_in_host_tree(
    hit_hwnd: isize,
    host_hwnd: isize,
    host_root: isize,
    hit_root: isize,
    hit_is_child_of_host: bool,
    point_in_host_bounds: bool,
    hit_is_dismiss_companion: bool,
    hit_is_above_host: bool,
    hit_is_host_client_chrome: bool,
) -> bool {
    if host_hwnd == 0 {
        return false;
    }
    if hit_hwnd == 0 {
        return point_in_host_bounds;
    }
    if hit_hwnd == host_hwnd
        || hit_is_child_of_host
        || hit_is_dismiss_companion
        || hit_is_host_client_chrome
    {
        return true;
    }
    if host_root != 0 && hit_root == host_root {
        return true;
    }
    point_in_host_bounds && !hit_is_above_host
}

/// Pointer engagement can restore a tucked window. A dismissal companion (for
/// example this process's tray menu) must not become an expansion hot area.
pub fn is_screen_point_engaging_host(
    hit_hwnd: isize,
    host_hwnd: isize,
    host_root: isize,
    hit_root: isize,
    hit_is_child_of_host: bool,
    point_in_host_bounds: bool,
    hit_is_above_host: bool,
    hit_root_owner_is_host: bool,
) -> bool {
    let owned_popup = hit_root != 0 && hit_root != host_root && hit_root_owner_is_host;
    if !point_in_host_bounds && !owned_popup {
        return false;
    }
    is_screen_point_in_host_tree(
        hit_hwnd,
        host_hwnd,
        host_root,
        hit_root,
        hit_is_child_of_host,
        point_in_host_bounds,
        false,
        hit_is_above_host,
        hit_root_owner_is_host,
    )
}

/// C# `HostHitTestPolicy.IsHostClientChrome`.
pub fn is_host_client_chrome(hit_root_owner_is_host: bool, hit_is_host_webview_runtime: bool) -> bool {
    hit_root_owner_is_host || hit_is_host_webview_runtime
}

/// C# `HostHitTestPolicy.IsProcessInHostTree`.
pub fn is_process_in_host_tree(
    pid: u32,
    host_pid: u32,
    mut get_parent: impl FnMut(u32) -> u32,
    max_depth: i32,
) -> bool {
    if pid == 0 || host_pid == 0 || max_depth <= 0 {
        return false;
    }
    let mut current = pid;
    for _ in 0..max_depth {
        if current == host_pid {
            return true;
        }
        let parent = get_parent(current);
        if parent == 0 || parent == current {
            return false;
        }
        current = parent;
    }
    false
}

/// C# `HostHitTestPolicy.IsHostRelatedWindow`.
pub fn is_host_related_window(
    host_hwnd: isize,
    foreground_hwnd: isize,
    host_root: isize,
    foreground_root: isize,
    foreground_is_child_or_owner_of_host: bool,
    host_process_id: u32,
    foreground_process_id: u32,
    is_dismiss_companion: bool,
    is_host_client_chrome: bool,
) -> bool {
    if host_hwnd == 0 || foreground_hwnd == 0 {
        return false;
    }
    if foreground_hwnd == host_hwnd
        || foreground_is_child_or_owner_of_host
        || is_dismiss_companion
        || is_host_client_chrome
    {
        return true;
    }
    if host_root != 0 && foreground_root == host_root {
        return true;
    }
    host_process_id != 0 && host_process_id == foreground_process_id
}

/// C# `HostHitTestPolicy.IsDismissCompanion`.
pub fn is_dismiss_companion(
    hit_is_same_process: bool,
    hit_is_quicker_ignore_window: bool,
    hit_is_quicker_no_activate: bool,
) -> bool {
    hit_is_same_process || hit_is_quicker_ignore_window || hit_is_quicker_no_activate
}

/// C# `LowLevelKeyboardPassThroughPolicy`: owned top-level windows receive
/// their own keyboard input; ordinary host children still use fake-focus routing.
pub(crate) fn is_owned_dialog(host_hwnd: isize, foreground_hwnd: isize) -> bool {
    if host_hwnd == 0 || foreground_hwnd == 0 || host_hwnd == foreground_hwnd {
        return false;
    }
    #[cfg(windows)]
    unsafe {
        use windows::Win32::{Foundation::HWND, UI::WindowsAndMessaging::{GetWindow, GW_OWNER}};
        GetWindow(HWND(foreground_hwnd as *mut _), GW_OWNER)
            .map(|owner| owner.0 as isize == host_hwnd)
            .unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// C# `HostHitTest.IsScreenPointInHostTree(screenX, screenY, hostHwnd)`.
pub fn screen_point_in_host_tree(screen_x: i32, screen_y: i32, host_hwnd: isize) -> bool {
    if host_hwnd == 0 {
        return false;
    }
    #[cfg(windows)]
    {
        live_screen_point_in_host_tree(screen_x, screen_y, host_hwnd, true)
    }
    #[cfg(not(windows))]
    {
        let _ = (screen_x, screen_y);
        false
    }
}

/// The actual window tree, owned popups and native bounds that may restore an
/// EdgeHide window. Same-process or Quicker companion windows only guard dismiss.
pub fn screen_point_engages_host(screen_x: i32, screen_y: i32, host_hwnd: isize) -> bool {
    if host_hwnd == 0 {
        return false;
    }
    #[cfg(windows)]
    {
        live_screen_point_in_host_tree(screen_x, screen_y, host_hwnd, false)
    }
    #[cfg(not(windows))]
    {
        let _ = (screen_x, screen_y);
        false
    }
}

/// Only the clipboard's exposed frame can restore an EdgeHide window. Owned
/// surfaces such as the super menu must not turn their entire area into a
/// restore hot zone, even when they overlap the exposed strip.
pub fn screen_point_engages_edge_strip(screen_x: i32, screen_y: i32, host_hwnd: isize) -> bool {
    if host_hwnd == 0 {
        return false;
    }
    #[cfg(windows)]
    {
        let in_bounds = crate::edge_hide::visible_window_rect(host_hwnd)
            .or_else(|| crate::edge_hide::window_rect(host_hwnd))
            .is_some_and(|rect| {
                f64::from(screen_x) >= rect.left && f64::from(screen_x) < rect.right()
                    && f64::from(screen_y) >= rect.top && f64::from(screen_y) < rect.bottom()
            });
        if !in_bounds {
            return false;
        }
        let hit = crate::window_menu::window_from_point(screen_x, screen_y);
        // GA_ROOT includes real children but deliberately does not follow the
        // GW_OWNER relationship used by independent top-level surfaces.
        hit == 0 || root_window(hit) == root_window(host_hwnd)
    }
    #[cfg(not(windows))]
    {
        let _ = (screen_x, screen_y);
        false
    }
}

/// Owned top-level popups extend the pointer area without accepting stale WebView
/// child bounds or windows belonging to another ClipHost surface.
pub fn screen_point_in_owned_popup(x: i32, y: i32, host_hwnd: isize) -> bool {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::{Foundation::HWND, UI::WindowsAndMessaging::{GetWindow, IsWindowVisible, GW_OWNER}};
        let hit = crate::window_menu::window_from_point(x, y);
        let root = root_window(hit);
        let owner = GetWindow(HWND(root as *mut _), GW_OWNER).map(|hwnd| hwnd.0 as isize).unwrap_or(0);
        return owned_popup_hit(host_hwnd, root, owner,
            root != 0 && IsWindowVisible(HWND(root as *mut _)).as_bool(),
            window_pid(root) == window_pid(host_hwnd));
    }
    #[cfg(not(windows))]
    { let _ = (x, y, host_hwnd); false }
}

fn owned_popup_hit(host: isize, root: isize, owner: isize, visible: bool, same_process: bool) -> bool {
    host != 0 && root != 0 && root != host && owner == host && visible && same_process
}

/// Keep one owner-to-popup link without enumerating desktop windows at each poll.
/// Bind on the owner's UI thread; its root-only subclass removes our property
/// before WM_NCDESTROY reaches the original window procedure.
pub fn register_native_hover_popup(host: isize, popup: isize) -> Result<(), String> {
    #[cfg(windows)]
    unsafe {
        use windows::{core::w, Win32::{Foundation::{HANDLE, HWND}, System::Threading::GetCurrentThreadId,
            UI::{Shell::{SetWindowSubclass, RemoveWindowSubclass}, WindowsAndMessaging::{GetWindowThreadProcessId, SetPropW}}}};
        if host == 0 || popup == 0 || host == popup
            || window_pid(host) != std::process::id() || window_pid(popup) != std::process::id()
        { return Err("Hover link requires two current-process windows".into()); }
        let hwnd = HWND(host as *mut _);
        if GetWindowThreadProcessId(hwnd, None) != GetCurrentThreadId() {
            return Err("Hover link must be bound on the owner UI thread".into());
        }
        if !SetWindowSubclass(hwnd, Some(native_hover_owner_proc), 71, 0).as_bool() {
            return Err("Cannot register hover owner cleanup".into());
        }
        let result = SetPropW(hwnd, w!("ClipHost.NativeHoverPreview"), HANDLE(popup as *mut _));
        if result.is_err() { let _ = RemoveWindowSubclass(hwnd, Some(native_hover_owner_proc), 71); }
        return result.map_err(|error| error.to_string());
    }
    #[cfg(not(windows))]
    { let _ = (host, popup); Err("Native hover is unavailable".into()) }
}

#[cfg(windows)]
unsafe extern "system" fn native_hover_owner_proc(
    hwnd: windows::Win32::Foundation::HWND, message: u32,
    wp: windows::Win32::Foundation::WPARAM, lp: windows::Win32::Foundation::LPARAM,
    id: usize, _data: usize,
) -> windows::Win32::Foundation::LRESULT {
    use windows::{core::w, Win32::UI::{Shell::{DefSubclassProc, RemoveWindowSubclass},
        WindowsAndMessaging::{RemovePropW, WM_NCDESTROY}}};
    if message == WM_NCDESTROY {
        let _ = RemovePropW(hwnd, w!("ClipHost.NativeHoverPreview"));
        let _ = RemoveWindowSubclass(hwnd, Some(native_hover_owner_proc), id);
    }
    DefSubclassProc(hwnd, message, wp, lp)
}

/// A visible preview includes its own 180ms cursor-leave grace. Only automatic
/// polling uses this: it must not create fake focus or block explicit dismissal.
pub fn has_visible_native_hover_popup(host: isize) -> bool {
    #[cfg(windows)]
    unsafe {
        use windows::{core::w, Win32::{Foundation::HWND, UI::WindowsAndMessaging::{GetPropW, GetWindow, IsWindowVisible, GW_OWNER}}};
        if host == 0 || window_pid(host) != std::process::id() { return false; }
        let popup = GetPropW(HWND(host as *mut _), w!("ClipHost.NativeHoverPreview")).0 as isize;
        if popup == 0 { return false; }
        let owner = GetWindow(HWND(popup as *mut _), GW_OWNER).map(|h| h.0 as isize).unwrap_or(0);
        return owned_popup_hit(host, popup, owner, IsWindowVisible(HWND(popup as *mut _)).as_bool(),
            window_pid(popup) == std::process::id());
    }
    #[cfg(not(windows))]
    { let _ = host; false }
}

/// C# `HostHitTest.IsHostRelatedWindow(hostHwnd, otherHwnd)`.
pub fn host_related_hwnd(host_hwnd: isize, other_hwnd: isize) -> bool {
    if host_hwnd == 0 || other_hwnd == 0 {
        return false;
    }
    #[cfg(windows)]
    {
        live_host_related(host_hwnd, other_hwnd)
    }
    #[cfg(not(windows))]
    {
        host_hwnd == other_hwnd
    }
}

#[cfg(windows)]
fn live_screen_point_in_host_tree(
    screen_x: i32,
    screen_y: i32,
    host_hwnd: isize,
    include_dismiss_companions: bool,
) -> bool {
    let hit = crate::window_menu::window_from_point(screen_x, screen_y);
    let host_root = root_window(host_hwnd);
    let hit_root = if hit == 0 { 0 } else { root_window(hit) };
    let in_bounds = if include_dismiss_companions {
        window_contains_point(host_hwnd, screen_x, screen_y)
    } else {
        // The edge restore strip uses DWM's current visible frame. A child's
        // old unclipped bounds or an invisible resize border must not widen it.
        crate::edge_hide::visible_window_rect(host_hwnd)
            .or_else(|| crate::edge_hide::window_rect(host_hwnd))
            .is_some_and(|rect| {
                f64::from(screen_x) >= rect.left && f64::from(screen_x) < rect.right()
                    && f64::from(screen_y) >= rect.top && f64::from(screen_y) < rect.bottom()
            })
    };
    let hit_root_owner = if hit == 0 { 0 } else { root_owner_window(hit) };
    let owner_is_host = hit_root_owner == host_hwnd || (host_root != 0 && hit_root_owner == host_root);
    let hit_is_above = hit_root != 0
        && host_root != 0
        && hit_root != host_root
        && is_above_in_z_order(hit_root, host_root);
    let hit_is_child_of_host = hit != 0 && is_equal_or_parent_of(host_hwnd, hit);
    if !include_dismiss_companions {
        return is_screen_point_engaging_host(
            hit,
            host_hwnd,
            host_root,
            hit_root,
            hit_is_child_of_host,
            in_bounds,
            hit_is_above,
            owner_is_host,
        );
    }
    let host_pid = window_pid(host_hwnd);
    let companion = hit != 0 && dismiss_companion(hit, host_pid);
    let client_chrome = hit != 0 && is_host_client_chrome(owner_is_host, is_host_webview_runtime(hit, host_pid));
    is_screen_point_in_host_tree(
        hit,
        host_hwnd,
        host_root,
        hit_root,
        hit_is_child_of_host,
        in_bounds,
        companion,
        hit_is_above,
        client_chrome,
    )
}

#[cfg(windows)]
fn live_host_related(host_hwnd: isize, other_hwnd: isize) -> bool {
    let host_pid = window_pid(host_hwnd);
    let host_root = root_window(host_hwnd);
    let other_root_owner = root_owner_window(other_hwnd);
    let owner_is_host =
        other_root_owner == host_hwnd || (host_root != 0 && other_root_owner == host_root);
    is_host_related_window(
        host_hwnd,
        other_hwnd,
        host_root,
        root_window(other_hwnd),
        is_equal_or_parent_of(host_hwnd, other_hwnd) || is_equal_or_parent_of(other_hwnd, host_hwnd),
        host_pid,
        window_pid(other_hwnd),
        dismiss_companion(other_hwnd, host_pid),
        is_host_client_chrome(owner_is_host, is_host_webview_runtime(other_hwnd, host_pid)),
    )
}

/// C# `HostHitTest.IsAboveInZOrder`: walk `GW_HWNDNEXT` from `upper` until `lower`.
pub fn is_above_in_z_order(upper: isize, lower: isize) -> bool {
    if upper == 0 || lower == 0 || upper == lower {
        return false;
    }
    #[cfg(windows)]
    unsafe {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{GetWindow, GW_HWNDNEXT};
        let mut hwnd = GetWindow(HWND(upper as *mut _), GW_HWNDNEXT)
            .map(|h| h.0 as isize)
            .unwrap_or(0);
        while hwnd != 0 {
            if hwnd == lower {
                return true;
            }
            hwnd = GetWindow(HWND(hwnd as *mut _), GW_HWNDNEXT)
                .map(|h| h.0 as isize)
                .unwrap_or(0);
        }
        false
    }
    #[cfg(not(windows))]
    false
}

#[cfg(windows)]
fn root_window(hwnd: isize) -> isize {
    unsafe {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{GetAncestor, GetDesktopWindow, GA_ROOT};
        let h = HWND(hwnd as *mut _);
        let ancestor = GetAncestor(h, GA_ROOT);
        if ancestor.0.is_null() || ancestor == GetDesktopWindow() {
            hwnd
        } else {
            ancestor.0 as isize
        }
    }
}

#[cfg(windows)]
fn root_owner_window(hwnd: isize) -> isize {
    unsafe {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{GetAncestor, GetDesktopWindow, GA_ROOTOWNER};
        let h = HWND(hwnd as *mut _);
        let ancestor = GetAncestor(h, GA_ROOTOWNER);
        if ancestor.0.is_null() || ancestor == GetDesktopWindow() {
            hwnd
        } else {
            ancestor.0 as isize
        }
    }
}

#[cfg(windows)]
fn is_equal_or_parent_of(host: isize, target: isize) -> bool {
    unsafe {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::GetParent;
        let mut cur = target;
        for _ in 0..32 {
            if cur == 0 {
                return false;
            }
            if cur == host {
                return true;
            }
            cur = GetParent(HWND(cur as *mut _))
                .map(|h| h.0 as isize)
                .unwrap_or(0);
        }
        false
    }
}

#[cfg(windows)]
fn window_contains_point(hwnd: isize, x: i32, y: i32) -> bool {
    unsafe {
        use windows::Win32::Foundation::{HWND, RECT};
        use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;
        let mut rc = RECT::default();
        if GetWindowRect(HWND(hwnd as *mut _), &mut rc).is_err() {
            return false;
        }
        crate::hide_policy::point_in_window_rect(x, y, rc.left, rc.top, rc.right, rc.bottom, 0)
    }
}

#[cfg(windows)]
fn window_pid(hwnd: isize) -> u32 {
    unsafe {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
        let mut pid = 0u32;
        GetWindowThreadProcessId(HWND(hwnd as *mut _), Some(&mut pid));
        pid
    }
}

#[cfg(windows)]
fn window_title_and_process(hwnd: isize) -> (String, String) {
    unsafe {
        use windows::Win32::Foundation::{CloseHandle, HWND};
        use windows::Win32::System::Threading::{
            OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        use windows::Win32::UI::WindowsAndMessaging::{GetWindowTextW, GetWindowThreadProcessId};
        let h = HWND(hwnd as *mut _);
        let mut title_buf = [0u16; 512];
        let tn = GetWindowTextW(h, &mut title_buf);
        let title = String::from_utf16_lossy(&title_buf[..tn as usize]);
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, Some(&mut pid));
        if pid == 0 {
            return (title, String::new());
        }
        let Ok(proc) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return (title, String::new());
        };
        let mut buf = vec![0u16; 520];
        let mut size = buf.len() as u32;
        let name = if QueryFullProcessImageNameW(
            proc,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut size,
        )
        .is_ok()
        {
            csharp_process_name(&String::from_utf16_lossy(&buf[..size as usize]))
        } else {
            String::new()
        };
        let _ = CloseHandle(proc);
        (title, name)
    }
}

/// C# `Process.ProcessName`: file stem without `.exe`.
fn csharp_process_name(path_or_name: &str) -> String {
    let file = path_or_name
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(path_or_name);
    let lower = file.to_ascii_lowercase();
    if let Some(stem) = lower.strip_suffix(".exe") {
        file[..stem.len()].to_string()
    } else {
        file.to_string()
    }
}

#[cfg(windows)]
fn is_webview_runtime_name(process_name: &str) -> bool {
    let key = process_name.to_ascii_lowercase();
    key == "msedgewebview2" || key.contains("webview2")
}

#[cfg(windows)]
fn is_host_webview_runtime(hit_hwnd: isize, host_pid: u32) -> bool {
    if hit_hwnd == 0 || host_pid == 0 {
        return false;
    }
    let (_, proc) = window_title_and_process(hit_hwnd);
    if !is_webview_runtime_name(&proc) {
        return false;
    }
    is_process_in_host_tree(window_pid(hit_hwnd), host_pid, parent_process_id, 16)
}

#[cfg(windows)]
pub(crate) fn dismiss_companion(hit_hwnd: isize, host_pid: u32) -> bool {
    if hit_hwnd == 0 {
        return false;
    }
    let hit_pid = window_pid(hit_hwnd);
    let (title, proc) = window_title_and_process(hit_hwnd);
    let same = host_pid != 0 && host_pid == hit_pid;
    let ignore = crate::window_menu::is_quicker_ignore_window(&proc, &title);
    let quicker_no_activate = proc == "Quicker" && hwnd_no_activate(hit_hwnd);
    is_dismiss_companion(same, ignore, quicker_no_activate)
}

#[cfg(windows)]
fn hwnd_no_activate(hwnd: isize) -> bool {
    unsafe {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{GetWindowLongW, GWL_EXSTYLE};
        const WS_EX_NOACTIVATE: i32 = 0x0800_0000;
        GetWindowLongW(HWND(hwnd as *mut _), GWL_EXSTYLE) & WS_EX_NOACTIVATE != 0
    }
}

/// C# `NativeMethods.GetParentProcessId` via Toolhelp (`th32ParentProcessID`).
pub fn parent_process_id(pid: u32) -> u32 {
    if pid == 0 {
        return 0;
    }
    #[cfg(windows)]
    unsafe {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
        };
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return 0;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut found = 0u32;
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                if entry.th32ProcessID == pid {
                    found = entry.th32ParentProcessID;
                    break;
                }
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
        found
    }
    #[cfg(not(windows))]
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlapping_foreign_toplevel_is_outside() {
        assert!(!is_screen_point_in_host_tree(
            200, 100, 100, 200, false, true, false, true, false
        ));
    }

    #[test]
    fn webview_or_owned_client_chrome_above_host_is_inside() {
        assert!(is_host_client_chrome(true, false));
        assert!(is_host_client_chrome(false, true));
        assert!(!is_host_client_chrome(false, false));
        assert!(is_screen_point_in_host_tree(
            500, 100, 100, 500, false, true, false, true, true
        ));
    }

    #[test]
    fn foreign_webview2_above_host_is_outside() {
        assert!(!is_host_client_chrome(false, false));
        assert!(!is_screen_point_in_host_tree(
            500, 100, 100, 500, false, true, false, true, false
        ));
        assert!(!is_host_related_window(
            100, 500, 100, 500, false, 42, 99, false, false
        ));
    }

    #[test]
    fn process_tree_treats_only_host_descendants_as_ours() {
        let get_parent = |pid: u32| match pid {
            200 => 100,
            201 => 200,
            300 => 1,
            _ => 0,
        };
        assert!(is_process_in_host_tree(100, 100, get_parent, 16));
        assert!(is_process_in_host_tree(200, 100, get_parent, 16));
        assert!(is_process_in_host_tree(201, 100, get_parent, 16));
        assert!(!is_process_in_host_tree(300, 100, get_parent, 16));
        assert!(!is_process_in_host_tree(0, 100, get_parent, 16));
        assert!(!is_process_in_host_tree(200, 0, get_parent, 16));
        assert!(!is_process_in_host_tree(200, 100, |_| 200, 16));
    }

    #[test]
    fn parent_pid_query_walks_this_process() {
        let pid = std::process::id();
        let parent = parent_process_id(pid);
        assert_ne!(parent, 0);
        assert!(is_process_in_host_tree(pid, parent, parent_process_id, 16));
        assert!(!is_process_in_host_tree(parent, pid, parent_process_id, 16));
    }

    #[test]
    fn foreign_window_behind_host_chrome_is_inside() {
        assert!(is_screen_point_in_host_tree(
            200, 100, 100, 200, false, true, false, false, false
        ));
    }

    #[test]
    fn window_from_point_miss_falls_back_to_host_bounds() {
        assert!(is_screen_point_in_host_tree(
            0, 100, 100, 0, false, true, false, false, false
        ));
        assert!(!is_screen_point_in_host_tree(
            0, 100, 100, 0, false, false, false, false, false
        ));
    }

    #[test]
    fn same_root_or_child_is_inside() {
        assert!(is_screen_point_in_host_tree(
            100, 100, 100, 100, false, false, false, false, false
        ));
        assert!(is_screen_point_in_host_tree(
            150, 100, 100, 100, true, false, false, false, false
        ));
        assert!(is_screen_point_in_host_tree(
            151, 100, 100, 100, false, false, false, false, false
        ));
    }

    #[test]
    fn same_process_foreground_is_host_related() {
        assert!(is_host_related_window(
            100, 300, 100, 300, false, 42, 42, false, false
        ));
        assert!(!is_host_related_window(
            100, 300, 100, 300, false, 42, 7, false, false
        ));
    }

    #[test]
    fn same_process_or_quicker_panel_is_inside() {
        assert!(is_screen_point_in_host_tree(
            300, 100, 100, 300, false, false, true, false, false
        ));
        assert!(!is_screen_point_in_host_tree(
            300, 100, 100, 300, false, true, false, true, false
        ));
    }

    #[test]
    fn tray_menu_guards_dismiss_without_engaging_clipboard_pointer() {
        // A same-process tray menu is outside the clipboard's bounds and is
        // owned by the tray HWND. It can keep a window open, never restore it.
        assert!(is_screen_point_in_host_tree(
            300, 100, 100, 300, false, false, true, true, false
        ));
        assert!(!is_screen_point_engaging_host(
            300, 100, 100, 300, false, false, true, false
        ));
        // The same rule applies to another surface's WebView runtime and to
        // Quicker companions: process membership is not pointer geometry.
        assert!(!is_screen_point_engaging_host(
            500, 100, 100, 500, false, false, true, false
        ));
    }

    #[test]
    fn pointer_engagement_keeps_host_children_owned_popups_and_bounds() {
        assert!(is_screen_point_engaging_host(
            150, 100, 100, 100, true, true, false, false
        ));
        assert!(is_screen_point_engaging_host(
            200, 100, 100, 200, false, false, true, true
        ));
        assert!(is_screen_point_engaging_host(
            0, 100, 100, 0, false, true, false, false
        ));
        assert!(!is_screen_point_engaging_host(
            500, 100, 100, 500, false, true, true, false
        ));
        // A WebView child can retain its expanded geometry while the actual
        // clipboard has moved off screen. The visible restore strip is the gate.
        assert!(!is_screen_point_engaging_host(
            150, 100, 100, 100, true, false, false, true
        ));
    }

    #[test]
    fn quicker_panel_is_host_related_companion() {
        assert!(is_dismiss_companion(false, true, false));
        assert!(is_dismiss_companion(true, false, false));
        assert!(!is_dismiss_companion(false, false, false));
        assert!(is_host_related_window(
            100, 400, 100, 400, false, 42, 7, true, false
        ));
    }

    #[test]
    fn owned_host_chrome_is_host_related_even_when_root_differs() {
        assert!(is_host_related_window(
            100, 500, 100, 500, false, 42, 99, false, true
        ));
        assert!(!is_host_related_window(
            100, 500, 100, 500, false, 42, 99, false, false
        ));
    }

    #[test]
    fn owned_popup_pointer_area_requires_its_visible_owner() {
        assert!(owned_popup_hit(100, 200, 100, true, true));
        assert!(!owned_popup_hit(100, 200, 300, true, true));
        assert!(!owned_popup_hit(100, 200, 100, false, true));
        assert!(!owned_popup_hit(100, 200, 100, true, false));
        assert!(!owned_popup_hit(100, 100, 100, true, true));
        assert!(!owned_popup_hit(0, 200, 0, true, true));
    }

    #[test]
    fn z_order_same_hwnd_is_not_above() {
        assert!(!is_above_in_z_order(0, 1));
        assert!(!is_above_in_z_order(1, 0));
        assert!(!is_above_in_z_order(10, 10));
    }

    #[test]
    fn csharp_process_name_strips_exe_like_diagnostics_process() {
        assert_eq!(csharp_process_name(r"C:\Apps\Quicker.exe"), "Quicker");
        assert_eq!(csharp_process_name("msedgewebview2.EXE"), "msedgewebview2");
        assert_eq!(csharp_process_name("explorer"), "explorer");
    }
}

#[cfg(all(test, windows))]
mod native_hover_cleanup_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use windows::{core::{w, HSTRING, PCWSTR}, Win32::{
        Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM},
        System::LibraryLoader::GetModuleHandleW, UI::WindowsAndMessaging::*,
    }};
    static CLEANED_BEFORE_BASE: AtomicBool = AtomicBool::new(false);
    unsafe extern "system" fn base(hwnd: HWND, message: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        if message == WM_NCDESTROY {
            CLEANED_BEFORE_BASE.store(GetPropW(hwnd, w!("ClipHost.NativeHoverPreview")).0.is_null(), Ordering::SeqCst);
        }
        DefWindowProcW(hwnd, message, wp, lp)
    }
    #[test]
    fn native_hover_link_is_removed_before_original_window_destroy() {
        unsafe {
            CLEANED_BEFORE_BASE.store(false, Ordering::SeqCst);
            let instance = HINSTANCE(GetModuleHandleW(None).unwrap().0);
            let class = HSTRING::from(format!("ClipHostHoverCleanup{}", std::process::id()));
            assert_ne!(RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(base), hInstance: instance,
                lpszClassName: PCWSTR(class.as_ptr()), ..Default::default() }), 0);
            let owner = CreateWindowExW(WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW, &class, &class, WS_POPUP,
                0, 0, 1, 1, None, None, instance, None).unwrap();
            let popup = CreateWindowExW(WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW, w!("STATIC"), w!("Hover cleanup popup"),
                WS_POPUP, 0, 0, 1, 1, owner, None, instance, None).unwrap();
            register_native_hover_popup(owner.0 as isize, popup.0 as isize).unwrap();
            assert_eq!(GetPropW(owner, w!("ClipHost.NativeHoverPreview")).0, popup.0);
            DestroyWindow(popup).unwrap();
            DestroyWindow(owner).unwrap();
            assert!(CLEANED_BEFORE_BASE.load(Ordering::SeqCst));
            UnregisterClassW(&class, instance).unwrap();
        }
    }
}
