use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::Path;
use crate::extra_settings::FileMenuSettings;

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct PersistedFileMenu {
    #[serde(flatten)]
    settings: FileMenuSettings,
    #[serde(rename = "RustCustomPathsVersion")]
    paths_version: u32,
    #[serde(rename = "RustOpenedFolders", skip_serializing_if = "Vec::is_empty")]
    opened_folders: Vec<String>,
}

fn read_optional<T: Default + Serialize + serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>, String> {
    if crate::config_compat::read(path)?.is_none() { return Ok(None) }
    crate::config_compat::load(path).map(Some)
}

/// FileMenuConfig.Files is the single persisted list. Import the old Rust
/// sidecar once, including an intentionally empty list, without deleting it.
pub(crate) fn load_config(path: &Path, legacy_path: &Path) -> Result<(FileMenuSettings, FileMenuPaths), String> {
    let mut saved: PersistedFileMenu = read_optional(path)?.unwrap_or_default();
    if saved.paths_version > 1 {
        return Err(format!("Unsupported file-menu storage version: {}", saved.paths_version));
    }
    let mut paths = FileMenuPaths {
        opened_folders: saved.opened_folders,
        ..FileMenuPaths::default()
    };
    paths.replace_all(saved.settings.files.lines().map(str::to_string).collect());
    if saved.paths_version == 0 {
        if let Some(legacy) = read_optional::<FileMenuPaths>(legacy_path)? {
            paths = legacy;
        }
        save_config(&paths, &saved.settings, path)?;
    }
    saved.settings.files = paths.lines.join("\r\n");
    Ok((saved.settings, paths))
}

pub(crate) fn save_config(paths: &FileMenuPaths, settings: &FileMenuSettings, path: &Path) -> Result<(), String> {
    let mut settings = settings.clone();
    settings.files = paths.lines.join("\r\n");
    let saved = PersistedFileMenu { settings, paths_version: 1, opened_folders: paths.opened_folders.clone() };
    crate::config_file::save(path, &saved)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CustomEntry {
    Path(String),
    Separator,
}

pub fn is_separator_line(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty() && trimmed.chars().all(|c| c == '-')
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileMenuPaths {
    pub lines: Vec<String>,
    #[serde(default)]
    pub opened_folders: Vec<String>,
}

impl FileMenuPaths {
    pub fn paths(&self) -> Vec<String> {
        self.lines
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty() && !s.starts_with('#') && !is_separator_line(s))
            .map(|s| s.to_string())
            .collect()
    }

    pub fn custom_entries(&self) -> Vec<CustomEntry> {
        self.lines
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty() && !s.starts_with('#'))
            .map(|s| {
                if is_separator_line(s) {
                    CustomEntry::Separator
                } else {
                    CustomEntry::Path(s.to_string())
                }
            })
            .collect()
    }

    pub fn snapshot(&self) -> Value {
        json!({
            "lines": self.lines,
            "paths": self.paths(),
            "openedFolders": self.opened_folders,
        })
    }

    /// C# PUT/add/remove/move/pick-folders omit `openedFolders` (GET-only).
    pub fn mutation_body(&self) -> Value {
        json!({
            "lines": self.lines,
            "paths": self.paths(),
        })
    }

    pub fn replace_all(&mut self, lines: Vec<String>) {
        self.lines = lines
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }

    pub fn try_add(&mut self, path: &str) -> bool {
        // C# `FileMenuCustomPaths.TryAddPath` trims quotes; existence is checked by
        // `FileMenuCustomPathActions`.
        let path = path.trim().trim_matches('"').trim();
        if path.is_empty() {
            return false;
        }
        if self.paths().iter().any(|p| p.eq_ignore_ascii_case(path)) {
            return false;
        }
        self.lines.push(path.to_string());
        true
    }

    pub fn try_remove(&mut self, path: &str) -> bool {
        let before = self.lines.len();
        self.lines
            .retain(|line| !line.trim().eq_ignore_ascii_case(path.trim()));
        self.lines.len() != before
    }

    pub fn try_move(&mut self, path: &str, delta: i32) -> bool {
        let Some(idx) = self
            .lines
            .iter()
            .position(|line| line.trim().eq_ignore_ascii_case(path.trim()))
        else {
            return false;
        };
        let next = idx as i32 + delta;
        if next < 0 || next >= self.lines.len() as i32 {
            return false;
        }
        self.lines.swap(idx, next as usize);
        true
    }
}

/// C# `FileMenuCustomPathActions.TryAddFromClipboard` first non-empty line.
pub fn first_clipboard_path_line(text: &str) -> Option<String> {
    text.split(['\r', '\n'])
        .map(|line| line.trim().trim_matches('"').trim())
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

/// C# `FileMenuCustomPathActions.ClipboardLooksLikeExistingPath`.
pub fn clipboard_looks_like_existing_path(text: &str) -> Option<String> {
    let line = first_clipboard_path_line(text)?;
    path_is_available(&line).then_some(line)
}

/// Shell namespace locations are opened by Explorer, not filesystem APIs.
pub(crate) fn path_is_available(path: &str) -> bool {
    !path.is_empty() && !path.contains('\0')
        && (crate::explorer::is_shell_path(path) || Path::new(path).exists())
}

#[cfg(test)]
mod tests {
    use super::{clipboard_looks_like_existing_path, first_clipboard_path_line};

    #[test]
    fn file_menu_config_migrates_legacy_once_and_preserves_settings() {
        for legacy_lines in [Some(vec!["saved-a", "---", "saved-b"]), Some(vec![]), None] {
            let dir = tempfile::tempdir().unwrap();
            let config = dir.path().join("FileMenuConfig.json");
            let legacy = dir.path().join("FileMenuCustomPaths.json");
            std::fs::write(&config, r#"{"Files":"settings-a","ShowExtension":false,"FileSortType":"Name"}"#).unwrap();
            if let Some(lines) = &legacy_lines {
                std::fs::write(&legacy, serde_json::json!({"lines":lines,"openedFolders":["opened-a"]}).to_string()).unwrap();
            }
            let legacy_bytes = std::fs::read(&legacy).ok();
            let (settings, mut paths) = super::load_config(&config, &legacy).unwrap();
            let expected = legacy_lines.unwrap_or_else(|| vec!["settings-a"]);
            assert_eq!(paths.lines, expected);
            assert!(!settings.show_extension);
            // The compatibility loader canonicalizes enum spellings while
            // preserving the selected sort order.
            assert_eq!(settings.file_sort_type, "name");
            assert_eq!(settings.files, expected.join("\r\n"));
            assert_eq!(std::fs::read(&legacy).ok(), legacy_bytes, "migration must retain the original sidecar");
            let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
            assert_eq!(saved["Files"], settings.files);
            assert_eq!(saved["RustCustomPathsVersion"], 1);
            // The former sidecar is no longer authoritative, even if a stale
            // process or manual edit changes it after migration.
            std::fs::write(&legacy, b"broken old sidecar").unwrap();
            let (_, reloaded) = super::load_config(&config, &legacy).unwrap();
            assert_eq!(reloaded.snapshot(), paths.snapshot());
            paths.replace_all(vec![]);
            super::save_config(&paths, &settings, &config).unwrap();
            let (cleared_settings, cleared) = super::load_config(&config, &legacy).unwrap();
            assert!(cleared.lines.is_empty());
            assert!(cleared_settings.files.is_empty());
        }
    }

    #[test]
    fn file_menu_invalid_config_is_reported_without_overwriting_inputs() {
        for (config_bytes, legacy_bytes) in [
            ("broken current config", r#"{"lines":["saved"]}"#),
            (r#"{"Files":"settings-a"}"#, "broken legacy config"),
            (r#"{"Files":"future","RustCustomPathsVersion":2}"#, r#"{"lines":[]}"#),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let config = dir.path().join("FileMenuConfig.json");
            let legacy = dir.path().join("FileMenuCustomPaths.json");
            std::fs::write(&config, config_bytes).unwrap();
            std::fs::write(&legacy, legacy_bytes).unwrap();
            assert!(super::load_config(&config, &legacy).is_err());
            assert_eq!(std::fs::read_to_string(&config).unwrap(), config_bytes);
            assert_eq!(std::fs::read_to_string(&legacy).unwrap(), legacy_bytes);
        }
    }

    #[cfg(windows)]
    #[test]
    fn failed_file_menu_replace_preserves_old_bytes_and_cleans_staging_file() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("paths.json");
        let mut paths = super::FileMenuPaths::default();
        let settings = super::FileMenuSettings::default();
        paths.replace_all(vec!["previous".into()]);
        super::save_config(&paths, &settings, &path).unwrap();
        let previous = std::fs::read(&path).unwrap();
        // Allow in-place writes but deny replacing the file. The staged writer
        // must fail without truncating or changing the committed bytes.
        let held = std::fs::OpenOptions::new().read(true)
            .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0).open(&path).unwrap();
        paths.replace_all(vec!["replacement".into()]);
        assert!(super::save_config(&paths, &settings, &path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), previous);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        drop(held);
        super::save_config(&paths, &settings, &path).unwrap();
        assert_eq!(super::load_config(&path, &dir.path().join("legacy.json")).unwrap().1.lines, paths.lines);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn first_clipboard_path_line_trims_quotes_and_blank_lines() {
        assert_eq!(
            first_clipboard_path_line("\r\n  \"D:\\share\"  \nmore"),
            Some(r"D:\share".into())
        );
        assert_eq!(first_clipboard_path_line("   \r\n\t"), None);
    }

    #[test]
    fn clipboard_looks_like_existing_path_uses_first_line_only() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().to_string_lossy().into_owned();
        assert_eq!(
            clipboard_looks_like_existing_path(&format!("\"{existing}\"\r\nlater")),
            Some(existing.clone())
        );
        assert!(clipboard_looks_like_existing_path(&format!("missing-path\r\n{existing}")).is_none());
        assert!(clipboard_looks_like_existing_path("").is_none());
    }

    #[test]
    fn clipboard_shell_location_is_available_without_a_filesystem_entry() {
        let shell = crate::explorer::THIS_PC_SHELL;
        assert_eq!(clipboard_looks_like_existing_path(&format!("\r\n\"{shell}\"\r\nignored")), Some(shell.into()));
        assert!(super::path_is_available("::{20d04fe0-3aea-1069-a2d8-08002b30309d}"));
        assert!(!super::path_is_available("shell:\0invalid"));
    }

    #[test]
    fn mutation_body_omits_opened_folders_like_csharp_put() {
        let mut paths = super::FileMenuPaths::default();
        paths.lines = vec![r"C:\share".into()];
        paths.opened_folders = vec![r"C:\open".into()];
        let snap = paths.snapshot();
        assert_eq!(snap["openedFolders"][0], r"C:\open");
        assert_eq!(snap["paths"][0], r"C:\share");
        let body = paths.mutation_body();
        assert!(body.get("openedFolders").is_none());
        assert_eq!(body["lines"][0], r"C:\share");
        assert_eq!(body["paths"][0], r"C:\share");
    }
}
