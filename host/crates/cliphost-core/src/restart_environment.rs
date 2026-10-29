//! Carry ClipHost overrides across an Explorer-mediated restart without changing
//! Explorer's environment or forwarding unrelated process environment variables.

use std::collections::BTreeMap;
use std::path::Path;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};

pub(crate) const ARG_PREFIX: &str = "--cliphost-restart-environment=";
const MAX_MARKER_BYTES: usize = 32 * 1024;

fn allowed_key(key: &str) -> bool {
    let key = key.to_ascii_uppercase();
    key.starts_with("BETTERCLIP_TAURI_")
        || key.starts_with("INTELLITOOLS_CLIPHOST_TAURI_")
        || key.starts_with("CLIPHOST_TAURI_")
        || key.starts_with("WEBVIEW2_")
        || key == "CEA_CLIPHOST_ALLOW_DEBUG_ELEVATION"
}

fn capture(values: impl IntoIterator<Item = (String, String)>, cwd: &Path) -> BTreeMap<String, String> {
    values.into_iter().filter(|(key, _)| allowed_key(key)).map(|(key, value)| {
        let key = key.to_ascii_uppercase();
        let value = if key == "BETTERCLIP_TAURI_DATA_ROOT" && !value.is_empty() && Path::new(&value).is_relative() {
            cwd.join(value).to_string_lossy().into_owned()
        } else { value };
        (key, value)
    }).collect()
}

pub(crate) fn append_marker(args: &mut Vec<String>) {
    args.retain(|arg| !arg.starts_with(ARG_PREFIX));
    let cwd = std::env::current_dir().unwrap_or_default();
    let values = capture(std::env::vars_os().map(|(key, value)| {
        (key.to_string_lossy().into_owned(), value.to_string_lossy().into_owned())
    }), &cwd);
    // A string map always serializes. The Windows launch path enforces its
    // command-line limit before starting any replacement process.
    let payload = serde_json::to_vec(&values).expect("serialize restart overrides");
    args.push(format!("{ARG_PREFIX}{}", URL_SAFE_NO_PAD.encode(payload)));
}

fn parse(args: &[String]) -> Result<Option<BTreeMap<String, String>>, String> {
    let mut markers = args.iter().filter_map(|arg| arg.strip_prefix(ARG_PREFIX));
    let Some(marker) = markers.next() else { return Ok(None); };
    if markers.next().is_some() || marker.len() > MAX_MARKER_BYTES {
        return Err("Invalid restart environment marker.".into());
    }
    let decoded = URL_SAFE_NO_PAD.decode(marker).map_err(|_| "Invalid restart environment marker.")?;
    let values: BTreeMap<String, String> = serde_json::from_slice(&decoded)
        .map_err(|_| "Invalid restart environment marker.")?;
    if values.iter().any(|(key, value)| !allowed_key(key) || key.contains(['=', '\0']) || value.contains('\0')) {
        return Err("Invalid restart environment override.".into());
    }
    Ok(Some(values))
}

pub(crate) fn restore(args: Vec<String>) -> Result<Vec<String>, String> {
    if let Some(values) = parse(&args)? {
        // Validate all values before applying any. Explorer may have its own
        // overrides; absence in the original host means clear them in this child.
        for (key, _) in std::env::vars_os().filter(|(key, _)| allowed_key(&key.to_string_lossy())) {
            std::env::remove_var(key);
        }
        for (key, value) in values { std::env::set_var(key, value); }
    }
    Ok(args.into_iter().filter(|arg| !arg.starts_with(ARG_PREFIX)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marker(values: serde_json::Value) -> String {
        format!("{ARG_PREFIX}{}", URL_SAFE_NO_PAD.encode(serde_json::to_vec(&values).unwrap()))
    }

    #[test]
    fn restart_environment_limits_capture_to_app_overrides_and_resolves_relative_data() {
        let values = capture([
            ("betterclip_tauri_data_root".into(), ".tmp/测试 folder".into()),
            ("INTELLITOOLS_CLIPHOST_TAURI_PORT".into(), "29965".into()),
            ("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS".into(), "--fixture".into()),
            ("CLIPHOST_TAURI_HEADLESS".into(), "1".into()),
            ("PATH".into(), "must not be forwarded".into()),
            ("PRIVATE_FIXTURE_TOKEN".into(), "must not be forwarded".into()),
        ], Path::new(r"D:\workspace"));
        assert_eq!(values.len(), 4);
        assert_eq!(values["BETTERCLIP_TAURI_DATA_ROOT"], Path::new(r"D:\workspace").join(".tmp/测试 folder").to_string_lossy());
        let args = vec!["clip show".into(), marker(serde_json::to_value(&values).unwrap())];
        assert_eq!(parse(&args).unwrap(), Some(values));
        assert_eq!(parse(&["clip show".into()]).unwrap(), None);
    }

    #[test]
    fn restart_environment_rejects_invalid_duplicate_and_foreign_overrides() {
        for args in [
            vec![format!("{ARG_PREFIX}invalid!")],
            vec![marker(serde_json::json!({})), marker(serde_json::json!({}))],
            vec![marker(serde_json::json!({"PATH":"foreign"}))],
            vec![marker(serde_json::json!({"CLIPHOST_TAURI_X":"bad\0value"}))],
            vec![marker(serde_json::json!({"CLIPHOST_TAURI_X=Y":"value"}))],
            vec![marker(serde_json::json!({"CLIPHOST_TAURI_X":false}))],
            vec![format!("{ARG_PREFIX}{}", "A".repeat(MAX_MARKER_BYTES + 1))],
        ] { assert!(parse(&args).is_err()); }
    }
}
