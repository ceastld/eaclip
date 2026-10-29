//! C# `WindowBackgroundImagePaths`: stage copies under `{dataRoot}/WindowBackground`.

use std::path::{Path, PathBuf};

use crate::lan::sha256_hex;

pub const SUPPORTED_EXTENSIONS: &[&str] = &[".png", ".jpg", ".jpeg", ".gif", ".bmp", ".webp"];

pub fn storage_directory(data_root: &Path) -> PathBuf {
    data_root.join("WindowBackground")
}

pub fn is_supported_extension(ext: &str) -> bool {
    let normalized = if ext.starts_with('.') {
        ext.to_ascii_lowercase()
    } else {
        format!(".{}", ext.to_ascii_lowercase())
    };
    SUPPORTED_EXTENSIONS.iter().any(|item| *item == normalized)
}

/// C# `WindowBackgroundImagePaths.ResolveImageContentType` / loopback `ResolveImageContentType`.
pub fn resolve_content_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        _ => "application/octet-stream",
    }
}

pub fn is_under_storage(data_root: &Path, path: &Path) -> bool {
    let full = normalize_path(&path.canonicalize().unwrap_or_else(|_| path.to_path_buf()));
    let storage = normalize_path(&storage_directory(data_root));
    full == storage || full.starts_with(&(storage.clone() + "\\"))
}

fn normalize_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase()
}

pub fn stage_bytes(data_root: &Path, content: &[u8], extension: &str) -> Result<PathBuf, String> {
    stage_bytes_guarded(data_root, content, extension, &|| Ok(()))
}

pub(crate) fn stage_bytes_guarded(
    data_root: &Path, content: &[u8], extension: &str,
    verify: &impl Fn() -> Result<(), String>,
) -> Result<PathBuf, String> {
    verify()?;
    if content.is_empty() {
        return Err("Failed to stage background image.".into());
    }
    let ext = if extension.starts_with('.') {
        extension.to_ascii_lowercase()
    } else {
        format!(".{}", extension.to_ascii_lowercase())
    };
    if !is_supported_extension(&ext) {
        return Err("Unsupported image type.".into());
    }
    let dir = storage_directory(data_root);
    let staged = dir.join(format!("{}{ext}", sha256_hex(content)));
    verify()?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    verify()?;
    if !staged.exists() {
        #[cfg(test)]
        stage_test_hook(StageTestPhase::BeforeWrite);
        verify()?;
        std::fs::write(&staged, content).map_err(|e| e.to_string())?;
    }
    verify()?;
    Ok(staged)
}

pub fn stage_copy(data_root: &Path, source: &Path) -> Result<PathBuf, String> {
    stage_copy_guarded(data_root, source, &|| Ok(()))
}

pub(crate) fn stage_copy_guarded(
    data_root: &Path, source: &Path,
    verify: &impl Fn() -> Result<(), String>,
) -> Result<PathBuf, String> {
    verify()?;
    let ext = source
        .extension()
        .and_then(|e| e.to_str())
        .ok_or_else(|| "Unsupported image type.".to_string())?;
    if !is_supported_extension(ext) {
        return Err("Unsupported image type.".into());
    }
    let source_is_file = source.is_file();
    verify()?;
    if !source_is_file {
        return Err("Failed to copy background image to app data folder.".into());
    }
    let already_stored = is_under_storage(data_root, source);
    verify()?;
    if already_stored {
        return Ok(source.to_path_buf());
    }
    let bytes = std::fs::read(source).map_err(|e| e.to_string());
    #[cfg(test)]
    stage_test_hook(StageTestPhase::AfterRead);
    verify()?;
    let bytes = bytes?;
    stage_bytes_guarded(data_root, &bytes, &format!(".{ext}"), verify)
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum StageTestPhase { AfterRead, BeforeWrite }

#[cfg(test)]
thread_local! {
    static STAGE_TEST_HOOK: std::cell::RefCell<Option<Box<dyn FnMut(StageTestPhase)>>> =
        std::cell::RefCell::new(None);
}

#[cfg(test)]
pub(crate) fn with_stage_test_hook<T>(hook: impl FnMut(StageTestPhase) + 'static, run: impl FnOnce() -> T) -> T {
    struct Restore(Option<Box<dyn FnMut(StageTestPhase)>>);
    impl Drop for Restore { fn drop(&mut self) { STAGE_TEST_HOOK.with(|hook| *hook.borrow_mut() = self.0.take()); } }
    let previous = STAGE_TEST_HOOK.with(|current| current.borrow_mut().replace(Box::new(hook)));
    let _restore = Restore(previous);
    run()
}

#[cfg(test)]
fn stage_test_hook(phase: StageTestPhase) {
    STAGE_TEST_HOOK.with(|hook| { if let Some(hook) = hook.borrow_mut().as_mut() { hook(phase); } });
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn stages_bytes_under_hashed_name() {
        let dir = tempdir().unwrap();
        let path = stage_bytes(dir.path(), b"png-bytes", ".png").unwrap();
        assert!(path.starts_with(storage_directory(dir.path())));
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("png"));
        assert_eq!(std::fs::read(&path).unwrap(), b"png-bytes");
        let again = stage_bytes(dir.path(), b"png-bytes", ".png").unwrap();
        assert_eq!(again, path);
    }

    #[test]
    fn rejects_unsupported_extension() {
        let dir = tempdir().unwrap();
        assert!(stage_bytes(dir.path(), b"x", ".exe").is_err());
    }

    #[test]
    fn resolve_content_type_matches_csharp() {
        assert_eq!(resolve_content_type(Path::new("a.png")), "image/png");
        assert_eq!(resolve_content_type(Path::new("a.JPG")), "image/jpeg");
        assert_eq!(resolve_content_type(Path::new("a.jpeg")), "image/jpeg");
        assert_eq!(resolve_content_type(Path::new("a.gif")), "image/gif");
        assert_eq!(resolve_content_type(Path::new("a.webp")), "image/webp");
        assert_eq!(resolve_content_type(Path::new("a.bmp")), "image/bmp");
        assert_eq!(resolve_content_type(Path::new("a.bin")), "application/octet-stream");
        assert_eq!(resolve_content_type(Path::new("a")), "application/octet-stream");
    }
}
