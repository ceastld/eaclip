//! C# RevealExistingLocalPath: dialog first, otherwise select a file or open a folder.

pub(crate) fn reveal(
    path: &str,
    open_folder: impl FnOnce(&str) -> Result<(), String>,
) -> Result<(), String> {
    reveal_with_dialog(path, crate::file_dialog_locate::try_locate_result, open_folder)
}

pub(crate) fn reveal_with_dialog(
    path: &str,
    locate_dialog: impl FnOnce(&str) -> Result<bool, String>,
    open_folder: impl FnOnce(&str) -> Result<(), String>,
) -> Result<(), String> {
    reveal_with(
        path,
        locate_dialog,
        select_file,
        open_folder,
    )
}

fn reveal_with(
    path: &str,
    locate_dialog: impl FnOnce(&str) -> Result<bool, String>,
    select_file: impl FnOnce(&str) -> Result<(), String>,
    open_folder: impl FnOnce(&str) -> Result<(), String>,
) -> Result<(), String> {
    let full = crate::shell_path::normalize_local_shell_path(path)?;
    let metadata = std::fs::metadata(&full).map_err(|_| format!("路径不存在或无法访问：{full}"))?;
    if !metadata.is_file() && !metadata.is_dir() {
        return Err(format!("目标不是文件或文件夹：{full}"));
    }
    if locate_dialog(&full)? {
        return Ok(());
    }
    if metadata.is_dir() {
        open_folder(&full)
    } else {
        select_file(&full)
    }
}

fn select_file(path: &str) -> Result<(), String> {
    if crate::clipboard::is_headless() {
        return Ok(());
    }
    select_file_native(path)
}

#[cfg(windows)]
fn select_file_native(path: &str) -> Result<(), String> {
    use windows::core::PCWSTR;
    use windows::Win32::UI::Shell::{
        Common::ITEMIDLIST, ILFree, SHOpenFolderAndSelectItems, SHParseDisplayName,
    };

    struct ItemIdList(*mut ITEMIDLIST);
    impl Drop for ItemIdList {
        fn drop(&mut self) {
            unsafe { ILFree(Some(self.0)) };
        }
    }
    let _apartment = crate::explorer::ShellApartment::initialize()
        .map_err(|error| format!("无法初始化定位操作：{error}"))?;
    let wide: Vec<u16> = path.encode_utf16().chain([0]).collect();
    let mut item = ItemIdList(std::ptr::null_mut());
    unsafe { SHParseDisplayName(PCWSTR(wide.as_ptr()), None, &mut item.0, 0, None) }
        .map_err(|error| format!("无法解析定位路径“{path}”：{error}"))?;
    if item.0.is_null() {
        return Err(format!("无法解析定位路径“{path}”"));
    }
    // cidl == 0 accepts the item's absolute PIDL and selects it in its parent.
    // https://learn.microsoft.com/windows/win32/api/shlobj_core/nf-shlobj_core-shopenfolderandselectitems
    unsafe { SHOpenFolderAndSelectItems(item.0, None, 0) }
        .map_err(|error| format!("无法在资源管理器中定位“{path}”：{error}"))
}

#[cfg(not(windows))]
fn select_file_native(path: &str) -> Result<(), String> {
    let parent = std::path::Path::new(path)
        .parent()
        .ok_or_else(|| "无法解析目标文件夹路径。".to_string())?;
    crate::shell_open::open(&parent.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn file_path_actions_reveal_dialog_precedes_explorer_for_files_and_directories() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("%中文 +#.txt");
        std::fs::write(&file, "fixture").unwrap();
        for path in [&file, dir.path()] {
            let uri = url::Url::from_file_path(path).unwrap();
            let observed = RefCell::new(String::new());
            reveal_with(
                uri.as_str(),
                |full| {
                    *observed.borrow_mut() = full.to_string();
                    Ok(true)
                },
                |_| panic!("dialog navigation must not also select in Explorer"),
                |_| panic!("dialog navigation must not also open a folder"),
            )
            .unwrap();
            assert_eq!(std::path::Path::new(&*observed.borrow()), path);
        }
    }

    #[test]
    fn file_path_actions_reveal_fallback_preserves_target_and_returns_failures() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file, %中文#.txt");
        std::fs::write(&file, "fixture").unwrap();
        let error = reveal_with(
            file.to_str().unwrap(),
            |_| Ok(false),
            |full| {
                assert_eq!(std::path::Path::new(full), file);
                Err("Explorer selection failed".into())
            },
            |_| panic!("file must be selected, not opened"),
        )
        .unwrap_err();
        assert_eq!(error, "Explorer selection failed");
        let error = reveal_with(
            dir.path().to_str().unwrap(),
            |_| Ok(false),
            |_| panic!("folder must be opened, not selected in its parent"),
            |full| {
                assert_eq!(std::path::Path::new(full), dir.path());
                Err("folder launch failed".into())
            },
        )
        .unwrap_err();
        assert_eq!(error, "folder launch failed");
        reveal_with(
            file.to_str().unwrap(),
            |_| Ok(false),
            |_| Ok(()),
            |_| unreachable!(),
        )
        .unwrap();
        for invalid in [
            dir.path().join("missing").to_string_lossy().into_owned(),
            "file:relative".into(),
        ] {
            assert!(reveal_with(
                &invalid,
                |_| panic!("invalid path touched a dialog"),
                |_| panic!("invalid path touched Explorer"),
                |_| panic!("invalid path launched")
            )
            .is_err());
        }
    }

    #[test]
    fn file_path_actions_busy_dialog_does_not_replay_in_explorer() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(reveal_with(dir.path().to_str().unwrap(),
            |_| Err("dialog is busy".into()),
            |_| panic!("an attempted dialog navigation must not be replayed"),
            |_| panic!("an attempted dialog navigation must not open Explorer"),
        ).unwrap_err(), "dialog is busy");
    }

    #[cfg(windows)]
    #[test]
    fn file_path_actions_native_selection_rejects_missing_item_without_opening_explorer() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing %中文.txt");
        std::thread::spawn(move || {
            let error = select_file_native(missing.to_str().unwrap()).unwrap_err();
            assert!(error.starts_with("无法解析定位路径"), "{error}");
        })
        .join()
        .unwrap();
    }
}
