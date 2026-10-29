//! One Windows startup registration for ClipHost, shared with the former C# host.
//!
//! Release startup and setting changes migrate the old Tauri alias into the canonical
//! value. Debug, headless and native-acceptance builds never touch the real Run key.

pub const RUN_REGISTRY_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// Stable product identity, also used by the former C# host and installer.
pub const STARTUP_VALUE_NAME: &str = "IntelliTools.ClipHost";
pub const CSHARP_STARTUP_VALUE_NAME: &str = STARTUP_VALUE_NAME;

/// Previous Rust releases used this additional value; upgrades remove it.
pub const LEGACY_TAURI_STARTUP_VALUE_NAME: &str = "IntelliTools.ClipHost.Tauri";

/// Login startup keeps the tray and capture service running without presenting UI.
pub const BACKGROUND_ARGUMENT: &str = "--background";

pub fn is_background_launch(args: impl IntoIterator<Item = impl AsRef<str>>) -> bool {
    args.into_iter().any(|arg| arg.as_ref() == BACKGROUND_ARGUMENT)
}

pub fn startup_command(exe_path: &str) -> String {
    format!("\"{exe_path}\" {BACKGROUND_ARGUMENT}")
}

pub fn should_sync_registry() -> bool {
    !cfg!(debug_assertions) && !cfg!(feature = "native-acceptance") && !crate::clipboard::is_headless()
        && std::env::var("CLIPHOST_GPUI_ISOLATED").as_deref() != Ok("1")
}

pub fn apply(enabled: bool) {
    if !should_sync_registry() {
        return;
    }
    if let Err(err) = apply_inner(enabled) {
        tracing::warn!("Failed to sync Windows startup registration: {err}");
    }
}

trait StartupRegistrationStore {
    fn set_command(&mut self, name: &str, command: &str) -> Result<(), String>;
    fn delete_registration(&mut self, name: &str) -> Result<(), String>;
}

fn sync_registration(
    store: &mut impl StartupRegistrationStore,
    enabled: bool,
    exe_path: &str,
) -> Result<(), String> {
    if enabled {
        // Validate before removing anything. A failed alias deletion must not add
        // a second registration, so canonical writes always follow its deletion.
        if exe_path.trim().is_empty() {
            return Err("Cannot enable startup because process path is empty.".into());
        }
        store
            .delete_registration(LEGACY_TAURI_STARTUP_VALUE_NAME)
            .map_err(|err| format!("Cannot remove legacy startup value {LEGACY_TAURI_STARTUP_VALUE_NAME}: {err}"))?;
        store
            .set_command(STARTUP_VALUE_NAME, &startup_command(exe_path))
            .map_err(|err| format!("Cannot write startup value {STARTUP_VALUE_NAME}: {err}"))
    } else {
        // Try both removals even if one fails. Report incomplete cleanup instead
        // of treating a remaining enabled registration as a successful disable.
        let legacy = store
            .delete_registration(LEGACY_TAURI_STARTUP_VALUE_NAME)
            .map_err(|err| format!("Cannot remove legacy startup value {LEGACY_TAURI_STARTUP_VALUE_NAME}: {err}"));
        let canonical = store
            .delete_registration(STARTUP_VALUE_NAME)
            .map_err(|err| format!("Cannot remove startup value {STARTUP_VALUE_NAME}: {err}"));
        match (legacy, canonical) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(legacy), Err(canonical)) => Err(format!("{legacy}; {canonical}")),
            (Err(err), _) | (_, Err(err)) => Err(err),
        }
    }
}

fn apply_inner(enabled: bool) -> Result<(), String> {
    #[cfg(windows)]
    {
        let exe_path = if enabled {
            std::env::current_exe()
                .map_err(|err| format!("Cannot resolve startup executable: {err}"))?
                .to_string_lossy()
                .into_owned()
        } else {
            String::new()
        };
        let mut store = WindowsStartupRegistrationStore::open(RUN_REGISTRY_PATH)?;
        sync_registration(&mut store, enabled, &exe_path)
    }
    #[cfg(not(windows))]
    {
        let _ = enabled;
        Ok(())
    }
}

#[cfg(windows)]
struct WindowsStartupRegistrationStore {
    key: windows::Win32::System::Registry::HKEY,
}

#[cfg(windows)]
impl WindowsStartupRegistrationStore {
    // Only apply_inner chooses the production key. The isolated Windows test
    // uses this same adapter with its own unique subkey; there is no env override.
    fn open(subkey: &str) -> Result<Self, String> {
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::ERROR_SUCCESS;
        use windows::Win32::System::Registry::{
            RegCreateKeyExW, HKEY_CURRENT_USER, KEY_WRITE, REG_OPTION_NON_VOLATILE,
        };

        let sub = to_wide(subkey);
        let mut key = Default::default();
        let status = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(sub.as_ptr()),
                0,
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE,
                None,
                &mut key,
                None,
            )
        };
        if status.0 != ERROR_SUCCESS.0 {
            return Err(format!("Cannot open startup registry key: {}", status.0));
        }
        Ok(Self { key })
    }
}

#[cfg(windows)]
impl StartupRegistrationStore for WindowsStartupRegistrationStore {
    fn set_command(&mut self, name: &str, command: &str) -> Result<(), String> {
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::ERROR_SUCCESS;
        use windows::Win32::System::Registry::{RegSetValueExW, REG_SZ};

        let name_w = to_wide(name);
        let data = to_wide(command);
        let bytes = unsafe {
            std::slice::from_raw_parts(data.as_ptr() as *const u8, data.len() * 2)
        };
        let status = unsafe { RegSetValueExW(self.key, PCWSTR(name_w.as_ptr()), 0, REG_SZ, Some(bytes)) };
        if status.0 != ERROR_SUCCESS.0 {
            return Err(format!("RegSetValueExW failed: {}", status.0));
        }
        Ok(())
    }

    fn delete_registration(&mut self, name: &str) -> Result<(), String> {
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
        use windows::Win32::System::Registry::RegDeleteValueW;

        let name_w = to_wide(name);
        let status = unsafe { RegDeleteValueW(self.key, PCWSTR(name_w.as_ptr())) };
        if status.0 != ERROR_SUCCESS.0 && status.0 != ERROR_FILE_NOT_FOUND.0 {
            return Err(format!("RegDeleteValueW failed: {}", status.0));
        }
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for WindowsStartupRegistrationStore {
    fn drop(&mut self) {
        let _ = unsafe { windows::Win32::System::Registry::RegCloseKey(self.key) };
    }
}

#[cfg(windows)]
fn to_wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    const OLD_CSHARP: &str = r#""D:\Custom CSharp\IntelliTools.ClipHost.exe""#;
    const OLD_RUST: &str = r#""D:\Old Rust\cliphost-app.exe""#;
    const CURRENT_EXE: &str = r"E:\Custom Rust\cliphost-app.exe";

    #[derive(Default)]
    struct MemoryStore {
        values: BTreeMap<String, String>,
        fail_delete: Vec<String>,
        fail_write: bool,
        calls: Vec<String>,
    }

    impl StartupRegistrationStore for MemoryStore {
        fn set_command(&mut self, name: &str, command: &str) -> Result<(), String> {
            self.calls.push(format!("set:{name}"));
            if self.fail_write {
                return Err("write denied".into());
            }
            self.values.insert(name.into(), command.into());
            Ok(())
        }

        fn delete_registration(&mut self, name: &str) -> Result<(), String> {
            self.calls.push(format!("delete:{name}"));
            if self.fail_delete.iter().any(|blocked| blocked == name) {
                return Err("delete denied".into());
            }
            self.values.remove(name);
            Ok(())
        }
    }

    fn old_store(canonical: bool, alias: bool) -> MemoryStore {
        let mut store = MemoryStore::default();
        if canonical {
            store.values.insert(STARTUP_VALUE_NAME.into(), OLD_CSHARP.into());
        }
        if alias {
            store.values.insert(LEGACY_TAURI_STARTUP_VALUE_NAME.into(), OLD_RUST.into());
        }
        store.values.insert("OtherUserApplication".into(), "untouched".into());
        store
    }

    #[test]
    fn startup_identity_reuses_canonical_and_verification_never_writes_run() {
        assert_eq!(STARTUP_VALUE_NAME, "IntelliTools.ClipHost");
        assert_eq!(STARTUP_VALUE_NAME, CSHARP_STARTUP_VALUE_NAME);
        assert_eq!(LEGACY_TAURI_STARTUP_VALUE_NAME, "IntelliTools.ClipHost.Tauri");
        assert_ne!(STARTUP_VALUE_NAME, LEGACY_TAURI_STARTUP_VALUE_NAME);
        assert_eq!(startup_command(CURRENT_EXE), format!("\"{CURRENT_EXE}\" --background"));
        assert!(!should_sync_registry(), "debug/headless tests must not write HKCU Run");
    }

    #[test]
    fn background_launch_requires_the_exact_standalone_argument() {
        assert!(is_background_launch(["--background"]));
        assert!(is_background_launch(["cliphost-app.exe", "--background", "--cliphost-demote-attempted"]));
        assert!(!is_background_launch(Vec::<String>::new()));
        assert!(!is_background_launch(["cliphost-app.exe", "--background=false", "C:\\--background"]));
    }

    #[test]
    fn startup_upgrade_migrates_both_csharp_rust_or_no_previous_registration() {
        for (canonical, alias) in [(true, true), (true, false), (false, true), (false, false)] {
            let mut store = old_store(canonical, alias);
            sync_registration(&mut store, true, CURRENT_EXE).unwrap();
            assert_eq!(store.values.len(), 2, "one product value plus the other application");
            assert_eq!(store.values[STARTUP_VALUE_NAME], startup_command(CURRENT_EXE));
            assert!(!store.values.contains_key(LEGACY_TAURI_STARTUP_VALUE_NAME));
            assert_eq!(store.values["OtherUserApplication"], "untouched");
            assert_eq!(store.calls, [
                format!("delete:{LEGACY_TAURI_STARTUP_VALUE_NAME}"),
                format!("set:{STARTUP_VALUE_NAME}"),
            ]);
        }
    }

    #[test]
    fn startup_repeated_upgrade_updates_path_without_adding_registration() {
        let mut store = old_store(true, true);
        for exe in [CURRENT_EXE, CURRENT_EXE, r"F:\Next Rust\cliphost-app.exe"] {
            sync_registration(&mut store, true, exe).unwrap();
            assert_eq!(store.values.len(), 2);
            assert_eq!(store.values[STARTUP_VALUE_NAME], startup_command(exe));
            assert!(!store.values.contains_key(LEGACY_TAURI_STARTUP_VALUE_NAME));
        }
    }

    #[test]
    fn startup_disable_removes_both_owned_values_and_preserves_other_apps() {
        let mut store = old_store(true, true);
        sync_registration(&mut store, false, "").unwrap();
        sync_registration(&mut store, false, "").unwrap();
        assert_eq!(store.values.len(), 1);
        assert_eq!(store.values["OtherUserApplication"], "untouched");
        sync_registration(&mut store, true, CURRENT_EXE).unwrap();
        assert_eq!(store.values[STARTUP_VALUE_NAME], startup_command(CURRENT_EXE));
        sync_registration(&mut store, false, "").unwrap();
        assert_eq!(store.values.len(), 1);
    }

    #[test]
    fn startup_enable_validates_path_before_any_removal() {
        for exe in ["", " \t\r\n"] {
            let mut store = old_store(true, true);
            let original = store.values.clone();
            let err = sync_registration(&mut store, true, exe).unwrap_err();
            assert!(err.contains("process path is empty"));
            assert_eq!(store.values, original);
            assert!(store.calls.is_empty());
        }
    }

    #[test]
    fn startup_alias_delete_failure_never_adds_canonical_registration() {
        let mut store = old_store(false, true);
        store.fail_delete.push(LEGACY_TAURI_STARTUP_VALUE_NAME.into());
        let err = sync_registration(&mut store, true, CURRENT_EXE).unwrap_err();
        assert!(err.contains(LEGACY_TAURI_STARTUP_VALUE_NAME));
        assert!(err.contains("delete denied"));
        assert!(!store.values.contains_key(STARTUP_VALUE_NAME));
        assert_eq!(store.values[LEGACY_TAURI_STARTUP_VALUE_NAME], OLD_RUST);
        assert_eq!(store.calls, [format!("delete:{LEGACY_TAURI_STARTUP_VALUE_NAME}")]);
    }

    #[test]
    fn startup_canonical_write_failure_reports_error_and_keeps_old_canonical() {
        let mut store = old_store(true, true);
        store.fail_write = true;
        let err = sync_registration(&mut store, true, CURRENT_EXE).unwrap_err();
        assert!(err.contains(STARTUP_VALUE_NAME));
        assert!(err.contains("write denied"));
        assert!(!store.values.contains_key(LEGACY_TAURI_STARTUP_VALUE_NAME));
        assert_eq!(store.values[STARTUP_VALUE_NAME], OLD_CSHARP);
        assert_eq!(store.values["OtherUserApplication"], "untouched");
    }

    #[test]
    fn startup_disable_attempts_both_removals_and_reports_each_failure() {
        for failures in [
            vec![LEGACY_TAURI_STARTUP_VALUE_NAME],
            vec![STARTUP_VALUE_NAME],
            vec![LEGACY_TAURI_STARTUP_VALUE_NAME, STARTUP_VALUE_NAME],
        ] {
            let mut store = old_store(true, true);
            store.fail_delete = failures.iter().map(|name| name.to_string()).collect();
            let err = sync_registration(&mut store, false, "").unwrap_err();
            assert_eq!(store.calls, [
                format!("delete:{LEGACY_TAURI_STARTUP_VALUE_NAME}"),
                format!("delete:{STARTUP_VALUE_NAME}"),
            ]);
            for name in [LEGACY_TAURI_STARTUP_VALUE_NAME, STARTUP_VALUE_NAME] {
                assert_eq!(store.values.contains_key(name), failures.contains(&name));
                if failures.contains(&name) {
                    assert!(err.contains(name));
                }
            }
            assert_eq!(store.values["OtherUserApplication"], "untouched");
        }
    }

    #[cfg(windows)]
    #[test]
    fn startup_windows_adapter_migrates_in_unique_isolated_registry_key() {
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
        use windows::Win32::System::Registry::{
            RegCloseKey, RegDeleteKeyW, RegOpenKeyExW, RegQueryValueExW,
            HKEY_CURRENT_USER, KEY_READ, REG_SZ,
        };

        struct TemporaryKey {
            path: String,
            removed: bool,
        }

        impl TemporaryKey {
            fn remove(mut self) {
                let path = to_wide(&self.path);
                let status = unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR(path.as_ptr())) };
                assert_eq!(status.0, ERROR_SUCCESS.0, "remove only the isolated fixture key");
                self.removed = true;
            }
        }

        impl Drop for TemporaryKey {
            fn drop(&mut self) {
                if !self.removed {
                    let path = to_wide(&self.path);
                    let _ = unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR(path.as_ptr())) };
                }
            }
        }

        fn read_value(subkey: &str, name: &str) -> Option<String> {
            let subkey = to_wide(subkey);
            let name = to_wide(name);
            let mut key = Default::default();
            let opened = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(subkey.as_ptr()), 0, KEY_READ, &mut key) };
            assert_eq!(opened.0, ERROR_SUCCESS.0);
            let mut kind = REG_SZ;
            let mut size = 0;
            let status = unsafe { RegQueryValueExW(key, PCWSTR(name.as_ptr()), None, Some(&mut kind), None, Some(&mut size)) };
            if status.0 == ERROR_FILE_NOT_FOUND.0 {
                let _ = unsafe { RegCloseKey(key) };
                return None;
            }
            assert_eq!(status.0, ERROR_SUCCESS.0);
            assert_eq!(kind, REG_SZ);
            let mut data = vec![0u16; size as usize / 2];
            let status = unsafe { RegQueryValueExW(key, PCWSTR(name.as_ptr()), None, Some(&mut kind), Some(data.as_mut_ptr().cast()), Some(&mut size)) };
            let _ = unsafe { RegCloseKey(key) };
            assert_eq!(status.0, ERROR_SUCCESS.0);
            Some(String::from_utf16_lossy(&data).trim_end_matches('\0').into())
        }

        let fixture = TemporaryKey {
            path: format!(r"Software\IntelliTools\ClipHost\StartupTests\{}", uuid::Uuid::new_v4()),
            removed: false,
        };
        assert_ne!(fixture.path, RUN_REGISTRY_PATH);
        let mut store = WindowsStartupRegistrationStore::open(&fixture.path).unwrap();
        store.set_command("OtherUserApplication", "untouched").unwrap();
        for (canonical, alias) in [(true, true), (true, false), (false, true), (false, false)] {
            store.delete_registration(STARTUP_VALUE_NAME).unwrap();
            store.delete_registration(LEGACY_TAURI_STARTUP_VALUE_NAME).unwrap();
            if canonical {
                store.set_command(STARTUP_VALUE_NAME, OLD_CSHARP).unwrap();
            }
            if alias {
                store.set_command(LEGACY_TAURI_STARTUP_VALUE_NAME, OLD_RUST).unwrap();
            }
            sync_registration(&mut store, true, CURRENT_EXE).unwrap();
            assert_eq!(read_value(&fixture.path, STARTUP_VALUE_NAME), Some(startup_command(CURRENT_EXE)));
            assert_eq!(read_value(&fixture.path, LEGACY_TAURI_STARTUP_VALUE_NAME), None);
            sync_registration(&mut store, true, r"F:\Next Rust\cliphost-app.exe").unwrap();
            assert_eq!(read_value(&fixture.path, STARTUP_VALUE_NAME), Some(startup_command(r"F:\Next Rust\cliphost-app.exe")));
            sync_registration(&mut store, false, "").unwrap();
            assert_eq!(read_value(&fixture.path, STARTUP_VALUE_NAME), None);
            assert_eq!(read_value(&fixture.path, LEGACY_TAURI_STARTUP_VALUE_NAME), None);
            assert_eq!(read_value(&fixture.path, "OtherUserApplication"), Some("untouched".into()));
        }
        drop(store);
        fixture.remove();
    }
}
