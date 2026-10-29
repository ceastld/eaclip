//! C# `ClipboardAppService.TryNormalizeLocalShellPath` / `TryNormalizeShellUri`.

use std::path::Path;

pub fn normalize_local_shell_path(raw: &str) -> Result<String, String> {
    let mut p = raw.trim().to_string();
    if p.is_empty() {
        return Err("路径不能为空。".into());
    }
    if p.to_ascii_lowercase().starts_with("file:") {
        p = crate::explorer::file_url_to_local_path(&p)
            .ok_or_else(|| "file: 链接格式无效或不是本地文件 URL。".to_string())?;
    }
    if p.starts_with(r"\\.\") || p.to_ascii_uppercase().starts_with(r"\\?\GLOBALROOT") {
        return Err("不允许打开此类系统设备路径。".into());
    }
    if p.contains('\0') {
        return Err("路径无效：包含空字符。".into());
    }
    let full = std::path::absolute(&p).map_err(|e| format!("路径无效：{e}"))?;
    if !full.is_absolute() {
        return Err(r"需要绝对路径（例如 D:\folder\file.txt）。".into());
    }
    Ok(full.to_string_lossy().into_owned())
}

pub fn normalize_shell_uri(raw: &str) -> Result<String, String> {
    let value = raw.trim();
    if value.is_empty() {
        return Err("URI 不能为空。".into());
    }
    let Some((scheme, rest)) = value.split_once(':') else {
        return Err("需要绝对 URI（例如 mailto:user@example.com）。".into());
    };
    let scheme = scheme.to_ascii_lowercase();
    match scheme.as_str() {
        "mailto" => {
            if rest.is_empty() {
                return Err("需要绝对 URI（例如 mailto:user@example.com）。".into());
            }
            Ok(format!("mailto:{rest}"))
        }
        "http" | "https" => {
            if !rest.starts_with("//") {
                return Err("需要绝对 URI（例如 mailto:user@example.com）。".into());
            }
            Ok(value.to_string())
        }
        _ => Err("仅支持 mailto、http、https 协议。".into()),
    }
}

pub fn path_exists_dto(raw: &str) -> serde_json::Value {
    match normalize_local_shell_path(raw) {
        Ok(full) => serde_json::json!({
            "ok": true,
            "exists": Path::new(&full).exists(),
        }),
        Err(_) => serde_json::json!({ "ok": false, "exists": false }),
    }
}

/// C# `ClipboardAppService.GetLocalImageContentPath` (50 MiB, WindowBackground extensions).
pub const LOCAL_IMAGE_PREVIEW_MAX_BYTES: u64 = 50 * 1024 * 1024;

pub fn local_image_content_path(raw: &str) -> Option<String> {
    let full = normalize_local_shell_path(raw).ok()?;
    let path = Path::new(&full);
    if !path.is_file() {
        return None;
    }
    let ext = path.extension()?.to_str()?;
    if !crate::window_background::is_supported_extension(ext) {
        return None;
    }
    let size = path.metadata().ok()?.len();
    if size == 0 || size > LOCAL_IMAGE_PREVIEW_MAX_BYTES {
        return None;
    }
    Some(full)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_device_and_empty_paths() {
        assert!(normalize_local_shell_path(r"\\.\pipe\foo").unwrap_err().contains("系统设备"));
        assert!(normalize_local_shell_path(r"\\?\GLOBALROOT\Device\Harddisk0").is_err());
        assert!(normalize_local_shell_path("").is_err());
        let dto = path_exists_dto(r"\\.\C:");
        assert_eq!(dto["ok"], false);
        assert_eq!(dto["exists"], false);
        assert!(local_image_content_path(r"\\.\C:").is_none());
        assert!(local_image_content_path("").is_none());
    }

    #[test]
    fn file_url_becomes_windows_path() {
        let path = normalize_local_shell_path("file:///C:/Windows").unwrap();
        assert!(path.replace('/', "\\").to_ascii_lowercase().contains("c:\\windows"));
    }

    #[test]
    fn file_uri_invalid_input_is_rejected_instead_of_becoming_a_working_directory_path() {
        for value in ["file:", "file:relative", "file://user@server/share", "file:///C:/a%00b", "file://server", "file://server/"] {
            assert!(normalize_local_shell_path(value).is_err(), "accepted {value}");
        }
        assert!(normalize_local_shell_path("C:\\a\0b").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn file_uri_share_remains_unc_without_accessing_the_network() {
        assert_eq!(normalize_local_shell_path("file://server/share/folder").unwrap(), r"\\server\share\folder");
    }

    #[test]
    fn file_uri_image_and_existence_use_the_same_decoded_local_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("%中文 +#.png");
        std::fs::write(&path, b"fixture").unwrap();
        let mut uri = url::Url::from_file_path(&path).unwrap();
        uri.set_query(Some("ignored=yes"));
        uri.set_fragment(Some("part"));
        assert_eq!(normalize_local_shell_path(uri.as_str()).unwrap(), path.to_str().unwrap());
        assert_eq!(path_exists_dto(uri.as_str()), serde_json::json!({"ok":true, "exists":true}));
        assert_eq!(local_image_content_path(uri.as_str()).as_deref(), path.to_str());
    }

    #[test]
    fn uri_allows_http_mailto_rejects_file() {
        assert!(normalize_shell_uri("https://example.com/a").is_ok());
        assert!(normalize_shell_uri("mailto:user@example.com").is_ok());
        assert!(normalize_shell_uri("file:///C:/Windows").unwrap_err().contains("mailto"));
        assert!(normalize_shell_uri("javascript:alert(1)").is_err());
        assert!(normalize_shell_uri("").is_err());
        assert!(normalize_shell_uri("http:not-absolute").is_err());
    }
}
