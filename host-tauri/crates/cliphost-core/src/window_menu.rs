//! C# `WindowMenuFactory` / `WindowMenuItemWrapper` listing and actions.

use serde_json::{json, Value};

#[derive(Debug, Clone, Default)]
pub struct WindowInfo {
    pub hwnd: i64,
    pub title: String,
    pub process_name: String,
    pub exe_path: String,
    pub class_name: String,
    pub is_foreground: bool,
    pub topmost: bool,
    pub minimized: bool,
    pub maximized: bool,
    pub opacity: u8,
    pub is_explorer: bool,
    pub mouse_penetration: bool,
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
    pub pid: u32,
}

impl WindowInfo {
    pub fn menu_title(&self) -> String {
        let mut sb = String::new();
        if self.is_foreground {
            sb.push_str("【前台】");
        }
        if self.topmost {
            sb.push_str("【置顶】");
        }
        if self.minimized {
            sb.push_str("【最小化】");
        }
        if self.maximized {
            sb.push_str("【最大化】");
        }
        sb.push_str(&self.short_title());
        sb
    }

    pub fn group_title(&self) -> String {
        let mut sb = String::new();
        if self.is_foreground {
            sb.push_str("【前台】");
        }
        sb.push_str(&self.process_name);
        sb.push(' ');
        sb.push_str(&self.short_title());
        sb
    }

    /// C# `WinProperty.GetShortTitle`.
    pub fn short_title(&self) -> String {
        if is_explorer_window_class(&self.class_name) && self.title.chars().count() > 10 {
            std::path::Path::new(&self.title)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| self.title.clone())
        } else {
            self.title.clone()
        }
    }

    pub fn payload(&self) -> Value {
        json!({ "windowHandle": self.hwnd })
    }

    pub fn icon(&self) -> String {
        if self.exe_path.is_empty() {
            "fa:Light_WindowRestore".into()
        } else {
            format!("icon:{}", self.exe_path)
        }
    }

    /// Executable file name shown beside the app name, e.g. `Code.exe`.
    pub fn exe_file_name(&self) -> String {
        std::path::Path::new(&self.exe_path)
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string)
            .unwrap_or_else(|| self.process_name.clone())
    }

    /// Friendly application name (the executable's FileDescription), falling back to the process name.
    pub fn app_name(&self) -> String {
        app_display_name(&self.exe_path, &self.process_name)
    }

    /// C# `WinProperty.IsExplorer` (`ProcessName == "explorer"`).
    pub fn is_explorer_process(&self) -> bool {
        self.process_name.eq_ignore_ascii_case("explorer")
    }
}

/// C# `WinProperty.IsQuickerIgnoreWindow`.
pub fn is_quicker_ignore_window(process_name: &str, title: &str) -> bool {
    process_name == "Quicker"
        && matches!(
            title,
            "SupperMenuWindow" | "FloatButtonWindow" | "CircleMenuWindow"
        )
}

/// C# `WindowEnumerator.EnumFilter` + `WindowMenuFactory.WindowFilter`.
/// Foreground is always a candidate (HashSet starts with FG) even when title/toolwindow would fail EnumFilter.
pub fn include_in_window_menu(
    is_foreground: bool,
    visible: bool,
    tool_window: bool,
    no_activate: bool,
    class_name: &str,
    title: &str,
    process_name: &str,
) -> bool {
    if no_activate {
        return false;
    }
    if matches!(
        class_name,
        "Windows.UI.Core.CoreWindow"
            | "ApplicationFrameWindow"
            | "Windows.Internal.Shell.TabProxyWindow"
    ) {
        return false;
    }
    if is_quicker_ignore_window(process_name, title) {
        return false;
    }
    if !is_foreground && (!visible || tool_window || title.trim().is_empty()) {
        return false;
    }
    true
}

/// C# `WindowGroupMenuItemWrapper.GetLastFocusWindows`: first if not minimized, else last.
pub fn last_focus_window(wins: &[WindowInfo]) -> Option<&WindowInfo> {
    let first = wins.first()?;
    if !first.minimized {
        Some(first)
    } else {
        wins.last()
    }
}

/// C# `WinProperty.IsExplorerWindow` (class, not process name).
fn is_explorer_window_class(class_name: &str) -> bool {
    class_name == "CabinetWClass" || class_name == "CloverWidgitWin_0"
}

pub fn list_windows(skip_hwnd: isize, grouping: bool) -> Vec<WindowEntry> {
    group_window_infos(collect_windows(skip_hwnd), grouping)
}

/// C# `GetAllWindowInternal(withMouse: true, targetHwnd)`: append target when missing, no WindowFilter.
pub fn list_windows_with_mouse(skip_hwnd: isize, grouping: bool, target_hwnd: i64) -> Vec<WindowEntry> {
    let infos = append_mouse_target(collect_windows(skip_hwnd), inspect_raw(target_hwnd));
    group_window_infos(infos, grouping)
}

/// C# `list.Contains(targetHwnd)` then `list.Add(targetHwnd)` without re-running `WindowFilter`.
pub fn append_mouse_target(mut infos: Vec<WindowInfo>, target: Option<WindowInfo>) -> Vec<WindowInfo> {
    let Some(target) = target else {
        return infos;
    };
    if target.hwnd == 0 || infos.iter().any(|info| info.hwnd == target.hwnd) {
        return infos;
    }
    infos.push(target);
    infos
}

fn group_window_infos(mut infos: Vec<WindowInfo>, grouping: bool) -> Vec<WindowEntry> {
    if !grouping {
        return infos.into_iter().map(WindowEntry::Single).collect();
    }
    let mut groups: Vec<(String, Vec<WindowInfo>)> = Vec::new();
    for info in infos.drain(..) {
        if let Some((_, bucket)) = groups.iter_mut().find(|(name, _)| name.eq_ignore_ascii_case(&info.process_name))
        {
            bucket.push(info);
        } else {
            groups.push((info.process_name.clone(), vec![info]));
        }
    }
    groups
        .into_iter()
        .map(|(_, mut wins)| {
            if wins.len() == 1 {
                WindowEntry::Single(wins.remove(0))
            } else {
                WindowEntry::Group(wins)
            }
        })
        .collect()
}

/// C# `WindowMenuFactory.GetAllWindows` — visible top-level HWNDs for thumbnail warmup.
pub fn list_hwnds(skip_hwnd: isize) -> Vec<isize> {
    collect_windows(skip_hwnd)
        .into_iter()
        .map(|info| info.hwnd as isize)
        .collect()
}

/// C# `ExecuteContext.TargetWinp` for a specific HWND (not the Super Menu listing filters).
pub fn query_target(hwnd: i64) -> crate::favor_menu::MenuTargetWindow {
    if hwnd == 0 {
        return crate::favor_menu::MenuTargetWindow::default();
    }
    #[cfg(windows)]
    {
        query_target_win(hwnd)
    }
    #[cfg(not(windows))]
    {
        let _ = hwnd;
        crate::favor_menu::MenuTargetWindow::default()
    }
}

#[cfg(windows)]
fn query_target_win(hwnd: i64) -> crate::favor_menu::MenuTargetWindow {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{GetClassNameW, GetWindowTextW};
    let handle = HWND(hwnd as *mut core::ffi::c_void);
    let mut class_buf = [0u16; 128];
    let cn = unsafe { GetClassNameW(handle, &mut class_buf) };
    let class_name = if cn > 0 {
        String::from_utf16_lossy(&class_buf[..cn as usize])
    } else {
        String::new()
    };
    let mut title_buf = [0u16; 256];
    let tn = unsafe { GetWindowTextW(handle, &mut title_buf) };
    let title = if tn > 0 {
        String::from_utf16_lossy(&title_buf[..tn as usize])
    } else {
        String::new()
    };
    let (process_name, _) = process_of(handle);
    crate::favor_menu::MenuTargetWindow {
        process_name,
        class_name,
        title,
        browser_url: None,
    }
}

#[derive(Debug, Clone)]
pub enum WindowEntry {
    Single(WindowInfo),
    Group(Vec<WindowInfo>),
}

fn collect_windows(skip_hwnd: isize) -> Vec<WindowInfo> {
    #[cfg(windows)]
    {
        collect_windows_win(skip_hwnd)
    }
    #[cfg(not(windows))]
    {
        let _ = skip_hwnd;
        Vec::new()
    }
}

#[cfg(windows)]
fn collect_windows_win(skip_hwnd: isize) -> Vec<WindowInfo> {
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetClassNameW, GetForegroundWindow, GetWindowLongW, GetWindowTextW,
        IsWindowVisible, GWL_EXSTYLE,
    };

    struct State {
        skip: isize,
        fg: isize,
        items: Vec<WindowInfo>,
    }

    unsafe extern "system" fn proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let state = unsafe { &mut *(lparam.0 as *mut State) };
        let id = hwnd.0 as isize;
        if id == 0 || id == state.skip {
            return BOOL(1);
        }
        if unsafe { !IsWindowVisible(hwnd).as_bool() } && id != state.fg {
            return BOOL(1);
        }
        let visible = unsafe { IsWindowVisible(hwnd).as_bool() };
        let ex = unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) } as u32;
        let tool_window = ex & 0x0000_0080 != 0;
        let no_activate = ex & 0x0800_0000 != 0;
        let mut class_buf = [0u16; 128];
        let cn = unsafe { GetClassNameW(hwnd, &mut class_buf) };
        let class_name = if cn > 0 {
            String::from_utf16_lossy(&class_buf[..cn as usize])
        } else {
            String::new()
        };
        let mut title_buf = [0u16; 256];
        let tn = unsafe { GetWindowTextW(hwnd, &mut title_buf) };
        let title = if tn > 0 {
            String::from_utf16_lossy(&title_buf[..tn as usize])
        } else {
            String::new()
        };
        let (process_name, _exe_path) = process_of(hwnd);
        if !include_in_window_menu(
            id == state.fg,
            visible,
            tool_window,
            no_activate,
            &class_name,
            &title,
            &process_name,
        ) {
            return BOOL(1);
        }
        if let Some(info) = read_window_info_win(hwnd, state.fg) {
            state.items.push(info);
        }
        BOOL(1)
    }

    let mut state = State {
        skip: skip_hwnd,
        fg: unsafe { GetForegroundWindow().0 as isize },
        items: Vec::new(),
    };
    unsafe {
        let _ = EnumWindows(Some(proc), LPARAM(&mut state as *mut State as isize));
    }
    state.items
}

/// C# `WinProperty.Get` for a HWND without `WindowFilter` (used by withMouse inject).
pub fn inspect_raw(hwnd: i64) -> Option<WindowInfo> {
    if hwnd == 0 || crate::clipboard::is_headless() {
        return None;
    }
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
        let handle = HWND(hwnd as *mut core::ffi::c_void);
        let fg = unsafe { GetForegroundWindow().0 as isize };
        read_window_info_win(handle, fg)
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// C# `WinProperty.FromCursor().GetRootWindow()`.
pub fn cursor_root_hwnd() -> i64 {
    if crate::clipboard::is_headless() {
        return 0;
    }
    #[cfg(windows)]
    unsafe {
        use windows::Win32::Foundation::POINT;
        use windows::Win32::UI::WindowsAndMessaging::{
            GetAncestor, GetCursorPos, GetDesktopWindow, WindowFromPoint, GA_ROOT,
        };
        let mut pt = POINT::default();
        if GetCursorPos(&mut pt).is_err() {
            return 0;
        }
        let hit = WindowFromPoint(pt);
        let id = hit.0 as i64;
        if id == 0 {
            return 0;
        }
        let ancestor = GetAncestor(hit, GA_ROOT);
        let desktop = GetDesktopWindow().0 as i64;
        let root = ancestor.0 as i64;
        if root == 0 || root == desktop {
            id
        } else {
            root
        }
    }
    #[cfg(not(windows))]
    {
        0
    }
}

#[cfg(windows)]
fn read_window_info_win(hwnd: windows::Win32::Foundation::HWND, fg: isize) -> Option<WindowInfo> {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetClassNameW, GetWindowLongW, GetWindowRect, GetWindowTextW, IsIconic, IsZoomed, GWL_EXSTYLE,
    };
    let id = hwnd.0 as isize;
    if id == 0 {
        return None;
    }
    let ex = unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) } as u32;
    let mut class_buf = [0u16; 128];
    let cn = unsafe { GetClassNameW(hwnd, &mut class_buf) };
    let class_name = if cn > 0 {
        String::from_utf16_lossy(&class_buf[..cn as usize])
    } else {
        String::new()
    };
    let mut title_buf = [0u16; 256];
    let tn = unsafe { GetWindowTextW(hwnd, &mut title_buf) };
    let title = if tn > 0 {
        String::from_utf16_lossy(&title_buf[..tn as usize])
    } else {
        String::new()
    };
        let (process_name, exe_path) = process_of(hwnd);
    let minimized = unsafe { IsIconic(hwnd).as_bool() };
    let maximized = unsafe { IsZoomed(hwnd).as_bool() };
    let mut rect = windows::Win32::Foundation::RECT::default();
    let _ = unsafe { GetWindowRect(hwnd, &mut rect) };
    let mut pid = 0u32;
    unsafe { windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    Some(WindowInfo {
        hwnd: id as i64,
        title,
        is_explorer: process_name.eq_ignore_ascii_case("explorer"),
        process_name,
        exe_path,
        class_name,
        is_foreground: id == fg,
        topmost: ex & 0x0000_0008 != 0,
        minimized,
        maximized,
        opacity: window_opacity(hwnd),
        mouse_penetration: ex & 0x0000_0020 != 0,
        left: rect.left,
        top: rect.top,
        width: rect.right.saturating_sub(rect.left),
        height: rect.bottom.saturating_sub(rect.top),
        pid,
    })
}

/// Friendly application name of the process that owns `hwnd`.
#[cfg(windows)]
pub fn window_app_name(hwnd: isize) -> Option<String> {
    let (name, exe) = process_of(windows::Win32::Foundation::HWND(hwnd as *mut _));
    (!name.is_empty() || !exe.is_empty()).then(|| app_display_name(&exe, &name))
}

/// Cached per executable: version resources are read once per path.
pub fn app_display_name(exe_path: &str, process_name: &str) -> String {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static NAMES: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    if exe_path.is_empty() {
        return process_name.to_string();
    }
    let names = NAMES.get_or_init(|| Mutex::new(HashMap::new()));
    let key = exe_path.to_ascii_lowercase();
    if let Some(name) = names.lock().ok().and_then(|names| names.get(&key).cloned()) {
        return name;
    }
    let name = file_description(exe_path)
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| process_name.to_string());
    if let Ok(mut names) = names.lock() {
        if names.len() > 256 {
            names.clear();
        }
        names.insert(key, name.clone());
    }
    name
}

#[cfg(windows)]
fn file_description(exe_path: &str) -> Option<String> {
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
    if crate::clipboard::is_headless() {
        return None;
    }
    let wide = |text: &str| text.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let path = wide(exe_path);
    unsafe {
        let size = GetFileVersionInfoSizeW(PCWSTR(path.as_ptr()), None);
        if size == 0 || size > 4 * 1024 * 1024 {
            return None;
        }
        let mut block = vec![0u8; size as usize];
        GetFileVersionInfoW(PCWSTR(path.as_ptr()), 0, size, block.as_mut_ptr().cast()).ok()?;
        let mut pointer = std::ptr::null_mut();
        let mut len = 0u32;
        let translation = wide(r"\VarFileInfo\Translation");
        let mut code_pages = vec![(0x0409u16, 0x04B0u16), (0x0804, 0x04B0)];
        if VerQueryValueW(block.as_ptr().cast(), PCWSTR(translation.as_ptr()), &mut pointer, &mut len).as_bool() && len >= 4 {
            let pairs = std::slice::from_raw_parts(pointer as *const u16, (len / 2) as usize);
            code_pages = pairs.chunks_exact(2).map(|pair| (pair[0], pair[1])).chain(code_pages).collect();
        }
        for (language, code_page) in code_pages {
            let query = wide(&format!(r"\StringFileInfo\{language:04x}{code_page:04x}\FileDescription"));
            let mut value = std::ptr::null_mut();
            let mut chars = 0u32;
            if VerQueryValueW(block.as_ptr().cast(), PCWSTR(query.as_ptr()), &mut value, &mut chars).as_bool() && chars > 1 {
                let text = std::slice::from_raw_parts(value as *const u16, chars as usize);
                let text = String::from_utf16_lossy(text.split(|unit| *unit == 0).next().unwrap_or_default());
                let text = text.trim().to_string();
                if !text.is_empty() {
                    return Some(text);
                }
            }
        }
        None
    }
}

#[cfg(not(windows))]
fn file_description(_exe_path: &str) -> Option<String> {
    None
}

#[cfg(windows)]
fn process_of(hwnd: windows::Win32::Foundation::HWND) -> (String, String) {
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return (String::new(), String::new());
        }
        let proc = match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(h) => h,
            Err(_) => return (String::new(), String::new()),
        };
        let mut size = 520u32;
        let mut buf = [0u16; 520];
        let exe = if QueryFullProcessImageNameW(proc, PROCESS_NAME_WIN32, windows::core::PWSTR(buf.as_mut_ptr()), &mut size)
            .is_ok()
        {
            String::from_utf16_lossy(&buf[..size as usize])
        } else {
            String::new()
        };
        let _ = windows::Win32::Foundation::CloseHandle(proc);
        let name = std::path::Path::new(&exe)
            .file_stem()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        (name, exe)
    }
}

#[cfg(windows)]
fn window_opacity(hwnd: windows::Win32::Foundation::HWND) -> u8 {
    use windows::Win32::UI::WindowsAndMessaging::{GetLayeredWindowAttributes, GetWindowLongW, GWL_EXSTYLE};
    unsafe {
        let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        if ex & 0x0008_0000 == 0 {
            return 255;
        }
        let mut alpha = 255u8;
        if GetLayeredWindowAttributes(hwnd, None, Some(&mut alpha), None).is_ok() {
            alpha
        } else {
            255
        }
    }
}

/// C# `IsWindow` + `IsWindowVisible` for paste-target validation.
pub fn is_window_visible(hwnd: isize) -> bool {
    if hwnd == 0 {
        return false;
    }
    #[cfg(windows)]
    unsafe {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{IsWindow, IsWindowVisible};
        let handle = HWND(hwnd as *mut _);
        IsWindow(handle).as_bool() && IsWindowVisible(handle).as_bool()
    }
    #[cfg(not(windows))]
    {
        let _ = hwnd;
        false
    }
}

pub fn foreground_hwnd() -> isize {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
        GetForegroundWindow().0 as isize
    }
    #[cfg(not(windows))]
    {
        0
    }
}

/// C# `MenuItemWrapper.EnsureLastWindowFocus`: restore last FG only when `GetForegroundWindow() == SourceHandle`.
pub fn menu_source_still_foreground(foreground: i64, source: i64) -> bool {
    source != 0 && foreground == source
}

/// C# Super Menu overlay defers the outside-click hook to `ApplicationIdle` (~one poll).
pub const SUPER_MENU_OUTSIDE_CLICK_ARM_MS: u64 = 50;
/// C# `_contentPointerSuppressUntilTick = TickCount64 + 750`.
pub const SUPER_MENU_CONTENT_SUPPRESS_MS: u64 = 750;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuperMenuMouseKind {
    LeftDown,
    LeftUp,
    RightDown,
}

pub fn super_menu_mouse_kind(msg: u32) -> Option<SuperMenuMouseKind> {
    match msg {
        0x0201 => Some(SuperMenuMouseKind::LeftDown),
        0x0202 => Some(SuperMenuMouseKind::LeftUp),
        0x0204 => Some(SuperMenuMouseKind::RightDown),
        _ => None,
    }
}

pub fn super_menu_outside_click_armed(shown_at: std::time::Instant, now: std::time::Instant) -> bool {
    now.saturating_duration_since(shown_at).as_millis()
        >= u128::from(SUPER_MENU_OUTSIDE_CLICK_ARM_MS)
}

/// C# `OnGlobalMouseLeftButtonDown` / `OnGlobalMouseButtonOutside`. Returns true to HideSession.
pub fn apply_super_menu_outside_click(
    kind: SuperMenuMouseKind,
    hitting_overlay: bool,
    armed: bool,
    now: std::time::Instant,
    suppress_until: &mut Option<std::time::Instant>,
) -> bool {
    if !armed {
        return false;
    }
    if kind == SuperMenuMouseKind::LeftDown {
        if hitting_overlay {
            *suppress_until =
                Some(now + std::time::Duration::from_millis(SUPER_MENU_CONTENT_SUPPRESS_MS));
        }
        return false;
    }
    if hitting_overlay {
        return false;
    }
    if suppress_until.is_some_and(|until| now < until) {
        return false;
    }
    matches!(
        kind,
        SuperMenuMouseKind::LeftUp | SuperMenuMouseKind::RightDown
    )
}

pub fn window_from_point(x: i32, y: i32) -> isize {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::Foundation::POINT;
        use windows::Win32::UI::WindowsAndMessaging::WindowFromPoint;
        WindowFromPoint(POINT { x, y }).0 as isize
    }
    #[cfg(not(windows))]
    {
        let _ = (x, y);
        0
    }
}

pub fn is_overlay_tree_hit(overlay: isize, hit: isize) -> bool {
    if overlay == 0 || hit == 0 {
        return false;
    }
    if overlay == hit {
        return true;
    }
    #[cfg(windows)]
    unsafe {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{GetAncestor, IsChild, GA_ROOT};
        let host = HWND(overlay as *mut _);
        let target = HWND(hit as *mut _);
        if IsChild(host, target).as_bool() {
            return true;
        }
        let host_root = GetAncestor(host, GA_ROOT);
        let hit_root = GetAncestor(target, GA_ROOT);
        let host_root_id = if host_root.0.is_null() {
            overlay
        } else {
            host_root.0 as isize
        };
        let hit_root_id = if hit_root.0.is_null() {
            hit
        } else {
            hit_root.0 as isize
        };
        if host_root_id != 0 && host_root_id == hit_root_id {
            return true;
        }
        is_parent_or_equal(overlay, hit) || is_parent_or_equal(hit, overlay)
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn is_parent_or_equal(host: isize, target: isize) -> bool {
    #[cfg(windows)]
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
    #[cfg(not(windows))]
    {
        let _ = (host, target);
        false
    }
}

pub fn screen_point_hits_overlay(overlay: isize, x: i32, y: i32) -> bool {
    is_overlay_tree_hit(overlay, window_from_point(x, y))
}

pub fn activate(hwnd: i64) -> Result<(), String> {
    if crate::clipboard::is_headless() || hwnd == 0 {
        return Ok(());
    }
    #[cfg(windows)]
    unsafe {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, IsIconic, SetForegroundWindow, ShowWindowAsync, SW_RESTORE};
        let handle = HWND(hwnd as *mut _);
        if IsIconic(handle).as_bool() {
            let _ = ShowWindowAsync(handle, SW_RESTORE);
        }
        if GetForegroundWindow() != handle && !SetForegroundWindow(handle).as_bool() {
            return Err("窗口未能获得焦点。".into());
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = hwnd;
        Ok(())
    }
}

/// C# `WinProperty.OpenOrShow`: already foreground and not minimized → open exe; else Show.
pub fn window_click_opens_exe(is_foreground: bool, minimized: bool) -> bool {
    is_foreground && !minimized
}

pub fn open_or_show(hwnd: i64, exe_path: &str) -> Result<(), String> {
    if crate::clipboard::is_headless() || hwnd == 0 {
        return Ok(());
    }
    let info = inspect(hwnd);
    let foreground = info.as_ref().is_some_and(|i| i.is_foreground);
    let minimized = info.as_ref().is_some_and(|i| i.minimized);
    let path = if exe_path.trim().is_empty() {
        info.as_ref().map(|i| i.exe_path.as_str()).unwrap_or("")
    } else {
        exe_path
    };
    if window_click_opens_exe(foreground, minimized) {
        crate::shell_open::open(path)
    } else {
        activate(hwnd)
    }
}

pub fn set_topmost(hwnd: i64, on: bool) -> Result<(), String> {
    apply(hwnd, || {
        #[cfg(windows)]
        unsafe {
            use windows::Win32::Foundation::HWND;
            use windows::Win32::UI::WindowsAndMessaging::{
                SetWindowPos, HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOMOVE, SWP_NOSIZE,
            };
            let handle = HWND(hwnd as *mut _);
            let after = if on { HWND_TOPMOST } else { HWND_NOTOPMOST };
            let _ = SetWindowPos(handle, after, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
        }
        Ok(())
    })
}

pub fn set_minimized(hwnd: i64, on: bool) -> Result<(), String> {
    apply(hwnd, || {
        #[cfg(windows)]
        unsafe {
            use windows::Win32::Foundation::HWND;
            use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_MINIMIZE, SW_RESTORE};
            let handle = HWND(hwnd as *mut _);
            let _ = ShowWindow(handle, if on { SW_MINIMIZE } else { SW_RESTORE });
        }
        Ok(())
    })
}

pub fn set_maximized(hwnd: i64, on: bool) -> Result<(), String> {
    apply(hwnd, || {
        #[cfg(windows)]
        unsafe {
            use windows::Win32::Foundation::HWND;
            use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_MAXIMIZE, SW_RESTORE};
            let handle = HWND(hwnd as *mut _);
            let _ = ShowWindow(handle, if on { SW_MAXIMIZE } else { SW_RESTORE });
        }
        Ok(())
    })
}

pub fn set_mouse_penetrate(hwnd: i64, on: bool) -> Result<(), String> {
    apply(hwnd, || {
        #[cfg(windows)]
        unsafe {
            use windows::Win32::Foundation::HWND;
            use windows::Win32::UI::WindowsAndMessaging::{
                GetWindowLongW, SetWindowLongW, GWL_EXSTYLE,
            };
            let handle = HWND(hwnd as *mut _);
            let mut ex = GetWindowLongW(handle, GWL_EXSTYLE);
            if on {
                ex |= 0x0008_0000 | 0x0000_0020;
            } else {
                ex &= !0x0000_0020;
            }
            SetWindowLongW(handle, GWL_EXSTYLE, ex);
        }
        Ok(())
    })
}

/// C# `WinProperty.NoActivate` on the clipboard host HWND (`WS_EX_NOACTIVATE`).
pub fn set_no_activate(hwnd: i64, on: bool) -> Result<(), String> {
    apply(hwnd, || {
        #[cfg(windows)]
        unsafe {
            use windows::Win32::Foundation::HWND;
            use windows::Win32::UI::WindowsAndMessaging::{
                GetWindowLongW, SetWindowLongW, GWL_EXSTYLE,
            };
            const WS_EX_NOACTIVATE: i32 = 0x0800_0000;
            let handle = HWND(hwnd as *mut _);
            let mut ex = GetWindowLongW(handle, GWL_EXSTYLE);
            if on {
                ex |= WS_EX_NOACTIVATE;
            } else {
                ex &= !WS_EX_NOACTIVATE;
            }
            SetWindowLongW(handle, GWL_EXSTYLE, ex);
        }
        Ok(())
    })
}

/// C# overlay `WinProperty.NoActivate` + `IsToolWindow` + HWND_TOPMOST without activation.
pub fn apply_overlay_no_activate(hwnd: isize) -> Result<(), String> {
    apply(hwnd as i64, || {
        #[cfg(windows)]
        unsafe {
            use windows::Win32::Foundation::HWND;
            use windows::Win32::UI::WindowsAndMessaging::{
                GetWindowLongW, SetWindowLongW, SetWindowPos, GWL_EXSTYLE, HWND_TOPMOST,
                SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW,
            };
            let handle = HWND(hwnd as *mut _);
            let mut ex = GetWindowLongW(handle, GWL_EXSTYLE);
            ex |= 0x0000_0080 | 0x0800_0000; // WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE
            SetWindowLongW(handle, GWL_EXSTYLE, ex);
            let _ = SetWindowPos(
                handle,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
        }
        Ok(())
    })
}

/// C# overlay `WinProperty.MousePenetration` (WS_EX_TRANSPARENT only).
pub fn set_overlay_pass_through(hwnd: isize, pass_through: bool) -> Result<(), String> {
    apply(hwnd as i64, || {
        #[cfg(windows)]
        unsafe {
            use windows::Win32::Foundation::HWND;
            use windows::Win32::UI::WindowsAndMessaging::{
                GetWindowLongW, SetWindowLongW, GWL_EXSTYLE,
            };
            let handle = HWND(hwnd as *mut _);
            let mut ex = GetWindowLongW(handle, GWL_EXSTYLE);
            if pass_through {
                ex |= 0x0000_0020; // WS_EX_TRANSPARENT
            } else {
                ex &= !0x0000_0020;
            }
            SetWindowLongW(handle, GWL_EXSTYLE, ex);
        }
        Ok(())
    })
}

/// C# `ApplyWin32Hide`: penetrate + SWP_HIDEWINDOW without activating.
pub fn apply_overlay_hidden(hwnd: isize) -> Result<(), String> {
    set_overlay_pass_through(hwnd, true)?;
    apply(hwnd as i64, || {
        #[cfg(windows)]
        unsafe {
            use windows::Win32::Foundation::HWND;
            use windows::Win32::UI::WindowsAndMessaging::{
                SetWindowPos, HWND_NOTOPMOST, SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
            };
            let handle = HWND(hwnd as *mut _);
            let _ = SetWindowPos(
                handle,
                HWND_NOTOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_HIDEWINDOW,
            );
        }
        Ok(())
    })
}

/// C# overlay `WarmupWindowSizeDip` / `OffScreenDip` while the HWND stays alive.
pub const OVERLAY_WARMUP_SIZE_DIP: f64 = 64.0;
pub const OVERLAY_OFFSCREEN_DIP: f64 = -10000.0;

pub fn overlay_warmup_bounds(hwnd: isize) -> (i32, i32, i32, i32) {
    let scale = dpi_scale_for_window(hwnd).unwrap_or(1.0);
    let size = (OVERLAY_WARMUP_SIZE_DIP * scale).ceil() as i32;
    let off = (OVERLAY_OFFSCREEN_DIP * scale) as i32;
    (off, off, size.max(1), size.max(1))
}

fn dpi_scale_for_window(hwnd: isize) -> Option<f64> {
    if hwnd == 0 {
        return None;
    }
    #[cfg(windows)]
    unsafe {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::HiDpi::GetDpiForWindow;
        let dpi = GetDpiForWindow(HWND(hwnd as *mut _));
        if dpi == 0 {
            None
        } else {
            Some(dpi as f64 / 96.0)
        }
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// C# QP `ShouldUseInputPassThrough`: `_overlayTrulyHidden || Opacity <= 0`.
pub fn qp_overlay_pass_through(overlay_visible: bool, overlay_opacity: f64) -> bool {
    !overlay_visible || overlay_opacity <= 0.0
}

/// C# Super Menu / toast: `!sessionActive || trulyHidden || Opacity <= 0`.
pub fn session_overlay_pass_through(session_active: bool) -> bool {
    !session_active
}

pub fn move_to_center(hwnd: i64) -> Result<(), String> {
    apply(hwnd, || {
        #[cfg(windows)]
        unsafe {
            use windows::Win32::Foundation::{HWND, RECT};
            use windows::Win32::Graphics::Gdi::{
                GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
            };
            use windows::Win32::UI::WindowsAndMessaging::{GetWindowRect, SetWindowPos, SWP_NOSIZE, HWND_TOP};
            let handle = HWND(hwnd as *mut _);
            let mut wr = RECT::default();
            GetWindowRect(handle, &mut wr).map_err(|e| e.to_string())?;
            let mon = MonitorFromWindow(handle, MONITOR_DEFAULTTONEAREST);
            let mut mi = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if !GetMonitorInfoW(mon, &mut mi).as_bool() {
                return Err("GetMonitorInfoW failed".into());
            }
            let ww = wr.right - wr.left;
            let wh = wr.bottom - wr.top;
            let mw = mi.rcWork.right - mi.rcWork.left;
            let mh = mi.rcWork.bottom - mi.rcWork.top;
            let x = mi.rcWork.left + (mw - ww) / 2;
            let y = mi.rcWork.top + (mh - wh) / 2;
            let _ = SetWindowPos(handle, HWND_TOP, x, y, 0, 0, SWP_NOSIZE);
        }
        Ok(())
    })
}

pub fn set_opacity(hwnd: i64, alpha: u8) -> Result<(), String> {
    apply(hwnd, || {
        #[cfg(windows)]
        unsafe {
            use windows::Win32::Foundation::HWND;
            use windows::Win32::UI::WindowsAndMessaging::{
                GetWindowLongW, SetLayeredWindowAttributes, SetWindowLongW, GWL_EXSTYLE, LWA_ALPHA,
            };
            let handle = HWND(hwnd as *mut _);
            let ex = GetWindowLongW(handle, GWL_EXSTYLE) | 0x0008_0000;
            SetWindowLongW(handle, GWL_EXSTYLE, ex);
            SetLayeredWindowAttributes(handle, windows::Win32::Foundation::COLORREF(0), alpha, LWA_ALPHA)
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    })
}

pub fn close(hwnd: i64) -> Result<(), String> {
    close_guarded(hwnd, &|| Ok(()))
}

fn close_guarded(hwnd: i64, verify: &impl Fn() -> Result<(), String>) -> Result<(), String> {
    apply(hwnd, || {
        #[cfg(windows)]
        unsafe {
            use windows::Win32::Foundation::HWND;
            use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_CLOSE};
            let handle = HWND(hwnd as *mut _);
            execute_prepared_system_action_guarded(verify, || Ok(handle), |handle| {
                let _ = PostMessageW(*handle, WM_CLOSE, windows::Win32::Foundation::WPARAM(0), windows::Win32::Foundation::LPARAM(0));
                Ok(())
            }).map_err(GuardedSystemActionError::into_string)?;
        }
        #[cfg(not(windows))]
        verify()?;
        Ok(())
    })
}

pub fn kill_process(hwnd: i64) -> Result<(), String> {
    apply(hwnd, || {
        #[cfg(windows)]
        unsafe {
            use windows::Win32::Foundation::HWND;
            use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
            let handle = HWND(hwnd as *mut _);
            let mut pid = 0u32;
            GetWindowThreadProcessId(handle, Some(&mut pid));
            if pid == 0 {
                return Err("process not found".into());
            }
            terminate_pid(pid)
        }
        #[cfg(not(windows))]
        {
            Ok(())
        }
    })
}

/// C# `WinProperty.IsRandomClassName`.
pub fn is_random_class_name(class_name: &str) -> bool {
    if !class_name.contains('-') {
        return false;
    }
    let mut run = 0usize;
    for ch in class_name.chars() {
        if ch.is_ascii_hexdigit() || ch == '-' {
            run += 1;
            if run >= 36 {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}

/// C# `WinProperty.GetSummary` (skips WPF-only current-process type keys).
pub fn summary_pairs(info: &WindowInfo) -> Vec<(String, String)> {
    vec![
        ("进程路径".into(), info.exe_path.clone()),
        ("进程名".into(), info.process_name.clone()),
        ("类名".into(), info.class_name.clone()),
        ("标题".into(), info.title.clone()),
        ("句柄".into(), info.hwnd.to_string()),
        (
            "位置".into(),
            format!("{},{},{},{}", info.left, info.top, info.width, info.height),
        ),
    ]
}

pub fn summary_text(info: &WindowInfo) -> String {
    summary_pairs(info)
        .into_iter()
        .map(|(key, value)| format!("{key}:  {value}"))
        .collect::<Vec<_>>()
        .join("\r\n")
}

/// C# `WinProperty.CloseAllByClassName`.
pub fn close_all_by_class_name(class_name: &str) -> Result<(), String> {
    close_all_by_class_name_guarded(class_name, || Ok(()))
}

pub(crate) fn close_all_by_class_name_guarded(
    class_name: &str,
    verify: impl Fn() -> Result<(), String>,
) -> Result<(), String> {
    verify()?;
    #[cfg(test)]
    if let Some(result) = run_window_batch_test_runner(WindowBatchTestAction::CloseClass, class_name, &verify) {
        return result;
    }
    close_all_by_class_name_with(class_name, &verify, || collect_windows(0), |hwnd| {
        close_guarded(hwnd, &verify)
    })
}

/// Enumeration/action seams share the production matching and cancel loop.
/// Tests supply both callbacks: headless alone does not disable collect_windows.
pub(crate) fn close_all_by_class_name_with(
    class_name: &str,
    verify: &impl Fn() -> Result<(), String>,
    enumerate: impl FnOnce() -> Vec<WindowInfo>,
    mut close: impl FnMut(i64) -> Result<(), String>,
) -> Result<(), String> {
    verify()?;
    if class_name.is_empty() {
        return Ok(());
    }
    let windows = enumerate();
    verify()?;
    let hwnds: Vec<i64> = windows
        .into_iter()
        .filter(|w| w.class_name == class_name)
        .map(|w| w.hwnd)
        .collect();
    for hwnd in hwnds {
        verify()?;
        close(hwnd)?;
        verify()?;
    }
    verify()
}

/// C# `WinProperty.KillAllProcessByName`. Explorer restart spawn is commented out in C#.
pub fn kill_all_by_process_name(process_name: &str) -> Result<(), String> {
    kill_all_by_process_name_guarded(process_name, || Ok(()))
}

pub(crate) fn kill_all_by_process_name_guarded(
    process_name: &str,
    verify: impl Fn() -> Result<(), String>,
) -> Result<(), String> {
    verify()?;
    #[cfg(test)]
    if let Some(result) = run_window_batch_test_runner(WindowBatchTestAction::KillName, process_name, &verify) {
        return result;
    }
    if crate::clipboard::is_headless() || process_name.trim().is_empty() {
        return Ok(());
    }
    kill_all_by_process_name_with(process_name, &verify, collect_process_entries, |pid| {
        #[cfg(windows)]
        unsafe { terminate_pid_guarded(pid, &verify) }
        #[cfg(not(windows))]
        { let _ = pid; Ok(()) }
    })
}

pub(crate) fn kill_all_by_process_name_with(
    process_name: &str,
    verify: &impl Fn() -> Result<(), String>,
    enumerate: impl FnOnce() -> Result<Vec<(u32, String)>, String>,
    mut terminate: impl FnMut(u32) -> Result<(), GuardedSystemActionError>,
) -> Result<(), String> {
    verify()?;
    if process_name.trim().is_empty() {
        return Ok(());
    }
    let processes = enumerate();
    verify()?;
    let processes = processes?;
    for (pid, raw) in processes {
        let stem = std::path::Path::new(&raw).file_stem().and_then(|s| s.to_str()).unwrap_or(&raw);
        if !stem.eq_ignore_ascii_case(process_name) {
            continue;
        }
        verify()?;
        match terminate(pid) {
            Err(GuardedSystemActionError::Cancelled(error)) => return Err(error),
            // Keep KillAll's per-process OS failure behavior. A cancellation
            // from the guard before TerminateProcess must never be swallowed.
            Err(GuardedSystemActionError::Operation(_)) | Ok(()) => {},
        }
        verify()?;
    }
    verify()
}

fn collect_process_entries() -> Result<Vec<(u32, String)>, String> {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
            TH32CS_SNAPPROCESS,
        };
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).map_err(|e| e.to_string())?;
        if snap == INVALID_HANDLE_VALUE {
            return Err("process snapshot failed".into());
        }
        let snap = SystemHandleGuard(snap);
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut processes = Vec::new();
        if Process32FirstW(snap.0, &mut entry).is_ok() {
            loop {
                let raw = String::from_utf16_lossy(
                    entry.szExeFile.split(|c| *c == 0).next().unwrap_or(&[]),
                );
                processes.push((entry.th32ProcessID, raw));
                if Process32NextW(snap.0, &mut entry).is_err() {
                    break;
                }
            }
        }
        // The snapshot is released before the batch executes any process.
        Ok(processes)
    }
    #[cfg(not(windows))]
    {
        Ok(Vec::new())
    }
}

#[cfg(windows)]
unsafe fn terminate_pid(pid: u32) -> Result<(), String> {
    terminate_pid_guarded(pid, &|| Ok(())).map_err(GuardedSystemActionError::into_string)
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum GuardedSystemActionError {
    Cancelled(String),
    Operation(String),
}

impl GuardedSystemActionError {
    fn into_string(self) -> String {
        match self { Self::Cancelled(error) | Self::Operation(error) => error }
    }
}

/// Drop the prepared resource on every exit, including cancellation after
/// OpenProcess. The final check occurs immediately before the OS callback.
pub(crate) fn execute_prepared_system_action_guarded<T>(
    verify: &impl Fn() -> Result<(), String>,
    prepare: impl FnOnce() -> Result<T, String>,
    execute: impl FnOnce(&T) -> Result<(), String>,
) -> Result<(), GuardedSystemActionError> {
    verify().map_err(GuardedSystemActionError::Cancelled)?;
    let resource = prepare().map_err(GuardedSystemActionError::Operation)?;
    verify().map_err(GuardedSystemActionError::Cancelled)?;
    execute(&resource).map_err(GuardedSystemActionError::Operation)
}

#[cfg(windows)]
struct SystemHandleGuard(windows::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl Drop for SystemHandleGuard {
    fn drop(&mut self) {
        unsafe { let _ = windows::Win32::Foundation::CloseHandle(self.0); }
    }
}

#[cfg(windows)]
unsafe fn terminate_pid_guarded(
    pid: u32,
    verify: &impl Fn() -> Result<(), String>,
) -> Result<(), GuardedSystemActionError> {
    use windows::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
    execute_prepared_system_action_guarded(verify, || {
        if pid == 0 { return Err("process not found".into()); }
        OpenProcess(PROCESS_TERMINATE, false, pid)
            .map(SystemHandleGuard).map_err(|e| e.to_string())
    }, |proc| {
        let _ = TerminateProcess(proc.0, 1);
        Ok(())
    })
}

pub fn inspect(hwnd: i64) -> Option<WindowInfo> {
    if hwnd == 0 {
        return None;
    }
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{
            GetAncestor, GetForegroundWindow, GetWindowLongW, IsWindow, IsWindowVisible,
            GA_ROOT, GWL_EXSTYLE,
        };
        let handle = HWND(hwnd as *mut core::ffi::c_void);
        if unsafe { !IsWindow(handle).as_bool() || GetAncestor(handle, GA_ROOT) != handle } {
            return None;
        }
        let foreground = unsafe { GetForegroundWindow().0 as isize };
        let visible = unsafe { IsWindowVisible(handle).as_bool() };
        let ex = unsafe { GetWindowLongW(handle, GWL_EXSTYLE) } as u32;
        let tool_window = ex & 0x0000_0080 != 0;
        let no_activate = ex & 0x0800_0000 != 0;
        if no_activate || (hwnd as isize != foreground && (!visible || tool_window)) {
            return None;
        }
        // A single-window refresh must not query unrelated titles. In-process
        // GetWindowTextW synchronously enters the owning UI thread, which may
        // itself be waiting for the menu session performing this inspection.
        let info = read_window_info_win(handle, foreground)?;
        include_in_window_menu(
            info.is_foreground,
            visible,
            tool_window,
            no_activate,
            &info.class_name,
            &info.title,
            &info.process_name,
        )
        .then_some(info)
    }
    #[cfg(not(windows))]
    {
        None
    }
}

fn apply(hwnd: i64, f: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
    if crate::clipboard::is_headless() || hwnd == 0 {
        return Ok(());
    }
    f()
}

#[cfg(test)]
#[derive(Clone, Copy)]
pub(crate) enum WindowBatchTestAction { CloseClass = 0, KillName = 1 }

#[cfg(test)]
type WindowBatchTestRunner = dyn FnMut(&str, &dyn Fn() -> Result<(), String>) -> Result<(), String>;

#[cfg(test)]
thread_local! {
    static WINDOW_BATCH_TEST_RUNNERS: std::cell::RefCell<[Option<Box<WindowBatchTestRunner>>; 2]> =
        std::cell::RefCell::new([None, None]);
}

/// The real native dispatch can be exercised without any desktop enumeration
/// or system action. This seam is thread-local and cannot affect another test.
#[cfg(test)]
pub(crate) fn with_window_batch_test_runner<T>(
    action: WindowBatchTestAction,
    runner: impl FnMut(&str, &dyn Fn() -> Result<(), String>) -> Result<(), String> + 'static,
    run: impl FnOnce() -> T,
) -> T {
    struct Restore(usize, Option<Box<WindowBatchTestRunner>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            WINDOW_BATCH_TEST_RUNNERS.with(|runners| runners.borrow_mut()[self.0] = self.1.take());
        }
    }
    let index = action as usize;
    let previous = WINDOW_BATCH_TEST_RUNNERS.with(|runners| runners.borrow_mut()[index].replace(Box::new(runner)));
    let _restore = Restore(index, previous);
    run()
}

#[cfg(test)]
fn run_window_batch_test_runner(
    action: WindowBatchTestAction,
    name: &str,
    verify: &dyn Fn() -> Result<(), String>,
) -> Option<Result<(), String>> {
    WINDOW_BATCH_TEST_RUNNERS.with(|runners| {
        runners.borrow_mut()[action as usize].as_mut().map(|runner| runner(name, verify))
    })
}

#[cfg(all(test, windows))]
#[path = "clipboard_private_station_test.rs"]
mod inspect_private_station;

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[cfg(windows)]
    #[test]
    #[ignore = "Requires access to a noninteractive private window station; run explicitly on a compatible token"]
    fn native_inspect_reads_only_the_requested_window() {
        // Window-station selection is process-wide. Run this fixture alone in
        // a child process, before creating any windows, on a noninteractive
        // private station; never enumerate or expose the user's desktop.
        const CHILD: &str = "CLIPHOST_MENU_INSPECT_NATIVE_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "window_menu::tests::native_inspect_reads_only_the_requested_window",
                    "--ignored",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(CHILD, "1")
                .status()
                .unwrap();
            assert!(status.success(), "private desktop inspection fixture failed");
            return;
        }
        use std::sync::atomic::{AtomicUsize, Ordering};
        use windows::core::{w, HSTRING, PCWSTR};
        use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::*;
        static UNRELATED_TITLE_QUERIES: AtomicUsize = AtomicUsize::new(0);
        unsafe extern "system" fn bystander_proc(
            hwnd: HWND, message: u32, wp: WPARAM, lp: LPARAM,
        ) -> LRESULT {
            if matches!(message, WM_GETTEXT | WM_GETTEXTLENGTH) {
                UNRELATED_TITLE_QUERIES.fetch_add(1, Ordering::SeqCst);
                // Model a slow UI without risking an unbounded failed-test hang.
                std::thread::sleep(Duration::from_millis(100));
            }
            unsafe { DefWindowProcW(hwnd, message, wp, lp) }
        }
        struct Fixture {
            windows: Vec<HWND>,
            instance: HINSTANCE,
            class: HSTRING,
        }
        impl Drop for Fixture {
            fn drop(&mut self) {
                unsafe {
                    for hwnd in self.windows.drain(..).rev() {
                        let _ = DestroyWindow(hwnd);
                    }
                    let _ = UnregisterClassW(&self.class, self.instance);
                }
            }
        }
        let _station = super::inspect_private_station::PrivateStation::create()
            .expect("private window station required; refusing desktop fallback");
        unsafe {
            let instance = HINSTANCE(GetModuleHandleW(None).unwrap().0);
            let class = HSTRING::from(format!("ClipHostInspectBystander{}", std::process::id()));
            assert_ne!(RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(bystander_proc),
                hInstance: instance,
                lpszClassName: PCWSTR(class.as_ptr()),
                ..Default::default()
            }), 0);
            let mut fixture = Fixture { windows: Vec::new(), instance, class };
            let target = CreateWindowExW(WINDOW_EX_STYLE::default(), w!("STATIC"),
                w!("owned inspection target"), WS_POPUP | WS_VISIBLE,
                0, 0, 100, 100, None, None, instance, None).unwrap();
            fixture.windows.push(target);
            let child = CreateWindowExW(WINDOW_EX_STYLE::default(), w!("STATIC"),
                w!("owned child target"), WS_CHILD | WS_VISIBLE,
                0, 0, 20, 20, target, None, instance, None).unwrap();
            fixture.windows.push(child);
            let hidden = CreateWindowExW(WINDOW_EX_STYLE::default(), w!("STATIC"),
                w!("owned hidden target"), WS_POPUP,
                0, 0, 100, 100, None, None, instance, None).unwrap();
            fixture.windows.push(hidden);
            let bystander = CreateWindowExW(WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                &fixture.class, w!("unrelated slow UI"), WS_POPUP | WS_VISIBLE,
                0, 0, 100, 100, None, None, instance, None).unwrap();
            fixture.windows.push(bystander);
            UNRELATED_TITLE_QUERIES.store(0, Ordering::SeqCst);

            let info = inspect(target.0 as i64).expect("visible target remains a menu candidate");
            assert_eq!(info.hwnd, target.0 as i64);
            assert_eq!(info.title, "owned inspection target");
            assert!(inspect(child.0 as i64).is_none(), "desktop menus exclude child HWNDs");
            assert!(inspect(hidden.0 as i64).is_none());
            assert!(inspect(bystander.0 as i64).is_none(), "no-activate tools remain excluded");
            assert!(inspect(0).is_none());
            assert!(inspect(-1).is_none());
            assert_eq!(UNRELATED_TITLE_QUERIES.load(Ordering::SeqCst), 0,
                "a targeted inspection queried the unrelated UI's title");
        }
    }

    #[test]
    fn native_window_batch_close_keeps_matching_and_action_errors() {
        crate::clipboard::force_headless(true);
        let windows = || vec![
            WindowInfo { hwnd: 11, class_name: "OwnedClass".into(), ..Default::default() },
            WindowInfo { hwnd: 12, class_name: "ownedclass".into(), ..Default::default() },
            WindowInfo { hwnd: 13, class_name: " OwnedClass".into(), ..Default::default() },
            WindowInfo { hwnd: 14, class_name: "OwnedClass".into(), ..Default::default() },
            WindowInfo { hwnd: 15, class_name: "OwnedClass".into(), ..Default::default() },
        ];
        let mut calls = Vec::new();
        let error = close_all_by_class_name_with("OwnedClass", &|| Ok(()), windows, |hwnd| {
            calls.push(hwnd);
            if hwnd == 14 { Err("owned close failure".into()) } else { Ok(()) }
        }).unwrap_err();
        assert_eq!(error, "owned close failure");
        assert_eq!(calls, vec![11, 14]);
        assert!(close_all_by_class_name_with("", &|| Ok(()), || panic!("empty class cannot enumerate"), |_| panic!("no action")).is_ok());
    }

    #[test]
    fn native_window_batch_kill_keeps_name_and_ignored_os_error_rules() {
        crate::clipboard::force_headless(true);
        for name in ["owned", " owned", "owned.exe"] {
            let mut calls = Vec::new();
            kill_all_by_process_name_with(name, &|| Ok(()), || Ok(vec![
                (11, "OwNeD.exe".into()), (12, "other.exe".into()), (13, "owned.EXE".into()),
            ]), |pid| {
                calls.push(pid);
                if pid == 11 { Err(GuardedSystemActionError::Operation("owned OpenProcess failure".into())) } else { Ok(()) }
            }).unwrap();
            assert_eq!(calls, if name == "owned" { vec![11, 13] } else { vec![] });
        }
        assert!(kill_all_by_process_name_with("  ", &|| Ok(()), || panic!("empty name cannot enumerate"), |_| panic!("no action")).is_ok());
        assert_eq!(kill_all_by_process_name_with("owned", &|| Ok(()), || Err("owned snapshot failure".into()), |_| panic!("failed enumeration cannot execute")).unwrap_err(), "owned snapshot failure");
        let retired = std::cell::Cell::new(false);
        let error = kill_all_by_process_name_with("owned", &|| {
            if retired.get() { Err("owned cancellation".into()) } else { Ok(()) }
        }, || {
            retired.set(true);
            Err("owned snapshot failure".into())
        }, |_| panic!("retired failed enumeration cannot execute")).unwrap_err();
        assert_eq!(error, "owned cancellation");
    }

    #[test]
    fn native_window_batch_cancelled_action_cannot_be_ignored_like_os_error() {
        crate::clipboard::force_headless(true);
        let mut calls = Vec::new();
        let error = kill_all_by_process_name_with("owned", &|| Ok(()), || Ok(vec![
            (11, "owned.exe".into()), (12, "owned.exe".into()),
        ]), |pid| {
            calls.push(pid);
            // Even a guard error returned by the actual execute boundary is
            // structural cancellation, never the per-process ignored error.
            Err(GuardedSystemActionError::Cancelled("owned cancellation".into()))
        }).unwrap_err();
        assert_eq!(error, "owned cancellation");
        assert_eq!(calls, vec![11]);
    }

    #[test]
    fn native_window_prepared_action_checks_guard_and_releases_resource() {
        use std::cell::Cell;
        use std::rc::Rc;
        crate::clipboard::force_headless(true);
        struct OwnedResource(Rc<Cell<usize>>);
        impl Drop for OwnedResource { fn drop(&mut self) { self.0.set(self.0.get() + 1); } }
        for phase in ["before-prepare", "after-prepare", "action-error", "success"] {
            let retired = Cell::new(phase == "before-prepare");
            let prepared = Cell::new(0);
            let executed = Cell::new(0);
            let released = Rc::new(Cell::new(0));
            let result = execute_prepared_system_action_guarded(
                &|| if retired.get() { Err("owned cancellation".into()) } else { Ok(()) },
                || {
                    prepared.set(prepared.get() + 1);
                    if phase == "after-prepare" { retired.set(true); }
                    Ok(OwnedResource(released.clone()))
                },
                |_| {
                    executed.set(executed.get() + 1);
                    if phase == "action-error" { Err("owned OS failure".into()) } else { Ok(()) }
                },
            );
            assert_eq!(prepared.get(), usize::from(phase != "before-prepare"), "{phase}");
            assert_eq!(released.get(), usize::from(phase != "before-prepare"), "{phase}");
            assert_eq!(executed.get(), usize::from(matches!(phase, "action-error" | "success")), "{phase}");
            match phase {
                "before-prepare" | "after-prepare" => assert_eq!(result, Err(GuardedSystemActionError::Cancelled("owned cancellation".into()))),
                "action-error" => assert_eq!(result, Err(GuardedSystemActionError::Operation("owned OS failure".into()))),
                _ => assert_eq!(result, Ok(())),
            }
        }
    }

    #[test]
    fn overlay_tree_hit_matches_equal_hwnd() {
        assert!(!is_overlay_tree_hit(0, 1));
        assert!(!is_overlay_tree_hit(5, 0));
        assert!(is_overlay_tree_hit(5, 5));
    }

    #[test]
    fn ensure_last_window_focus_only_when_source_is_foreground_like_csharp() {
        assert!(menu_source_still_foreground(10, 10));
        assert!(!menu_source_still_foreground(11, 10));
        assert!(!menu_source_still_foreground(10, 0));
        assert!(!menu_source_still_foreground(0, 0));
    }

    #[test]
    fn overlay_pass_through_matches_csharp_hidden_or_zero_opacity() {
        assert!(qp_overlay_pass_through(false, 1.0));
        assert!(qp_overlay_pass_through(true, 0.0));
        assert!(!qp_overlay_pass_through(true, 1.0));
        assert!(session_overlay_pass_through(false));
        assert!(!session_overlay_pass_through(true));
    }

    #[test]
    fn overlay_warmup_bounds_match_csharp_dip_at_scale_one() {
        assert_eq!(overlay_warmup_bounds(0), (-10000, -10000, 64, 64));
    }

    #[test]
    fn outside_click_policy_matches_csharp() {
        let now = Instant::now();
        let shown = now - Duration::from_millis(10);
        assert!(!super_menu_outside_click_armed(shown, now));
        assert!(super_menu_outside_click_armed(
            now - Duration::from_millis(50),
            now
        ));

        let mut suppress = None;
        assert!(!apply_super_menu_outside_click(
            SuperMenuMouseKind::LeftUp,
            false,
            false,
            now,
            &mut suppress
        ));
        assert!(apply_super_menu_outside_click(
            SuperMenuMouseKind::LeftUp,
            false,
            true,
            now,
            &mut suppress
        ));
        assert!(!apply_super_menu_outside_click(
            SuperMenuMouseKind::LeftUp,
            true,
            true,
            now,
            &mut suppress
        ));
        assert!(!apply_super_menu_outside_click(
            SuperMenuMouseKind::LeftDown,
            true,
            true,
            now,
            &mut suppress
        ));
        assert!(suppress.is_some());
        assert!(!apply_super_menu_outside_click(
            SuperMenuMouseKind::LeftUp,
            false,
            true,
            now,
            &mut suppress
        ));
        assert!(apply_super_menu_outside_click(
            SuperMenuMouseKind::RightDown,
            false,
            true,
            now + Duration::from_millis(SUPER_MENU_CONTENT_SUPPRESS_MS),
            &mut suppress
        ));
    }

    #[test]
    fn random_class_and_summary_match_csharp() {
        assert!(!is_random_class_name("Notepad"));
        assert!(!is_random_class_name("CabinetWClass"));
        assert!(is_random_class_name(
            "HwndWrapper[app;;aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee]"
        ));
        let info = WindowInfo {
            hwnd: 123,
            title: "hi".into(),
            process_name: "app".into(),
            exe_path: r"C:\app.exe".into(),
            class_name: "Cls".into(),
            left: 1,
            top: 2,
            width: 3,
            height: 4,
            opacity: 255,
            ..WindowInfo::default()
        };
        assert_eq!(
            summary_text(&info),
            "进程路径:  C:\\app.exe\r\n进程名:  app\r\n类名:  Cls\r\n标题:  hi\r\n句柄:  123\r\n位置:  1,2,3,4"
        );
    }

    #[test]
    fn window_list_filter_and_open_or_show_match_csharp() {
        assert!(is_quicker_ignore_window("Quicker", "SupperMenuWindow"));
        assert!(is_quicker_ignore_window("Quicker", "FloatButtonWindow"));
        assert!(is_quicker_ignore_window("Quicker", "CircleMenuWindow"));
        assert!(!is_quicker_ignore_window("quicker", "SupperMenuWindow"));
        assert!(!is_quicker_ignore_window("notepad", "SupperMenuWindow"));

        assert!(!include_in_window_menu(
            false, true, false, false, "Notepad", "", "notepad"
        ));
        assert!(include_in_window_menu(
            true, true, true, false, "Notepad", "", "notepad"
        ));
        assert!(!include_in_window_menu(
            false, false, false, false, "Notepad", "hi", "notepad"
        ));
        assert!(include_in_window_menu(
            true, false, true, false, "Notepad", "", "notepad"
        ));
        assert!(!include_in_window_menu(
            false, true, true, false, "Notepad", "hi", "notepad"
        ));
        assert!(!include_in_window_menu(
            true,
            true,
            false,
            false,
            "Windows.UI.Core.CoreWindow",
            "x",
            "app"
        ));
        assert!(!include_in_window_menu(
            false, true, false, true, "Notepad", "hi", "notepad"
        ));
        assert!(!include_in_window_menu(
            true,
            true,
            false,
            false,
            "Chrome_WidgetWin_1",
            "SupperMenuWindow",
            "Quicker",
        ));

        assert!(window_click_opens_exe(true, false));
        assert!(!window_click_opens_exe(true, true));
        assert!(!window_click_opens_exe(false, false));

        let a = WindowInfo {
            hwnd: 1,
            title: "a".into(),
            minimized: true,
            ..WindowInfo::default()
        };
        let b = WindowInfo {
            hwnd: 2,
            title: "b".into(),
            minimized: false,
            ..WindowInfo::default()
        };
        assert_eq!(last_focus_window(&[a.clone(), b.clone()]).unwrap().hwnd, 2);
        let c = WindowInfo {
            hwnd: 3,
            title: "c".into(),
            minimized: false,
            ..WindowInfo::default()
        };
        assert_eq!(last_focus_window(&[c.clone(), b]).unwrap().hwnd, 3);
    }

    #[test]
    fn append_mouse_target_matches_csharp_get_all_window_internal() {
        let listed = WindowInfo {
            hwnd: 1,
            title: "a".into(),
            ..WindowInfo::default()
        };
        let extra = WindowInfo {
            hwnd: 2,
            title: "under cursor".into(),
            ..WindowInfo::default()
        };
        let injected = append_mouse_target(vec![listed.clone()], Some(extra.clone()));
        assert_eq!(injected.len(), 2);
        assert_eq!(injected[1].hwnd, 2);
        let same = append_mouse_target(vec![listed.clone()], Some(listed.clone()));
        assert_eq!(same.len(), 1);
        assert!(append_mouse_target(vec![listed], None).len() == 1);
        assert!(append_mouse_target(Vec::new(), Some(WindowInfo::default())).is_empty());
    }

    #[test]
    fn short_title_matches_csharp_get_short_title() {
        let explorer = WindowInfo {
            class_name: "CabinetWClass".into(),
            title: r"C:\Users\foo\Documents".into(),
            ..WindowInfo::default()
        };
        assert_eq!(explorer.short_title(), "Documents");
        assert_eq!(explorer.menu_title(), "Documents");

        let clover = WindowInfo {
            class_name: "CloverWidgitWin_0".into(),
            title: r"D:\share\work".into(),
            is_foreground: true,
            ..WindowInfo::default()
        };
        assert_eq!(clover.menu_title(), "【前台】work");

        let short_explorer = WindowInfo {
            class_name: "CabinetWClass".into(),
            title: "Downloads".into(),
            ..WindowInfo::default()
        };
        assert_eq!(short_explorer.short_title(), "Downloads");

        let notepad = WindowInfo {
            class_name: "Notepad".into(),
            title: "a".repeat(80),
            ..WindowInfo::default()
        };
        assert_eq!(notepad.short_title(), "a".repeat(80));
        assert_eq!(notepad.menu_title(), "a".repeat(80));
    }
}
