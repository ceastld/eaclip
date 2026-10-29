//! Snipaste / PixPin pin-overlay, matching C# `SnipastePinService` / `PixPinService`.
//! Availability is “process is running”, not merely on PATH.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::model::{ClipKind, ClipRecord};
use crate::paths::AppPaths;

/// C# PixPin script (typo `CilpBoard` is required by PixPin).
const PIXPIN_PIN_SCRIPT: &str = "pixpin.pinFromCilpBoard()";

pub fn status() -> (bool, bool) {
    if crate::clipboard::is_headless() {
        return (false, false);
    }
    (
        resolve_running_exe("snipaste").is_some(),
        resolve_running_exe("pixpin").is_some(),
    )
}

pub fn pin_clipboard(provider: &str) -> Result<(), String> {
    pin_clipboard_guarded(provider, || Ok(()))
}

pub(crate) fn pin_clipboard_guarded(
    provider: &str,
    verify: impl Fn() -> Result<(), String>,
) -> Result<(), String> {
    verify()?;
    if crate::clipboard::is_headless() {
        return Err("pin overlay is disabled in headless mode".into());
    }
    match provider.trim().to_ascii_lowercase().as_str() {
        "snipaste" => {
            // C# `SnipastePinService.RunCommand`: FileName = "snipaste", UseShellExecute = true.
            shell_open("snipaste", "paste", &verify)?;
            std::thread::sleep(Duration::from_millis(300));
            verify()
        }
        "pixpin" => {
            let (exe, parameters) = pixpin_command_guarded(|| resolve_running_exe("pixpin"), &verify)?;
            shell_open(&exe.to_string_lossy(), &parameters, &verify)?;
            std::thread::sleep(Duration::from_millis(300));
            verify()
        }
        _ => Err("Unknown pin overlay provider.".into()),
    }
}

fn pixpin_command_guarded(
    resolve: impl FnOnce() -> Option<PathBuf>,
    verify: &impl Fn() -> Result<(), String>,
) -> Result<(PathBuf, String), String> {
    let exe = resolve();
    verify()?;
    let exe = exe.ok_or_else(|| {
        "PixPin is not running or its executable path could not be resolved.".to_string()
    })?;
    // C#: Arguments = $"-r {JsonSerializer.Serialize(javascript)}"
    let quoted = serde_json::to_string(PIXPIN_PIN_SCRIPT).unwrap_or_else(|_| {
        format!("\"{PIXPIN_PIN_SCRIPT}\"")
    });
    Ok((exe, format!("-r {quoted}")))
}

/// C# `IsPinSupportedType`: `TextClipItem` (includes HTML/QQ) or image/file.
pub fn is_pin_supported_type(kind: ClipKind) -> bool {
    matches!(
        kind,
        ClipKind::TextClipItem
            | ClipKind::HtmlClipItem
            | ClipKind::QQRichTextItem
            | ClipKind::ImageClipItem
            | ClipKind::FileClipItem
    )
}

/// C# `RunPinOverlaySequenceAsync`: batch `SetToClipboard` only when every item `is TextClipItem`.
pub fn all_text_clip_items(items: &[ClipRecord]) -> bool {
    !items.is_empty()
        && items.iter().all(|item| {
            matches!(
                item.kind,
                ClipKind::TextClipItem | ClipKind::HtmlClipItem | ClipKind::QQRichTextItem
            )
        })
}

/// C# `ClipItemContentReadyForPin`.
pub fn item_ready_for_pin(item: &ClipRecord, paths: &AppPaths) -> bool {
    if !is_pin_supported_type(item.kind) {
        return false;
    }
    match item.kind {
        ClipKind::TextClipItem | ClipKind::HtmlClipItem | ClipKind::QQRichTextItem => item
            .body_text
            .as_deref()
            .is_some_and(|text| !text.trim().is_empty()),
        ClipKind::FileClipItem => item.file_paths.iter().any(|path| {
            let path = path.trim();
            !path.is_empty() && Path::new(path).exists()
        }),
        ClipKind::ImageClipItem => item.image_path.as_deref().is_some_and(|path| {
            let file = Path::new(path);
            file.is_file() && paths.is_allowed_image_content_file(file)
        }),
        _ => false,
    }
}

fn shell_open(
    file: &str,
    parameters: &str,
    verify: &impl Fn() -> Result<(), String>,
) -> Result<(), String> {
    #[cfg(windows)]
    unsafe {
        use windows::core::{w, HSTRING, PCWSTR};
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::Shell::ShellExecuteW;
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

        let file = HSTRING::from(file);
        let params = HSTRING::from(parameters);
        verify()?;
        let rc = ShellExecuteW(
            HWND::default(),
            w!("open"),
            PCWSTR(file.as_ptr()),
            PCWSTR(params.as_ptr()),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
        if rc.0 as isize <= 32 {
            return Err(format!("ShellExecute failed ({})", rc.0 as isize));
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = (file, parameters);
        verify()?;
        Err("pin overlay is Windows-only".into())
    }
}

pub fn resolve_running_exe(stem: &str) -> Option<PathBuf> {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
            TH32CS_SNAPPROCESS,
        };

        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
        if snap == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut found = None;
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                let raw = String::from_utf16_lossy(
                    entry
                        .szExeFile
                        .split(|c| *c == 0)
                        .next()
                        .unwrap_or(&[]),
                );
                let file_stem = Path::new(&raw)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or(&raw);
                let needle = Path::new(stem)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or(stem);
                if file_stem.eq_ignore_ascii_case(needle) {
                    found = process_image_path(entry.th32ProcessID);
                    break;
                }
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
        found
    }
    #[cfg(not(windows))]
    {
        let _ = stem;
        None
    }
}

#[cfg(windows)]
unsafe fn process_image_path(pid: u32) -> Option<PathBuf> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    if pid == 0 {
        return None;
    }
    let proc = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
    let mut buf = vec![0u16; 520];
    let mut size = buf.len() as u32;
    let path = if QueryFullProcessImageNameW(
        proc,
        PROCESS_NAME_WIN32,
        windows::core::PWSTR(buf.as_mut_ptr()),
        &mut size,
    )
    .is_ok()
    {
        Some(PathBuf::from(String::from_utf16_lossy(&buf[..size as usize])))
    } else {
        None
    };
    let _ = CloseHandle(proc);
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ClipKind, ClipRecord};
    use tempfile::tempdir;

    #[test]
    fn native_pin_provider_rechecks_after_pixpin_resolution() {
        use std::cell::Cell;
        let retired = Cell::new(false);
        let stub = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../.tmp/owned-pixpin-provider-stub.exe");
        let error = pixpin_command_guarded(
            || { retired.set(true); Some(stub.clone()) },
            &|| if retired.get() { Err("retired provider request".into()) } else { Ok(()) },
        ).unwrap_err();
        assert_eq!(error, "retired provider request");
        let (path, parameters) = pixpin_command_guarded(|| Some(stub.clone()), &|| Ok(())).unwrap();
        assert_eq!(path, stub);
        assert_eq!(parameters, "-r \"pixpin.pinFromCilpBoard()\"");
    }

    #[test]
    fn native_pin_provider_rejects_before_shell_execute() {
        // The guard fails after command strings are prepared, before the
        // Win32 launch. No process resolution, provider or real input occurs.
        let stub = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../.tmp/owned-pin-provider-stub.exe");
        let error = shell_open(&stub.to_string_lossy(), "paste", &|| {
            Err("retired provider request".into())
        }).unwrap_err();
        assert_eq!(error, "retired provider request");
    }

    #[test]
    fn headless_status_is_unavailable() {
        crate::clipboard::force_headless(true);
        assert_eq!(super::status(), (false, false));
        assert!(super::pin_clipboard("snipaste").is_err());
    }

    #[test]
    fn pin_ready_matches_csharp_supported_types() {
        crate::clipboard::force_headless(true);
        let dir = tempdir().unwrap();
        let paths = AppPaths::from_data_root(dir.path().join("BetterClipTauri"));
        paths.ensure().unwrap();
        let mut text = ClipRecord::new_text("hello", "a");
        assert!(item_ready_for_pin(&text, &paths));
        text.body_text = Some("   ".into());
        assert!(!item_ready_for_pin(&text, &paths));

        let mut link = ClipRecord::new_text("https://example.com", "a");
        link.kind = ClipKind::LinkClipItem;
        assert!(!is_pin_supported_type(link.kind));
        assert!(!item_ready_for_pin(&link, &paths));

        let mut html = ClipRecord::new_text("<b>x</b>", "a");
        html.kind = ClipKind::HtmlClipItem;
        let mut image = ClipRecord::new_text("", "a");
        image.kind = ClipKind::ImageClipItem;
        image.body_text = None;
        image.image_path = Some(paths.image_temp.join("nope.png").to_string_lossy().into_owned());
        assert!(!item_ready_for_pin(&image, &paths));
        let ok_img = paths.image_temp.join("pin.png");
        std::fs::write(&ok_img, b"png").unwrap();
        image.image_path = Some(ok_img.to_string_lossy().into_owned());
        assert!(item_ready_for_pin(&image, &paths));

        let mut file = ClipRecord::new_text("", "a");
        file.kind = ClipKind::FileClipItem;
        file.body_text = None;
        file.file_paths = vec![ok_img.to_string_lossy().into_owned()];
        assert!(item_ready_for_pin(&file, &paths));
        file.file_paths = vec!["D:\\missing-pin-file.bin".into()];
        assert!(!item_ready_for_pin(&file, &paths));

        assert!(all_text_clip_items(&[html.clone(), text.clone()]));
        assert!(!all_text_clip_items(&[html, image]));
        assert!(!all_text_clip_items(&[link]));
    }
}
