//! Live Explorer windows aligned with C# `ExplorerShellWindows` + `ExplorerPathCodec`.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::{json, Value};

use crate::store::AppStore;

#[cfg(any(windows, test))]
#[path = "explorer_snapshot.rs"]
mod snapshot;

pub const THIS_PC_SHELL: &str = "shell:::{20D04FE0-3AEA-1069-A2D8-08002B30309D}";
pub const HOME_SHELL: &str = "shell:::{679f85cb-0220-4080-b29b-5540cc05aab6}";
pub const RECYCLE_BIN_SHELL: &str = "shell:::{645FF040-5081-101B-9F08-00AA002F954E}";
pub const NETWORK_SHELL: &str = "shell:::{F02C1A0D-BE21-4350-88B0-7367FC96EF3C}";

#[derive(Debug, Clone)]
pub struct OpenedFolder {
    pub hwnd: i64,
    pub path: String,
    pub title: String,
}

/// Win11 File Explorer tabs that share one top-level HWND (C# `ExplorerTabSnapshot`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplorerTabSnapshot {
    pub window_handle: i64,
    pub tab_handle: i64,
    pub path: String,
    pub is_active: bool,
}

/// C# `ExplorerTabMatcher.HwndEquals` — compare as unsigned 32-bit HWND.
pub fn hwnd_equals(left: i64, right: i64) -> bool {
    left as u32 == right as u32
}

pub fn select_for_handle(
    handle: i64,
    tabs: &[ExplorerTabSnapshot],
    active_tab_handle: i64,
) -> Option<ExplorerTabSnapshot> {
    if handle == 0 || tabs.is_empty() {
        return None;
    }
    if let Some(tab) = tabs
        .iter()
        .find(|tab| tab.tab_handle != 0 && hwnd_equals(tab.tab_handle, handle))
    {
        return Some(tab.clone());
    }
    let window_tabs: Vec<&ExplorerTabSnapshot> = tabs
        .iter()
        .filter(|tab| hwnd_equals(tab.window_handle, handle))
        .collect();
    if window_tabs.is_empty() {
        return None;
    }
    if window_tabs.len() == 1 {
        return Some(window_tabs[0].clone());
    }
    if active_tab_handle != 0 {
        if let Some(tab) = window_tabs
            .iter()
            .find(|tab| tab.tab_handle != 0 && hwnd_equals(tab.tab_handle, active_tab_handle))
        {
            return Some((*tab).clone());
        }
    }
    if let Some(tab) = window_tabs.iter().find(|tab| tab.is_active) {
        return Some((*tab).clone());
    }
    Some(window_tabs[0].clone())
}

pub fn mark_active_first(window_tabs: &[ExplorerTabSnapshot], active_tab_handle: i64) -> Vec<ExplorerTabSnapshot> {
    if window_tabs.is_empty() {
        return Vec::new();
    }
    let mut result = Vec::with_capacity(window_tabs.len());
    let mut active: Option<ExplorerTabSnapshot> = None;
    for tab in window_tabs {
        let is_active = active_tab_handle != 0
            && tab.tab_handle != 0
            && hwnd_equals(tab.tab_handle, active_tab_handle);
        let mut marked = tab.clone();
        marked.is_active = is_active;
        if is_active && active.is_none() {
            active = Some(marked);
        } else {
            result.push(marked);
        }
    }
    if active.is_none() && window_tabs.len() == 1 {
        let mut only = window_tabs[0].clone();
        only.is_active = true;
        return vec![only];
    }
    if let Some(active) = active {
        result.insert(0, active);
    }
    result
}

pub fn order_by_window_z_order(
    tabs: &[ExplorerTabSnapshot],
    windows_front_to_back: &[i64],
    get_active_tab_handle: impl Fn(i64) -> i64,
) -> Vec<ExplorerTabSnapshot> {
    if tabs.is_empty() {
        return Vec::new();
    }
    let mut remaining = tabs.to_vec();
    let mut result = Vec::with_capacity(tabs.len());
    for window in windows_front_to_back {
        let mut window_tabs = Vec::new();
        for i in (0..remaining.len()).rev() {
            if !hwnd_equals(remaining[i].window_handle, *window) {
                continue;
            }
            window_tabs.push(remaining.remove(i));
        }
        if window_tabs.is_empty() {
            continue;
        }
        window_tabs.reverse();
        let active_tab = get_active_tab_handle(*window);
        result.extend(mark_active_first(&window_tabs, active_tab));
    }
    result.extend(remaining);
    result
}

impl OpenedFolder {
    pub fn payload(&self) -> Value {
        json!({ "path": self.path, "windowHandle": self.hwnd })
    }
}

pub fn list_opened_folders() -> Vec<OpenedFolder> {
    if crate::clipboard::is_headless() {
        return Vec::new();
    }
    let live = live_explorer_tabs();
    live.into_iter()
        .filter(|folder| is_valid_reopen_path(&folder.path))
        .collect()
}

/// Management needs live tab identities without opening or checking the
/// locations they refer to. Offline folders must remain manageable.
pub fn list_opened_folders_unprobed() -> Vec<OpenedFolder> {
    if crate::clipboard::is_headless() { return Vec::new(); }
    live_explorer_tabs().into_iter().filter(|folder| !folder.path.trim().is_empty()).collect()
}

/// Custom-path editor: actual Explorer directories, once per path, in tab/z order.
pub fn list_opened_directory_paths() -> Vec<String> {
    if crate::clipboard::is_headless() {
        return Vec::new();
    }
    opened_directory_paths_with(live_explorer_tabs(), crate::file_menu_extract::menu_directory_path_status)
}

fn opened_directory_paths_with(
    folders: Vec<OpenedFolder>,
    mut probe: impl FnMut(&str) -> crate::file_menu_extract::MenuPathStatus,
) -> Vec<String> {
    let mut seen = HashSet::new();
    folders.into_iter().filter_map(|folder| {
        let path = folder.path.trim();
        if path.is_empty() || is_shell_path(path) || !seen.insert(path.to_lowercase()) {
            return None;
        }
        // Keep a live tab when a slow share cannot be confirmed within the deadline.
        (probe(path) != crate::file_menu_extract::MenuPathStatus::Missing).then(|| path.to_string())
    }).collect()
}

// Shell automation can use an existing apartment. Only balance successful
// initializations that this guard owns, including the nested S_FALSE case.
#[cfg(windows)]
pub(crate) struct ShellApartment {
    initialized: bool,
    _thread_bound: std::marker::PhantomData<std::rc::Rc<()>>,
}

#[cfg(windows)]
impl ShellApartment {
    pub(crate) fn initialize() -> windows::core::Result<Self> {
        use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
        use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
        let result = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if result != RPC_E_CHANGED_MODE {
            result.ok()?;
        }
        Ok(Self { initialized: result.is_ok(), _thread_bound: std::marker::PhantomData })
    }
}

#[cfg(windows)]
impl Drop for ShellApartment {
    fn drop(&mut self) {
        if self.initialized {
            unsafe { windows::Win32::System::Com::CoUninitialize(); }
        }
    }
}

/// C# `ExplorerPathHelper.IsShellPath`.
pub fn is_shell_path(path: &str) -> bool {
    path.to_ascii_lowercase().starts_with("shell:") || path.starts_with("::")
}

/// Known virtual folders have menu labels and icons without filesystem probes.
pub(crate) fn shell_location_presentation(path: &str) -> Option<(&'static str, &'static str)> {
    let path = path.trim();
    [
        (THIS_PC_SHELL, "此电脑", "fa:Light_Desktop"),
        (HOME_SHELL, "主文件夹", "fa:Light_Home"),
        (RECYCLE_BIN_SHELL, "回收站", "fa:Light_TrashAlt"),
        (NETWORK_SHELL, "网络", "fa:Light_NetworkWired"),
    ]
    .into_iter()
    .find(|(shell, _, _)| {
        path.eq_ignore_ascii_case(shell) || path.eq_ignore_ascii_case(&shell[6..])
    })
    .map(|(_, title, icon)| (title, icon))
}

pub(crate) fn is_this_pc(path: &str) -> bool {
    let path = path.trim();
    path.eq_ignore_ascii_case(THIS_PC_SHELL) || path.eq_ignore_ascii_case(&THIS_PC_SHELL[6..])
}

/// C# `ExplorerPathHelper.IsValidReOpenPath`.
pub fn is_valid_reopen_path(path: &str) -> bool {
    is_valid_reopen_path_with(path, crate::file_menu_extract::menu_file_path_status)
}

fn is_valid_reopen_path_with(
    path: &str,
    probe: impl FnOnce(&str) -> crate::file_menu_extract::MenuPathStatus,
) -> bool {
    if path.is_empty() || path.chars().all(char::is_whitespace) {
        return false;
    }
    if is_shell_path(path) {
        return true;
    }
    probe(path) != crate::file_menu_extract::MenuPathStatus::Missing
}

/// First explorer history path that is not currently opened (C# `GetLastClosed`).
pub fn last_closed(records: &[String], opened: &[String]) -> Option<String> {
    let opened: HashSet<String> = opened.iter().filter(|p| !p.is_empty()).map(|p| crate::reopen_history::directory_key(p)).collect();
    records
        .iter()
        .find(|p| !p.is_empty() && !opened.contains(&crate::reopen_history::directory_key(p)))
        .cloned()
}

/// Paths that left the live Explorer set since the previous snapshot.
pub fn newly_closed(previous: &[String], current: &[String]) -> Vec<String> {
    let current: HashSet<String> = current.iter().filter(|p| !p.is_empty()).map(|p| crate::reopen_history::directory_key(p)).collect();
    previous
        .iter()
        .filter(|p| !p.is_empty() && !current.contains(&crate::reopen_history::directory_key(p)))
        .cloned()
        .collect()
}

/// Closed-folder history for the reopen submenu (C# `GetAllClosedPath`: not opened, no exists filter).
pub fn closed_history(records: &[String], opened: &[String], limit: usize) -> Vec<String> {
    let opened: HashSet<String> = opened.iter().filter(|p| !p.is_empty()).map(|p| crate::reopen_history::directory_key(p)).collect();
    records
        .iter()
        .filter(|p| !p.is_empty() && !opened.contains(&crate::reopen_history::directory_key(p)))
        .take(limit)
        .cloned()
        .collect()
}

pub struct ExplorerMonitor {
    stop: Arc<AtomicBool>,
}

#[derive(Default)]
struct ExplorerHistoryWatch {
    previous: Option<Vec<String>>,
}

impl ExplorerHistoryWatch {
    fn poll(
        &mut self,
        enabled: bool,
        read_opened: impl FnOnce() -> Vec<String>,
        mut valid_closed: impl FnMut(&str) -> bool,
    ) -> Vec<String> {
        if !enabled {
            // Re-enabling starts from the current tabs; do not record anything
            // that happened while the user had history recording turned off.
            self.previous = None;
            return Vec::new();
        }
        let current = read_opened();
        let closed = self.previous.as_ref()
            .map(|previous| newly_closed(previous, &current))
            .unwrap_or_default();
        self.previous = Some(current);
        // A live tab remains open even when its directory is temporarily
        // inaccessible. Only validate paths that actually left the Shell set.
        closed.into_iter().filter(|path| valid_closed(path)).collect()
    }
}

impl ExplorerMonitor {
    pub fn start(store: Arc<Mutex<AppStore>>, enabled: Arc<AtomicBool>, history_limit: Arc<AtomicUsize>, capture: crate::clipboard::CaptureHub) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = stop.clone();
        let _ = thread::Builder::new()
            .name("cliphost-explorer".into())
            .spawn(move || {
                let mut watch = ExplorerHistoryWatch::default();
                while !stop_flag.load(Ordering::Relaxed) {
                    let closed = watch.poll(
                        enabled.load(Ordering::Relaxed),
                        || list_opened_folders_unprobed().into_iter().map(|f| f.path).collect(),
                        is_valid_reopen_path,
                    );
                    for path in closed {
                        let store = store.lock();
                        if enabled.load(Ordering::Relaxed) {
                            let changed = store.upsert_explorer_record_with_limit(
                                &path, history_limit.load(Ordering::Relaxed),
                            );
                            drop(store);
                            if matches!(changed, Ok(true)) {
                                capture.push("reopenHistoryChanged", json!({"action":"record","changed":1}));
                            }
                        }
                    }
                    thread::sleep(Duration::from_millis(400));
                }
            });
        Self { stop }
    }
}

impl Drop for ExplorerMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn live_explorer_tabs() -> Vec<OpenedFolder> {
    #[cfg(windows)]
    {
        static SNAPSHOTS: std::sync::OnceLock<snapshot::ExplorerSnapshots> = std::sync::OnceLock::new();
        SNAPSHOTS.get_or_init(|| snapshot::ExplorerSnapshots::new(shell_windows_tabs))
            .snapshot(snapshot::QUERY_BUDGET)
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

pub fn from_location(url: &str, name: &str) -> String {
    let from_url = from_location_url(url);
    if !from_url.is_empty() {
        return from_url;
    }
    from_location_name(name)
}

/// Prefer cheap location properties; only query Document for unresolved Shell locations.
fn read_explorer_path(url: &str, name: &str, document: impl FnOnce() -> Option<String>) -> String {
    let location = from_location(url, name);
    let bytes = location.as_bytes();
    let concrete = location.to_ascii_lowercase().starts_with("shell:::")
        || location.starts_with(r"\\")
        || (bytes.len() >= 3 && bytes[1] == b':' && matches!(bytes[2], b'\\' | b'/'));
    if concrete {
        location
    } else {
        document().unwrap_or_default()
    }
}

pub fn from_location_url(location_url: &str) -> String {
    let trimmed = location_url.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.to_ascii_lowercase().starts_with("file:") {
        return file_url_to_local_path(trimmed).unwrap_or_default();
    }
    trimmed.to_string()
}

pub fn from_location_name(location_name: &str) -> String {
    match location_name.trim() {
        "此电脑" | "This PC" | "Computer" => THIS_PC_SHELL.into(),
        "主文件夹" | "Home" | "快速访问" | "Quick access" => HOME_SHELL.into(),
        "回收站" | "Recycle Bin" => RECYCLE_BIN_SHELL.into(),
        "网络" | "Network" => NETWORK_SHELL.into(),
        _ => String::new(),
    }
}

pub(crate) fn file_url_to_local_path(value: &str) -> Option<String> {
    // Without an absolute path, WHATWG parsing can interpret file:relative as
    // /relative. Do not turn a malformed shell link into a different local file.
    let (_, rest) = value.split_once(':')?;
    if !rest.starts_with(['/', '\\']) {
        return None;
    }
    let uri = url::Url::parse(value).ok()?;
    if uri.scheme() != "file" {
        return None;
    }
    // A UNC path needs a share as well as a server. Reject server-only URLs
    // before conversion, which otherwise cannot yield an absolute Windows path.
    if uri.host_str().is_some_and(|host| host != "localhost") {
        let share = uri.path_segments()?.next()?.to_ascii_lowercase();
        if share.is_empty() || ["%2f", "%5c", "%00"].iter().any(|escape| share.contains(escape)) {
            return None;
        }
    }
    if cfg!(windows) && uri.host_str().is_none()
        && uri.path_segments().is_none_or(|mut parts| parts.nth(1).is_none())
    {
        return None;
    }
    let path = uri.to_file_path().ok()?;
    if !path.is_absolute() {
        return None;
    }
    let path = path.into_os_string().into_string().ok()?;
    (!path.contains('\0')).then_some(path)
}

#[cfg(windows)]
fn shell_windows_tabs() -> Result<Vec<OpenedFolder>, String> {
    let (tabs, titles) = enumerate_tab_snapshots()?;
    if tabs.is_empty() {
        return Ok(Vec::new());
    }
    let ordered = order_by_window_z_order(&tabs, &enum_windows_front_to_back(), get_active_tab_handle);
    Ok(ordered
        .into_iter()
        .map(|tab| {
            let title = titles
                .get(&(tab.window_handle, tab.tab_handle))
                .cloned()
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| folder_title(&tab.path));
            OpenedFolder {
                hwnd: tab.window_handle,
                path: tab.path,
                title,
            }
        })
        .collect())
}

#[cfg(windows)]
fn folder_title(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("资源管理器")
        .to_string()
}

#[cfg(windows)]
fn enumerate_tab_snapshots() -> Result<(Vec<ExplorerTabSnapshot>, HashMap<(i64, i64), String>), String> {
    use windows::core::{Interface, VARIANT};
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
    use windows::Win32::UI::Shell::{IShellWindows, IWebBrowser2, ShellWindows};

    unsafe {
        let _apartment = ShellApartment::initialize().map_err(|e| e.to_string())?;
        let windows = CoCreateInstance::<_, IShellWindows>(&ShellWindows, None, CLSCTX_ALL)
            .map_err(|e| e.to_string())?;
        let count = windows.Count().map_err(|e| e.to_string())?;
        let mut tabs = Vec::new();
        let mut titles = HashMap::new();
        for index in 0..count {
            let Ok(dispatch) = windows.Item(&VARIANT::from(index)) else {
                continue;
            };
            let Ok(ie) = dispatch.cast::<IWebBrowser2>() else {
                continue;
            };
            if !is_explorer_process(&ie) {
                continue;
            }
            let window = ie.HWND().map(|h| h.0 as i64).unwrap_or(0);
            let tab_handle = try_get_tab_handle(&ie);
            let url = ie.LocationURL().map(|s| s.to_string()).unwrap_or_default();
            let name = ie.LocationName().map(|s| s.to_string()).unwrap_or_default();
            let path = read_explorer_path(&url, &name, || try_document_folder_path(&ie));
            let active_tab = get_active_tab_handle(window);
            let mut is_active = tab_handle != 0 && hwnd_equals(tab_handle, active_tab);
            if !is_active && active_tab != 0 {
                let tab_title = window_title(active_tab);
                is_active = !tab_title.is_empty() && name.eq_ignore_ascii_case(&tab_title);
            }
            titles.insert((window, tab_handle), name);
            tabs.push(ExplorerTabSnapshot {
                window_handle: window,
                tab_handle,
                path,
                is_active,
            });
        }
        Ok((tabs, titles))
    }
}

/// C# `ExplorerShellWindows.TryGetTabHandle` — IServiceProvider + SID_STopLevelBrowser.
#[cfg(windows)]
fn try_get_tab_handle(ie: &windows::Win32::UI::Shell::IWebBrowser2) -> i64 {
    use windows::core::Interface;
    use windows::Win32::System::Com::IServiceProvider;
    use windows::Win32::System::Ole::IOleWindow;
    use windows::Win32::UI::Shell::{IShellBrowser, IUnknown_QueryService, SID_STopLevelBrowser};

    unsafe fn ole_hwnd(browser: &IShellBrowser) -> Option<i64> {
        let ole = browser.cast::<IOleWindow>().ok()?;
        let hwnd = unsafe { ole.GetWindow().ok()? };
        let value = hwnd.0 as i64;
        (value != 0).then_some(value)
    }

    unsafe {
        if let Ok(provider) = ie.cast::<IServiceProvider>() {
            if let Ok(browser) = provider.QueryService::<IShellBrowser>(&SID_STopLevelBrowser) {
                if let Some(hwnd) = ole_hwnd(&browser) {
                    return hwnd;
                }
            }
        }
        if let Ok(browser) = IUnknown_QueryService::<_, IShellBrowser>(ie, &SID_STopLevelBrowser) {
            if let Some(hwnd) = ole_hwnd(&browser) {
                return hwnd;
            }
        }
    }
    0
}

/// C# `ExplorerShellWindows.TryGetDocumentFolderPath` — Document.Folder.Self.Path.
#[cfg(windows)]
fn try_document_folder_path(ie: &windows::Win32::UI::Shell::IWebBrowser2) -> Option<String> {
    use windows::core::Interface;
    use windows::Win32::UI::Shell::{Folder2, IShellFolderViewDual};
    unsafe {
        let document = ie.Document().ok()?;
        let dual = document.cast::<IShellFolderViewDual>().ok()?;
        let folder = dual.Folder().ok()?;
        let folder2 = folder.cast::<Folder2>().ok()?;
        let item = folder2.Self_().ok()?;
        let path = item.Path().ok()?.to_string();
        let trimmed = path.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    }
}

#[cfg(windows)]
fn is_explorer_process(ie: &windows::Win32::UI::Shell::IWebBrowser2) -> bool {
    let Ok(full) = (unsafe { ie.FullName() }) else {
        return false;
    };
    std::path::Path::new(&full.to_string())
        .file_stem()
        .and_then(|n| n.to_str())
        .is_some_and(|stem| stem.eq_ignore_ascii_case("explorer"))
}

pub fn window_class_name(hwnd: i64) -> String {
    if hwnd == 0 {
        return String::new();
    }
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::GetClassNameW;
        let mut buf = [0u16; 64];
        let n = unsafe { GetClassNameW(HWND(hwnd as *mut _), &mut buf) };
        if n <= 0 {
            String::new()
        } else {
            String::from_utf16_lossy(&buf[..n as usize])
        }
    }
    #[cfg(not(windows))]
    {
        let _ = hwnd;
        String::new()
    }
}

/// C# `WinProperty.IsExplorerWindow` (not `ExploreWClass`).
pub fn is_explorer_window_class(class_name: &str) -> bool {
    class_name == "CabinetWClass" || class_name == "CloverWidgitWin_0"
}

pub fn is_explorer_hwnd(hwnd: i64) -> bool {
    is_explorer_window_class(&window_class_name(hwnd))
}

/// C# `ExplorerWindowNavigator.TryNavigate`: folder as-is, file → parent, then `IWebBrowser2.Navigate`.
pub fn try_navigate_path(hwnd: i64, path: &str) -> bool {
    if crate::clipboard::is_headless() || hwnd == 0 || !is_explorer_hwnd(hwnd) {
        return false;
    }
    let Some(folder) = navigation_folder_with(path, filesystem_navigation_folder) else {
        return false;
    };
    if try_navigate(hwnd, &folder) {
        let _ = crate::window_menu::activate(hwnd);
        true
    } else {
        false
    }
}

/// Shell parsing names must reach Explorer without filesystem normalization.
fn navigation_folder_with(
    path: &str,
    resolve_filesystem: impl FnOnce(&str) -> Option<String>,
) -> Option<String> {
    let trimmed = path.trim().trim_matches('"');
    if trimmed.is_empty() {
        None
    } else if is_shell_path(trimmed) {
        Some(trimmed.to_string())
    } else {
        resolve_filesystem(trimmed)
    }
}

fn filesystem_navigation_folder(path: &str) -> Option<String> {
    if std::path::Path::new(path).is_dir() {
        Some(
            std::path::absolute(path)
                .ok()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string()),
        )
    } else if std::path::Path::new(path).is_file() {
        match std::path::Path::new(path).parent() {
            Some(parent) if parent.is_dir() => Some(parent.to_string_lossy().into_owned()),
            _ => None,
        }
    } else {
        None
    }
}

pub fn try_navigate(hwnd: i64, folder: &str) -> bool {
    if crate::clipboard::is_headless() || hwnd == 0 || folder.trim().is_empty() {
        return false;
    }
    #[cfg(windows)]
    {
        use windows::core::{BSTR, Interface, VARIANT};
        use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
        use windows::Win32::UI::Shell::{IShellWindows, IWebBrowser2, ShellWindows};
        unsafe {
            let Ok(_apartment) = ShellApartment::initialize() else { return false; };
            let Ok(windows) = CoCreateInstance::<_, IShellWindows>(&ShellWindows, None, CLSCTX_ALL) else {
                return false;
            };
            let Ok(count) = windows.Count() else {
                return false;
            };
            let mut explorers: Vec<IWebBrowser2> = Vec::new();
            let mut snapshots: Vec<ExplorerTabSnapshot> = Vec::new();
            for index in 0..count {
                let Ok(dispatch) = windows.Item(&VARIANT::from(index)) else {
                    continue;
                };
                let Ok(ie) = dispatch.cast::<IWebBrowser2>() else {
                    continue;
                };
                if !is_explorer_process(&ie) {
                    continue;
                }
                let window = ie.HWND().map(|h| h.0 as i64).unwrap_or(0);
                if !hwnd_equals(window, hwnd) {
                    continue;
                }
                snapshots.push(ExplorerTabSnapshot {
                    window_handle: window,
                    tab_handle: try_get_tab_handle(&ie),
                    path: String::new(),
                    is_active: false,
                });
                explorers.push(ie);
            }
            if explorers.is_empty() {
                return false;
            }
            let selected = select_for_handle(hwnd, &snapshots, get_active_tab_handle(hwnd));
            let mut target_index = 0usize;
            if let Some(selected) = selected {
                if selected.tab_handle != 0 {
                    if let Some((index, _)) = snapshots.iter().enumerate().find(|(_, snap)| {
                        snap.tab_handle != 0 && hwnd_equals(snap.tab_handle, selected.tab_handle)
                    }) {
                        target_index = index;
                    }
                }
            }
            // Optional Automation values still need non-null VARIANT pointers
            // across Explorer's COM proxy (null fails with RPC_X_NULL_REF_POINTER).
            let empty = VARIANT::default();
            return explorers[target_index]
                .Navigate(&BSTR::from(folder), Some(&empty), Some(&empty), Some(&empty), Some(&empty))
                .is_ok();
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (hwnd, folder);
        false
    }
}

#[cfg(windows)]
fn get_active_tab_handle(explorer_hwnd: i64) -> i64 {
    use windows::core::w;
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumChildWindows, FindWindowExW, GetClassNameW,
    };
    if explorer_hwnd == 0 {
        return 0;
    }
    unsafe {
        if let Ok(hwnd) = FindWindowExW(
            HWND(explorer_hwnd as *mut _),
            HWND::default(),
            w!("ShellTabWindowClass"),
            windows::core::PCWSTR::null(),
        ) {
            if !hwnd.0.is_null() {
                return hwnd.0 as i64;
            }
        }
        struct State {
            found: i64,
        }
        unsafe extern "system" fn proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
            let state = unsafe { &mut *(lparam.0 as *mut State) };
            let mut class = [0u16; 64];
            let n = unsafe { GetClassNameW(hwnd, &mut class) };
            if n > 0 {
                let class = String::from_utf16_lossy(&class[..n as usize]);
                if class == "ShellTabWindowClass" {
                    state.found = hwnd.0 as i64;
                    return BOOL(0);
                }
            }
            BOOL(1)
        }
        let mut state = State { found: 0 };
        let _ = EnumChildWindows(
            HWND(explorer_hwnd as *mut _),
            Some(proc),
            LPARAM(&mut state as *mut State as isize),
        );
        state.found
    }
}

#[cfg(windows)]
fn enum_windows_front_to_back() -> Vec<i64> {
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::EnumWindows;

    struct State {
        items: Vec<i64>,
    }
    unsafe extern "system" fn proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let state = unsafe { &mut *(lparam.0 as *mut State) };
        state.items.push(hwnd.0 as i64);
        BOOL(1)
    }
    let mut state = State { items: Vec::new() };
    unsafe {
        let _ = EnumWindows(Some(proc), LPARAM(&mut state as *mut State as isize));
    }
    state.items
}

#[cfg(windows)]
fn window_title(hwnd: i64) -> String {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::GetWindowTextW;
    if hwnd == 0 {
        return String::new();
    }
    let mut title = [0u16; 256];
    let n = unsafe { GetWindowTextW(HWND(hwnd as *mut _), &mut title) };
    if n > 0 {
        String::from_utf16_lossy(&title[..n as usize])
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explorer_history_disabled_skips_queries_and_reenable_sets_a_new_baseline() {
        let mut watch = ExplorerHistoryWatch::default();
        assert!(watch.poll(false,
            || panic!("disabled history must not query Shell"),
            |_| panic!("disabled history must not probe paths"),
        ).is_empty());
        assert!(watch.poll(true, || vec![r"C:\before".into()],
            |_| panic!("the initial baseline must not probe paths"),
        ).is_empty());
        assert!(watch.poll(false,
            || panic!("disabled history must not query Shell"),
            |_| panic!("disabled history must not probe paths"),
        ).is_empty());
        assert!(watch.poll(true, || vec![r"C:\after".into()],
            |_| panic!("re-enabling must not record tabs closed while disabled"),
        ).is_empty());
        assert_eq!(watch.poll(true, Vec::new, |_| true), vec![r"C:\after"]);
    }

    #[test]
    fn explorer_history_probes_only_tabs_that_leave_the_live_set() {
        let mut watch = ExplorerHistoryWatch::default();
        let opened = || vec![r"C:\still-open".into(), r"C:\closed".into(), r"C:\missing".into()];
        assert!(watch.poll(true, opened,
            |_| panic!("the initial baseline must not probe paths"),
        ).is_empty());
        for _ in 0..3 {
            assert!(watch.poll(true, opened,
                |_| panic!("live paths must not be probed on idle ticks"),
            ).is_empty());
        }
        let mut probed = Vec::new();
        let closed = watch.poll(true, || vec![r"c:\still-open\".into()], |path| {
            probed.push(path.to_owned());
            path != r"C:\missing"
        });
        assert_eq!(probed, vec![r"C:\closed", r"C:\missing"]);
        assert_eq!(closed, vec![r"C:\closed"]);
        assert!(watch.poll(true, || vec![r"C:\still-open".into()],
            |_| panic!("closed paths must not be reprobed every tick"),
        ).is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn explorer_location_avoids_document_for_concrete_paths_and_falls_back_for_virtual_urls() {
        for (url, name, expected) in [
            ("file:///C:/Docs/%E4%B8%AD%E6%96%87", "中文", r"C:\Docs\中文"),
            ("file://server/share", "share", r"\\server\share"),
            ("", "This PC", THIS_PC_SHELL),
            (HOME_SHELL, "Home", HOME_SHELL),
        ] {
            assert_eq!(read_explorer_path(url, name, || panic!("Document must not be queried for {url}")), expected);
        }
        assert_eq!(read_explorer_path("search-ms:query=test", "Search", || Some(r"C:\Docs".into())), r"C:\Docs");
        assert_eq!(read_explorer_path("", "Unknown virtual folder", || Some("::{fixture}".into())), "::{fixture}");
        assert_eq!(read_explorer_path("search-ms:query=test", "Search", || None), "");
    }

    #[test]
    fn reopen_path_timeout_keeps_the_path_but_confirmed_missing_rejects_it() {
        use crate::file_menu_extract::MenuPathStatus::{Present, Missing, Unknown};
        for status in [Present, Missing, Unknown] {
            assert_eq!(is_valid_reopen_path_with(r"\\slow-share\docs", |_| status), status != Missing);
        }
        assert!(!is_valid_reopen_path_with("  ", |_| panic!("empty path must not be probed")));
        assert!(is_valid_reopen_path_with("shell:::this-pc", |_| panic!("shell path must not be probed")));
    }

    #[cfg(windows)]
    #[test]
    fn location_url_unescapes_file_uri() {
        assert_eq!(
            from_location_url("file:///C:/Users/foo%20bar"),
            "C:\\Users\\foo bar"
        );
        assert_eq!(from_location_url("file:///D:/share"), "D:\\share");
    }

    #[cfg(windows)]
    #[test]
    fn file_uri_preserves_unc_and_decodes_unicode_without_query_or_fragment() {
        assert_eq!(from_location_url("file://server/share/folder%20one/%E4%B8%AD%E6%96%87"),
            r"\\server\share\folder one\中文");
        assert_eq!(from_location_url("file:///C:/a%23b.png?ignored=yes#part"), r"C:\a#b.png");
        assert_eq!(from_location_url("FILE://LOCALHOST/C:/Windows"), r"C:\Windows");
    }

    #[cfg(windows)]
    #[test]
    fn file_uri_literal_percent_before_unicode_does_not_panic() {
        for (encoded, decoded) in [
            ("%中文", "%中文"), ("%1中文", "%1中文"), ("中文%", "中文%"),
            ("%25E4", "%E4"), ("a+b", "a+b"), ("%F0%9F%98%80", "😀"),
        ] {
            assert_eq!(from_location_url(&format!("file:///C:/{encoded}")), format!(r"C:\{decoded}"));
        }
    }

    #[test]
    fn file_uri_roundtrips_native_filenames_and_keeps_non_file_locations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("%中文 +#.png");
        let uri = url::Url::from_file_path(&path).unwrap();
        assert_eq!(from_location_url(uri.as_str()), path.to_str().unwrap());
        assert_eq!(from_location_url(THIS_PC_SHELL), THIS_PC_SHELL);
        assert_eq!(from_location_url("  "), "");
        assert_eq!(from_location("file:relative", "This PC"), THIS_PC_SHELL);
    }

    #[test]
    fn file_uri_incomplete_or_escaped_components_never_panic_or_produce_relative_paths() {
        for value in [
            "file://server", "file://server/", "file://server/%2F", "file://server/%5C",
            "file://server/%00", "file:///C%3A", "file:///C%3A/", "file:///C%3a/a",
            "file:///C:/%FF", "file:///C:/%E4%B8", "file:///C:/%中文", "file://server/%中文",
        ] {
            let decoded = std::panic::catch_unwind(|| file_url_to_local_path(value))
                .unwrap_or_else(|_| panic!("file URI parser panicked for {value}"));
            if let Some(path) = decoded {
                assert!(std::path::Path::new(&path).is_absolute(), "{value}: {path}");
                assert!(!path.contains('\0'));
            }
        }
    }

    #[test]
    fn location_name_maps_special_shell_folders() {
        assert_eq!(from_location_name("此电脑"), THIS_PC_SHELL);
        assert_eq!(from_location_name("This PC"), THIS_PC_SHELL);
        assert_eq!(from_location_name("回收站"), RECYCLE_BIN_SHELL);
        assert_eq!(from_location_name("docs"), "");
    }

    #[test]
    fn shell_location_presentation_accepts_parsing_name_forms_and_guid_case() {
        for (path, title, icon) in [
            (THIS_PC_SHELL, "此电脑", "fa:Light_Desktop"),
            (HOME_SHELL, "主文件夹", "fa:Light_Home"),
            (RECYCLE_BIN_SHELL, "回收站", "fa:Light_TrashAlt"),
            (NETWORK_SHELL, "网络", "fa:Light_NetworkWired"),
        ] {
            for form in [path.to_string(), path[6..].to_string(), path.to_ascii_lowercase(),
                path.to_ascii_uppercase(), format!("  {}  ", path[6..].to_ascii_uppercase())]
            {
                assert_eq!(shell_location_presentation(&form), Some((title, icon)), "{form}");
                assert_eq!(is_this_pc(&form), path == THIS_PC_SHELL, "{form}");
            }
        }
        for path in ["", r"C:\Docs", "shell:::{unknown}", "shell:Downloads", "::{unknown}"] {
            assert_eq!(shell_location_presentation(path), None, "{path}");
            assert!(!is_this_pc(path), "{path}");
        }
    }

    #[test]
    fn navigation_folder_preserves_shell_paths_without_filesystem_resolution() {
        for path in [THIS_PC_SHELL, &THIS_PC_SHELL[6..], HOME_SHELL, RECYCLE_BIN_SHELL,
            NETWORK_SHELL, "ShElL:::{unknown}", "::{unknown}"]
        {
            let quoted = format!("  \"{path}\"  ");
            assert_eq!(
                navigation_folder_with(&quoted, |_| panic!("Shell path must not use the filesystem")),
                Some(path.to_string()),
            );
        }
        assert_eq!(navigation_folder_with("  \"\"  ", |_| panic!("empty path must not be resolved")), None);
        assert_eq!(navigation_folder_with(r#"  "C:\Docs"  "#, |path| {
            assert_eq!(path, r"C:\Docs");
            Some("resolved folder".into())
        }), Some("resolved folder".into()));
    }

    #[test]
    fn navigation_folder_keeps_filesystem_directory_and_file_parent_behavior() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("中文 % folder");
        std::fs::create_dir(&folder).unwrap();
        let file = folder.join("note.txt");
        std::fs::write(&file, "fixture").unwrap();
        let expected = folder.to_string_lossy().into_owned();
        for path in [&folder, &file] {
            assert_eq!(navigation_folder_with(path.to_str().unwrap(), filesystem_navigation_folder),
                Some(expected.clone()));
        }
        assert_eq!(navigation_folder_with(folder.join("missing.txt").to_str().unwrap(),
            filesystem_navigation_folder), None);
    }

    #[test]
    fn opened_directory_paths_preserve_order_and_deduplicate_without_virtual_or_missing_paths() {
        use crate::file_menu_extract::MenuPathStatus::{Present, Missing, Unknown};
        let paths = ["  ", THIS_PC_SHELL, "::virtual", r"C:\Docs", r"c:\docs",
            r"C:\Missing", r"C:\a.txt", r"\\slow-share\docs", r" C:\Äpfel ", r"c:\äpfel", r"C:\Last"];
        let live = paths.iter().map(|path| OpenedFolder {
            hwnd: 1, path: (*path).into(), title: String::new(),
        }).collect();
        let mut probed = Vec::new();
        let result = opened_directory_paths_with(live, |path| {
            probed.push(path.to_string());
            match path {
                r"C:\Missing" | r"C:\a.txt" => Missing,
                r"\\slow-share\docs" => Unknown,
                _ => Present,
            }
        });
        assert_eq!(result, [r"C:\Docs", r"\\slow-share\docs", r"C:\Äpfel", r"C:\Last"]);
        assert_eq!(probed, [r"C:\Docs", r"C:\Missing", r"C:\a.txt", r"\\slow-share\docs", r"C:\Äpfel", r"C:\Last"]);
    }

    #[test]
    fn opened_folders_headless_does_not_query_the_desktop() {
        crate::clipboard::force_headless(true);
        assert!(list_opened_folders().is_empty());
        assert!(list_opened_directory_paths().is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn shell_apartment_balances_repeated_and_nested_initialization() {
        thread::spawn(|| unsafe {
            use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
            use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
            for _ in 0..32 {
                let outer = ShellApartment::initialize().unwrap();
                drop(ShellApartment::initialize().unwrap());
                assert_eq!(CoInitializeEx(None, COINIT_MULTITHREADED), RPC_E_CHANGED_MODE);
                drop(outer);
                CoInitializeEx(None, COINIT_MULTITHREADED).ok().expect("leaked STA initialization");
                CoUninitialize();
            }
        }).join().unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn shell_apartment_preserves_the_callers_existing_mta() {
        thread::spawn(|| unsafe {
            use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
            use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_MULTITHREADED};
            CoInitializeEx(None, COINIT_MULTITHREADED).ok().unwrap();
            for _ in 0..32 { drop(ShellApartment::initialize().unwrap()); }
            assert_eq!(CoInitializeEx(None, COINIT_APARTMENTTHREADED), RPC_E_CHANGED_MODE);
            CoUninitialize();
            CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().expect("changed caller's COM lifetime");
            CoUninitialize();
        }).join().unwrap();
    }

    fn tab(window: i64, tab: i64, path: &str, active: bool) -> ExplorerTabSnapshot {
        ExplorerTabSnapshot {
            window_handle: window,
            tab_handle: tab,
            path: path.into(),
            is_active: active,
        }
    }

    #[test]
    fn hwnd_equals_uses_unsigned_32bit() {
        assert!(hwnd_equals(-1, u32::MAX as i64));
        assert!(!hwnd_equals(1, 2));
    }

    #[test]
    fn select_for_handle_prefers_tab_then_active_window_tab() {
        let tabs = vec![
            tab(10, 101, "C:\\a", false),
            tab(10, 102, "C:\\b", true),
            tab(11, 111, "C:\\c", true),
        ];
        assert_eq!(select_for_handle(102, &tabs, 0).unwrap().path, "C:\\b");
        assert_eq!(select_for_handle(10, &tabs, 101).unwrap().path, "C:\\a");
        assert_eq!(select_for_handle(10, &tabs, 0).unwrap().path, "C:\\b");
        assert!(select_for_handle(0, &tabs, 0).is_none());
    }

    #[test]
    fn try_navigate_rejects_empty_handle() {
        assert!(!try_navigate(0, "C:\\Windows"));
        assert!(!try_navigate(1, "   "));
        assert!(!try_navigate_path(0, r"C:\Windows"));
        assert!(!is_explorer_hwnd(0));
        assert!(is_explorer_window_class("CabinetWClass"));
        assert!(is_explorer_window_class("CloverWidgitWin_0"));
        assert!(!is_explorer_window_class("ExploreWClass"));
        assert!(!is_explorer_window_class("#32770"));
    }

    #[test]
    fn reopen_helpers_preserve_closed_set_with_windows_path_identities() {
        assert!(is_shell_path("shell:::{20D04FE0-3AEA-1069-A2D8-08002B30309D}"));
        assert!(is_valid_reopen_path("shell:::foo"));
        assert!(!is_valid_reopen_path(""));
        let records = vec!["shell:::a".into(), "shell:::b".into(), "shell:::c".into()];
        let opened = vec!["shell:::b".into()];
        assert_eq!(last_closed(&records, &opened).as_deref(), Some("shell:::a"));
        assert_eq!(
            newly_closed(&["C:\\a".into(), "C:\\b".into()], &["C:\\b".into()]),
            vec!["C:\\a".to_string()]
        );
        assert_eq!(
            closed_history(&records, &opened, 10),
            vec!["shell:::a".to_string(), "shell:::c".to_string()]
        );
        assert!(
            !is_valid_reopen_path(r"C:\cliphost-missing-reopen-xyz"),
            "C# PathExistsWithTimeout 50ms"
        );
        assert_eq!(
            last_closed(
                &["C:\\Foo".into(), "C:\\Bar".into()],
                &["c:\\foo".into()]
            )
            .as_deref(),
            Some(r"C:\Bar"),
            "Windows aliases of an open path are excluded from recent history"
        );
        assert!(newly_closed(&[r"C:\Foo".into()], &[r"c:\foo\".into()]).is_empty());
        assert_eq!(
            closed_history(
                &[r"C:\cliphost-missing-reopen-xyz".into(), "shell:::keep".into()],
                &[],
                10
            ),
            vec![
                r"C:\cliphost-missing-reopen-xyz".to_string(),
                "shell:::keep".to_string()
            ]
        );
    }

    #[test]
    fn mark_active_first_and_z_order_match_csharp() {
        let window_tabs = vec![tab(10, 101, "C:\\a", false), tab(10, 102, "C:\\b", false)];
        let marked = mark_active_first(&window_tabs, 102);
        assert!(marked[0].is_active);
        assert_eq!(marked[0].path, "C:\\b");
        let ordered = order_by_window_z_order(
            &[tab(11, 201, "D:\\x", false), tab(10, 102, "C:\\b", false)],
            &[10, 11],
            |hwnd| if hwnd == 10 { 102 } else { 0 },
        );
        assert_eq!(ordered[0].window_handle, 10);
        assert!(ordered[0].is_active);
        assert_eq!(ordered[1].window_handle, 11);
    }
}
