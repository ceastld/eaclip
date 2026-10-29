//! Current-user DPAPI envelopes for original archives which may contain secrets.
//! These helpers only transform caller-provided bytes; they never open a file,
//! change a source, show UI, or fall back to plaintext on another platform.
//!
//! Format v1: 16-byte magic, LE u32 version, LE u64 plaintext length, LE u64
//! ciphertext length, then the DPAPI blob. The fixed context and first 28 header
//! bytes are DPAPI optional entropy, binding the archive purpose and length.

use std::path::Path;

use serde_json::Value;

const MAGIC: &[u8; 16] = b"EACLIP-V2-DPAPI\0";
const VERSION: u32 = 1;
const HEADER_LEN: usize = 16 + 4 + 8 + 8;
const AUTHENTICATED_HEADER_LEN: usize = 16 + 4 + 8;
const CONTEXT: &[u8] = b"eaclip.storage-v2.protected-original-archive.v1";

/// Maximum original archive size. DPAPI/container overhead has its own small
/// bounded allowance; no input or decoded length is truncated to a Windows DWORD.
pub const MAX_ARCHIVE_BYTES: usize = 512 * 1024 * 1024;
const MAX_CIPHERTEXT_BYTES: usize = MAX_ARCHIVE_BYTES + 1024 * 1024;

pub fn protect(bytes: &[u8]) -> Result<Vec<u8>, String> {
    check_plaintext_length(bytes.len())?;
    #[cfg(windows)]
    {
        let mut header = [0u8; HEADER_LEN];
        header[..MAGIC.len()].copy_from_slice(MAGIC);
        header[16..20].copy_from_slice(&VERSION.to_le_bytes());
        header[20..28].copy_from_slice(&(bytes.len() as u64).to_le_bytes());
        let entropy = entropy(&header);
        let encrypted = windows_dpapi::transform(bytes, &entropy, false)?;
        let ciphertext = encrypted.bytes(MAX_CIPHERTEXT_BYTES)?;
        if ciphertext.is_empty() {
            return Err("DPAPI 返回了空保护包，未保存明文归档。".into());
        }
        header[28..36].copy_from_slice(&(ciphertext.len() as u64).to_le_bytes());
        let capacity = HEADER_LEN
            .checked_add(ciphertext.len())
            .ok_or("保护归档长度溢出。")?;
        let mut result = Vec::new();
        result
            .try_reserve_exact(capacity)
            .map_err(|_| "无法分配保护归档缓冲区。")?;
        result.extend_from_slice(&header);
        result.extend_from_slice(ciphertext);
        Ok(result)
    }
    #[cfg(not(windows))]
    {
        let _ = bytes;
        Err("当前平台尚未实现迁移归档的用户级保护；拒绝输出明文。".into())
    }
}

pub fn unprotect(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let original_length = validate_envelope(bytes)?;
    #[cfg(windows)]
    {
        let entropy = entropy(&bytes[..HEADER_LEN]);
        let decrypted = windows_dpapi::transform(&bytes[HEADER_LEN..], &entropy, true)?;
        let plaintext = decrypted.bytes(MAX_ARCHIVE_BYTES)?;
        if plaintext.len() != original_length {
            return Err("保护归档的原始长度不一致；拒绝返回部分数据。".into());
        }
        let mut result = Vec::new();
        result
            .try_reserve_exact(plaintext.len())
            .map_err(|_| "无法分配解密归档缓冲区。")?;
        result.extend_from_slice(plaintext);
        // The Windows-owned plaintext buffer is wiped and LocalFree'd by Drop.
        Ok(result)
    }
    #[cfg(not(windows))]
    {
        let _ = original_length;
        Err("当前平台尚未实现迁移归档的用户级解密；不会猜测明文格式。".into())
    }
}

fn check_plaintext_length(length: usize) -> Result<(), String> {
    if length > MAX_ARCHIVE_BYTES {
        return Err("原始迁移归档超过 512 MiB 上限；本次操作已停止，未生成归档。".into());
    }
    u32::try_from(length).map_err(|_| "迁移归档长度超出 Windows DWORD 范围。")?;
    Ok(())
}

fn validate_envelope(bytes: &[u8]) -> Result<usize, String> {
    if bytes.len() < HEADER_LEN || bytes.len() > HEADER_LEN + MAX_CIPHERTEXT_BYTES {
        return Err("保护归档长度无效或超过大小限制。".into());
    }
    if &bytes[..16] != MAGIC {
        return Err("归档不是受支持的用户级保护格式；不接受明文回退。".into());
    }
    let version = u32::from_le_bytes(bytes[16..20].try_into().unwrap());
    if version != VERSION {
        return Err("保护归档版本不受支持。".into());
    }
    let original = u64::from_le_bytes(bytes[20..28].try_into().unwrap());
    let original = usize::try_from(original).map_err(|_| "归档原始长度无法在当前平台表示。")?;
    check_plaintext_length(original)?;
    let ciphertext = u64::from_le_bytes(bytes[28..36].try_into().unwrap());
    let ciphertext = usize::try_from(ciphertext).map_err(|_| "保护包长度无法在当前平台表示。")?;
    if ciphertext == 0
        || ciphertext > MAX_CIPHERTEXT_BYTES
        || bytes.len() - HEADER_LEN != ciphertext
    {
        return Err("保护归档被截断、追加或长度不一致。".into());
    }
    Ok(original)
}

#[cfg(windows)]
fn entropy(header: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(CONTEXT.len() + AUTHENTICATED_HEADER_LEN);
    bytes.extend_from_slice(CONTEXT);
    bytes.extend_from_slice(&header[..AUTHENTICATED_HEADER_LEN]);
    bytes
}

#[cfg(windows)]
mod windows_dpapi {
    use windows::{
        core::PCWSTR,
        Win32::{
            Foundation::{LocalFree, HLOCAL},
            Security::Cryptography::{
                CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
            },
        },
    };

    pub(super) struct LocalBlob {
        blob: CRYPT_INTEGER_BLOB,
        plaintext: bool,
    }

    impl LocalBlob {
        pub(super) fn bytes(&self, maximum: usize) -> Result<&[u8], String> {
            let length = self.blob.cbData as usize;
            if length > maximum {
                return Err("DPAPI 输出超过归档大小限制。".into());
            }
            if length == 0 {
                return Ok(&[]);
            }
            if self.blob.pbData.is_null() {
                return Err("DPAPI 返回了无效缓冲区。".into());
            }
            // The successful DPAPI call owns this LocalAlloc buffer for exactly
            // cbData bytes. The borrow cannot outlive the RAII owner below.
            Ok(unsafe { std::slice::from_raw_parts(self.blob.pbData, length) })
        }
    }

    impl Drop for LocalBlob {
        fn drop(&mut self) {
            if self.blob.pbData.is_null() {
                return;
            }
            unsafe {
                if self.plaintext {
                    // Use volatile writes: ordinary memset/write_bytes can be
                    // removed once the allocation is about to be released.
                    for offset in 0..self.blob.cbData as usize {
                        std::ptr::write_volatile(self.blob.pbData.add(offset), 0);
                    }
                }
                let _ = LocalFree(HLOCAL(self.blob.pbData.cast()));
            }
            self.blob.pbData = std::ptr::null_mut();
            self.blob.cbData = 0;
        }
    }

    pub(super) fn transform(
        bytes: &[u8],
        entropy: &[u8],
        decrypt: bool,
    ) -> Result<LocalBlob, String> {
        let input = CRYPT_INTEGER_BLOB {
            cbData: u32::try_from(bytes.len()).map_err(|_| "DPAPI 输入长度超过 DWORD 范围。")?,
            // DPAPI's legacy DATA_BLOB is not const-correct; neither function
            // mutates its input bytes. Empty input is represented by cbData=0.
            pbData: bytes.as_ptr().cast_mut(),
        };
        let additional = CRYPT_INTEGER_BLOB {
            cbData: u32::try_from(entropy.len()).map_err(|_| "DPAPI 上下文长度无效。")?,
            pbData: entropy.as_ptr().cast_mut(),
        };
        let mut output = LocalBlob {
            blob: CRYPT_INTEGER_BLOB::default(),
            plaintext: decrypt,
        };
        let result = unsafe {
            if decrypt {
                CryptUnprotectData(
                    &input,
                    None,
                    Some(&additional as *const CRYPT_INTEGER_BLOB),
                    None,
                    None,
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut output.blob,
                )
            } else {
                // No CRYPTPROTECT_LOCAL_MACHINE flag: protection stays bound to
                // the current user's Windows DPAPI context; no UI is permitted.
                CryptProtectData(
                    &input,
                    PCWSTR::null(),
                    Some(&additional as *const CRYPT_INTEGER_BLOB),
                    None,
                    None,
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut output.blob,
                )
            }
        };
        result.map_err(|error| {
            format!(
                "Windows DPAPI {}失败：{error}",
                if decrypt { "解密" } else { "保护" }
            )
        })?;
        Ok(output)
    }
}

/// A conservative classification, not permission to read a path. The caller
/// must still validate containment, file attributes and absence of links first.
pub fn is_sensitive_path(relative: &Path) -> bool {
    let Some(raw) = relative.to_str() else {
        return true;
    };
    if relative.is_absolute() || raw.starts_with('/') || raw.starts_with('\\') || raw.contains(':')
    {
        return true;
    }
    let normalized = raw.replace('\\', "/").to_ascii_lowercase();
    let components: Vec<_> = normalized
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    if components.iter().any(|part| *part == "..") {
        return true;
    }
    let Some(name) = components.last().copied() else {
        return false;
    };
    let extension = name.rsplit_once('.').map(|(_, extension)| extension);
    if matches!(extension, Some("key" | "pem" | "der" | "pfx" | "p12")) {
        return true;
    }
    if components
        .iter()
        .any(|part| matches!(*part, "secrets" | "credentials"))
    {
        return true;
    }
    if components.first() == Some(&"configs") {
        if components.get(1).is_some_and(|part| {
            matches!(
                *part,
                "lanshare"
                    | "clipboardsync"
                    | "identity"
                    | "pairing"
                    | "accounts"
                    | "account"
                    | "auth"
            )
        }) {
            return true;
        }
        // Root settings remain inspectable individually; these explicitly
        // security-related legacy documents are exceptions to that policy.
        if matches!(
            name,
            "lanshareconfig.json"
                | "lanshare.json"
                | "clipboardsyncconfig.json"
                | "clipboardsync.json"
        ) {
            return true;
        }
    }
    if components.first() == Some(&"resource") && extension == Some("json") {
        return true;
    }
    if components
        .first()
        .is_some_and(|part| matches!(*part, "lanshare" | "clipboardsync"))
    {
        if components
            .iter()
            .any(|part| matches!(*part, "identity" | "pairing" | "auth"))
        {
            return true;
        }
        if matches!(
            name,
            "identity.json"
                | "peers.json"
                | "pairing.json"
                | "credentials.json"
                | "tokens.json"
                | "state.json"
        ) {
            return true;
        }
    }
    matches!(name, "userdata.db" | "source.sqlite3")
}

/// Detect material credentials even in previously unknown complete settings.
/// Iterative traversal avoids recursion on caller-constructed deeply nested JSON.
pub fn json_contains_credentials(value: &Value) -> bool {
    let mut pending = vec![value];
    while let Some(value) = pending.pop() {
        match value {
            Value::Object(object) => {
                for (key, value) in object {
                    let key: String = key
                        .chars()
                        .filter(char::is_ascii_alphanumeric)
                        .flat_map(char::to_lowercase)
                        .collect();
                    let sensitive = matches!(
                        key.as_str(),
                        "token"
                            | "password"
                            | "passwd"
                            | "pwd"
                            | "apikey"
                            | "accesskey"
                            | "accountkey"
                            | "authorization"
                            | "bearer"
                            | "pairingcode"
                            | "pairingkey"
                            | "signingkey"
                            | "encryptionkey"
                            | "keyder"
                            | "privkey"
                    ) || key.ends_with("token")
                        || key.ends_with("tokens")
                        || key.ends_with("tokenhash")
                        || key.contains("password")
                        || key.ends_with("passwd")
                        || key.ends_with("pwd")
                        || key.starts_with("apikey")
                        || key.starts_with("authorization")
                        || key.contains("privatekey")
                        || key.contains("secret")
                        || key.contains("credential");
                    if sensitive && has_material(value) {
                        return true;
                    }
                    pending.push(value);
                }
            }
            Value::Array(values) => pending.extend(values),
            _ => {}
        }
    }
    false
}

fn has_material(value: &Value) -> bool {
    let mut pending = vec![value];
    while let Some(value) = pending.pop() {
        match value {
            Value::Null | Value::Bool(false) => {}
            Value::String(value) if value.trim().is_empty() => {}
            Value::Array(values) => pending.extend(values),
            Value::Object(object) => pending.extend(object.values()),
            _ => return true,
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[cfg(windows)]
    #[test]
    fn protected_archive_roundtrip_preserves_binary_and_original_input() {
        let original = b"synthetic token\0\x01\xff\r\n".to_vec();
        let before = original.clone();
        let protected = protect(&original).unwrap();
        assert!(protected.starts_with(MAGIC));
        assert!(!protected
            .windows(original.len())
            .any(|part| part == original));
        assert_eq!(unprotect(&protected).unwrap(), original);
        assert_eq!(original, before);
    }

    #[cfg(windows)]
    #[test]
    fn protected_archive_accepts_empty_original_without_plaintext_fallback() {
        let protected = protect(b"").unwrap();
        assert!(protected.len() > HEADER_LEN);
        assert_eq!(validate_envelope(&protected).unwrap(), 0);
        assert_eq!(unprotect(&protected).unwrap(), b"");
        assert!(unprotect(b"").is_err());
        assert!(unprotect(b"plain text is not a protected archive").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn protected_archive_rejects_corruption_version_length_and_truncation() {
        let protected = protect(b"synthetic secret material").unwrap();
        for offset in [
            0,
            16,
            20,
            28,
            HEADER_LEN + (protected.len() - HEADER_LEN) / 2,
            protected.len() - 1,
        ] {
            let mut damaged = protected.clone();
            damaged[offset] ^= 0x40;
            assert!(
                unprotect(&damaged).is_err(),
                "accepted corruption at {offset}"
            );
        }
        for length in [0, 15, HEADER_LEN - 1, HEADER_LEN, protected.len() - 1] {
            assert!(
                unprotect(&protected[..length]).is_err(),
                "accepted truncation to {length}"
            );
        }
        let mut appended = protected.clone();
        appended.push(0);
        assert!(unprotect(&appended).is_err());
    }

    #[test]
    fn protected_archive_bounds_are_checked_without_allocating_large_inputs() {
        assert!(check_plaintext_length(MAX_ARCHIVE_BYTES).is_ok());
        assert!(check_plaintext_length(MAX_ARCHIVE_BYTES + 1).is_err());
        assert!(check_plaintext_length(usize::MAX).is_err());
        let mut oversized = vec![0; HEADER_LEN + 1];
        oversized[..16].copy_from_slice(MAGIC);
        oversized[16..20].copy_from_slice(&VERSION.to_le_bytes());
        oversized[20..28].copy_from_slice(&((MAX_ARCHIVE_BYTES as u64) + 1).to_le_bytes());
        oversized[28..36].copy_from_slice(&1u64.to_le_bytes());
        assert!(unprotect(&oversized).is_err());
    }

    #[test]
    fn protected_archive_sensitive_paths_cover_legacy_identity_and_unknown_resource_json() {
        for path in [
            "Configs/LanShare/identity.json",
            "Configs/ClipboardSync/state.json",
            "Configs/LanShare",
            "Configs\\LANSHARE\\peers.json",
            "LanShare/identity.json",
            "LanShare/identity.pfx",
            "LanShare/cert.der",
            "LanShare/key.der",
            "nested/private.KEY",
            "Resource/unknown.json",
            "secrets/device.dpapi",
            "auth/client.PEM",
            "cert.p12",
            "Configs/LanShareConfig.json",
            "userdata.db",
            "../Configs/GlobalConfig.json",
        ] {
            assert!(is_sensitive_path(Path::new(path)), "not protected: {path}");
        }
        for path in [
            "Configs/GlobalConfig.json",
            "Configs/ClipConfig.json",
            "Configs/FavorConfig.json",
            "Image/Temp/picture.png",
            "Resource/icon.svg",
            "LanShare/Inbox/image.jpg",
        ] {
            assert!(
                !is_sensitive_path(Path::new(path)),
                "ordinary content misclassified: {path}"
            );
        }
    }

    #[test]
    fn protected_archive_credential_keys_are_recursive_case_and_separator_tolerant() {
        for value in [
            json!({"TOKEN":"synthetic"}),
            json!({"nested":[{"access_token":"synthetic"}]}),
            json!({"Private-Key":[1,2,3]}),
            json!({"clientSecret":"synthetic"}),
            json!({"authorization":"Bearer synthetic"}),
            json!({"inbound_token_hash":"synthetic"}),
            json!({"unknown":{"credentials":{"data":"synthetic"}}}),
            json!({"Password":123456}),
            json!({"tokens":["synthetic"]}),
            json!({"PasswordHash":"synthetic"}),
            json!({"apiKeyValue":"synthetic"}),
        ] {
            assert!(
                json_contains_credentials(&value),
                "missed synthetic credential: {value}"
            );
        }
        for value in [
            json!({"token":null,"password":"  ","private_key":[],"secret":{}}),
            json!({"UiThemeMode":"Dark","deviceId":"public-id","certificateFingerprint":"public"}),
            json!({"tokenCount":5,"tokenizer":"ordinary","publicKey":"public"}),
            json!([null, {}, [], ""]),
        ] {
            assert!(
                !json_contains_credentials(&value),
                "unexpected credential: {value}"
            );
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn protected_archive_unsupported_platform_never_returns_plaintext() {
        assert!(protect(b"synthetic secret").is_err());
        assert!(unprotect(b"synthetic secret").is_err());
    }
}
