//! Open a file, shell location or registered URI, returning launch failures.

fn prepare_target(raw: &str) -> Result<String, String> {
    let target = raw.trim();
    if target.is_empty() || target.contains('\0') {
        return Err("打开目标为空或包含无效字符。".into());
    }
    if crate::explorer::is_shell_path(target) {
        return Ok(target.to_string());
    }
    // Keep registered protocols (including browser and Quicker links), while
    // treating drive letters and file: links as filesystem paths.
    if url::Url::parse(target).is_ok_and(|uri| uri.scheme().len() > 1 && uri.scheme() != "file") {
        return Ok(target.to_string());
    }
    let full = crate::shell_path::normalize_local_shell_path(target)?;
    if !std::path::Path::new(&full).exists() {
        return Err(format!("路径不存在或无法访问：{full}"));
    }
    Ok(full)
}

pub(crate) fn open(raw: &str) -> Result<(), String> {
    open_guarded(raw, &|| Ok(()))
}

pub(crate) fn open_guarded(
    raw: &str,
    verify: &impl Fn() -> Result<(), String>,
) -> Result<(), String> {
    verify()?;
    let target = prepare_target(raw);
    verify()?;
    let target = target?;
    if crate::clipboard::is_headless() {
        return Ok(());
    }
    open_native_guarded(&target, verify)
}

/// File-menu "open with the current application" passes one filesystem path,
/// not a command line or JSON string. The captured executable may have gone away.
pub(crate) fn open_with_process(executable: &str, path: &str) -> Result<(), String> {
    let executable = crate::shell_path::normalize_local_shell_path(executable)?;
    if !std::path::Path::new(&executable).is_file() {
        return Err(format!("程序不存在或无法访问：{executable}"));
    }
    let path = crate::shell_path::normalize_local_shell_path(path)?;
    if !std::path::Path::new(&path).exists() {
        return Err(format!("路径不存在或无法访问：{path}"));
    }
    if crate::clipboard::is_headless() {
        return Ok(());
    }
    open_with_process_native(&executable, &path)
}

fn open_with_process_native(executable: &str, path: &str) -> Result<(), String> {
    std::process::Command::new(executable)
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("无法使用“{executable}”打开“{path}”：{error}"))
}

/// Preparation may include filesystem/COM work. The guard is checked after
/// it completes, immediately before the OS callback; success has no postcheck.
fn run_prepared_shell_call<T>(
    verify: &impl Fn() -> Result<(), String>,
    prepare: impl FnOnce() -> Result<T, String>,
    execute: impl FnOnce(&mut T) -> Result<(), String>,
) -> Result<(), String> {
    verify()?;
    let prepared = prepare();
    verify()?;
    let mut prepared = prepared?;
    execute(&mut prepared)
}

#[cfg(all(windows, test))]
fn open_native(target: &str) -> Result<(), String> {
    open_native_guarded(target, &|| Ok(()))
}

#[cfg(windows)]
fn open_native_guarded(target: &str, verify: &impl Fn() -> Result<(), String>) -> Result<(), String> {
    use windows::core::PCWSTR;
    use windows::Win32::UI::Shell::{
        ShellExecuteExW, SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW,
    };
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    struct PreparedShell {
        info: SHELLEXECUTEINFOW,
        _file: Vec<u16>,
        // Struct drop order releases every prepared value before COM uninit.
        _apartment: crate::explorer::ShellApartment,
    }
    run_prepared_shell_call(verify, || {
        let apartment = crate::explorer::ShellApartment::initialize()
            .map_err(|error| format!("无法初始化打开操作：{error}"))?;
        let file: Vec<u16> = target.encode_utf16().chain([0]).collect();
        let info = SHELLEXECUTEINFOW {
            cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
            // Finish shell startup before releasing the worker's apartment.
            fMask: SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
            lpFile: PCWSTR(file.as_ptr()),
            nShow: SW_SHOWNORMAL.0,
            ..Default::default()
        };
        Ok(PreparedShell { info, _file: file, _apartment: apartment })
    }, |prepared| {
        verify()?;
        unsafe { ShellExecuteExW(&mut prepared.info) }.map_err(|error| format!("无法打开“{target}”：{error}"))
    })
}

#[cfg(not(windows))]
fn open_native_guarded(target: &str, verify: &impl Fn() -> Result<(), String>) -> Result<(), String> {
    run_prepared_shell_call(verify, || Ok(target.to_string()), |prepared| {
        verify()?;
        open::that(prepared.as_str()).map_err(|error| format!("无法打开“{target}”：{error}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_shell_final_boundary_rejects_retired_preparation_and_releases_resource() {
        use std::{cell::Cell, rc::Rc};
        struct Prepared(Rc<Cell<usize>>);
        impl Drop for Prepared { fn drop(&mut self) { self.0.set(self.0.get() + 1); } }
        let retired = Cell::new(false);
        let launches = Cell::new(0);
        let releases = Rc::new(Cell::new(0));
        let result = run_prepared_shell_call(
            &|| if retired.get() { Err("retired shell".into()) } else { Ok(()) },
            || { retired.set(true); Ok(Prepared(releases.clone())) },
            |_| { launches.set(launches.get() + 1); Ok(()) },
        );
        assert_eq!(result.unwrap_err(), "retired shell");
        assert_eq!(launches.get(), 0);
        assert_eq!(releases.get(), 1);
    }

    #[test]
    fn native_shell_final_boundary_keeps_launch_result_after_foreground_change() {
        use std::cell::Cell;
        for failure in [false, true] {
            let retired = Cell::new(false);
            let launches = Cell::new(0);
            let result = run_prepared_shell_call(
                &|| if retired.get() { Err("retired shell".into()) } else { Ok(()) }, || Ok(()),
                |_| {
                    launches.set(launches.get() + 1);
                    retired.set(true);
                    if failure { Err("owned launch failure".into()) } else { Ok(()) }
                },
            );
            assert_eq!(launches.get(), 1);
            assert_eq!(result, if failure { Err("owned launch failure".into()) } else { Ok(()) });
        }
    }

    #[test]
    fn process_open_validates_program_and_target_before_headless_success() {
        crate::clipboard::force_headless(true);
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("fixture.exe");
        let target = dir.path().join("目录 %中文 &;.txt");
        let exe = executable.to_str().unwrap();
        let path = target.to_str().unwrap();
        assert!(open_with_process(exe, path)
            .unwrap_err()
            .contains("程序不存在"));
        std::fs::write(&executable, "headless fixture").unwrap();
        assert!(open_with_process(exe, path)
            .unwrap_err()
            .contains("路径不存在"));
        std::fs::write(&target, "fixture").unwrap();
        assert!(open_with_process(exe, path).is_ok());
        assert!(open_with_process(exe, dir.path().to_str().unwrap()).is_ok());
        assert!(open_with_process(dir.path().to_str().unwrap(), path).is_err());
        assert!(open_with_process("", path).is_err());
        assert!(open_with_process(exe, "").is_err());
        assert!(open_with_process(exe, "bad\0target").is_err());
        assert!(open_with_process("bad\0program", path).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn process_open_native_rejects_a_damaged_executable() {
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("invalid.exe");
        let target = dir.path().join("input.txt");
        std::fs::write(&executable, "intentionally not an executable").unwrap();
        std::fs::write(&target, "fixture").unwrap();
        let error =
            open_with_process_native(executable.to_str().unwrap(), target.to_str().unwrap())
                .unwrap_err();
        assert!(error.starts_with("无法使用"), "{error}");
        assert!(error.contains(executable.to_str().unwrap()), "{error}");
        assert!(error.contains(target.to_str().unwrap()), "{error}");
    }

    #[test]
    fn shell_open_validates_missing_files_and_invalid_targets_in_headless_mode() {
        crate::clipboard::force_headless(true);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("%中文 +#.txt");
        assert!(open(file.to_str().unwrap())
            .unwrap_err()
            .contains("路径不存在"));
        std::fs::write(&file, "fixture").unwrap();
        assert!(open(file.to_str().unwrap()).is_ok());
        assert!(open(url::Url::from_file_path(&file).unwrap().as_str()).is_ok());
        for target in ["", "  ", "file:relative", "https://example.com/\0"] {
            assert!(open(target).is_err(), "{target}");
        }
        for target in [
            "https://example.com/?x=中文",
            "mailto:user@example.com",
            "chrome://bookmarks/",
            crate::explorer::THIS_PC_SHELL,
        ] {
            assert!(open(target).is_ok(), "{target}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn shell_open_native_missing_file_returns_an_error_without_an_error_dialog() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing-中文.exe");
        std::thread::spawn(move || {
            let error =
                open_native(missing.to_str().unwrap()).expect_err("missing executable must fail");
            assert!(error.starts_with("无法打开"), "{error}");
        })
        .join()
        .unwrap();
    }
}
