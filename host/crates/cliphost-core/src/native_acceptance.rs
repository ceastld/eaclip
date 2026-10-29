//! Explicitly opted-in desktop test build; absent from normal application builds.
use crate::paths::{AppPaths, API_PORT_ENV, DATA_ROOT_ENV};
use std::{path::{Path, PathBuf}, sync::OnceLock};

static NONCE: OnceLock<String> = OnceLock::new();
static TARGET_PID: OnceLock<u32> = OnceLock::new();
static CLIPBOARD_READ_ONLY: OnceLock<bool> = OnceLock::new();
static CONTEXT: OnceLock<AcceptanceContext> = OnceLock::new();

#[derive(Debug)]
struct AcceptanceContext {
    data_root: PathBuf,
    api_port: u16,
}

/// The receiver authorization also belongs to one exact storage/listener pair.
/// A caller cannot initialize a harmless fixture and reuse it for another root.
pub fn verify_context(paths: &AppPaths, api_port: u16) -> Result<(), String> {
    let context = CONTEXT.get().ok_or("Native acceptance was not initialized")?;
    validate_core_copy_tree(paths)?;
    let data_root = paths.data_root.canonicalize().map_err(|e| e.to_string())?;
    validate_context(Some(context), &data_root, api_port)
}

/// Core-copy GPUI callers run this before initialize/ensure. Host entry repeats
/// the metadata check at its own boundary; no result is cached across mutation.
/// It does not open files, resolve links, hydrate placeholders or change state.
pub fn validate_core_copy_tree(paths: &AppPaths) -> Result<(), String> {
    crate::host::validate_isolated_core_copy_tree(paths)
}

fn validate_context(context: Option<&AcceptanceContext>, data_root: &Path, api_port: u16) -> Result<(), String> {
    let context = context.ok_or("Native acceptance was not initialized")?;
    if context.data_root != data_root || context.api_port != api_port {
        return Err("Native acceptance storage/listener context does not match initialization".into());
    }
    Ok(())
}

/// Compare only path spelling here: an inherited environment override must not
/// make us inspect a production root, network share, placeholder or alias first.
fn validate_data_root_override(data_root: &Path, data_override: &Path) -> Result<(), String> {
    fn lexical_key(path: &Path) -> Result<String, String> {
        use std::path::Component;
        let raw = path.to_str().ok_or("Native acceptance root has invalid text")?;
        if raw.contains('\0') || raw.split(['/', '\\']).any(|part| part == "." || part == "..") {
            return Err("Native acceptance root must not contain dot or parent traversal segments".into());
        }
        for part in raw.split(['/', '\\']) {
            let lower = part.to_ascii_lowercase();
            if lower.starts_with("onedrive") || lower == "dropbox" || lower.starts_with("icloud") {
                return Err("Native acceptance root must not name a cloud directory".into());
            }
        }
        #[cfg(windows)]
        let path = PathBuf::from(raw.replace('/', "\\"));
        #[cfg(not(windows))]
        let path = path.to_path_buf();
        if !path.is_absolute() {
            return Err("Native acceptance root must be an absolute local path".into());
        }
        let mut normalized = PathBuf::new();
        for component in path.components() {
            match component {
                #[cfg(windows)]
                Component::Prefix(prefix) => {
                    use std::path::Prefix;
                    match prefix.kind() {
                        Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => normalized.push(format!("{}:", drive as char)),
                        _ => return Err("Native acceptance root must use a local disk".into()),
                    }
                }
                Component::Normal(name) => {
                    #[cfg(windows)]
                    {
                        let text = name.to_str().ok_or("Native acceptance root has invalid text")?;
                        if text.contains(':') || text.ends_with('.') || text.ends_with(' ') {
                            return Err("Native acceptance root has an ambiguous name or alternate stream".into());
                        }
                    }
                    normalized.push(name);
                }
                _ => normalized.push(component.as_os_str()),
            }
        }
        #[cfg(windows)]
        return Ok(normalized.to_string_lossy().to_ascii_lowercase());
        #[cfg(not(windows))]
        Ok(normalized.to_string_lossy().into_owned())
    }
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(3)
        .ok_or("Native acceptance workspace boundary is unavailable")?;
    let scratch = lexical_key(&workspace.join(".tmp"))?;
    let expected = lexical_key(data_root)?;
    let actual = lexical_key(data_override)?;
    let prefix = format!("{}{}", scratch.trim_end_matches(std::path::MAIN_SEPARATOR), std::path::MAIN_SEPARATOR);
    if expected == scratch || !expected.starts_with(&prefix) {
        return Err("Native acceptance root must be below the fixed workspace .tmp".into());
    }
    if actual != expected {
        return Err("Native acceptance environment root does not match the explicit CLI root".into());
    }
    Ok(())
}

/// Validate isolation before the application opens storage or initializes hooks.
pub fn initialize(paths: &AppPaths) -> Result<String, String> {
    let nonce = std::env::var("CLIPHOST_NATIVE_ACCEPTANCE_NONCE")
        .map_err(|_| "Native acceptance builds require an explicit fixture nonce")?;
    let data_override = std::env::var(DATA_ROOT_ENV)
        .map_err(|_| "Native acceptance builds require an explicit data root")?;
    validate_data_root_override(&paths.data_root, Path::new(&data_override))?;
    let port = std::env::var(API_PORT_ENV).ok().and_then(|v| v.parse::<u16>().ok())
        .ok_or("Native acceptance builds require an explicit loopback API port")?;
    validate_nonce_and_port(&nonce, port)?;
    let clipboard_read_only = match std::env::var("CLIPHOST_NATIVE_ACCEPTANCE_READ_ONLY_CLIPBOARD").as_deref() {
        Err(std::env::VarError::NotPresent) | Ok("0") => false,
        Ok("1") => true,
        _ => return Err("Native acceptance clipboard read-only flag must be 0 or 1".into()),
    };
    let target_pid = std::env::var("CLIPHOST_NATIVE_ACCEPTANCE_TARGET_PID")
        .ok().and_then(|value| value.parse::<u32>().ok())
        .filter(|pid| *pid != 0 && *pid != std::process::id())
        .ok_or("Native acceptance builds require an explicit receiver process ID")?;
    // clip/host/crates/cliphost-core: fixtures belong to clip/.tmp.
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(3).unwrap();
    let scratch = workspace.join(".tmp").canonicalize().map_err(|e| e.to_string())?;
    let data = paths.data_root.canonicalize().map_err(|e| e.to_string())?;
    if data == scratch || !data.starts_with(&scratch)
        || Path::new(&data_override).canonicalize().map_err(|e| e.to_string())? != data {
        return Err("Native acceptance data must be an existing directory inside workspace .tmp".into());
    }
    TARGET_PID.set(target_pid).map_err(|_| "Native acceptance already initialized")?;
    NONCE.set(nonce.clone()).map_err(|_| "Native acceptance already initialized")?;
    CLIPBOARD_READ_ONLY.set(clipboard_read_only).map_err(|_| "Native acceptance already initialized")?;
    CONTEXT.set(AcceptanceContext { data_root: data, api_port: port })
        .map_err(|_| "Native acceptance already initialized")?;
    std::thread::Builder::new().name("native-lock-diagnostics".into()).spawn(|| loop {
        std::thread::sleep(std::time::Duration::from_secs(3));
        for cycle in parking_lot::deadlock::check_deadlock() {
            for thread in cycle {
                tracing::error!(thread_id = ?thread.thread_id(), backtrace = ?thread.backtrace(),
                    "Native acceptance detected a Rust lock deadlock");
            }
        }
    }).map_err(|error| error.to_string())?;
    // A fixture must not claim the optional Quicker connector's named pipes.
    std::env::set_var("CLIPHOST_TAURI_VERB_PIPE", "0");
    Ok(format!("net.getquicker.cliphost.acceptance.{nonce}"))
}

fn validate_nonce_and_port(nonce: &str, port: u16) -> Result<(), String> {
    if nonce.len() != 32 || !nonce.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err("Native acceptance nonce must contain 32 hexadecimal characters".into());
    }
    if matches!(port, 0 | 28865 | 28965 | 18765 | 18776 | 28766 | 29766) {
        return Err("Native acceptance must use a separate API port".into());
    }
    Ok(())
}

#[cfg(not(test))]
pub(crate) fn clipboard_nonce() -> Result<&'static str, String> {
    NONCE.get().map(String::as_str).ok_or_else(|| "Native acceptance was not initialized".into())
}

/// Only this fixture's explicitly marked capture may enter its isolated history.
#[cfg(not(test))]
pub(crate) fn capture_allowed(marker: Option<&[u8]>) -> bool {
    capture_marker_matches(NONCE.get().map(String::as_str), marker)
}

fn capture_marker_matches(expected: Option<&str>, marker: Option<&[u8]>) -> bool {
    let (Some(expected), Some(marker)) = (expected, marker) else { return false; };
    !expected.is_empty() && marker.split(|byte| *byte == 0).next() == Some(expected.as_bytes())
}

#[cfg(not(test))]
pub(crate) fn verify_target_process(actual_pid: u32) -> Result<(), String> {
    validate_target_process(TARGET_PID.get().copied().unwrap_or(0), actual_pid)
}

fn validate_target_process(expected_pid: u32, actual_pid: u32) -> Result<(), String> {
    if expected_pid == 0 || actual_pid != expected_pid {
        tracing::debug!(expected_pid, actual_pid, "Rejected native acceptance target");
        return Err("Native acceptance stopped input to a non-receiver process".into());
    }
    Ok(())
}

/// Cancel-only shortcut probes may exercise real hooks without risking a paste.
#[cfg(not(test))]
pub(crate) fn verify_clipboard_write() -> Result<(), String> {
    validate_clipboard_write(CLIPBOARD_READ_ONLY.get().copied())
}

fn validate_clipboard_write(read_only: Option<bool>) -> Result<(), String> {
    match read_only {
        Some(false) => Ok(()),
        Some(true) => Err("Native acceptance clipboard is read-only".into()),
        None => Err("Native acceptance was not initialized".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn environment_root_comparison_is_pure_and_refuses_external_or_alias_spellings() {
        // No directories are created: success itself verifies that this gate
        // does not require metadata/canonicalization of either input.
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(3).unwrap();
        let scratch = workspace.join(".tmp");
        let expected = scratch.join("native-env-pure-fixture").join("candidate");
        assert!(validate_data_root_override(&expected, &expected).is_ok());
        let forbidden = [
            workspace.join("production-never-probed"),
            scratch.join("OneDrive-never-probed").join("candidate"),
            PathBuf::from(r"\\server-never-probed\share\candidate"),
            PathBuf::from(r"\\?\UNC\server-never-probed\share\candidate"),
            scratch.join("native-env-pure-fixture").join("junction-never-probed").join("candidate"),
            PathBuf::from(format!("{}/child/../candidate", expected.parent().unwrap().display())),
            PathBuf::from(format!(r"{}\child\..\candidate", expected.parent().unwrap().display())),
            PathBuf::from(format!(r"{}/child\../candidate", expected.parent().unwrap().display())),
            PathBuf::from(format!("{}/./candidate", expected.parent().unwrap().display())),
            PathBuf::from("relative-candidate"),
        ];
        for attempted in forbidden {
            assert!(validate_data_root_override(&expected, &attempted).is_err());
        }
        let outside = workspace.join("production-never-probed");
        assert!(validate_data_root_override(&outside, &outside).is_err());
        assert!(validate_data_root_override(&scratch, &scratch).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn environment_root_comparison_accepts_only_equivalent_local_disk_spellings() {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(3).unwrap();
        let expected = workspace.join(".tmp").join("native-env-pure-fixture").join("candidate");
        let ordinary = expected.to_string_lossy().replace('/', "\\");
        let ordinary = ordinary.strip_prefix(r"\\?\").unwrap_or(&ordinary);
        assert!(validate_data_root_override(&expected, Path::new(&ordinary.to_ascii_uppercase())).is_ok());
        assert!(validate_data_root_override(&expected, Path::new(&ordinary.replace('\\', "/"))).is_ok());
        assert!(validate_data_root_override(&expected, Path::new(&format!(r"\\?\{ordinary}"))).is_ok());
        assert!(validate_data_root_override(&expected, Path::new(&format!(r"\\?\{ordinary}/../candidate"))).is_err());
    }

    #[test]
    fn receiver_context_requires_initialization_and_exact_storage_listener_pair() {
        let root = PathBuf::from("acceptance-fixture");
        let context = AcceptanceContext { data_root: root.clone(), api_port: 29065 };
        assert!(validate_context(None, &root, 29065).unwrap_err().contains("not initialized"));
        assert!(validate_context(Some(&context), &root, 29065).is_ok());
        assert!(validate_context(Some(&context), Path::new("another-fixture"), 29065).unwrap_err().contains("does not match"));
        assert!(validate_context(Some(&context), &root, 29066).unwrap_err().contains("does not match"));
    }
    #[test]
    fn cancel_only_fixture_rejects_clipboard_writes() {
        assert!(validate_clipboard_write(Some(false)).is_ok());
        assert!(validate_clipboard_write(Some(true)).unwrap_err().contains("read-only"));
        assert!(validate_clipboard_write(None).is_err());
    }
    #[test]
    fn capture_requires_its_own_explicit_marker() {
        let nonce = "0123456789abcdef0123456789abcdef";
        assert!(capture_marker_matches(Some(nonce), Some(nonce.as_bytes())));
        assert!(capture_marker_matches(Some(nonce), Some(format!("{nonce}\0").as_bytes())));
        assert!(!capture_marker_matches(None, Some(nonce.as_bytes())));
        assert!(!capture_marker_matches(Some(nonce), None));
        assert!(!capture_marker_matches(Some(nonce), Some(b"another-fixture")));
        assert!(!capture_marker_matches(Some(nonce), Some(b"")));
        assert!(!capture_marker_matches(Some(""), Some(b"")));
    }

    #[test]
    fn input_requires_the_explicit_receiver_process() {
        assert!(validate_target_process(41, 41).is_ok());
        for (expected, actual) in [(0, 0), (0, 41), (41, 0), (41, 42)] {
            assert!(validate_target_process(expected, actual).is_err());
        }
    }

    #[test]
    fn rejects_missing_identity_and_reserved_ports() {
        let nonce = "0123456789abcdef0123456789abcdef";
        assert!(validate_nonce_and_port(nonce, 32123).is_ok());
        for port in [0, 28865, 28965, 18765, 18776, 28766, 29766] {
            assert!(validate_nonce_and_port(nonce, port).is_err());
        }
        for bad in ["", "../fixture", "0123456789abcdef0123456789abcdeg"] {
            assert!(validate_nonce_and_port(bad, 32123).is_err());
        }
    }
}
