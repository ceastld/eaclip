//! C# `Shell.ExecuteVerb` / `FileUtils.MoveToRecycbin` / `MoveFileOrDirToDir`.

use std::path::Path;

pub fn execute_verb(path: &str, verb: &str) -> Result<(), String> {
    let trimmed = path.trim();
    if trimmed.is_empty() || verb.trim().is_empty() {
        return Ok(());
    }
    if crate::clipboard::is_headless() {
        return Ok(());
    }
    let full = crate::shell_path::normalize_local_shell_path(trimmed)?;
    execute_verb_windows(&full, verb)
}

pub fn show_properties(path: &str) -> Result<(), String> {
    execute_verb(path, "properties")
}

pub fn open_as(path: &str) -> Result<(), String> {
    execute_verb(path, "openas")
}

pub fn cut(path: &str) -> Result<(), String> {
    execute_verb(path, "cut")
}

pub fn recycle(path: &str) -> Result<(), String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Ok(());
    }
    if crate::clipboard::is_headless() {
        return Ok(());
    }
    let full = crate::shell_path::normalize_local_shell_path(trimmed)?;
    if !Path::new(&full).exists() {
        return Ok(());
    }
    recycle_windows(&full)
}

pub fn move_into_dir(source: &str, dir: &str) -> Result<(), String> {
    let src = crate::shell_path::normalize_local_shell_path(source)?;
    let dest_dir = crate::shell_path::normalize_local_shell_path(dir)?;
    let name = Path::new(&src)
        .file_name()
        .ok_or_else(|| "无法解析文件名。".to_string())?;
    let dest = Path::new(&dest_dir).join(name);
    if crate::clipboard::is_headless() {
        return Ok(());
    }
    std::fs::rename(&src, &dest).map_err(|e| format!("移动失败：{e}"))
}

pub fn json_quoted_path(path: &str) -> String {
    serde_json::to_string(path).unwrap_or_else(|_| format!("\"{path}\""))
}

#[cfg(windows)]
fn execute_verb_windows(path: &str, verb: &str) -> Result<(), String> {
    execute_verb_windows_with(path, verb, |info| unsafe {
        windows::Win32::UI::Shell::ShellExecuteExW(info)
    })
}

#[cfg(windows)]
fn execute_verb_windows_with(
    path: &str,
    verb: &str,
    execute: impl FnOnce(&mut windows::Win32::UI::Shell::SHELLEXECUTEINFOW) -> windows::core::Result<()>,
) -> Result<(), String> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Shell::{SEE_MASK_INVOKEIDLIST, SEE_MASK_NOASYNC, SEE_MASK_FLAG_NO_UI, SHELLEXECUTEINFOW};
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOW;
    let _apartment = crate::explorer::ShellApartment::initialize()
        .map_err(|error| format!("无法初始化文件操作“{verb}”：{error}"))?;
    let mut file: Vec<u16> = path.encode_utf16().chain([0]).collect();
    let mut verb_wide: Vec<u16> = verb.encode_utf16().chain([0]).collect();
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        // These verbs run on blocking workers without a message loop. Finish
        // Shell activation before balancing COM; surface failures in the app.
        fMask: SEE_MASK_INVOKEIDLIST | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
        hwnd: HWND::default(),
        lpVerb: PCWSTR(verb_wide.as_mut_ptr()),
        lpFile: PCWSTR(file.as_mut_ptr()),
        nShow: SW_SHOW.0 as i32,
        ..Default::default()
    };
    execute(&mut info).map_err(|error| format!("无法执行文件操作“{verb}”（{path}）：{error}"))
}

#[cfg(not(windows))]
fn execute_verb_windows(_path: &str, _verb: &str) -> Result<(), String> {
    Ok(())
}

#[cfg(windows)]
fn recycle_windows(path: &str) -> Result<(), String> {
    use windows::core::PCWSTR;
    use windows::Win32::UI::Shell::{
        SHFileOperationW, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FO_DELETE, SHFILEOPSTRUCTW,
    };
    let mut from: Vec<u16> = path.encode_utf16().chain([0, 0]).collect();
    let mut op = SHFILEOPSTRUCTW {
        wFunc: FO_DELETE,
        pFrom: PCWSTR(from.as_mut_ptr()),
        fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION).0 as u16,
        ..Default::default()
    };
    let code = unsafe { SHFileOperationW(&mut op) };
    if code == 0 {
        Ok(())
    } else {
        Err(format!("无法删除到回收站（{code}）"))
    }
}

#[cfg(not(windows))]
fn recycle_windows(_path: &str) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn shell_verbs_initialize_com_and_preserve_callers_apartment() {
        use windows::Win32::Foundation::S_OK;
        use windows::Win32::System::Com::{CoGetApartmentType, CoInitializeEx, CoUninitialize,
            APTTYPE, APTTYPEQUALIFIER, APTTYPE_STA, APTTYPE_MAINSTA,
            COINIT_MULTITHREADED, COINIT_APARTMENTTHREADED};
        use windows::Win32::UI::Shell::{SEE_MASK_INVOKEIDLIST, SEE_MASK_NOASYNC, SEE_MASK_FLAG_NO_UI};
        let (ready, started) = std::sync::mpsc::channel();
        let (stop, stopped) = std::sync::mpsc::channel::<()>();
        let owner = std::thread::spawn(move || unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok().unwrap();
            ready.send(()).unwrap();
            let _ = stopped.recv();
            CoUninitialize();
        });
        started.recv().unwrap();
        for caller in [None, Some(COINIT_MULTITHREADED), Some(COINIT_APARTMENTTHREADED)] {
            std::thread::spawn(move || unsafe {
                if let Some(mode) = caller { CoInitializeEx(None, mode).ok().unwrap(); }
                let mut original = APTTYPE::default();
                let mut original_qualifier = APTTYPEQUALIFIER::default();
                CoGetApartmentType(&mut original, &mut original_qualifier).unwrap();
                let path = r"C:\fixture\目录 %中文 &;.txt";
                for verb in ["properties", "openas", "cut"] {
                    execute_verb_windows_with(path, verb, |info| {
                        let mut apartment = APTTYPE::default();
                        let mut qualifier = APTTYPEQUALIFIER::default();
                        CoGetApartmentType(&mut apartment, &mut qualifier).unwrap();
                        if caller.is_some() {
                            assert_eq!((apartment, qualifier), (original, original_qualifier));
                        } else {
                            // An implicit MTA is not the caller's explicit apartment;
                            // this guard legitimately creates a temporary STA.
                            assert!(matches!(apartment, APTTYPE_STA | APTTYPE_MAINSTA));
                        }
                        assert_eq!(info.lpFile.to_string().unwrap(), path);
                        assert_eq!(info.lpVerb.to_string().unwrap(), verb);
                        let required = SEE_MASK_INVOKEIDLIST | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI;
                        assert_eq!(info.fMask & required, required,
                            "background verbs must finish activation and report errors through the app");
                        Ok(())
                    }).unwrap();
                    let mut after = APTTYPE::default();
                    let mut after_qualifier = APTTYPEQUALIFIER::default();
                    CoGetApartmentType(&mut after, &mut after_qualifier).unwrap();
                    assert_eq!((after, after_qualifier), (original, original_qualifier),
                        "Shell verb changed the caller's COM apartment");
                }
                if caller.is_some() { CoUninitialize(); }
                assert_eq!(CoInitializeEx(None, COINIT_MULTITHREADED), S_OK,
                    "Shell verb left an explicit COM initialization count");
                CoUninitialize();
            }).join().unwrap();
        }
        drop(stop);
        owner.join().unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn shell_verbs_native_failure_reports_context_without_leaking_com() {
        use windows::Win32::Foundation::{E_ACCESSDENIED, S_OK};
        use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
        std::thread::spawn(|| unsafe {
            let error = execute_verb_windows_with(r"C:\fixture\test.txt", "properties", |_| {
                Err(windows::core::Error::from_hresult(E_ACCESSDENIED))
            }).unwrap_err();
            assert!(error.contains("properties") && error.contains("test.txt") && error.contains("80070005"), "{error}");
            // An implicit MTA can exist because another test initialized COM.
            // S_OK verifies this thread has no remaining explicit initialization.
            assert_eq!(CoInitializeEx(None, COINIT_MULTITHREADED), S_OK,
                "failed Shell verb left an explicit COM initialization count");
            CoUninitialize();
        }).join().unwrap();
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "launches an explicitly supplied owned no-UI executable under workspace .tmp; run alone"]
    fn native_shell_verb_launches_owned_fixture_from_uninitialized_sta_and_mta_workers() {
        use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED, COINIT_APARTMENTTHREADED};
        let path = std::path::PathBuf::from(std::env::var("CLIPHOST_SHELL_VERB_FIXTURE_EXE")
            .expect("explicit owned fixture required")).canonicalize().unwrap();
        let temp_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(5).unwrap()
            .join(".tmp").canonicalize().unwrap();
        assert!(path.starts_with(temp_root) && path.file_name().unwrap() == "ClipHostShellFixture.exe");
        let output = path.with_extension("completed.txt");
        let count = |path: &std::path::Path| std::fs::read_to_string(path).unwrap_or_default().lines().count();
        for caller in [None, Some(COINIT_MULTITHREADED), Some(COINIT_APARTMENTTHREADED)] {
            let path = path.clone();
            let output = output.clone();
            std::thread::spawn(move || unsafe {
                let before = count(&output);
                if let Some(mode) = caller { CoInitializeEx(None, mode).ok().unwrap(); }
                let result = execute_verb_windows(path.to_str().unwrap(), "open");
                if caller.is_some() { CoUninitialize(); }
                result.unwrap();
                let started = std::time::Instant::now();
                while count(&output) == before && started.elapsed() < std::time::Duration::from_secs(5) {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                assert_eq!(count(&output), before + 1, "owned child did not finish after Shell activation");
            }).join().unwrap();
        }
    }

    #[test]
    fn headless_shell_verbs_are_noop() {
        crate::clipboard::force_headless(true);
        assert!(show_properties(r"C:\Windows").is_ok());
        assert!(open_as(r"C:\Windows\notepad.exe").is_ok());
        assert!(cut(r"C:\Windows\notepad.exe").is_ok());
        assert!(recycle(r"C:\cliphost-tauri-missing-recycle").is_ok());
        assert!(move_into_dir(r"C:\Windows\notepad.exe", r"C:\Windows").is_ok());
        assert_eq!(json_quoted_path(r"D:\share"), r#""D:\\share""#);
    }
}
