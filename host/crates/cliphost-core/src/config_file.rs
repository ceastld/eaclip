//! Stage complete configuration files before replacing their previous versions.
use serde::Serialize;
use serde_json::Value;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};
use tempfile::{NamedTempFile, PersistError};

const REPLACE_RETRIES: usize = 8;
const REPLACE_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(50);

pub(crate) fn save(path: &Path, value: &impl Serialize) -> Result<(), String> {
    save_values(&[(
        path.to_path_buf(),
        serde_json::to_value(value).map_err(|e| e.to_string())?,
    )])
}

pub(crate) fn save_values(values: &[(PathBuf, Value)]) -> Result<(), String> {
    save_with(values, replace)
}

fn stage(path: &Path, bytes: &[u8]) -> Result<NamedTempFile, String> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let mut file = NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
    file.write_all(bytes).map_err(|e| e.to_string())?;
    file.as_file().sync_all().map_err(|e| e.to_string())?;
    Ok(file)
}

fn replace(file: NamedTempFile, path: &Path) -> Result<(), String> {
    persist_with_retry(file, path).map_err(|e| format!("保存 {} 失败: {e}", path.display()))
}

fn persist_with_retry(file: NamedTempFile, path: &Path) -> Result<(), PersistError> {
    persist_with_retry_using(file, path, |file, path| file.persist(path).map(|_| ()))
}

fn persist_with_retry_using(
    mut file: NamedTempFile,
    path: &Path,
    mut persist: impl FnMut(NamedTempFile, &Path) -> Result<(), PersistError>,
) -> Result<(), PersistError> {
    for attempt in 0..=REPLACE_RETRIES {
        match persist(file, path) {
            Ok(()) => return Ok(()),
            Err(error) => {
                // Windows scanners/readers can briefly deny replacement after
                // a file was just published. Keep the staged bytes while retrying.
                let retryable =
                    cfg!(windows) && matches!(error.error.raw_os_error(), Some(5 | 32 | 33));
                if !retryable || attempt == REPLACE_RETRIES {
                    return Err(error);
                }
                file = error.file;
                std::thread::sleep(REPLACE_RETRY_DELAY);
            }
        }
    }
    unreachable!("the final persist attempt always returns")
}

fn restore_previous(file: NamedTempFile, path: &Path) -> Result<(), String> {
    let Err(error) = persist_with_retry(file, path) else {
        return Ok(());
    };
    let reason = error.error.to_string();
    let backup_path = error.file.path().to_path_buf();
    // A rollback failure must not drop the only copy of the original config.
    // Leave it beside the target and report its exact path for recovery.
    match error.file.keep() {
        Ok((_file, backup)) => Err(format!("{reason}; 原配置备份保留在 {}", backup.display())),
        Err(mut keep_error) => {
            keep_error.file.disable_cleanup(true);
            Err(format!(
                "{reason}; 原配置备份保留在 {}（移除临时属性失败：{}）",
                backup_path.display(),
                keep_error.error
            ))
        }
    }
}

fn save_with(
    values: &[(PathBuf, Value)],
    mut publish: impl FnMut(NamedTempFile, &Path) -> Result<(), String>,
) -> Result<(), String> {
    let mut staged = Vec::new();
    for (path, value) in values {
        let previous_bytes = match fs::read(path) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(format!("读取 {} 失败: {e}", path.display())),
        };
        let bytes = crate::config_compat::encode_preserving(path, value, previous_bytes.as_deref())?;
        let previous = previous_bytes.as_ref().map(|bytes| stage(path, bytes)).transpose()?;
        staged.push((path.clone(), stage(path, &bytes)?, previous));
    }
    let mut committed: Vec<(PathBuf, Option<NamedTempFile>)> = Vec::new();
    for (path, file, previous) in staged {
        if let Err(mut error) = publish(file, &path) {
            for (path, previous) in committed.into_iter().rev() {
                let restored = match previous {
                    Some(file) => restore_previous(file, &path),
                    None => fs::remove_file(&path).map_err(|e| e.to_string()),
                };
                if let Err(rollback) = restored {
                    error.push_str(&format!("; 恢复 {} 失败: {rollback}", path.display()));
                }
            }
            return Err(error);
        }
        committed.push((path, previous));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[cfg(windows)]
    #[test]
    fn config_persistence_transient_denial_retries_the_same_staged_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("config.json");
        fs::write(&target, b"old").unwrap();
        let file = stage(&target, b"new").unwrap();
        let mut attempts = 0;
        persist_with_retry_using(file, &target, |file, path| {
            attempts += 1;
            assert_eq!(fs::read(file.path()).unwrap(), b"new");
            if attempts <= 2 {
                assert_eq!(fs::read(path).unwrap(), b"old");
                Err(PersistError {
                    error: std::io::Error::from_raw_os_error(5),
                    file,
                })
            } else {
                file.persist(path).map(|_| ())
            }
        })
        .unwrap();
        assert!(attempts >= 3 && attempts <= REPLACE_RETRIES + 1);
        assert_eq!(fs::read(&target).unwrap(), b"new");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[cfg(windows)]
    #[test]
    fn config_persistence_failed_rollback_retains_original_bytes_and_reports_backup() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old.json");
        let failed = dir.path().join("failed.json");
        let original = b"{\"original\":true}\r\n";
        fs::write(&old, original).unwrap();
        let mut locked = None;
        let error = save_with(
            &[
                (old.clone(), json!({"changed":true})),
                (failed.clone(), json!({})),
            ],
            |file, path| {
                if path == failed {
                    return Err("fixture publish failed".into());
                }
                replace(file, path)?;
                locked = Some(
                    fs::OpenOptions::new()
                        .read(true)
                        .share_mode(1)
                        .open(path)
                        .unwrap(),
                );
                Ok(())
            },
        )
        .unwrap_err();
        assert!(error.contains("fixture publish failed"));
        assert!(error.contains("原配置备份保留在"), "{error}");
        let backups = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path != &old)
            .collect::<Vec<_>>();
        assert_eq!(backups.len(), 1);
        assert!(error.contains(&backups[0].display().to_string()));
        assert_eq!(fs::read(&backups[0]).unwrap(), original);
        assert!(!failed.exists());
        drop(locked);
        let recovered: Value = serde_json::from_slice(&fs::read(&backups[0]).unwrap()).unwrap();
        save(&old, &recovered).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(&old).unwrap()).unwrap(),
            json!({"original":true})
        );
    }

    #[test]
    fn config_persistence_failed_batch_restores_existing_and_removes_new_files() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old.json");
        let new = dir.path().join("new.json");
        let failed = dir.path().join("failed.json");
        fs::write(&old, "{\"original\":true}\r\n").unwrap();
        let values = [
            (old.clone(), json!({"changed":true})),
            (new.clone(), json!({})),
            (failed.clone(), json!({})),
        ];
        let error = save_with(&values, |file, path| {
            if path == failed {
                Err("replace rejected".into())
            } else {
                replace(file, path)
            }
        })
        .unwrap_err();
        assert_eq!(error, "replace rejected");
        assert_eq!(fs::read_to_string(&old).unwrap(), "{\"original\":true}\r\n");
        assert!(!new.exists());
        assert!(!failed.exists());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
        save_values(&values).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(old).unwrap()).unwrap(),
            json!({"changed":true})
        );
    }

    #[test]
    fn config_persistence_stages_every_file_before_changing_any_destination() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old.json");
        let bad = dir.path().join("directory.json");
        fs::write(&old, "old bytes").unwrap();
        fs::create_dir(&bad).unwrap();
        assert!(save_values(&[(old.clone(), json!({})), (bad, json!({}))]).is_err());
        assert_eq!(fs::read_to_string(old).unwrap(), "old bytes");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
    }
}
