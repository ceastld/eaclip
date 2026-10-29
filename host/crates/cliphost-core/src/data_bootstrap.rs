//! Discover existing C#/WPF data without creating or moving its directories.
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Default)]
pub(crate) struct LegacyDataSources {
    pub environment_override: Option<PathBuf>,
    pub csharp_sidecar: Option<PathBuf>,
    pub documents_root: Option<PathBuf>,
    pub csharp_default: Option<PathBuf>,
    pub documents_from_system: bool,
}

impl LegacyDataSources {
    pub fn from_default_root(default_root: &Path) -> Result<Self, String> {
        let local_app = default_root.parent().ok_or("数据位置配置目录无父目录。")?;
        Ok(Self {
            environment_override: std::env::var("BETTERCLIP_DATA_ROOT")
                .ok()
                .map(|raw| raw.trim().to_string())
                .filter(|raw| !raw.is_empty())
                .map(PathBuf::from),
            csharp_sidecar: Some(
                local_app
                    .join("BetterClip")
                    .join("application-data-root.json"),
            ),
            documents_root: None,
            csharp_default: Some(local_app.join("BetterClip").join("userdata")),
            documents_from_system: true,
        })
    }

    pub fn select_existing(&self) -> Result<Option<PathBuf>, String> {
        if let Some(root) = &self.environment_override {
            let failure = |reason: &str| {
                format!(
                "BETTERCLIP_DATA_ROOT 指定的旧版数据目录不可用：{reason}。请修复路径或清除该环境变量后重试。"
            )
            };
            if !root.is_absolute() {
                return Err(failure("必须是绝对路径"));
            }
            if !has_database(root).map_err(|error| failure(&error))? {
                return Err(failure("没有 userdata.db 文件"));
            }
            // Check readability without reading clipboard contents or touching the database.
            fs::File::open(root.join("userdata.db"))
                .map_err(|error| failure(&error.to_string()))?;
            return Ok(Some(root.clone()));
        }
        if let Some(pointer) = &self.csharp_sidecar {
            match fs::read(pointer) {
                Ok(bytes) => {
                    let value: serde_json::Value = serde_json::from_slice(strip_utf8_bom(&bytes))
                        .map_err(|e| {
                        format!("读取旧版数据位置配置 {} 失败: {e}", pointer.display())
                    })?;
                    let root = data_root_value(&value).ok_or_else(|| {
                        format!("旧版数据位置配置 {} 缺少有效路径。", pointer.display())
                    })?;
                    let root = PathBuf::from(root);
                    if !root.is_absolute() {
                        return Err("旧版数据位置配置必须是绝对路径。".into());
                    }
                    if has_database(&root)? {
                        return Ok(Some(root));
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!(
                        "读取旧版数据位置配置 {} 失败: {error}",
                        pointer.display()
                    ))
                }
            }
        }
        let documents_root = if self.documents_from_system {
            known_documents_path()?.map(|p| p.join("Quicker").join("Ceastld"))
        } else {
            self.documents_root.clone()
        };
        for root in [&documents_root, &self.csharp_default]
            .into_iter()
            .flatten()
        {
            if has_database(root)? {
                return Ok(Some(root.clone()));
            }
        }
        Ok(None)
    }
}

pub(crate) fn strip_utf8_bom(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes)
}

pub(crate) fn data_root_value(value: &serde_json::Value) -> Option<&str> {
    value
        .get("dataRoot")
        .or_else(|| value.get("DataRoot"))
        .or_else(|| value.get("path"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

pub(crate) fn require_existing_database(root: &Path) -> Result<(), String> {
    if !has_database(root)? {
        return Err(format!(
            "旧版数据目录 {} 中没有 userdata.db 文件。",
            root.display()
        ));
    }
    fs::File::open(root.join("userdata.db"))
        .map_err(|e| format!("旧版数据库 {} 不可读: {e}", root.display()))?;
    Ok(())
}

fn has_database(root: &Path) -> Result<bool, String> {
    let database = root.join("userdata.db");
    match fs::metadata(&database) {
        Ok(metadata) => Ok(metadata.is_file()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!(
            "无法检查旧版数据库 {}: {error}",
            database.display()
        )),
    }
}

#[cfg(windows)]
fn known_documents_path() -> Result<Option<PathBuf>, String> {
    use std::os::windows::ffi::OsStringExt;
    use windows::Win32::{
        Foundation::HANDLE,
        System::Com::CoTaskMemFree,
        UI::Shell::{FOLDERID_Documents, SHGetKnownFolderPath, KF_FLAG_DONT_VERIFY},
    };
    unsafe {
        // Resolve Windows' redirected Known Folder, including OneDrive, not USERPROFILE/Documents.
        let path =
            SHGetKnownFolderPath(&FOLDERID_Documents, KF_FLAG_DONT_VERIFY, HANDLE::default())
                .map_err(|e| format!("无法读取 Windows 文档目录: {e}"))?;
        let result = PathBuf::from(std::ffi::OsString::from_wide(path.as_wide()));
        CoTaskMemFree(Some(path.0.cast()));
        Ok(Some(result))
    }
}

#[cfg(not(windows))]
fn known_documents_path() -> Result<Option<PathBuf>, String> {
    Ok(None)
}

#[cfg(test)]
#[path = "data_bootstrap_tests.rs"]
mod tests;
