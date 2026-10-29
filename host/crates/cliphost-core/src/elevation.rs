//! C# `StartupElevationPolicy` + `ClipHostElevationBootstrap`.
//!
//! Scheduled task name is **Tauri-specific** so coexistence never overwrites
//! C# `IntelliTools.ClipHost.AdminElevation`.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use crate::process_restart::{
    command_line_has_demote_attempted, quote_for_process_argument,
    try_launch_current_executable_unelevated,
};

pub const ALLOW_DEBUG_COLD_START_ELEVATION_ENV: &str = "CEA_CLIPHOST_ALLOW_DEBUG_ELEVATION";

/// Isolated from C# `IntelliTools.ClipHost.AdminElevation`.
pub const ELEVATION_SCHEDULED_TASK_NAME: &str = "IntelliTools.ClipHost.Tauri.AdminElevation";

/// C# task name — must never be used by this process.
pub const CSHARP_ELEVATION_SCHEDULED_TASK_NAME: &str = "IntelliTools.ClipHost.AdminElevation";

pub fn is_truthy_env_switch(value: Option<&str>) -> bool {
    matches!(
        value.map(str::trim).unwrap_or(""),
        "1" | "true" | "True" | "TRUE" | "yes" | "Yes" | "YES" | "on" | "On" | "ON"
    )
}

/// Release honors persisted admin; debug needs `CEA_CLIPHOST_ALLOW_DEBUG_ELEVATION`.
pub fn effective_run_as_administrator(persisted: bool) -> bool {
    if !persisted {
        return false;
    }
    if cfg!(debug_assertions) {
        return is_truthy_env_switch(std::env::var(ALLOW_DEBUG_COLD_START_ELEVATION_ENV).ok().as_deref());
    }
    true
}

pub fn cold_start_elevation_suppressed(persisted: bool, effective: bool) -> bool {
    persisted && !effective
}

pub fn is_current_process_elevated() -> bool {
    #[cfg(windows)]
    {
        windows_token_elevated()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElevationBootstrapKind {
    ContinueUnchanged,
    DisableAdminAndMaybeDemote,
    RegisterTask,
    ElevateNewProcess,
}

/// Pure C# `EnsureElevatedIfConfigured` branch (no process spawn).
pub fn classify_elevation_bootstrap(
    run_as_administrator_effective: bool,
    process_elevated: bool,
    headless: bool,
) -> ElevationBootstrapKind {
    if headless {
        return ElevationBootstrapKind::ContinueUnchanged;
    }
    if !run_as_administrator_effective {
        return ElevationBootstrapKind::DisableAdminAndMaybeDemote;
    }
    if process_elevated {
        return ElevationBootstrapKind::RegisterTask;
    }
    ElevationBootstrapKind::ElevateNewProcess
}

fn can_reuse_elevation_scheduled_task(arguments: impl IntoIterator<Item = impl AsRef<str>>) -> bool {
    // The existing on-demand task only launches the executable. A background
    // startup must use the UAC path below, which preserves its launch arguments.
    !crate::startup::is_background_launch(arguments)
}

/// C# `TryReadRunAsAdministrator`: `RunAsAdministrator` then `runAsAdministrator`; missing/null → None.
pub fn read_persisted_run_as_administrator(path: &Path) -> Option<bool> {
    let text = std::fs::read_to_string(path).ok()?;
    let json: Value = serde_json::from_str(&text).ok()?;
    let token = json
        .get("RunAsAdministrator")
        .or_else(|| json.get("runAsAdministrator"))?;
    if token.is_null() {
        return None;
    }
    token.as_bool()
}

/// C# `ClipHostElevationBootstrap.EnsureElevatedIfConfigured`. Headless tests never spawn UAC.
pub fn ensure_elevated_if_configured(global_config_path: &Path) {
    if crate::clipboard::is_headless() {
        return;
    }
    #[cfg(not(windows))]
    {
        let _ = global_config_path;
        return;
    }
    #[cfg(windows)]
    {
        let persisted = read_persisted_run_as_administrator(global_config_path).unwrap_or(false);
        let effective = effective_run_as_administrator(persisted);
        let elevated = is_current_process_elevated();
        let kind = classify_elevation_bootstrap(effective, elevated, false);
        execute_elevation_bootstrap(kind, elevated, command_line_has_demote_attempted());
    }
}

#[cfg(windows)]
fn execute_elevation_bootstrap(kind: ElevationBootstrapKind, process_elevated: bool, demote_attempted: bool) {
    match kind {
        ElevationBootstrapKind::ContinueUnchanged => {}
        ElevationBootstrapKind::DisableAdminAndMaybeDemote => {
            delete_tauri_elevation_scheduled_task();
            if process_elevated && !demote_attempted && try_launch_current_executable_unelevated() {
                std::process::exit(0);
            }
        }
        ElevationBootstrapKind::RegisterTask => {
            if let Ok(exe) = std::env::current_exe() {
                create_or_update_elevation_scheduled_task(&exe);
            }
        }
        ElevationBootstrapKind::ElevateNewProcess => {
            let Ok(exe) = std::env::current_exe() else {
                return;
            };
            if exe.as_os_str().is_empty() {
                return;
            }
            if can_reuse_elevation_scheduled_task(std::env::args().skip(1))
                && try_run_via_scheduled_task(&exe)
            {
                std::process::exit(0);
            }
            elevate_with_uac_and_exit(&exe);
        }
    }
}

/// C# `ClipHostRunAsAdministratorRestartUi` prompt copy.
pub const RUN_AS_ADMINISTRATOR_RESTART_PROMPT: &str =
    "「以管理员身份运行」设置已更改。是否立即重启以使设置生效？";

/// C# `preferMediumIntegrityReplacement = !newPersisted && elevated`.
pub fn prefer_medium_integrity_replacement(new_persisted: bool, process_elevated: bool) -> bool {
    !new_persisted && process_elevated
}

/// C# `ClipHostRunAsAdministratorRestartUi.TryPromptAfterPersistedChange`.
/// JSON is already saved by the caller. Headless skips the dialog (tests / CI).
/// Return the accepted restart mode; the host owns persistence and native shutdown.
pub fn try_prompt_after_persisted_run_as_administrator(previous: bool, new: bool, owner_hwnd: isize) -> Option<bool> {
    if previous == new {
        return None;
    }
    if crate::clipboard::is_headless() {
        return None;
    }
    if !new {
        delete_tauri_elevation_scheduled_task();
    }
    if !crate::clipboard::native_dialogs_enabled() {
        return None;
    }
    let prefer_medium = prefer_medium_integrity_replacement(new, is_current_process_elevated());
    if !prompt_restart_yes_no(owner_hwnd) {
        return None;
    }
    Some(prefer_medium)
}

fn delete_tauri_elevation_scheduled_task() {
    #[cfg(windows)]
    {
        let _ = schtasks(&["/Delete", "/TN", ELEVATION_SCHEDULED_TASK_NAME, "/F"]);
    }
}

fn prompt_restart_yes_no(owner_hwnd: isize) -> bool {
    #[cfg(windows)]
    {
        use windows::core::{w, HSTRING, PCWSTR};
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, IDYES, MB_ICONQUESTION, MB_YESNO};

        let text = HSTRING::from(RUN_AS_ADMINISTRATOR_RESTART_PROMPT);
        let owner = if owner_hwnd == 0 {
            HWND::default()
        } else {
            HWND(owner_hwnd as *mut _)
        };
        let rc = unsafe {
            MessageBoxW(
                owner,
                PCWSTR(text.as_ptr()),
                w!("BetterClip"),
                MB_YESNO | MB_ICONQUESTION,
            )
        };
        rc == IDYES
    }
    #[cfg(not(windows))]
    {
        let _ = owner_hwnd;
        false
    }
}

#[cfg(windows)]
fn create_or_update_elevation_scheduled_task(exe: &Path) {
    let _ = schtasks(&["/Delete", "/TN", ELEVATION_SCHEDULED_TASK_NAME, "/F"]);
    let tr = exe.to_string_lossy().into_owned();
    let _ = schtasks(&[
        "/Create",
        "/TN",
        ELEVATION_SCHEDULED_TASK_NAME,
        "/TR",
        &tr,
        "/SC",
        "ONCE",
        "/ST",
        "00:00",
        "/RL",
        "HIGHEST",
        "/F",
    ]);
}

#[cfg(windows)]
fn try_run_via_scheduled_task(exe: &Path) -> bool {
    if !scheduled_task_exists() || !scheduled_task_executable_matches(exe) {
        return false;
    }
    schtasks(&["/Run", "/TN", ELEVATION_SCHEDULED_TASK_NAME])
        .is_some_and(|(code, _)| code == 0)
}

#[cfg(windows)]
fn scheduled_task_exists() -> bool {
    schtasks(&["/Query", "/TN", ELEVATION_SCHEDULED_TASK_NAME]).is_some_and(|(code, _)| code == 0)
}

#[cfg(windows)]
fn scheduled_task_executable_matches(exe: &Path) -> bool {
    let Some((_, stdout)) = schtasks(&[
        "/Query",
        "/TN",
        ELEVATION_SCHEDULED_TASK_NAME,
        "/FO",
        "LIST",
        "/V",
    ]) else {
        return false;
    };
    let needle = exe_full_path_lower(exe);
    stdout.to_lowercase().contains(&needle)
}

fn exe_full_path_lower(exe: &Path) -> String {
    let full = if exe.is_absolute() {
        exe.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(exe))
            .unwrap_or_else(|_| exe.to_path_buf())
    };
    strip_extended_prefix(&full.to_string_lossy()).to_lowercase()
}

fn strip_extended_prefix(raw: &str) -> String {
    raw.strip_prefix(r"\\?\")
        .or_else(|| raw.strip_prefix("//?/"))
        .unwrap_or(raw)
        .to_string()
}

#[cfg(windows)]
fn schtasks(args: &[&str]) -> Option<(i32, String)> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let output = Command::new("schtasks")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    Some((
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    ))
}

#[cfg(windows)]
fn elevate_with_uac_and_exit(exe: &Path) {
    use windows::core::{w, HSTRING, PCWSTR};
    use windows::Win32::Foundation::{GetLastError, HWND};
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONWARNING, MB_OK, SW_SHOWNORMAL};

    let args = build_arguments_for_relaunch();
    let dir = exe
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows\System32"));
    let file = HSTRING::from(exe.to_string_lossy().as_ref());
    let params = HSTRING::from(args.as_str());
    let dir_h = HSTRING::from(dir.to_string_lossy().as_ref());
    unsafe {
        let rc = ShellExecuteW(
            HWND::default(),
            w!("runas"),
            PCWSTR(file.as_ptr()),
            PCWSTR(params.as_ptr()),
            PCWSTR(dir_h.as_ptr()),
            SW_SHOWNORMAL,
        );
        if (rc.0 as isize) > 32 {
            std::process::exit(0);
        }
        let err = GetLastError();
        // C# `Win32Exception.NativeErrorCode == 1223` (UAC cancelled): exit 1, no MessageBox.
        if err.0 == 1223 {
            std::process::exit(1);
        }
        let text = HSTRING::from(format!("无法以管理员身份启动：{}", err.0));
        let _ = MessageBoxW(
            HWND::default(),
            PCWSTR(text.as_ptr()),
            w!("ClipHost"),
            MB_OK | MB_ICONWARNING,
        );
        std::process::exit(1);
    }
}

fn build_arguments_for_relaunch() -> String {
    std::env::args()
        .skip(1)
        .map(|arg| quote_for_process_argument(&arg))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(windows)]
fn windows_token_elevated() -> bool {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut returned = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some((&mut elevation as *mut TOKEN_ELEVATION).cast()),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        );
        let _ = CloseHandle(token);
        ok.is_ok() && elevation.TokenIsElevated != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn debug_effective_admin_requires_env() {
        let previous = std::env::var(ALLOW_DEBUG_COLD_START_ELEVATION_ENV).ok();
        std::env::remove_var(ALLOW_DEBUG_COLD_START_ELEVATION_ENV);
        assert!(!effective_run_as_administrator(false));
        if cfg!(debug_assertions) {
            assert!(!effective_run_as_administrator(true));
            assert!(cold_start_elevation_suppressed(true, false));
            std::env::set_var(ALLOW_DEBUG_COLD_START_ELEVATION_ENV, "1");
            assert!(effective_run_as_administrator(true));
        } else {
            assert!(effective_run_as_administrator(true));
        }
        match previous {
            Some(value) => std::env::set_var(ALLOW_DEBUG_COLD_START_ELEVATION_ENV, value),
            None => std::env::remove_var(ALLOW_DEBUG_COLD_START_ELEVATION_ENV),
        }
    }

    #[test]
    fn scheduled_task_name_is_isolated_from_csharp() {
        assert_eq!(
            ELEVATION_SCHEDULED_TASK_NAME,
            "IntelliTools.ClipHost.Tauri.AdminElevation"
        );
        assert_ne!(ELEVATION_SCHEDULED_TASK_NAME, CSHARP_ELEVATION_SCHEDULED_TASK_NAME);
    }

    #[test]
    fn background_launch_bypasses_task_that_would_drop_arguments() {
        assert!(!can_reuse_elevation_scheduled_task([
            crate::startup::BACKGROUND_ARGUMENT,
        ]));
        assert!(!can_reuse_elevation_scheduled_task([
            "--other-option",
            crate::startup::BACKGROUND_ARGUMENT,
        ]));
    }

    #[test]
    fn normal_launch_can_still_reuse_elevation_task() {
        assert!(can_reuse_elevation_scheduled_task(std::iter::empty::<&str>()));
        assert!(can_reuse_elevation_scheduled_task(["--other-option"]));
    }

    #[test]
    fn classify_matches_csharp_ensure_elevated_branches() {
        assert_eq!(
            classify_elevation_bootstrap(true, false, true),
            ElevationBootstrapKind::ContinueUnchanged
        );
        assert_eq!(
            classify_elevation_bootstrap(false, false, false),
            ElevationBootstrapKind::DisableAdminAndMaybeDemote
        );
        assert_eq!(
            classify_elevation_bootstrap(false, true, false),
            ElevationBootstrapKind::DisableAdminAndMaybeDemote
        );
        assert_eq!(
            classify_elevation_bootstrap(true, true, false),
            ElevationBootstrapKind::RegisterTask
        );
        assert_eq!(
            classify_elevation_bootstrap(true, false, false),
            ElevationBootstrapKind::ElevateNewProcess
        );
    }

    #[test]
    fn read_persisted_run_as_administrator_accepts_pascal_and_camel() {
        let dir = tempdir().unwrap();
        let pascal = dir.path().join("pascal.json");
        std::fs::write(&pascal, r#"{"RunAsAdministrator":true}"#).unwrap();
        assert_eq!(read_persisted_run_as_administrator(&pascal), Some(true));
        let camel = dir.path().join("camel.json");
        std::fs::write(&camel, r#"{"runAsAdministrator":false}"#).unwrap();
        assert_eq!(read_persisted_run_as_administrator(&camel), Some(false));
        let missing = dir.path().join("missing.json");
        std::fs::write(&missing, r#"{}"#).unwrap();
        assert_eq!(read_persisted_run_as_administrator(&missing), None);
        let nulls = dir.path().join("null.json");
        std::fs::write(&nulls, r#"{"RunAsAdministrator":null}"#).unwrap();
        assert_eq!(read_persisted_run_as_administrator(&nulls), None);
    }

    #[test]
    fn ensure_elevated_is_noop_when_headless() {
        crate::clipboard::force_headless(true);
        std::env::set_var("CLIPHOST_TAURI_HEADLESS", "1");
        let dir = tempdir().unwrap();
        let path = dir.path().join("GlobalConfig.json");
        std::fs::write(&path, r#"{"RunAsAdministrator":true}"#).unwrap();
        ensure_elevated_if_configured(&path);
    }

    #[test]
    fn exe_full_path_lower_strips_extended_prefix() {
        assert_eq!(
            strip_extended_prefix(r"\\?\C:\Program Files\app.exe"),
            r"C:\Program Files\app.exe"
        );
    }

    #[test]
    fn prefer_medium_integrity_only_when_turning_admin_off_while_elevated() {
        assert!(!prefer_medium_integrity_replacement(true, true));
        assert!(!prefer_medium_integrity_replacement(true, false));
        assert!(!prefer_medium_integrity_replacement(false, false));
        assert!(prefer_medium_integrity_replacement(false, true));
    }

    #[test]
    fn run_as_admin_restart_prompt_matches_csharp() {
        assert_eq!(
            RUN_AS_ADMINISTRATOR_RESTART_PROMPT,
            "「以管理员身份运行」设置已更改。是否立即重启以使设置生效？"
        );
    }

    #[test]
    fn persisted_change_prompt_is_noop_when_unchanged_or_headless() {
        crate::clipboard::force_headless(true);
        std::env::set_var("CLIPHOST_TAURI_HEADLESS", "1");
        assert_eq!(try_prompt_after_persisted_run_as_administrator(false, true, 0), None);
        assert_eq!(try_prompt_after_persisted_run_as_administrator(true, true, 0), None);
    }
}
