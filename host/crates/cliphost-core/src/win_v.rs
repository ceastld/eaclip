//! Win+V takeover aligned with C# `WindowsWinVShortcutRegistry` + `ClipHostWinVShortcutRestartUi`.

use serde_json::{json, Value};

const EXPLORER_ADVANCED: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\Advanced";
const CLIPBOARD_KEY: &str = r"Software\Microsoft\Clipboard";
const EXPLORER_POLICIES: &str = r"Software\Microsoft\Windows\CurrentVersion\Policies\Explorer";
const SYSTEM_POLICIES: &str = r"Software\Policies\Microsoft\Windows\System";
const DISABLED_HOTKEYS: &str = "DisabledHotkeys";
const REGISTRY_WRITE_FAILED: &str =
    "无法写入注册表以配置 Win+V。请确认当前用户有权限修改 HKCU 下的 Explorer/Clipboard 设置。";

pub fn apply(enable: bool) -> Value {
    if crate::clipboard::is_headless() || cfg!(feature = "native-acceptance")
        || std::env::var("CLIPHOST_GPUI_ISOLATED").as_deref() == Ok("1") {
        return json!({
            "ok": true,
            "skipped": true,
            "registryChanged": false,
            "needsExplorerRestart": false,
            "text": "headless: Win+V registry write skipped",
        });
    }
    match apply_inner(enable) {
        Ok((changed, restart)) => json!({
            "ok": true,
            "skipped": false,
            "registryChanged": changed,
            "needsExplorerRestart": restart,
            "text": prompt_text(enable, restart),
        }),
        Err(err) => {
            tracing::warn!("Win+V registry write failed: {err}");
            json!({
                "ok": false,
                "skipped": false,
                "registryChanged": false,
                "needsExplorerRestart": false,
                "text": REGISTRY_WRITE_FAILED,
            })
        }
    }
}

/// C# `ClipHostWinVShortcutRestartUi.EnsureRegistryOnStartup`.
pub fn ensure_on_startup(use_win_v_shortcut: bool) {
    if !use_win_v_shortcut || crate::clipboard::is_headless()
        || std::env::var("CLIPHOST_GPUI_ISOLATED").as_deref() == Ok("1") {
        return;
    }
    if is_system_win_v_disabled() {
        return;
    }
    let _ = apply(true);
}

/// C# `ClipHostWinVShortcutRestartUi.TryPromptAfterPersistedChange` dialog half.
/// Caller already applied the registry and refreshed hotkey bindings.
pub fn prompt_applied(applied: &Value, owner_hwnd: isize) {
    if !crate::clipboard::native_dialogs_enabled() {
        return;
    }
    let ok = applied.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    let restart = applied
        .get("needsExplorerRestart")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let text = applied
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or(if ok {
            ""
        } else {
            REGISTRY_WRITE_FAILED
        });
    let ask_restart = ok && restart;
    if show_prompt(text, ask_restart, owner_hwnd) && ask_restart {
        restart_explorer();
    }
}

/// C# `WindowsWinVShortcutRegistry.IsSystemWinVDisabled`.
pub fn is_system_win_v_disabled() -> bool {
    #[cfg(windows)]
    {
        query_sz(EXPLORER_ADVANCED, DISABLED_HOTKEYS)
            .unwrap_or_default()
            .to_ascii_uppercase()
            .contains('V')
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// C# `ClipHostWinVShortcutRestartUi` MessageBox copy.
pub fn prompt_text(enable: bool, needs_explorer_restart: bool) -> String {
    let mut text = if enable {
        "已启用 Win+V 打开剪贴板：已修改注册表以禁用系统剪贴板历史，并尝试注册 Win+V 全局快捷键。"
            .to_string()
    } else {
        "已关闭 Win+V 接管，并尝试恢复系统剪贴板历史相关注册表项。".to_string()
    };
    if needs_explorer_restart {
        if enable {
            text.push_str("\n\n需要重启 Windows 资源管理器后 Win+V 才会从系统释放。是否立即重启资源管理器？");
        } else {
            text.push_str("\n\n需要重启 Windows 资源管理器后系统 Win+V 才会恢复。是否立即重启资源管理器？");
        }
    }
    text
}

pub fn merge_disabled_hotkeys(current: &str, enable: bool) -> DisabledHotkeysUpdate {
    let upper = current.to_ascii_uppercase();
    if enable {
        if upper.contains('V') {
            return DisabledHotkeysUpdate::Unchanged(current.to_string());
        }
        return DisabledHotkeysUpdate::Set(format!("{current}V"));
    }
    if !upper.contains('V') {
        return DisabledHotkeysUpdate::Unchanged(current.to_string());
    }
    let clean: String = upper.chars().filter(|c| *c != 'V').collect();
    if clean.is_empty() {
        DisabledHotkeysUpdate::Delete
    } else {
        DisabledHotkeysUpdate::Set(clean)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisabledHotkeysUpdate {
    Unchanged(String),
    Set(String),
    Delete,
}

fn show_prompt(message: &str, ask_restart_explorer: bool, owner_hwnd: isize) -> bool {
    #[cfg(windows)]
    {
        use windows::core::{w, HSTRING, PCWSTR};
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{
            MessageBoxW, IDYES, MB_ICONINFORMATION, MB_ICONQUESTION, MB_OK, MB_YESNO,
        };

        let text = HSTRING::from(message);
        let owner = if owner_hwnd == 0 {
            HWND::default()
        } else {
            HWND(owner_hwnd as *mut _)
        };
        let flags = if ask_restart_explorer {
            MB_YESNO | MB_ICONQUESTION
        } else {
            MB_OK | MB_ICONINFORMATION
        };
        let rc = unsafe { MessageBoxW(owner, PCWSTR(text.as_ptr()), w!("BetterClip"), flags) };
        ask_restart_explorer && rc == IDYES
    }
    #[cfg(not(windows))]
    {
        let _ = (message, ask_restart_explorer, owner_hwnd);
        false
    }
}

/// C# `WindowsWinVShortcutRegistry.RestartExplorer`.
pub fn restart_explorer() {
    if crate::clipboard::is_headless() || !crate::clipboard::native_dialogs_enabled() {
        return;
    }
    let cmd = std::env::var_os("WINDIR")
        .or_else(|| std::env::var_os("SystemRoot"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows"))
        .join("System32")
        .join("cmd.exe");
    let mut process = std::process::Command::new(cmd);
    process
        .arg("/C")
        .arg("taskkill /F /IM explorer.exe & start explorer.exe");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        process
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
    }
    let _ = process.spawn();
}

#[cfg(windows)]
fn apply_inner(enable: bool) -> Result<(bool, bool), String> {
    let mut changed = false;
    let mut restart = false;
    match apply_disabled_hotkeys(enable)? {
        DisabledHotkeysUpdate::Unchanged(_) => {}
        DisabledHotkeysUpdate::Set(_) | DisabledHotkeysUpdate::Delete => {
            changed = true;
            restart = true;
        }
    }
    if apply_clipboard_history(enable)? {
        changed = true;
    }
    if !enable && clear_blocking_policies()? {
        changed = true;
        restart = true;
    }
    Ok((changed, restart))
}

#[cfg(not(windows))]
fn apply_inner(_enable: bool) -> Result<(bool, bool), String> {
    Ok((false, false))
}

#[cfg(windows)]
fn apply_disabled_hotkeys(enable: bool) -> Result<DisabledHotkeysUpdate, String> {
    let current = query_sz(EXPLORER_ADVANCED, DISABLED_HOTKEYS).unwrap_or_default();
    let update = merge_disabled_hotkeys(&current, enable);
    match &update {
        DisabledHotkeysUpdate::Unchanged(_) => {}
        DisabledHotkeysUpdate::Set(value) => set_sz(EXPLORER_ADVANCED, DISABLED_HOTKEYS, value)?,
        DisabledHotkeysUpdate::Delete => delete_value(EXPLORER_ADVANCED, DISABLED_HOTKEYS)?,
    }
    Ok(update)
}

#[cfg(windows)]
fn apply_clipboard_history(enable: bool) -> Result<bool, String> {
    let val: u32 = if enable { 0 } else { 1 };
    let prev_history = query_dword(CLIPBOARD_KEY, "EnableClipboardHistory");
    let prev_cloud = query_dword(CLIPBOARD_KEY, "EnableCloudClipboard");
    set_dword(CLIPBOARD_KEY, "EnableClipboardHistory", val)?;
    set_dword(CLIPBOARD_KEY, "EnableCloudClipboard", val)?;
    Ok(prev_history != Some(val) || prev_cloud != Some(val))
}

#[cfg(windows)]
fn clear_blocking_policies() -> Result<bool, String> {
    let mut changed = false;
    if query_dword(EXPLORER_POLICIES, "DisallowClipboardHistory") == Some(1) {
        delete_value(EXPLORER_POLICIES, "DisallowClipboardHistory")?;
        changed = true;
    }
    if query_dword(SYSTEM_POLICIES, "AllowClipboardHistory") == Some(0) {
        delete_value(SYSTEM_POLICIES, "AllowClipboardHistory")?;
        changed = true;
    }
    if query_dword(SYSTEM_POLICIES, "AllowCrossDeviceClipboard") == Some(0) {
        delete_value(SYSTEM_POLICIES, "AllowCrossDeviceClipboard")?;
        changed = true;
    }
    Ok(changed)
}

#[cfg(windows)]
fn query_sz(subkey: &str, name: &str) -> Option<String> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY_CURRENT_USER, KEY_READ, REG_SZ,
    };

    unsafe {
        let sub = to_wide(subkey);
        let value = to_wide(name);
        let mut key = Default::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(sub.as_ptr()), 0, KEY_READ, &mut key).0 != ERROR_SUCCESS.0 {
            return None;
        }
        let mut kind = REG_SZ;
        let mut size = 0u32;
        let _ = RegQueryValueExW(key, PCWSTR(value.as_ptr()), None, Some(&mut kind), None, Some(&mut size));
        if size == 0 {
            let _ = RegCloseKey(key);
            return Some(String::new());
        }
        let mut buf = vec![0u8; size as usize];
        let status = RegQueryValueExW(
            key,
            PCWSTR(value.as_ptr()),
            None,
            Some(&mut kind),
            Some(buf.as_mut_ptr()),
            Some(&mut size),
        );
        let _ = RegCloseKey(key);
        if status.0 != ERROR_SUCCESS.0 {
            return None;
        }
        let wide = std::slice::from_raw_parts(buf.as_ptr() as *const u16, (size as usize) / 2);
        Some(String::from_utf16_lossy(wide).trim_end_matches('\0').to_string())
    }
}

#[cfg(windows)]
fn query_dword(subkey: &str, name: &str) -> Option<u32> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY_CURRENT_USER, KEY_READ, REG_DWORD,
    };

    unsafe {
        let sub = to_wide(subkey);
        let value = to_wide(name);
        let mut key = Default::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(sub.as_ptr()), 0, KEY_READ, &mut key).0 != ERROR_SUCCESS.0 {
            return None;
        }
        let mut kind = REG_DWORD;
        let mut data = 0u32;
        let mut size = 4u32;
        let status = RegQueryValueExW(
            key,
            PCWSTR(value.as_ptr()),
            None,
            Some(&mut kind),
            Some((&mut data as *mut u32).cast()),
            Some(&mut size),
        );
        let _ = RegCloseKey(key);
        if status.0 != ERROR_SUCCESS.0 {
            return None;
        }
        Some(data)
    }
}

#[cfg(windows)]
fn set_sz(subkey: &str, name: &str, value: &str) -> Result<(), String> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegSetValueExW, REG_SZ,
    };

    unsafe {
        let key = open_write(subkey)?;
        let name_w = to_wide(name);
        let data = to_wide(value);
        let bytes = std::slice::from_raw_parts(data.as_ptr() as *const u8, data.len() * 2);
        let status = RegSetValueExW(key, PCWSTR(name_w.as_ptr()), 0, REG_SZ, Some(bytes));
        let _ = RegCloseKey(key);
        if status.0 != ERROR_SUCCESS.0 {
            return Err(format!("RegSetValueExW sz failed: {}", status.0));
        }
        Ok(())
    }
}

#[cfg(windows)]
fn set_dword(subkey: &str, name: &str, value: u32) -> Result<(), String> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegSetValueExW, KEY_WRITE, REG_DWORD,
    };

    unsafe {
        let key = open_write(subkey)?;
        let name_w = to_wide(name);
        let bytes = value.to_le_bytes();
        let status = RegSetValueExW(key, PCWSTR(name_w.as_ptr()), 0, REG_DWORD, Some(&bytes));
        let _ = RegCloseKey(key);
        if status.0 != ERROR_SUCCESS.0 {
            return Err(format!("RegSetValueExW dword failed: {}", status.0));
        }
        let _ = KEY_WRITE;
        Ok(())
    }
}

#[cfg(windows)]
fn delete_value(subkey: &str, name: &str) -> Result<(), String> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows::Win32::System::Registry::{RegCloseKey, RegDeleteValueW};

    unsafe {
        let key = open_write(subkey)?;
        let name_w = to_wide(name);
        let status = RegDeleteValueW(key, PCWSTR(name_w.as_ptr()));
        let _ = RegCloseKey(key);
        if status.0 != ERROR_SUCCESS.0 && status.0 != ERROR_FILE_NOT_FOUND.0 {
            return Err(format!("RegDeleteValueW failed: {}", status.0));
        }
        Ok(())
    }
}

#[cfg(windows)]
fn open_write(subkey: &str) -> Result<windows::Win32::System::Registry::HKEY, String> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        RegCreateKeyExW, HKEY_CURRENT_USER, KEY_WRITE, REG_OPTION_NON_VOLATILE,
    };

    unsafe {
        let sub = to_wide(subkey);
        let mut key = Default::default();
        let status = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(sub.as_ptr()),
            0,
            windows::core::PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut key,
            None,
        );
        if status.0 != ERROR_SUCCESS.0 {
            return Err(format!("RegCreateKeyExW failed: {}", status.0));
        }
        Ok(key)
    }
}

#[cfg(windows)]
fn to_wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_and_removes_v() {
        assert_eq!(merge_disabled_hotkeys("", true), DisabledHotkeysUpdate::Set("V".into()));
        assert_eq!(
            merge_disabled_hotkeys("V", true),
            DisabledHotkeysUpdate::Unchanged("V".into())
        );
        assert_eq!(merge_disabled_hotkeys("V", false), DisabledHotkeysUpdate::Delete);
        assert_eq!(
            merge_disabled_hotkeys("NV", false),
            DisabledHotkeysUpdate::Set("N".into())
        );
    }

    #[test]
    fn prompt_text_matches_csharp_explorer_restart_copy() {
        assert_eq!(
            prompt_text(true, false),
            "已启用 Win+V 打开剪贴板：已修改注册表以禁用系统剪贴板历史，并尝试注册 Win+V 全局快捷键。"
        );
        assert!(prompt_text(true, true).contains("是否立即重启资源管理器？"));
        assert!(prompt_text(true, true).contains("Win+V 才会从系统释放"));
        assert_eq!(
            prompt_text(false, false),
            "已关闭 Win+V 接管，并尝试恢复系统剪贴板历史相关注册表项。"
        );
        assert!(prompt_text(false, true).contains("系统 Win+V 才会恢复"));
    }
}
