//! Dev-only local settings. Credentials and the recovery key never enter ordinary settings sync.
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct DevConfig {
    pub profile_id: String,
    pub endpoint: String,
    pub username: String,
    pub password: String,
    pub recovery_key: String,
    pub enabled: bool,
    pub sync_settings: bool,
    pub interval_seconds: u64,
}

pub(super) fn load(path: &Path) -> Result<Option<DevConfig>, String> {
    match fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("无法读取 WebDAV 本机配置。".into()),
        Ok(m) if m.len() > 256 * 1024 => return Err("WebDAV 本机配置过大。".into()),
        _ => {}
    }
    let envelope: serde_json::Value =
        serde_json::from_slice(&fs::read(path).map_err(|_| "无法读取 WebDAV 本机配置。")?)
            .map_err(|_| "WebDAV 本机配置损坏。")?;
    if envelope["version"] != 1 {
        return Err("不支持的 WebDAV 本机配置版本。".into());
    }
    let encrypted = base64::engine::general_purpose::STANDARD
        .decode(
            envelope["protected"]
                .as_str()
                .ok_or("WebDAV 本机配置损坏。")?,
        )
        .map_err(|_| "WebDAV 本机配置损坏。")?;
    let mut plain = crate::storage_v2::protected_archive::unprotect(&encrypted)
        .map_err(|_| "无法为当前 Windows 用户解密 WebDAV 配置。")?;
    let result = serde_json::from_slice(&plain).map_err(|_| "WebDAV 本机配置损坏。".to_string());
    plain.fill(0);
    result.map(Some)
}

pub(super) fn save(path: &Path, config: &DevConfig) -> Result<(), String> {
    let mut plain = serde_json::to_vec(config).map_err(|_| "无法保存 WebDAV 本机配置。")?;
    let result = crate::storage_v2::protected_archive::protect(&plain);
    plain.fill(0);
    let encrypted = result.map_err(|_| "无法为当前 Windows 用户保护 WebDAV 配置。")?;
    crate::config_file::save(path, &serde_json::json!({"version":1,"protected":base64::engine::general_purpose::STANDARD.encode(encrypted)}))
        .map_err(|_| "无法原子保存 WebDAV 本机配置。".into())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    #[test]
    fn webdav_credentials_are_protected_and_corruption_is_not_an_empty_profile() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.json");
        let config = DevConfig {
            profile_id: uuid::Uuid::new_v4().simple().to_string(),
            endpoint: "https://example.invalid/dav/clip/".into(),
            username: "test-user".into(),
            password: "synthetic-password".into(),
            recovery_key: "synthetic-key".into(),
            enabled: false,
            sync_settings: false,
            interval_seconds: 120,
        };
        save(&path, &config).unwrap();
        let stored = fs::read_to_string(&path).unwrap();
        assert!(!stored.contains("synthetic-password"));
        assert!(!stored.contains("synthetic-key"));
        assert_eq!(load(&path).unwrap().unwrap().password, config.password);
        fs::write(&path, b"{}").unwrap();
        assert!(load(&path).is_err());
    }
}
