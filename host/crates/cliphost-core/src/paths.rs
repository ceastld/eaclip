use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use thiserror::Error;

#[path = "data_bootstrap.rs"]
pub(crate) mod data_bootstrap;

pub const DATA_ROOT_ENV: &str = "BETTERCLIP_TAURI_DATA_ROOT";
pub const API_PORT_ENV: &str = "INTELLITOOLS_CLIPHOST_TAURI_PORT";
pub const HEALTH_SERVICE_ID: &str = "intellitools-cliphost-tauri";
pub const DEFAULT_API_PORT: u16 = 28965;
pub const CSHARP_DEFAULT_API_PORT: u16 = 28865;
pub const QUICKER_MCP_PORT: u16 = 18765;
pub const GPUI_IPC_PORT: u16 = 18776;
pub const CSHARP_LAN_PORT: u16 = 28766;
pub const DEFAULT_LAN_PORT: u16 = 29766;
pub const PRODUCT_FOLDER: &str = "BetterClipTauri";
pub const CSHARP_PRODUCT_FOLDER: &str = "BetterClip";
pub const GPUI_PRODUCT_FOLDER: &str = "BetterClipRs";
pub const DATA_ROOT_SIDECAR_FILE: &str = "application-data-root.json";
pub const CSHARP_VERB_RPC_PIPE_V1: &str = "IntelliTools_ClipHost_PipeRpc_ITCP2026";
pub const CSHARP_VERB_RPC_PIPE_V2: &str = "IntelliTools_ClipHost_PipeRpc_V2_ITCP2026";
pub const VERB_RPC_PIPE_V1: &str = "IntelliTools_ClipHost_Tauri_PipeRpc_ITCP2026";
pub const VERB_RPC_PIPE_V2: &str = "IntelliTools_ClipHost_Tauri_PipeRpc_V2_ITCP2026";
pub const VERB_RPC_PIPES: &[&str] = &[VERB_RPC_PIPE_V1, VERB_RPC_PIPE_V2];
pub const DEFAULT_RPC_PORT_OFFSET: u16 = 1;

/// C# `ClipHostBridgePorts.ResolveRpcPort` — DEBUG `host.ping.legacyDevRpcPort`.
/// Fallback uses the Tauri loopback default, never C# `28865`.
pub fn resolve_legacy_dev_rpc_port(api_listen_port: u16) -> u16 {
    for key in ["CLIPHOST_RPC_PORT", "INTELLITOOLS_RPC_PORT"] {
        if let Ok(raw) = std::env::var(key) {
            if let Ok(port) = raw.parse::<u32>() {
                if (1..=65535).contains(&port) {
                    return port as u16;
                }
            }
        }
    }
    let derived = u32::from(api_listen_port) + u32::from(DEFAULT_RPC_PORT_OFFSET);
    if (1..=65535).contains(&derived) {
        derived as u16
    } else {
        DEFAULT_API_PORT.saturating_add(DEFAULT_RPC_PORT_OFFSET)
    }
}

#[derive(Debug, Error)]
pub enum PathsError {
    #[error("LOCALAPPDATA is not set")]
    NoLocalAppData,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    InvalidRoot(String),
}

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub data_root: PathBuf,
    pub db: PathBuf,
    pub blobs: PathBuf,
    pub configs: PathBuf,
    pub logs: PathBuf,
    pub inbox: PathBuf,
    /// C# `ClipImageStorageRoots.LibraryFolder` → `data/Image`.
    pub image_library: PathBuf,
    /// C# `ClipImageStorageRoots.TempFolder` → `data/Image/Temp`.
    pub image_temp: PathBuf,
    /// C# `ClipImageStorageRoots.ThumbnailsFolder` → `data/Image/Thumbnails`.
    pub image_thumbnails: PathBuf,
}

impl AppPaths {
    pub fn resolve() -> Result<Self, PathsError> {
        crate::data_location::DataLocation::from_environment().resolve()
            .map(Self::from_data_root).map_err(PathsError::InvalidRoot)
    }

    pub fn default_sidecar_dir() -> Option<PathBuf> {
        std::env::var_os("LOCALAPPDATA").map(|local| PathBuf::from(local).join(PRODUCT_FOLDER))
    }

    pub fn sidecar_file_path() -> Option<PathBuf> {
        Self::default_sidecar_dir().map(|dir| dir.join(DATA_ROOT_SIDECAR_FILE))
    }

    pub fn sidecar_override_raw() -> String {
        Self::default_sidecar_dir()
            .and_then(|dir| read_data_root_sidecar(&dir))
            .map(|path| path.display().to_string())
            .unwrap_or_default()
    }

    pub fn write_data_root_sidecar(data_root: Option<&std::path::Path>) -> Result<(), String> {
        crate::data_location::DataLocation::from_environment().write_override(data_root)
    }

    pub fn from_data_root(data_root: PathBuf) -> Self {
        let image_library = data_root.join("Image");
        Self {
            db: data_root.join("userdata.db"),
            blobs: data_root.join("blobs"),
            image_temp: image_library.join("Temp"),
            image_thumbnails: image_library.join("Thumbnails"),
            image_library,
            configs: data_root.join("Configs"),
            logs: data_root.join("logs"),
            inbox: data_root.join("LanShare").join("Inbox"),
            data_root,
        }
    }

    pub fn ensure(&self) -> Result<(), PathsError> {
        fs::create_dir_all(&self.data_root)?;
        fs::create_dir_all(&self.blobs)?;
        fs::create_dir_all(&self.image_library)?;
        fs::create_dir_all(&self.image_temp)?;
        fs::create_dir_all(&self.image_thumbnails)?;
        fs::create_dir_all(&self.configs)?;
        fs::create_dir_all(&self.logs)?;
        fs::create_dir_all(&self.inbox)?;
        fs::create_dir_all(self.data_root.join("LanShare").join("Temp"))?;
        fs::create_dir_all(self.data_root.join("LanShare").join("Outbox"))?;
        Ok(())
    }

    pub fn is_isolated_from_csharp(&self) -> bool {
        // Kept for existing callers: C#/WPF data is now reused in place.
        // GPUI remains a separate product with an incompatible data format.
        let root = self.data_root.to_string_lossy().replace('/', "\\");
        let lower = root.to_ascii_lowercase();
        !lower.ends_with("\\bettercliprs") && !lower.contains("\\bettercliprs\\")
    }

    /// C# `ClipImageStorageRoots.IsAllowedImageContentFile`.
    pub fn is_allowed_image_content_file(&self, full_path: &std::path::Path) -> bool {
        if !full_path.is_absolute() {
            return false;
        }
        let Some(full) = image_access_comparison_path(full_path) else { return false; };
        [&self.image_library, &self.image_temp].into_iter().any(|root| {
            image_access_comparison_path(root).is_some_and(|root| is_under_directory(&full, &root))
        })
    }

    /// C# `ClipImageStorageRoots.CollectReferencedTempImagePaths` after resolving against the library root.
    pub fn referenced_temp_image_paths(
        &self,
        raw_paths: impl IntoIterator<Item = String>,
    ) -> HashSet<String> {
        let mut set = HashSet::new();
        for path in raw_paths {
            if let Some(full) = resolve_persisted_image_full_path(&path, &self.image_library) {
                if is_under_directory(&full, &self.image_temp) {
                    set.insert(normalize_path_slash(&full).to_ascii_lowercase());
                }
            }
        }
        set
    }

    /// C# `ClipDataService.CleanUpImages`: delete unreferenced files directly under `Image/Temp`.
    pub fn cleanup_unreferenced_temp_images(&self, referenced: &HashSet<String>) {
        let Ok(entries) = fs::read_dir(&self.image_temp) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let full = std::path::absolute(&path).unwrap_or(path);
            let key = normalize_path_slash(&full).to_ascii_lowercase();
            if !referenced.contains(&key) {
                let _ = fs::remove_file(&full);
            }
        }
    }
}

/// C# `ClipImageReferenceIndex.CollectImagePathTokens` (`ImagePath` / `APath` / `ImageSource`).
pub fn collect_image_path_tokens(value: &serde_json::Value, out: &mut HashSet<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                if matches!(
                    key.as_str(),
                    "ImagePath" | "APath" | "ImageSource" | "imagePath" | "aPath" | "imageSource"
                ) {
                    if let Some(text) = child.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                        out.insert(text.to_string());
                    }
                }
                collect_image_path_tokens(child, out);
            }
        }
        serde_json::Value::Array(items) => {
            for child in items {
                collect_image_path_tokens(child, out);
            }
        }
        _ => {}
    }
}

fn resolve_persisted_image_full_path(path: &str, library_root: &Path) -> Option<PathBuf> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return None;
    }
    let candidate = Path::new(trimmed);
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        library_root.join(candidate)
    };
    Some(std::path::absolute(&joined).unwrap_or(joined))
}

fn is_under_directory(file_path: &std::path::Path, directory_path: &std::path::Path) -> bool {
    let file = normalize_path_slash(file_path);
    let dir = normalize_path_slash(directory_path)
        .trim_end_matches('/')
        .to_string();
    file.eq_ignore_ascii_case(&dir)
        || file
            .to_ascii_lowercase()
            .starts_with(&format!("{}/", dir.to_ascii_lowercase()))
}

// Compare normal and Windows extended-length spellings without disk/network
// I/O. Keep the original path for opening the file, and don't change cleanup
// keys or reinterpret arbitrary device namespaces as managed image paths.
fn image_access_comparison_path(path: &Path) -> Option<PathBuf> {
    let absolute = std::path::absolute(path).ok()?;
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};
        let mut parts = absolute.components();
        let Component::Prefix(prefix) = parts.next()? else { return None; };
        let mut normalized = match prefix.kind() {
            Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => {
                PathBuf::from(format!("{}:\\", drive as char))
            }
            Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
                let mut root = std::ffi::OsString::from(r"\\");
                root.push(server);
                root.push(r"\");
                root.push(share);
                root.push(r"\");
                PathBuf::from(root)
            }
            _ => return None,
        };
        for part in parts {
            match part {
                Component::Normal(name) => normalized.push(name),
                Component::ParentDir => { if !normalized.pop() { return None; } }
                Component::RootDir | Component::CurDir => {}
                Component::Prefix(_) => return None,
            }
        }
        Some(normalized)
    }
    #[cfg(not(windows))]
    { Some(absolute) }
}

fn normalize_path_slash(path: &std::path::Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn read_data_root_sidecar(default_root: &std::path::Path) -> Option<PathBuf> {
    let bytes = fs::read(default_root.join(DATA_ROOT_SIDECAR_FILE)).ok()?;
    let parsed: serde_json::Value = serde_json::from_slice(data_bootstrap::strip_utf8_bom(&bytes)).ok()?;
    if parsed.get("useDefault").and_then(serde_json::Value::as_bool) == Some(true) {
        return None;
    }
    let raw = data_bootstrap::data_root_value(&parsed)?;
    Some(PathBuf::from(raw))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn isolated_root_is_not_csharp_userdata() {
        let dir = tempdir().unwrap();
        let paths = AppPaths::from_data_root(dir.path().join(PRODUCT_FOLDER));
        paths.ensure().unwrap();
        assert!(paths.is_isolated_from_csharp());
        assert_ne!(
            paths.data_root.file_name().and_then(|s| s.to_str()),
            Some(CSHARP_PRODUCT_FOLDER)
        );
    }

    #[test]
    fn coexist_ports_and_folders_differ_from_csharp() {
        assert_ne!(DEFAULT_API_PORT, CSHARP_DEFAULT_API_PORT);
        assert_ne!(DEFAULT_LAN_PORT, CSHARP_LAN_PORT);
        assert_ne!(PRODUCT_FOLDER, CSHARP_PRODUCT_FOLDER);
        assert_ne!(HEALTH_SERVICE_ID, "intellitools-cliphost");
        assert_ne!(VERB_RPC_PIPE_V1, CSHARP_VERB_RPC_PIPE_V1);
        assert_ne!(VERB_RPC_PIPE_V2, CSHARP_VERB_RPC_PIPE_V2);
        assert!(!VERB_RPC_PIPES.iter().any(|p| *p == CSHARP_VERB_RPC_PIPE_V1 || *p == CSHARP_VERB_RPC_PIPE_V2));
    }

    #[test]
    fn resolve_legacy_dev_rpc_port_matches_csharp_offset_on_tauri_default() {
        let saved_clip = std::env::var("CLIPHOST_RPC_PORT").ok();
        let saved_intel = std::env::var("INTELLITOOLS_RPC_PORT").ok();
        std::env::remove_var("CLIPHOST_RPC_PORT");
        std::env::remove_var("INTELLITOOLS_RPC_PORT");
        assert_eq!(resolve_legacy_dev_rpc_port(DEFAULT_API_PORT), DEFAULT_API_PORT + 1);
        assert_ne!(
            resolve_legacy_dev_rpc_port(DEFAULT_API_PORT),
            CSHARP_DEFAULT_API_PORT + 1
        );
        std::env::set_var("CLIPHOST_RPC_PORT", "30111");
        assert_eq!(resolve_legacy_dev_rpc_port(DEFAULT_API_PORT), 30111);
        match saved_clip {
            Some(value) => std::env::set_var("CLIPHOST_RPC_PORT", value),
            None => std::env::remove_var("CLIPHOST_RPC_PORT"),
        }
        match saved_intel {
            Some(value) => std::env::set_var("INTELLITOOLS_RPC_PORT", value),
            None => std::env::remove_var("INTELLITOOLS_RPC_PORT"),
        }
    }

    #[test]
    fn image_library_matches_csharp_clip_image_storage_roots() {
        let dir = tempdir().unwrap();
        let paths = AppPaths::from_data_root(dir.path().join(PRODUCT_FOLDER));
        paths.ensure().unwrap();
        assert_eq!(paths.image_library, paths.data_root.join("Image"));
        assert_eq!(paths.image_temp, paths.image_library.join("Temp"));
        assert_eq!(paths.image_thumbnails, paths.image_library.join("Thumbnails"));
        assert!(paths.image_library.is_dir());
        assert!(paths.image_temp.is_dir());
        assert!(paths.image_thumbnails.is_dir());
        let inside = paths.image_temp.join("shot.png");
        std::fs::write(&inside, b"x").unwrap();
        assert!(paths.is_allowed_image_content_file(&inside));
        assert!(paths.is_allowed_image_content_file(&paths.image_library.join("lib.png")));
        assert!(!paths.is_allowed_image_content_file(&dir.path().join("outside.png")));
        assert!(!paths.is_allowed_image_content_file(std::path::Path::new("relative.png")));
    }

    #[cfg(windows)]
    #[test]
    fn image_access_accepts_equivalent_windows_paths_without_widening_roots() {
        let paths = AppPaths::from_data_root(PathBuf::from(r"C:\isolated\BetterClipTauri"));
        assert!(paths.is_allowed_image_content_file(Path::new(r"\\?\C:\isolated\BetterClipTauri\Image\截图.png")));
        let extended_root = AppPaths::from_data_root(PathBuf::from(r"\\?\C:\isolated\BetterClipTauri"));
        assert!(extended_root.is_allowed_image_content_file(Path::new(r"C:\isolated\BetterClipTauri\Image\Temp\截图.png")));
        for denied in [
            r"\\?\C:\isolated\BetterClipTauri\Image-other\shot.png",
            r"\\?\C:\isolated\BetterClipTauri\Image\..\outside.png",
            r"C:\isolated\BetterClipTauri\Image\..\outside.png",
            r"\\.\C:\isolated\BetterClipTauri\Image\shot.png",
            r"\\?\GLOBALROOT\Device\HarddiskVolume1\isolated\BetterClipTauri\Image\shot.png",
        ] {
            assert!(!paths.is_allowed_image_content_file(Path::new(denied)), "{denied}");
        }
        let unc = AppPaths::from_data_root(PathBuf::from(r"\\server\share\BetterClipTauri"));
        assert!(unc.is_allowed_image_content_file(Path::new(r"\\?\UNC\server\share\BetterClipTauri\Image\shot.png")));
        assert!(!unc.is_allowed_image_content_file(Path::new(r"\\?\UNC\other\share\BetterClipTauri\Image\shot.png")));
    }

    #[test]
    fn cleanup_temp_images_keeps_referenced_and_walks_json_keys() {
        let dir = tempdir().unwrap();
        let paths = AppPaths::from_data_root(dir.path().join(PRODUCT_FOLDER));
        paths.ensure().unwrap();
        let keep = paths.image_temp.join("keep.png");
        let drop = paths.image_temp.join("drop.png");
        fs::write(&keep, b"keep").unwrap();
        fs::write(&drop, b"drop").unwrap();
        let relative = "Temp/keep.png";
        let mut tokens = HashSet::new();
        collect_image_path_tokens(
            &serde_json::json!({
                "item": { "ImagePath": relative, "nested": { "APath": "" } }
            }),
            &mut tokens,
        );
        collect_image_path_tokens(
            &serde_json::json!({ "imagePath": keep.to_string_lossy() }),
            &mut tokens,
        );
        assert!(tokens.contains(relative));
        let referenced = paths.referenced_temp_image_paths(tokens);
        paths.cleanup_unreferenced_temp_images(&referenced);
        assert!(keep.is_file());
        assert!(!drop.is_file());
        assert!(paths
            .referenced_temp_image_paths(["outside.png".into()])
            .is_empty());
    }
}
