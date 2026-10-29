//! Read-only data for the independent native file browser.
//!
//! Home only resolves known-folder addresses and the logical-drive mask/type.
//! Volume labels/capacity are a separate blocking request, and a directory
//! request enumerates exactly the location the user entered. Call these on
//! bounded workers; none of these functions creates a menu session or opens
//! file contents. `DirEntry::metadata` does not follow links and uses the
//! enumeration snapshot on Windows rather than probing every child path.

use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::i18n::tr;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserPlace {
    pub name: String,
    pub path: String,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserCapacity {
    pub free_bytes: u64,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserDrive {
    pub name: String,
    pub path: String,
    /// fixed / removable / network / cdrom / ramdisk / unknown.
    pub kind: String,
    pub system: bool,
    pub capacity: Option<BrowserCapacity>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size_bytes: Option<u64>,
    pub modified_unix_ms: Option<i64>,
    /// folder / file / image / video / audio / archive / code / pdf / link.
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserHome {
    pub places: Vec<BrowserPlace>,
    pub drives: Vec<BrowserDrive>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserDirectory {
    pub path: String,
    pub entries: Vec<BrowserEntry>,
}

/// Pure lexical validation; never resolves links, canonicalizes, or checks a
/// directory. Relative paths, Shell/URI addresses, device namespaces and ADS
/// names are intentionally outside the first browser's path contract.
pub fn browser_normalize_path(raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(tr("目录路径不能为空。", "The folder path cannot be empty.").into());
    }
    if raw.encode_utf16().count() > 32_760 || raw.chars().any(|character| character.is_control()) {
        return Err(tr(
            "目录路径包含无效字符或过长。",
            "The folder path contains invalid characters or is too long.",
        )
        .into());
    }
    let path = raw.replace('/', "\\");
    if path.starts_with(r"\\?\")
        || path.starts_with(r"\\.\")
        || path.starts_with(r"\??\")
        || path.starts_with(r"\\??\")
    {
        return Err(tr(
            "文件窗口不支持系统设备路径。",
            "The file browser does not support device paths.",
        )
        .into());
    }
    let bytes = path.as_bytes();
    if bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'\\' {
        let parts = normalized_windows_parts(&path[3..])?;
        let root = format!("{}:\\", (bytes[0] as char).to_ascii_uppercase());
        return Ok(format!("{root}{}", parts.join("\\")));
    }
    if let Some(unc) = path.strip_prefix(r"\\") {
        let mut parts = unc.split('\\');
        let server = parts.next().unwrap_or_default();
        let share = parts.next().unwrap_or_default();
        if !valid_windows_part(server, false) || !valid_windows_part(share, false) {
            return Err(tr(
                "UNC 路径需要服务器和共享名。",
                "A UNC path needs a server and share name.",
            )
            .into());
        }
        let remainder = parts.collect::<Vec<_>>().join("\\");
        let tail = normalized_windows_parts(&remainder)?;
        let root = format!(r"\\{server}\{share}");
        return Ok(if tail.is_empty() {
            root
        } else {
            format!("{root}\\{}", tail.join("\\"))
        });
    }
    #[cfg(not(windows))]
    if raw.starts_with('/') && !raw.starts_with("//") {
        let mut parts = Vec::new();
        for part in raw.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    parts.pop();
                }
                part => parts.push(part),
            }
        }
        return Ok(format!("/{}", parts.join("/")));
    }
    Err(tr(
        "请输入绝对目录路径，例如 C:\\Users 或 \\\\server\\share。",
        "Enter an absolute folder path, such as C:\\Users or \\\\server\\share.",
    )
    .into())
}

fn normalized_windows_parts(path: &str) -> Result<Vec<&str>, String> {
    let mut parts = Vec::new();
    for part in path.split('\\') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part if valid_windows_part(part, true) => parts.push(part),
            _ => {
                return Err(tr(
                    "目录路径包含不支持的文件名。",
                    "The folder path contains an unsupported filename.",
                )
                .into())
            }
        }
    }
    Ok(parts)
}

fn valid_windows_part(part: &str, reject_device_name: bool) -> bool {
    if part.is_empty()
        || matches!(part, "." | "..")
        || part.ends_with(['.', ' '])
        || part
            .chars()
            .any(|character| matches!(character, ':' | '<' | '>' | '"' | '|' | '?' | '*'))
    {
        return false;
    }
    if !reject_device_name {
        return true;
    }
    let base = part
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    !matches!(
        base.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) && !(base.len() == 4
        && (base.starts_with("COM") || base.starts_with("LPT"))
        && matches!(base.as_bytes()[3], b'1'..=b'9'))
}

/// No directory/volume probes: redirected places are only addresses, and drive
/// labels and capacity remain unknown until `browser_drive_details` is called.
pub fn browser_home() -> Result<BrowserHome, String> {
    #[cfg(windows)]
    {
        let mask = unsafe { native::GetLogicalDrives() };
        if mask == 0 {
            return Err(format!(
                "{} {}",
                tr("无法读取磁盘列表：", "Cannot read the drive list:"),
                std::io::Error::last_os_error()
            ));
        }
        let system = std::env::var("SystemDrive").unwrap_or_default();
        let drives = drives_from_mask(mask, &system, |path| {
            let wide = wide_path(path);
            drive_kind(unsafe { native::GetDriveTypeW(wide.as_ptr()) }).to_string()
        });
        Ok(BrowserHome {
            places: known_places(),
            drives,
        })
    }
    #[cfg(not(windows))]
    {
        Ok(BrowserHome {
            places: Vec::new(),
            drives: Vec::new(),
        })
    }
}

fn drives_from_mask(
    mask: u32,
    system: &str,
    mut kind: impl FnMut(&str) -> String,
) -> Vec<BrowserDrive> {
    (0..26)
        .filter(|index| mask & (1 << index) != 0)
        .map(|index| {
            let path = format!("{}:\\", (b'A' + index as u8) as char);
            BrowserDrive {
                name: path.trim_end_matches('\\').to_string(),
                system: path
                    .trim_end_matches('\\')
                    .eq_ignore_ascii_case(system.trim_end_matches(['\\', '/'])),
                kind: kind(&path),
                path,
                capacity: None,
            }
        })
        .collect()
}

fn drive_kind(kind: u32) -> &'static str {
    match kind {
        2 => "removable",
        3 => "fixed",
        4 => "network",
        5 => "cdrom",
        6 => "ramdisk",
        _ => "unknown",
    }
}

/// Blocking volume metadata only. Call separately per requested drive so an
/// unavailable device/provider cannot delay the other home cards. Automatic
/// callers should limit themselves to fixed drives; other kinds are on demand.
/// A virtual/cloud driver may report itself as fixed, so this is not a promise
/// that querying such a provider never wakes it. No content is read/downloaded.
pub fn browser_drive_details(drive: &BrowserDrive) -> Result<BrowserDrive, String> {
    let path = browser_normalize_path(&drive.path)?;
    if path.len() != 3 || path.as_bytes()[1] != b':' || !path.ends_with('\\') {
        return Err(tr(
            "磁盘详情只接受盘符根目录。",
            "Drive details require a drive root.",
        )
        .into());
    }
    #[cfg(windows)]
    {
        let _error_mode = native::ErrorMode::suppress_device_dialogs();
        let wide = wide_path(&path);
        let mut label = [0u16; 261];
        let label_ok = unsafe {
            native::GetVolumeInformationW(
                wide.as_ptr(),
                label.as_mut_ptr(),
                label.len() as u32,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
            )
        } != 0;
        let mut free = 0;
        let mut total = 0;
        let capacity_ok = unsafe {
            native::GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, &mut total, std::ptr::null_mut())
        } != 0;
        if !label_ok && !capacity_ok {
            return Err(format!(
                "{} {}",
                tr("无法读取磁盘详情：", "Cannot read drive details:"),
                std::io::Error::last_os_error()
            ));
        }
        let mut result = drive.clone();
        result.path = path;
        let label = label_ok.then(|| {
            String::from_utf16_lossy(
                &label[..label
                    .iter()
                    .position(|value| *value == 0)
                    .unwrap_or(label.len())],
            )
        });
        result.name = drive_name(&result.path, label.as_deref());
        result.capacity = capacity_ok.then(|| valid_capacity(free, total)).flatten();
        Ok(result)
    }
    #[cfg(not(windows))]
    {
        let _ = drive;
        Err(tr(
            "此平台暂不支持磁盘容量查询。",
            "Drive capacity is unavailable on this platform.",
        )
        .into())
    }
}

fn drive_name(path: &str, label: Option<&str>) -> String {
    let letter = path.trim_end_matches(['\\', '/']);
    let Some(label) = label else {
        return letter.to_string();
    };
    if label.is_empty() {
        format!("{} ({letter})", tr("本地磁盘", "Local Disk"))
    } else {
        format!("{label} ({letter})")
    }
}

fn valid_capacity(free: u64, total: u64) -> Option<BrowserCapacity> {
    (total != 0 && free <= total).then_some(BrowserCapacity {
        free_bytes: free,
        total_bytes: total,
    })
}

/// Enumerate only the entered directory, with one non-following metadata
/// snapshot per child. A vanished/unreadable child's name stays visible with
/// unknown metadata. Failure during enumeration is reported, not an empty or
/// silently incomplete folder. Folder contents/sizes are never prefetched.
pub fn browser_read_directory(path: &str) -> Result<BrowserDirectory, String> {
    read_directory_with(path, fs::DirEntry::metadata)
}

fn read_directory_with(
    path: &str,
    mut metadata: impl FnMut(&fs::DirEntry) -> std::io::Result<fs::Metadata>,
) -> Result<BrowserDirectory, String> {
    let path = browser_normalize_path(path)?;
    let root = fs::symlink_metadata(&path).map_err(|error| directory_error(&error))?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if unavailable_placeholder(root.file_attributes()) {
            return Err(tr(
                "目录处于离线或云端占位状态，尚未读取内容。",
                "This folder is offline or a cloud placeholder; its contents were not read.",
            )
            .into());
        }
    }
    if !metadata_is_directory(&root) {
        return Err(tr("该路径不是目录。", "This path is not a folder.").into());
    }
    let iterator = fs::read_dir(&path).map_err(|error| directory_error(&error))?;
    let mut entries = Vec::new();
    for entry in iterator {
        let entry = entry.map_err(|error| directory_error(&error))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let snapshot = metadata(&entry).ok();
        let is_dir = snapshot.as_ref().is_some_and(metadata_is_directory);
        let link = snapshot.as_ref().is_some_and(metadata_is_link);
        let size_bytes = snapshot
            .as_ref()
            .filter(|value| !is_dir && !link && value.is_file())
            .map(fs::Metadata::len);
        let modified_unix_ms = snapshot
            .as_ref()
            .and_then(|value| value.modified().ok())
            .and_then(unix_millis);
        let kind = entry_kind(&name, is_dir, link).to_string();
        let row = BrowserEntry {
            name,
            path: entry.path().to_string_lossy().into_owned(),
            is_dir,
            size_bytes,
            modified_unix_ms,
            kind,
        };
        entries.push((row.name.to_lowercase(), row));
    }
    entries.sort_by(|(left_name, left), (right_name, right)| {
        right
            .is_dir
            .cmp(&left.is_dir)
            .then_with(|| left_name.cmp(right_name))
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(BrowserDirectory {
        path,
        entries: entries.into_iter().map(|(_, entry)| entry).collect(),
    })
}

fn directory_error(error: &std::io::Error) -> String {
    format!(
        "{} {error}",
        tr("无法读取目录：", "Cannot read this folder:")
    )
}

fn metadata_is_directory(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // Includes directory junctions/links, without querying their target.
        metadata.file_attributes() & 0x10 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.is_dir()
    }
}

fn metadata_is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // Keep non-placeholder reparse entries visibly distinct without
        // opening a handle to discover or inspect their target. This includes
        // junctions; offline/recall cloud placeholders keep their file kind.
        metadata.file_type().is_symlink()
            || (metadata.file_attributes() & 0x400 != 0
                && !unavailable_placeholder(metadata.file_attributes()))
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn unavailable_placeholder(attributes: u32) -> bool {
    // OFFLINE / RECALL_ON_OPEN / RECALL_ON_DATA_ACCESS.
    attributes & (0x1000 | 0x40000 | 0x400000) != 0
}

fn unix_millis(time: SystemTime) -> Option<i64> {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_millis()).ok(),
        Err(error) => i64::try_from(error.duration().as_millis())
            .ok()?
            .checked_neg(),
    }
}

fn entry_kind(name: &str, directory: bool, link: bool) -> &'static str {
    if link {
        return "link";
    }
    if directory {
        return "folder";
    }
    let extension = Path::new(name)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "avif" | "tif" | "tiff" | "heic"
        | "ico" | "svg" => "image",
        "mp4" | "mkv" | "mov" | "avi" | "webm" | "wmv" | "m4v" => "video",
        "mp3" | "wav" | "flac" | "aac" | "ogg" | "m4a" | "wma" => "audio",
        "zip" | "7z" | "rar" | "tar" | "gz" | "bz2" | "xz" => "archive",
        "rs" | "cs" | "c" | "h" | "cpp" | "hpp" | "js" | "jsx" | "ts" | "tsx" | "py" | "ps1"
        | "sh" | "json" | "yaml" | "yml" | "toml" | "xml" | "html" | "css" | "sql" | "md"
        | "txt" | "log" => "code",
        "pdf" => "pdf",
        "lnk" | "url" => "link",
        _ => "file",
    }
}

#[cfg(windows)]
fn wide_path(path: &str) -> Vec<u16> {
    path.encode_utf16().chain(Some(0)).collect()
}

#[cfg(windows)]
fn known_places() -> Vec<BrowserPlace> {
    use std::os::windows::ffi::OsStringExt;
    use windows::Win32::{
        Foundation::HANDLE,
        System::Com::CoTaskMemFree,
        UI::Shell::{
            FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads, FOLDERID_Pictures,
            SHGetKnownFolderPath, KF_FLAG_DONT_VERIFY,
        },
    };
    [
        ("desktop", tr("桌面", "Desktop"), FOLDERID_Desktop),
        ("downloads", tr("下载", "Downloads"), FOLDERID_Downloads),
        ("documents", tr("文档", "Documents"), FOLDERID_Documents),
        ("pictures", tr("图片", "Pictures"), FOLDERID_Pictures),
    ]
    .into_iter()
    .filter_map(|(kind, name, id)| unsafe {
        let path = SHGetKnownFolderPath(&id, KF_FLAG_DONT_VERIFY, HANDLE::default()).ok()?;
        let value = PathBuf::from(std::ffi::OsString::from_wide(path.as_wide()));
        CoTaskMemFree(Some(path.0.cast()));
        let path = browser_normalize_path(&value.to_string_lossy()).ok()?;
        Some(BrowserPlace {
            name: name.to_string(),
            path,
            kind: kind.to_string(),
        })
    })
    .collect()
}

#[cfg(windows)]
mod native {
    #[link(name = "kernel32")]
    extern "system" {
        pub(super) fn GetLogicalDrives() -> u32;
        pub(super) fn GetDriveTypeW(root: *const u16) -> u32;
        pub(super) fn GetVolumeInformationW(
            root: *const u16,
            label: *mut u16,
            label_len: u32,
            serial: *mut u32,
            max_component_len: *mut u32,
            flags: *mut u32,
            file_system: *mut u16,
            file_system_len: u32,
        ) -> i32;
        pub(super) fn GetDiskFreeSpaceExW(
            root: *const u16,
            available: *mut u64,
            total: *mut u64,
            free: *mut u64,
        ) -> i32;
        fn GetThreadErrorMode() -> u32;
        fn SetThreadErrorMode(mode: u32, old: *mut u32) -> i32;
    }

    pub(super) struct ErrorMode(Option<u32>);
    impl ErrorMode {
        pub(super) fn suppress_device_dialogs() -> Self {
            let mut old = 0;
            let mode = unsafe { GetThreadErrorMode() } | 1;
            Self((unsafe { SetThreadErrorMode(mode, &mut old) } != 0).then_some(old))
        }
    }
    impl Drop for ErrorMode {
        fn drop(&mut self) {
            if let Some(old) = self.0 {
                unsafe {
                    SetThreadErrorMode(old, std::ptr::null_mut());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashMap, time::Duration};

    fn fixture() -> tempfile::TempDir {
        let temporary = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
        let temporary = browser_normalize_path(temporary.to_str().unwrap()).unwrap();
        fs::create_dir_all(&temporary).unwrap();
        tempfile::Builder::new()
            .prefix("file-browser-core-")
            .tempdir_in(temporary)
            .unwrap()
    }

    #[test]
    fn browser_path_validation_is_lexical_and_rejects_devices_relative_shell_and_ads() {
        assert_eq!(
            browser_normalize_path(r" c:/missing/./folder/../file ").unwrap(),
            r"C:\missing\file"
        );
        assert_eq!(
            browser_normalize_path(r"\\not-a-real-server\share\a\..\..\folder").unwrap(),
            r"\\not-a-real-server\share\folder"
        );
        assert_eq!(browser_normalize_path(r"C:\..\..").unwrap(), "C:\\");
        for path in [
            "",
            "relative",
            "C:relative",
            r"\rooted",
            r"\\server",
            r"\\?\C:\Windows",
            r"\\.\pipe\x",
            r"\??\C:\Windows",
            r"shell:::{GUID}",
            "https://example.com",
            "C:\\a\0b",
            r"C:\file:stream",
            r"C:\NUL.txt",
            r"C:\COM1",
            r"C:\trailing.",
            r"C:\wild*",
        ] {
            assert!(browser_normalize_path(path).is_err(), "accepted {path:?}");
        }
        assert_eq!(
            browser_normalize_path(r"C:\COM10\Console.txt").unwrap(),
            r"C:\COM10\Console.txt"
        );
    }

    #[test]
    fn browser_home_drive_snapshot_contains_no_labels_or_capacity() {
        let mut requested = Vec::new();
        let drives = drives_from_mask((1 << 2) | (1 << 6), "c:", |path| {
            requested.push(path.to_string());
            if path.starts_with('C') {
                "fixed"
            } else {
                "network"
            }
            .to_string()
        });
        assert_eq!(requested, ["C:\\", "G:\\"]);
        assert_eq!(
            drives
                .iter()
                .map(|drive| drive.name.as_str())
                .collect::<Vec<_>>(),
            ["C:", "G:"]
        );
        assert!(drives[0].system);
        assert!(!drives[1].system);
        assert!(drives.iter().all(|drive| drive.capacity.is_none()));
        assert_eq!(drive_kind(0), "unknown");
        assert_eq!(drive_kind(5), "cdrom");
    }

    #[test]
    fn browser_directory_uses_one_snapshot_per_child_and_never_reads_nested_contents() {
        let root = fixture();
        fs::create_dir(root.path().join("z-directory")).unwrap();
        fs::create_dir(root.path().join("A-directory")).unwrap();
        fs::write(
            root.path().join("z-directory/nested-secret.txt"),
            b"not enumerated",
        )
        .unwrap();
        fs::write(root.path().join("B.JPG"), b"abc").unwrap();
        fs::write(root.path().join("a.txt"), b"hello").unwrap();
        let mut reads = HashMap::new();
        let listing = read_directory_with(root.path().to_str().unwrap(), |entry| {
            *reads.entry(entry.path()).or_insert(0) += 1;
            entry.metadata()
        })
        .unwrap();
        assert_eq!(reads.len(), 4);
        assert!(reads.values().all(|count| *count == 1));
        assert_eq!(
            listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["A-directory", "z-directory", "a.txt", "B.JPG"]
        );
        assert!(listing.entries[..2]
            .iter()
            .all(|entry| entry.is_dir && entry.size_bytes.is_none()));
        assert_eq!(listing.entries[2].size_bytes, Some(5));
        assert_eq!(listing.entries[3].kind, "image");
        assert!(listing
            .entries
            .iter()
            .all(|entry| entry.modified_unix_ms.is_some()));
        assert_eq!(
            fs::read(root.path().join("z-directory/nested-secret.txt")).unwrap(),
            b"not enumerated"
        );
    }

    #[test]
    fn browser_unreadable_child_preserves_name_with_unknown_metadata_and_missing_root_is_an_error()
    {
        let root = fixture();
        fs::write(root.path().join("entry.txt"), b"fixture").unwrap();
        let listing = read_directory_with(root.path().to_str().unwrap(), |_| {
            Err(std::io::ErrorKind::PermissionDenied.into())
        })
        .unwrap();
        assert_eq!(listing.entries.len(), 1);
        assert_eq!(listing.entries[0].name, "entry.txt");
        assert_eq!(listing.entries[0].size_bytes, None);
        assert_eq!(listing.entries[0].modified_unix_ms, None);
        assert!(browser_read_directory(root.path().join("absent").to_str().unwrap()).is_err());
        assert!(browser_read_directory(root.path().join("entry.txt").to_str().unwrap()).is_err());
    }

    #[test]
    fn browser_capacity_and_placeholder_states_never_turn_unknown_into_zero() {
        assert_eq!(
            valid_capacity(0, 100),
            Some(BrowserCapacity {
                free_bytes: 0,
                total_bytes: 100
            })
        );
        assert_eq!(valid_capacity(0, 0), None);
        assert_eq!(valid_capacity(101, 100), None);
        assert_eq!(drive_name("C:\\", Some("Windows-SSD")), "Windows-SSD (C:)");
        assert_eq!(drive_name("C:\\", None), "C:");
        for attribute in [0x1000, 0x40000, 0x400000] {
            assert!(unavailable_placeholder(attribute | 0x10));
        }
        assert!(!unavailable_placeholder(0x10));
        assert_eq!(
            unix_millis(UNIX_EPOCH + Duration::from_millis(123)),
            Some(123)
        );
        assert_eq!(
            unix_millis(UNIX_EPOCH - Duration::from_millis(123)),
            Some(-123)
        );
        let invalid = BrowserDrive {
            name: "fixture".into(),
            path: r"C:\not-a-drive".into(),
            kind: "fixed".into(),
            system: false,
            capacity: None,
        };
        assert!(browser_drive_details(&invalid).is_err());
    }

    #[cfg(windows)]
    fn create_owned_junction(link: &Path, target: &Path) -> std::io::Result<()> {
        use std::{
            ffi::c_void,
            os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
        };

        #[link(name = "kernel32")]
        extern "system" {
            fn DeviceIoControl(
                handle: *mut c_void,
                control: u32,
                input: *const c_void,
                input_len: u32,
                output: *mut c_void,
                output_len: u32,
                returned: *mut u32,
                overlapped: *mut c_void,
            ) -> i32;
        }

        // Native mount-point reparse data avoids shell parsing and the
        // developer-mode privilege required to create directory symlinks.
        // Layout: https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/ntifs/ns-ntifs-_reparse_data_buffer
        let print = browser_normalize_path(target.to_str().unwrap()).unwrap();
        let substitute: Vec<u16> = format!(r"\??\{print}").encode_utf16().collect();
        let print: Vec<u16> = print.encode_utf16().collect();
        let substitute_len = u16::try_from(substitute.len() * 2).unwrap();
        let print_len = u16::try_from(print.len() * 2).unwrap();
        let data_len = 8 + substitute_len + 2 + print_len + 2;
        let mut buffer = Vec::with_capacity(usize::from(data_len) + 8);
        buffer.extend_from_slice(&0xA0000003u32.to_le_bytes());
        for value in [
            data_len,
            0,
            0,
            substitute_len,
            substitute_len + 2,
            print_len,
        ] {
            buffer.extend_from_slice(&value.to_le_bytes());
        }
        for value in substitute
            .into_iter()
            .chain(Some(0))
            .chain(print)
            .chain(Some(0))
        {
            buffer.extend_from_slice(&value.to_le_bytes());
        }
        fs::create_dir(link)?;
        let directory = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(7)
            .custom_flags(0x02200000)
            .open(link)?;
        let mut returned = 0;
        let ok = unsafe {
            DeviceIoControl(
                directory.as_raw_handle(),
                0x000900A4,
                buffer.as_ptr().cast(),
                buffer.len() as u32,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    #[cfg(windows)]
    #[test]
    fn browser_directory_link_is_listed_without_reading_its_target() {
        let root = fixture();
        let target = root.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("not-enumerated.txt"), b"fixture").unwrap();
        let directory = root.path().join("listing");
        fs::create_dir(&directory).unwrap();
        let link = directory.join("directory-link");
        create_owned_junction(&link, &target).expect("create owned junction fixture");
        let mut reads = Vec::new();
        let listing = read_directory_with(directory.to_str().unwrap(), |entry| {
            reads.push(entry.path());
            entry.metadata()
        })
        .unwrap();
        assert_eq!(reads, [link.clone()]);
        assert_eq!(listing.entries.len(), 1);
        assert!(listing.entries[0].is_dir);
        assert_eq!(listing.entries[0].kind, "link");
        assert_eq!(listing.entries[0].size_bytes, None);
        fs::remove_dir(link).unwrap();
        assert!(target.join("not-enumerated.txt").is_file());
    }
}
