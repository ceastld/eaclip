//! Lightweight explicit invocation contract shared by the CLI and Rust host.
//! No clipboard, database, GUI or implicit data-location selection.
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

pub const INFO_METHOD: &str = "cliphost.getInvocationInfoV1";
pub const RUN_METHOD: &str = "cliphost.runVerbWithTargetV1";
pub const TEST_PIPE_PREFIX: &str = "IntelliTools_ClipHost_ScriptTest_";
pub const MAX_TARGET_AGE_MS: u64 = 30_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetSnapshot {
    pub window_handle: String,
    pub process_id: u32,
    pub thread_id: u32,
    pub process_created_file_time: String,
    pub expires_at_unix_ms: u64,
}

fn decimal(value: &str) -> Result<u64, String> {
    if value.is_empty()
        || value.len() > 20
        || value.starts_with('0')
        || !value.bytes().all(|b| b.is_ascii_digit())
    {
        return Err("invalid_target_decimal".into());
    }
    value.parse().map_err(|_| "invalid_target_decimal".into())
}

impl TargetSnapshot {
    pub fn validate_at(&self, now: u64) -> Result<(i64, u64), String> {
        let hwnd = decimal(&self.window_handle)?;
        let created = decimal(&self.process_created_file_time)?;
        if hwnd > i64::MAX as u64
            || self.process_id == 0
            || self.thread_id == 0
            || self.expires_at_unix_ms <= now
            || self.expires_at_unix_ms - now > MAX_TARGET_AGE_MS
        {
            return Err("invalid_or_expired_target".into());
        }
        Ok((hwnd as i64, created))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoundVerb {
    pub schema_version: u32,
    pub expected_data_root: PathBuf,
    pub command: String,
    pub target: TargetSnapshot,
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

pub fn validate_root_spelling(path: &Path) -> Result<(), String> {
    let spelling = path.to_str().ok_or("non_unicode_data_root")?;
    let without_verbatim = spelling.strip_prefix(r"\\?\").unwrap_or(spelling);
    let local_verbatim = spelling.starts_with(r"\\?\")
        && without_verbatim.as_bytes().get(1) == Some(&b':')
        && without_verbatim
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphabetic);
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
        || spelling.starts_with(r"\\") && !local_verbatim
        || spelling.contains('\0')
        || spelling.split(['\\', '/']).any(|part| part == "..")
        || spelling.split(['\\', '/']).any(|part| {
            let name = part.to_lowercase();
            name.starts_with("onedrive") || name == "dropbox" || name.starts_with("icloud")
        })
    {
        return Err("explicit_plain_local_data_root_required".into());
    }
    Ok(())
}

/// Normalize only an explicitly supplied root. Inspect ancestors before any
/// canonicalization; never fall back to the product's saved or personal root.
pub fn checked_root(path: &Path) -> Result<PathBuf, String> {
    validate_root_spelling(path)?;
    let mut cursor = PathBuf::new();
    let mut existing = None;
    let mut suffix = PathBuf::new();
    let mut missing = false;
    for component in path.components() {
        cursor.push(component.as_os_str());
        // A verbatim drive prefix (\\?\D:) is a volume designator until the
        // root separator is appended. Query only the complete filesystem root.
        if matches!(component, Component::Prefix(_)) || !cursor.has_root() {
            continue;
        }
        if missing {
            suffix.push(component.as_os_str());
            continue;
        }
        match std::fs::symlink_metadata(&cursor) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err("linked_data_root".into());
                }
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    if metadata.file_attributes() & 0x441400 != 0 {
                        return Err("offline_or_linked_data_root".into());
                    }
                }
                if !metadata.is_dir() {
                    return Err("data_root_is_not_directory".into());
                }
                existing = Some(cursor.clone());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing = true;
                suffix.push(component.as_os_str());
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    let mut result = existing
        .ok_or("invalid_data_root")?
        .canonicalize()
        .map_err(|error| error.to_string())?;
    if !suffix.as_os_str().is_empty() {
        result.push(suffix);
    }
    Ok(result)
}

pub fn same_root(a: &Path, b: &Path) -> Result<bool, String> {
    let (a, b) = (checked_root(a)?, checked_root(b)?);
    #[cfg(windows)]
    {
        Ok(a.as_os_str().eq_ignore_ascii_case(b.as_os_str()))
    }
    #[cfg(not(windows))]
    {
        Ok(a == b)
    }
}

pub fn test_pipe_name(root: &Path) -> Result<String, String> {
    let root = checked_root(root)?;
    let digest = blake3::hash(root.to_string_lossy().to_lowercase().as_bytes());
    Ok(format!("{TEST_PIPE_PREFIX}{}", &digest.to_hex()[..16]))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    #[test]
    fn canonical_root_round_trip_and_pipe_identity_are_stable() {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .unwrap();
        let root = workspace
            .join(".tmp")
            .join(format!("invocation-roundtrip-{}", std::process::id()));
        let canonical = checked_root(&root).unwrap();
        assert_eq!(checked_root(&canonical).unwrap(), canonical);
        assert!(same_root(&root, &canonical).unwrap());
        assert_eq!(
            test_pipe_name(&root).unwrap(),
            test_pipe_name(&canonical).unwrap()
        );
    }
    #[test]
    fn root_creation_preserves_canonical_spelling_and_pipe_identity() {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .unwrap();
        let root = workspace.join(".tmp").join(format!(
            "invocation-create-{}-{}",
            std::process::id(),
            now_ms()
        ));
        assert!(!root.exists());
        let before = checked_root(&root).unwrap();
        let pipe_before = test_pipe_name(&root).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let after = checked_root(&root).unwrap();
        let pipe_after = test_pipe_name(&root).unwrap();
        std::fs::remove_dir(&root).unwrap();
        assert_eq!(before.as_os_str(), after.as_os_str());
        assert_eq!(pipe_before, pipe_after);
    }
    fn target() -> TargetSnapshot {
        TargetSnapshot {
            window_handle: "9223372036854775000".into(),
            process_id: 2,
            thread_id: 3,
            process_created_file_time: "18446744073709550000".into(),
            expires_at_unix_ms: 2000,
        }
    }
    #[test]
    fn explicit_target_precision_expiry_and_missing_fields() {
        let value = target();
        assert_eq!(
            value.validate_at(1000).unwrap(),
            (9223372036854775000, 18446744073709550000)
        );
        assert!(value.validate_at(2000).is_err());
        assert!(value.validate_at(0).is_ok());
        let mut future = value.clone();
        future.expires_at_unix_ms = 31_001;
        assert!(future.validate_at(1000).is_err());
        for field in [
            "window_handle",
            "process_id",
            "thread_id",
            "process_created_file_time",
            "expires_at_unix_ms",
        ] {
            let mut json = serde_json::to_value(&value).unwrap();
            json.as_object_mut().unwrap().remove(field);
            assert!(
                serde_json::from_value::<TargetSnapshot>(json).is_err(),
                "{field}"
            );
        }
        let mut zero = value;
        zero.process_created_file_time = "0".into();
        assert!(zero.validate_at(1000).is_err());
    }
    #[test]
    fn root_spelling_never_allows_relative_parent_or_cloud_fallback() {
        assert!(validate_root_spelling(Path::new("relative")).is_err());
        let root = std::env::temp_dir();
        assert!(validate_root_spelling(&root.join("safe/../other")).is_err());
        assert!(validate_root_spelling(&root.join("OneDrive-test/data")).is_err());
        assert_ne!(
            test_pipe_name(&root.join("script-root-a")).unwrap(),
            test_pipe_name(&root.join("script-root-b")).unwrap()
        );
    }
}
