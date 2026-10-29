//! C# `FileMenuExtrator` + Recent-folder listing.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use parking_lot::{Condvar, Mutex};

use crate::extra_settings::FileMenuSettings;

/// C# `FileSystemAsyncHelper.PathExistsWithTimeout` default / `GetFileMenuWrapper`.
const PATH_EXISTS_TIMEOUT: Duration = Duration::from_millis(50);
/// C# `FileMenuItemWrapper.GetContextMenuItems`: `FileExists` / `DirectoryExistsWithTimeout(..., 50)`.
const CONTEXT_FS_TIMEOUT: Duration = Duration::from_millis(50);
/// C# `FileMenuExtrator.ExtraFileName` / `TryIsHiddenFile` network probes.
const NETWORK_FS_TIMEOUT: Duration = Duration::from_millis(100);

const RECENT_MAX_RESULTS: usize = 50;
const RECENT_MAX_LINKS: usize = 120;
/// C# `RecentFilesService.CacheTtl`.
const RECENT_CACHE_TTL: Duration = Duration::from_secs(3 * 60);
/// C# `RecentFilesService.DebounceInvalidate`.
const RECENT_DEBOUNCE: Duration = Duration::from_millis(800);
/// C# `RecentFileMenuItemWrapper.ReadyWaitTimeout`.
const RECENT_READY_WAIT: Duration = Duration::from_secs(5);

static GLOBAL_RECENT_FILES: OnceLock<RecentFilesService> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShowType {
    DirFile,
    Dir,
    File,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortBy {
    Na,
    Name,
    CreateTime,
    AccessTime,
    EditTime,
}

impl FileMenuSettings {
    fn show_type(&self) -> ShowType {
        match self.show_type.to_ascii_lowercase().as_str() {
            "dir" => ShowType::Dir,
            "file" => ShowType::File,
            _ => ShowType::DirFile,
        }
    }

    fn sort_by(&self) -> SortBy {
        match self.file_sort_type.to_ascii_lowercase().as_str() {
            "name" => SortBy::Name,
            "createtime" => SortBy::CreateTime,
            "accesstime" => SortBy::AccessTime,
            "edittime" => SortBy::EditTime,
            _ => SortBy::Na,
        }
    }

    fn search_pattern(&self) -> &str {
        let p = self.file_search_parttern.trim();
        if p.is_empty() {
            "*"
        } else {
            p
        }
    }
}

/// C# `FileSystemAsyncHelper.IsNetworkPath` (`\\` UNC prefix).
pub fn is_network_path(path: &str) -> bool {
    path.trim_start().starts_with(r"\\")
}

fn wait_fs_probe(timeout: Duration, job: impl FnOnce() -> bool + Send + 'static) -> Option<bool> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new().spawn(move || {
        let _ = tx.send(job());
    }).ok()?;
    rx.recv_timeout(timeout).ok()
}

fn wait_fs_bool(timeout: Duration, job: impl FnOnce() -> bool + Send + 'static) -> bool {
    wait_fs_probe(timeout, job).unwrap_or(false)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuPathStatus {
    Present,
    Missing,
    Unknown,
}

impl MenuPathStatus {
    fn from_probe(probe: Option<bool>) -> Self {
        match probe {
            Some(true) => Self::Present,
            Some(false) => Self::Missing,
            None => Self::Unknown,
        }
    }
}

/// C# `FileSystemAsyncHelper.PathExistsWithTimeout`.
pub fn path_exists_with_timeout(path: &str, timeout: Duration) -> bool {
    if path.is_empty() {
        return false;
    }
    let owned = path.to_string();
    wait_fs_bool(timeout, move || {
        let p = Path::new(&owned);
        p.is_file() || p.is_dir()
    })
}

/// C# `FileSystemAsyncHelper.DirectoryExistsWithTimeout`.
pub fn directory_exists_with_timeout(path: &str, timeout: Duration) -> bool {
    if path.is_empty() {
        return false;
    }
    let owned = path.to_string();
    wait_fs_bool(timeout, move || Path::new(&owned).is_dir())
}

/// C# `FileSystemAsyncHelper.FileExists` (always 50ms) / `FileExistsWithTimeout`.
pub fn file_exists_with_timeout(path: &str, timeout: Duration) -> bool {
    if path.is_empty() {
        return false;
    }
    let owned = path.to_string();
    wait_fs_bool(timeout, move || Path::new(&owned).is_file())
}

/// C# `FileMenuItemWrapper.GetContextMenuItems` directory probe (50ms).
pub fn context_menu_is_directory(path: &str) -> bool {
    directory_exists_with_timeout(path, CONTEXT_FS_TIMEOUT)
}

/// C# `FileMenuItemWrapper.GetContextMenuItems` file probe (`FileExists`, 50ms).
pub fn context_menu_is_file(path: &str) -> bool {
    file_exists_with_timeout(path, CONTEXT_FS_TIMEOUT)
}

/// Unlike C#'s bool helper, a late probe is not evidence that a path is missing.
pub fn menu_file_path_status(path: &str) -> MenuPathStatus {
    if path.is_empty() {
        return MenuPathStatus::Missing;
    }
    if crate::explorer::is_shell_path(path) {
        return MenuPathStatus::Present;
    }
    let owned = path.to_string();
    MenuPathStatus::from_probe(wait_fs_probe(PATH_EXISTS_TIMEOUT, move || {
        let p = Path::new(&owned);
        p.is_file() || p.is_dir()
    }))
}

/// An Explorer directory probe timing out must not remove its menu entry.
pub fn menu_directory_path_status(path: &str) -> MenuPathStatus {
    if path.is_empty() {
        return MenuPathStatus::Missing;
    }
    let owned = path.to_string();
    MenuPathStatus::from_probe(wait_fs_probe(CONTEXT_FS_TIMEOUT, move || Path::new(&owned).is_dir()))
}

/// C# `FileMenuItemWrapper.GetIsEnabled`: UNC uses `IsPathExist` (50ms); local `File.Exists || Directory.Exists` catch false.
pub fn file_menu_item_enabled(path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    if crate::explorer::is_shell_path(path) {
        return true;
    }
    if is_network_path(path) {
        return path_exists_with_timeout(path, PATH_EXISTS_TIMEOUT);
    }
    let p = Path::new(path);
    p.is_file() || p.is_dir()
}

pub fn is_directory(path: &str) -> bool {
    if is_network_path(path) {
        directory_exists_with_timeout(path, NETWORK_FS_TIMEOUT)
    } else {
        Path::new(path).is_dir()
    }
}

pub fn has_sub_entries(path: &str) -> bool {
    if is_network_path(path) {
        let owned = path.to_string();
        return wait_fs_bool(NETWORK_FS_TIMEOUT, move || dir_has_any_entry(&owned));
    }
    dir_has_any_entry(path)
}

fn dir_has_any_entry(path: &str) -> bool {
    let Ok(mut iter) = fs::read_dir(path) else {
        return false;
    };
    iter.next().is_some()
}

pub fn extra_file_name(path: &str, show_extension: bool) -> String {
    if crate::explorer::is_shell_path(path) {
        return crate::explorer::shell_location_presentation(path)
            .map(|(title, _)| title)
            .unwrap_or(path)
            .to_string();
    }
    if let Some(drive) = drive_display_name(path) {
        return drive;
    }
    let p = Path::new(path);
    let is_dir = if is_network_path(path) {
        directory_exists_with_timeout(path, NETWORK_FS_TIMEOUT)
    } else {
        p.is_dir()
    };
    file_name_for_type(path, is_dir, show_extension)
}

/// Display a known file type without probing its path again.
pub(crate) fn file_name_for_type(path: &str, is_dir: bool, show_extension: bool) -> String {
    // C# `Path.GetFileName`: UNC share roots (`\\server\share`) still yield `share`.
    let file_name = path_get_file_name(path);
    if is_dir || show_extension {
        return file_name.to_string();
    }
    let stem = Path::new(file_name)
        .file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    if stem.is_empty() {
        file_name.to_string()
    } else {
        stem.to_string()
    }
}

/// C# `Path.GetFileName` (last non-empty segment).
pub fn path_get_file_name(path: &str) -> &str {
    path.rsplit(['\\', '/'])
        .find(|s| !s.is_empty())
        .unwrap_or(path)
}

pub fn shell_icon_spec(path: &str) -> String {
    if crate::explorer::is_shell_path(path) {
        return crate::explorer::shell_location_presentation(path)
            .map(|(_, icon)| icon)
            .unwrap_or("fa:Light_FolderOpen")
            .to_string();
    }
    if path.trim().is_empty() || is_directory(path) {
        return "icon:.".into();
    }
    file_icon_spec_without_probe(path)
}

/// Safe fallback while filesystem metadata is unavailable; never probes the path.
pub fn file_icon_spec_without_probe(path: &str) -> String {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    if ext.is_empty() {
        return format!("icon:{path}");
    }
    let ext_dot = format!(".{ext}");
    if is_unique_shell_icon_ext(&ext_dot) {
        format!("icon:{path}")
    } else {
        format!("icon:{ext_dot}")
    }
}

fn is_unique_shell_icon_ext(ext: &str) -> bool {
    matches!(
        ext.to_ascii_lowercase().as_str(),
        ".exe" | ".lnk" | ".ico" | ".dll" | ".url" | ".msc"
    )
}

pub fn is_hidden(path: &str) -> bool {
    if is_drive(path) {
        return false;
    }
    if is_network_path(path) {
        let owned = path.to_string();
        return wait_fs_bool(NETWORK_FS_TIMEOUT, move || hidden_attr(&owned));
    }
    hidden_attr(path)
}

fn hidden_attr(path: &str) -> bool {
    #[cfg(windows)]
    {
        fs::metadata(path)
            .map(|m| hidden_from_metadata(path, &m))
            .unwrap_or(true)
    }
    #[cfg(not(windows))]
    {
        Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with('.'))
    }
}

/// Read the hidden flag from an already captured metadata snapshot.
pub(crate) fn hidden_from_metadata(path: &str, metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        let _ = path;
        metadata.file_attributes() & 2 != 0
    }
    #[cfg(not(windows))]
    {
        let _ = metadata;
        Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with('.'))
    }
}

fn is_drive(path: &str) -> bool {
    let p = Path::new(path);
    p.parent().is_none()
        || p.components().count() <= 1
        || matches!(path, s if s.len() == 3 && s.as_bytes()[1] == b':' && (s.ends_with('\\') || s.ends_with('/')))
}

pub fn drive_display_name(path: &str) -> Option<String> {
    if !is_drive(path) {
        return None;
    }
    #[cfg(windows)]
    {
        volume_label(path).map(|label| {
            let letter = path.trim_end_matches(['\\', '/']);
            if label.is_empty() {
                format!("本地磁盘({letter})")
            } else {
                format!("{label}({letter})")
            }
        })
    }
    #[cfg(not(windows))]
    {
        Some(path.to_string())
    }
}

#[cfg(windows)]
fn volume_label(path: &str) -> Option<String> {
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::GetVolumeInformationW;
    let root = if path.ends_with('\\') || path.ends_with('/') {
        path.to_string()
    } else {
        format!("{path}\\")
    };
    let wide: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();
    let mut buf = [0u16; 32];
    unsafe {
        GetVolumeInformationW(PCWSTR(wide.as_ptr()), Some(&mut buf), None, None, None, None).ok()?;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..end]))
}

fn matches_pattern(name: &str, pattern: &str) -> bool {
    if pattern == "*" || pattern.is_empty() {
        return true;
    }
    let pat = pattern.to_ascii_lowercase();
    let name = name.to_ascii_lowercase();
    if let Some(ext) = pat.strip_prefix("*.") {
        return name.ends_with(&format!(".{ext}")) || name.ends_with(&pat[1..]);
    }
    name.contains(&pat.replace('*', ""))
}

#[cfg(test)]
fn passes_attr(path: &Path, show_hidden: bool) -> bool {
    let metadata = fs::metadata(path).ok();
    passes_attr_from_metadata(path, metadata.as_ref(), show_hidden)
}

fn passes_attr_from_metadata(path: &Path, metadata: Option<&fs::Metadata>, show_hidden: bool) -> bool {
    // C# `FileMenuExtrator.FilterByAttr`: FileFilter starts as System; Hidden is OR'd when
    // `ShowHiddenFiles` is false. GetAttributes throw → exclude (`return false`).
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const HIDDEN: u32 = 0x2;
        const SYSTEM: u32 = 0x4;
        let _ = path;
        let Some(meta) = metadata else {
            return false;
        };
        let mut filter = SYSTEM;
        if !show_hidden {
            filter |= HIDDEN;
        }
        meta.file_attributes() & filter == 0
    }
    #[cfg(not(windows))]
    {
        let _ = metadata;
        if show_hidden {
            return true;
        }
        !path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with('.'))
    }
}

pub(crate) struct FileMenuEntry {
    pub(crate) path: PathBuf,
    pub(crate) metadata: Option<fs::Metadata>,
}

fn sort_entry_snapshots(mut entries: Vec<FileMenuEntry>, sort: SortBy) -> Vec<FileMenuEntry> {
    match sort {
        SortBy::Na => entries,
        SortBy::Name => {
            entries.sort_by(|a, b| {
                a.path.file_name()
                    .unwrap_or_default()
                    .to_ascii_lowercase()
                    .cmp(&b.path.file_name().unwrap_or_default().to_ascii_lowercase())
            });
            entries
        }
        SortBy::CreateTime | SortBy::AccessTime | SortBy::EditTime => {
            entries.sort_by_key(|entry| {
                let time = entry.metadata.as_ref().and_then(|metadata| match sort {
                    SortBy::CreateTime => metadata.created().ok(),
                    SortBy::AccessTime => metadata.accessed().ok(),
                    SortBy::EditTime => metadata.modified().ok(),
                    _ => unreachable!(),
                }).unwrap_or(SystemTime::UNIX_EPOCH);
                std::cmp::Reverse(time)
            });
            entries
        }
    }
}

pub fn list_entries(path: &str, cfg: &FileMenuSettings) -> Vec<String> {
    list_entry_snapshots(path, cfg)
        .into_iter()
        .map(|entry| entry.path.to_string_lossy().into_owned())
        .collect()
}

pub(crate) fn list_entry_snapshots(path: &str, cfg: &FileMenuSettings) -> Vec<FileMenuEntry> {
    list_entry_snapshots_with_metadata(path, cfg, |path| fs::metadata(path))
}

fn list_entry_snapshots_with_metadata(
    path: &str,
    cfg: &FileMenuSettings,
    mut read_metadata: impl FnMut(&Path) -> std::io::Result<fs::Metadata>,
) -> Vec<FileMenuEntry> {
    let Ok(iter) = fs::read_dir(path) else {
        return Vec::new();
    };
    let pattern = cfg.search_pattern();
    let show = cfg.show_type();
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    let mut mixed = Vec::new();
    for entry in iter.flatten() {
        let p = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if !matches_pattern(&name, pattern) {
            continue;
        }
        // Follow directory links just as the former path.is_dir()/metadata calls
        // did, and retain the snapshot for filtering, sorting and node creation.
        let metadata = read_metadata(&p).ok();
        if !passes_attr_from_metadata(&p, metadata.as_ref(), cfg.show_hidden_files) {
            continue;
        }
        let is_dir = metadata.as_ref().is_some_and(fs::Metadata::is_dir);
        match show {
            ShowType::Dir if !is_dir => continue,
            ShowType::File if is_dir => continue,
            _ => {}
        }
        let entry = FileMenuEntry { path: p, metadata };
        if cfg.split_dir_and_file && show == ShowType::DirFile {
            if is_dir {
                dirs.push(entry);
            } else {
                files.push(entry);
            }
        } else {
            mixed.push(entry);
        }
    }
    let sort = cfg.sort_by();
    if cfg.split_dir_and_file && show == ShowType::DirFile {
        let mut out = sort_entry_snapshots(dirs, sort);
        out.extend(sort_entry_snapshots(files, sort));
        out
    } else {
        sort_entry_snapshots(mixed, sort)
    }
}

pub fn list_drives() -> Vec<String> {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::Storage::FileSystem::GetLogicalDrives;
        let mask = GetLogicalDrives();
        (0..26)
            .filter(|i| mask & (1 << i) != 0)
            .map(|i| format!("{}:\\", (b'A' + i as u8) as char))
            .collect()
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

pub fn recent_folder() -> PathBuf {
    PathBuf::from(std::env::var_os("APPDATA").unwrap_or_default()).join("Microsoft/Windows/Recent")
}

/// C# `IHostedService.StartAsync` + deferred `RequestRefresh`.
pub fn start_recent_files_service() {
    let _ = global_recent_files();
}

/// Super Menu lazy children: C# `WhenReadyAsync(5s)` then `GetRecentFiles()`.
pub fn list_recent_files() -> Vec<String> {
    let svc = global_recent_files();
    svc.wait_ready(RECENT_READY_WAIT);
    svc.get_recent_files()
}

fn global_recent_files() -> &'static RecentFilesService {
    GLOBAL_RECENT_FILES.get_or_init(|| {
        let svc = RecentFilesService::new(recent_folder(), crate::clipboard::is_headless());
        svc.start();
        svc
    })
}

struct RecentFilesState {
    cache: Option<(Instant, Vec<String>)>,
    refresh_inflight: bool,
    debounce_scheduled: bool,
}

/// C# `RecentFilesService`.
#[derive(Clone)]
pub struct RecentFilesService {
    folder: PathBuf,
    headless: bool,
    state: Arc<Mutex<RecentFilesState>>,
    ready: Arc<Condvar>,
    refresh_starts: Arc<AtomicUsize>,
}

impl RecentFilesService {
    pub fn new(folder: PathBuf, headless: bool) -> Self {
        Self {
            folder,
            headless,
            state: Arc::new(Mutex::new(RecentFilesState {
                cache: None,
                refresh_inflight: false,
                debounce_scheduled: false,
            })),
            ready: Arc::new(Condvar::new()),
            refresh_starts: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn start(&self) {
        if !self.headless {
            self.start_watcher();
        }
        self.request_refresh();
    }

    /// C# `GetRecentFiles`: empty until the first snapshot; expired snapshots still return.
    pub fn get_recent_files(&self) -> Vec<String> {
        let (paths, expired, missing) = {
            let st = self.state.lock();
            match &st.cache {
                Some((at, paths)) => (
                    paths.clone(),
                    Instant::now().saturating_duration_since(*at) > RECENT_CACHE_TTL,
                    false,
                ),
                None => (Vec::new(), false, true),
            }
        };
        if missing || expired {
            self.request_refresh();
        }
        paths
    }

    /// C# `WhenReadyAsync`.
    pub fn wait_ready(&self, timeout: Duration) {
        let mut st = self.state.lock();
        if st.cache.is_some() {
            return;
        }
        let _ = self.ready.wait_for(&mut st, timeout);
    }

    /// C# `OnRecentFolderChanged` (leading-edge 800ms; extra events ignored until fire).
    pub fn on_recent_folder_changed(&self) {
        {
            let mut st = self.state.lock();
            if st.debounce_scheduled {
                return;
            }
            st.debounce_scheduled = true;
        }
        let this = self.clone();
        std::thread::spawn(move || {
            std::thread::sleep(RECENT_DEBOUNCE);
            this.state.lock().debounce_scheduled = false;
            this.request_refresh();
        });
    }

    pub fn request_refresh(&self) {
        {
            let mut st = self.state.lock();
            if st.refresh_inflight {
                return;
            }
            st.refresh_inflight = true;
        }
        self.refresh_starts.fetch_add(1, Ordering::SeqCst);
        let this = self.clone();
        std::thread::spawn(move || {
            let paths = list_recent_targets(&this.folder, RECENT_MAX_RESULTS, RECENT_MAX_LINKS);
            {
                let mut st = this.state.lock();
                st.cache = Some((Instant::now(), paths));
                st.refresh_inflight = false;
            }
            this.ready.notify_all();
        });
    }

    #[cfg(test)]
    fn seed_cache(&self, paths: Vec<String>, age: Duration) {
        let at = Instant::now().checked_sub(age).unwrap_or_else(Instant::now);
        self.state.lock().cache = Some((at, paths));
    }

    #[cfg(test)]
    fn refresh_count(&self) -> usize {
        self.refresh_starts.load(Ordering::SeqCst)
    }

    fn start_watcher(&self) {
        if !self.folder.is_dir() {
            return;
        }
        #[cfg(windows)]
        {
            let folder = self.folder.clone();
            let this = self.clone();
            std::thread::Builder::new()
                .name("cliphost-recent-files".into())
                .spawn(move || watch_recent_folder(folder, this))
                .ok();
        }
    }
}

#[cfg(windows)]
fn watch_recent_folder(folder: PathBuf, svc: RecentFilesService) {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::WAIT_OBJECT_0;
    use windows::Win32::Storage::FileSystem::{
        FindFirstChangeNotificationW, FindNextChangeNotification, FILE_NOTIFY_CHANGE_CREATION,
        FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE,
    };
    use windows::Win32::System::Threading::WaitForSingleObject;
    let wide: Vec<u16> = folder
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let Ok(handle) = FindFirstChangeNotificationW(
            PCWSTR(wide.as_ptr()),
            false,
            FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_LAST_WRITE | FILE_NOTIFY_CHANGE_CREATION,
        ) else {
            return;
        };
        loop {
            if WaitForSingleObject(handle, 500) == WAIT_OBJECT_0 {
                let _ = FindNextChangeNotification(handle);
                svc.on_recent_folder_changed();
            }
        }
    }
}

/// C# `RecentFilesService.EnumerateRecentTargets`: files only, last-write desc, skip unresolved links.
pub fn list_recent_targets(folder: &Path, max_results: usize, max_links: usize) -> Vec<String> {
    if max_results == 0 || !folder.is_dir() {
        return Vec::new();
    }
    let Ok(iter) = fs::read_dir(folder) else {
        return Vec::new();
    };
    let mut links: Vec<(SystemTime, PathBuf)> = iter
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.len() < 100)
        })
        .map(|p| {
            (
                fs::metadata(&p)
                    .and_then(|m| m.modified())
                    .unwrap_or(SystemTime::UNIX_EPOCH),
                p,
            )
        })
        .collect();
    links.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for (_, link) in links.into_iter().take(max_links) {
        if out.len() >= max_results {
            break;
        }
        let Some(target) = resolve_shortcut(&link) else {
            continue;
        };
        if target.trim().is_empty() {
            continue;
        }
        if !Path::new(&target).exists() {
            continue;
        }
        if !seen.insert(target.to_ascii_lowercase()) {
            continue;
        }
        out.push(target);
    }
    out
}

pub fn resolve_shortcut(path: &Path) -> Option<String> {
    if path
        .extension()
        .and_then(|e| e.to_str())
        .is_none_or(|e| !e.eq_ignore_ascii_case("lnk"))
    {
        return None;
    }
    #[cfg(windows)]
    unsafe {
        use windows::core::{Interface, PCWSTR};
        use windows::Win32::System::Com::{
            CoCreateInstance, CLSCTX_INPROC_SERVER, IPersistFile,
        };
        use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
        // Keep COM alive until both interfaces are released, including early
        // returns for missing/broken links. Reused workers retain no STA count.
        let _apartment = crate::explorer::ShellApartment::initialize().ok()?;
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).ok()?;
        let persist: IPersistFile = link.cast().ok()?;
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        persist
            .Load(PCWSTR(wide.as_ptr()), windows::Win32::System::Com::STGM(0))
            .ok()?;
        let mut buf = [0u16; 520];
        link.GetPath(&mut buf, std::ptr::null_mut(), 0).ok()?;
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        let target = String::from_utf16_lossy(&buf[..end]);
        (!target.is_empty()).then_some(target)
    }
    #[cfg(not(windows))]
    {
        None
    }
}

pub fn this_pc_shell_url() -> &'static str {
    // C# `MyComputeFileMenuWrapper.Execute`.
    "::{20d04fe0-3aea-1069-a2d8-08002b30309d}"
}

/// C# `FileUtilsClipHost.LocateFileInExplorer` `ProcessStartInfo.Arguments`.
/// Folder → `"full"`; file → `/select,"full"`; missing / empty → `None` (no Explorer).
pub fn explorer_locate_arguments(path: &str) -> Option<String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return None;
    }
    let full = std::path::absolute(trimmed).ok()?;
    let full_s = full.to_string_lossy().into_owned();
    if full.is_dir() {
        Some(format!("\"{full_s}\""))
    } else if full.is_file() {
        Some(format!("/select,\"{full_s}\""))
    } else {
        None
    }
}

/// C# `FileMenuItemWrapper.LocateFile`: `LocateFileInSaveWindowExceptHandle` then Explorer.
/// CLIPHOST_NO_AUTOIT ignores the menu HWND and enumerates visible file dialogs.
pub fn locate_file(path: &str) -> Result<(), String> {
    if crate::file_dialog_locate::try_locate_result(path)? {
        return Ok(());
    }
    locate_in_explorer(path)
}

pub fn locate_in_explorer(path: &str) -> Result<(), String> {
    if crate::clipboard::is_headless() {
        return Ok(());
    }
    let Some(arguments) = explorer_locate_arguments(path) else {
        return Ok(());
    };
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        std::process::Command::new("explorer.exe")
            .raw_arg(arguments)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    #[cfg(not(windows))]
    {
        let _ = arguments;
        open::that(path).map_err(|e| e.to_string())
    }
}

#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extra_settings::FileMenuSettings;

    #[cfg(windows)]
    fn shortcut_fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        std::thread::spawn(|| unsafe {
            use windows::core::{HSTRING, Interface, PCWSTR};
            use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER, IPersistFile};
            use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
            let dir = tempfile::tempdir().unwrap();
            let target = dir.path().join("目标 中文.txt");
            let shortcut = dir.path().join("入口.lnk");
            fs::write(&target, "fixture").unwrap();
            let _apartment = crate::explorer::ShellApartment::initialize().unwrap();
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).unwrap();
            link.SetPath(&HSTRING::from(target.to_string_lossy().as_ref())).unwrap();
            let persist: IPersistFile = link.cast().unwrap();
            let wide: Vec<u16> = shortcut.as_os_str().encode_wide().chain([0]).collect();
            persist.Save(PCWSTR(wide.as_ptr()), true).unwrap();
            (dir, target, shortcut)
        }).join().unwrap()
    }

    #[cfg(windows)]
    #[test]
    fn shortcut_failure_does_not_leave_the_worker_in_sta() {
        std::thread::spawn(|| unsafe {
            use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
            let dir = tempfile::tempdir().unwrap();
            let broken = dir.path().join("broken.lnk");
            fs::write(&broken, "not a shell link").unwrap();
            assert!(resolve_shortcut(&broken).is_none());
            assert!(resolve_shortcut(&dir.path().join("missing.lnk")).is_none());
            let result = CoInitializeEx(None, COINIT_MULTITHREADED);
            if result.is_ok() { CoUninitialize(); }
            result.ok().expect("shortcut failure leaked STA initialization");
        }).join().unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn shortcut_roundtrip_balances_nested_sta_initialization() {
        let (dir, target, shortcut) = shortcut_fixture();
        std::thread::spawn(move || unsafe {
            use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
            let outer = crate::explorer::ShellApartment::initialize().unwrap();
            for _ in 0..16 {
                let resolved = resolve_shortcut(&shortcut).expect("valid Unicode shortcut");
                assert_eq!(fs::canonicalize(resolved).unwrap(), fs::canonicalize(&target).unwrap());
                assert!(resolve_shortcut(&dir.path().join("missing.lnk")).is_none());
            }
            drop(outer);
            let result = CoInitializeEx(None, COINIT_MULTITHREADED);
            if result.is_ok() { CoUninitialize(); }
            result.ok().expect("shortcut resolution leaked nested STA references");
        }).join().unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn shortcut_roundtrip_preserves_existing_mta() {
        let (_dir, target, shortcut) = shortcut_fixture();
        std::thread::spawn(move || unsafe {
            use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
            use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_MULTITHREADED};
            CoInitializeEx(None, COINIT_MULTITHREADED).ok().unwrap();
            let resolved = resolve_shortcut(&shortcut).expect("shortcut resolution in caller's MTA");
            assert_eq!(fs::canonicalize(resolved).unwrap(), fs::canonicalize(&target).unwrap());
            assert_eq!(CoInitializeEx(None, COINIT_APARTMENTTHREADED), RPC_E_CHANGED_MODE);
            CoUninitialize();
            let result = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            if result.is_ok() { CoUninitialize(); }
            result.ok().expect("shortcut resolution changed caller's COM lifetime");
        }).join().unwrap();
    }

    #[test]
    fn directory_snapshot_reads_metadata_once_and_preserves_sorting() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("a-dir.match")).unwrap();
        for (name, seconds) in [("a.match", 100), ("b.match", 200)] {
            let path = dir.path().join(name);
            fs::write(&path, name).unwrap();
            let time = SystemTime::UNIX_EPOCH + Duration::from_secs(seconds);
            fs::OpenOptions::new().write(true).open(path).unwrap()
                .set_times(fs::FileTimes::new().set_accessed(time).set_modified(time)).unwrap();
        }
        fs::write(dir.path().join("excluded.bin"), "excluded").unwrap();
        fs::write(dir.path().join("failed.match"), "failed").unwrap();
        let cfg = FileMenuSettings {
            file_search_parttern: "*.match".into(),
            file_sort_type: "editTime".into(),
            ..Default::default()
        };
        let mut reads = std::collections::HashMap::new();
        let entries = list_entry_snapshots_with_metadata(dir.path().to_str().unwrap(), &cfg, |path| {
            *reads.entry(path.to_path_buf()).or_insert(0usize) += 1;
            if path.file_name().unwrap() == "failed.match" {
                return Err(std::io::Error::from(std::io::ErrorKind::NotFound));
            }
            fs::metadata(path)
        });
        assert_eq!(reads.len(), 4, "pattern-rejected paths must not be probed");
        assert!(reads.values().all(|&count| count == 1), "{reads:?}");
        let names: Vec<_> = entries.iter()
            .map(|entry| entry.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(&names[..3], &["a-dir.match", "b.match", "a.match"]);
        #[cfg(windows)]
        assert_eq!(names.len(), 3, "Windows attribute failures are excluded");
        #[cfg(not(windows))]
        assert_eq!(names[3], "failed.match", "Unix metadata failures remain non-directories");

        let metadata = entries.iter().find(|entry| entry.path.file_name().unwrap() == "a.match")
            .unwrap().metadata.as_ref().unwrap().clone();
        let tied_entries = || ["b.match", "a.match"].into_iter().map(|name| FileMenuEntry {
            path: dir.path().join(name), metadata: Some(metadata.clone()),
        }).collect::<Vec<_>>();
        // All date fields are equal because both entries share the same snapshot.
        // Stable sorting must retain enumeration order without reading either path.
        for sort in [SortBy::Na, SortBy::CreateTime, SortBy::AccessTime, SortBy::EditTime] {
            let sorted = sort_entry_snapshots(tied_entries(), sort);
            assert_eq!(sorted[0].path.file_name().unwrap(), "b.match");
            assert_eq!(sorted[1].path.file_name().unwrap(), "a.match");
        }
        let sorted = sort_entry_snapshots(tied_entries(), SortBy::Name);
        assert_eq!(sorted[0].path.file_name().unwrap(), "a.match");
        assert_eq!(file_name_for_type("a-dir.match", true, false), "a-dir.match");
        assert_eq!(file_name_for_type("a.match", false, false), "a");
    }

    #[test]
    fn lists_directory_children_split_dirs_first() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("b.txt"), "b").unwrap();
        fs::create_dir(dir.path().join("a-dir")).unwrap();
        fs::write(dir.path().join("a.txt"), "a").unwrap();
        let listed = list_entries(dir.path().to_str().unwrap(), &FileMenuSettings::default());
        let names: Vec<String> = listed
            .iter()
            .map(|p| Path::new(p).file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names[0], "a-dir");
        assert!(names.contains(&"a.txt".into()));
        assert!(names.contains(&"b.txt".into()));
    }

    #[test]
    fn explorer_locate_arguments_match_csharp_file_utils_cliphost() {
        let dir = tempfile::tempdir().unwrap();
        let folder = explorer_locate_arguments(dir.path().to_str().unwrap()).unwrap();
        assert!(folder.starts_with('"') && folder.ends_with('"'), "{folder}");
        assert!(!folder.contains("/select"), "{folder}");

        let file = dir.path().join("notes.txt");
        fs::write(&file, "x").unwrap();
        let select = explorer_locate_arguments(file.to_str().unwrap()).unwrap();
        assert!(select.starts_with("/select,\""), "{select}");
        assert!(select.ends_with('"'), "{select}");

        assert!(explorer_locate_arguments("").is_none());
        assert!(explorer_locate_arguments("   ").is_none());
        assert!(explorer_locate_arguments(dir.path().join("missing.bin").to_str().unwrap()).is_none());
        assert!(
            locate_file(dir.path().to_str().unwrap()).is_ok(),
            "headless locate_file must not spawn Explorer"
        );
    }

    #[test]
    fn hidden_filter_and_icon_specs() {
        assert_eq!(shell_icon_spec("C:\\Windows"), "icon:.");
        assert_eq!(shell_icon_spec("readme.md"), "icon:.md");
        assert!(shell_icon_spec("C:\\app.exe").starts_with("icon:"));
    }

    #[test]
    fn recent_targets_skip_non_links_like_csharp() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("notes.txt"), "plain").unwrap();
        fs::write(dir.path().join("broken.lnk"), "not-a-shell-link").unwrap();
        fs::create_dir(dir.path().join("a-folder")).unwrap();
        let listed = list_recent_targets(dir.path(), 50, 120);
        assert!(listed.is_empty(), "{listed:?}");
        assert!(list_recent_targets(dir.path(), 0, 120).is_empty());
        assert!(list_recent_targets(&dir.path().join("missing"), 50, 120).is_empty());
    }

    #[test]
    fn recent_files_service_matches_csharp_get_and_when_ready() {
        let dir = tempfile::tempdir().unwrap();
        let svc = RecentFilesService::new(dir.path().to_path_buf(), true);
        assert!(svc.get_recent_files().is_empty());
        assert_eq!(svc.refresh_count(), 1);
        svc.wait_ready(RECENT_READY_WAIT);
        assert!(svc.get_recent_files().is_empty());

        svc.seed_cache(vec![r"C:\stale.txt".into()], RECENT_CACHE_TTL + Duration::from_secs(5));
        assert_eq!(svc.get_recent_files(), vec![r"C:\stale.txt"]);
        assert!(svc.refresh_count() >= 2);
    }

    #[test]
    fn recent_files_debounce_ignores_events_until_timer_fires() {
        let dir = tempfile::tempdir().unwrap();
        let svc = RecentFilesService::new(dir.path().to_path_buf(), true);
        svc.seed_cache(vec![r"C:\keep.txt".into()], Duration::from_secs(1));
        svc.on_recent_folder_changed();
        svc.on_recent_folder_changed();
        svc.on_recent_folder_changed();
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(svc.refresh_count(), 0);
        std::thread::sleep(RECENT_DEBOUNCE + Duration::from_millis(120));
        assert_eq!(svc.refresh_count(), 1);
    }

    #[test]
    fn list_entries_filter_by_attr_matches_csharp() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("visible.txt"), "v").unwrap();
        fs::write(dir.path().join("ghost.txt"), "h").unwrap();
        fs::write(dir.path().join("sys.txt"), "s").unwrap();
        let ghost = dir.path().join("ghost.txt");
        let sys = dir.path().join("sys.txt");
        #[cfg(windows)]
        unsafe {
            use std::os::windows::ffi::OsStrExt;
            use windows::core::PCWSTR;
            use windows::Win32::Storage::FileSystem::{
                SetFileAttributesW, FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_SYSTEM,
            };
            let hide: Vec<u16> = ghost.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
            let system: Vec<u16> = sys.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
            SetFileAttributesW(PCWSTR(hide.as_ptr()), FILE_ATTRIBUTE_HIDDEN).unwrap();
            SetFileAttributesW(PCWSTR(system.as_ptr()), FILE_ATTRIBUTE_SYSTEM).unwrap();
        }
        #[cfg(not(windows))]
        {
            let _ = (&ghost, &sys);
        }
        let names = |cfg: FileMenuSettings| {
            list_entries(dir.path().to_str().unwrap(), &cfg)
                .into_iter()
                .map(|p| Path::new(&p).file_name().unwrap().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        let hidden_off = names(FileMenuSettings::default());
        assert!(hidden_off.contains(&"visible.txt".into()), "{hidden_off:?}");
        #[cfg(windows)]
        {
            assert!(!hidden_off.contains(&"ghost.txt".into()), "{hidden_off:?}");
            assert!(!hidden_off.contains(&"sys.txt".into()), "{hidden_off:?}");
        }
        let mut shown = FileMenuSettings::default();
        shown.show_hidden_files = true;
        let hidden_on = names(shown);
        assert!(hidden_on.contains(&"visible.txt".into()), "{hidden_on:?}");
        #[cfg(windows)]
        {
            assert!(hidden_on.contains(&"ghost.txt".into()), "{hidden_on:?}");
            assert!(
                !hidden_on.contains(&"sys.txt".into()),
                "C# FileFilter always includes System: {hidden_on:?}"
            );
        }
        assert!(!passes_attr(Path::new(r"C:\cliphost-missing-attr-xyz.bin"), true));
        assert!(!passes_attr(Path::new(r"C:\cliphost-missing-attr-xyz.bin"), false));
    }

    #[test]
    fn menu_path_status_and_timeout_helpers_handle_real_paths() {
        assert!(!is_network_path(""));
        assert!(!is_network_path(r"C:\missing.txt"));
        assert!(is_network_path(r"\\server\share\file.txt"));
        assert!(is_network_path(r"  \\server\share"));
        assert_eq!(menu_file_path_status(""), MenuPathStatus::Missing);
        assert_ne!(menu_file_path_status(r"C:\cliphost-missing-local-file-xyz.bin"), MenuPathStatus::Present);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("here.txt");
        fs::write(&file, "ok").unwrap();
        assert_ne!(menu_file_path_status(file.to_str().unwrap()), MenuPathStatus::Missing);
        // These assertions check filesystem results, not scheduler speed. The
        // short-deadline behavior is forced deterministically in the next test.
        let test_budget = Duration::from_secs(5);
        assert!(path_exists_with_timeout(file.to_str().unwrap(), test_budget));
        assert!(file_exists_with_timeout(file.to_str().unwrap(), test_budget));
        assert!(!directory_exists_with_timeout(file.to_str().unwrap(), test_budget));
        assert!(!context_menu_is_directory(file.to_str().unwrap()));
        assert!(!context_menu_is_file(r"C:\cliphost-missing-local-file-xyz.bin"));
        assert!(!context_menu_is_file(""));
        assert!(file_menu_item_enabled(file.to_str().unwrap()));
        assert!(file_menu_item_enabled(dir.path().to_str().unwrap()));
        assert!(!file_menu_item_enabled(""));
        assert!(!file_menu_item_enabled(r"C:\cliphost-missing-local-file-xyz.bin"));
        assert!(
            !file_menu_item_enabled(r"\\cliphost-no-such-host\share"),
            "C# IsPathExist 50ms: missing UNC is shown but disabled"
        );
        assert_ne!(menu_file_path_status(r"\\cliphost-no-such-host\share"), MenuPathStatus::Present);
    }

    #[test]
    fn late_fs_probe_remains_unknown_instead_of_reporting_a_missing_path() {
        let (release, blocked) = std::sync::mpsc::channel();
        let (finished, completed) = std::sync::mpsc::channel();
        let caller = std::thread::current().id();
        let result = wait_fs_probe(Duration::ZERO, move || {
            assert_ne!(caller, std::thread::current().id());
            blocked.recv().unwrap();
            finished.send(()).unwrap();
            true
        });
        assert_eq!(MenuPathStatus::from_probe(result), MenuPathStatus::Unknown);
        release.send(()).unwrap();
        completed.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(MenuPathStatus::from_probe(Some(false)), MenuPathStatus::Missing);
        assert_eq!(MenuPathStatus::from_probe(Some(true)), MenuPathStatus::Present);
    }

    #[test]
    fn this_pc_shell_url_matches_csharp_my_computer_execute() {
        assert_eq!(
            this_pc_shell_url(),
            "::{20d04fe0-3aea-1069-a2d8-08002b30309d}"
        );
    }
}
