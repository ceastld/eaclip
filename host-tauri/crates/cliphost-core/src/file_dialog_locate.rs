//! C# `ClipHostFileDialogLocate` / LocateFile Hybrid B: navigate a visible Save As / Open dialog.

use std::path::{Path, PathBuf};

const COMMON_DIALOG_CLASS: &str = "#32770";
const IDC_FILE_NAME: i32 = 1148;
const IDC_VALIDATE_1120: i32 = 1120;
const IDC_ADDRESS_CONTAINER: i32 = 13006;
const IDC_ADDRESS_EDIT: i32 = 1001;
const IDC_EMBEDDED_TREE: i32 = 1021;
const IDOK: usize = 1;
const ID_TOOLBAR_NAVIGATE: usize = 32833;
const WM_SETTEXT: u32 = 0x000C;
const WM_COMMAND: u32 = 0x0111;
const WM_USER: u32 = 0x0400;
const WM_CHAR: u32 = 0x0102;
const EM_SETSEL: u32 = 0x00B1;
const BFFM_SETSELECTIONW: u32 = WM_USER + 103;

/// Navigate a visible file dialog to `path`. Returns false when headless, the path is invalid,
/// or no Save As / Open / Browse dialog is present (caller should fall back to Explorer).
pub fn try_locate(path: &str) -> bool {
    try_locate_result(path).unwrap_or(false)
}

/// An attempted navigation that times out must not be replayed in another
/// dialog or fall through to opening Explorer.
pub fn try_locate_result(path: &str) -> Result<bool, String> {
    try_locate_guarded(path, &|| Ok(()))
}

pub(crate) fn try_locate_guarded(path: &str, verify: &dyn Fn() -> Result<(), String>) -> Result<bool, String> {
    verify()?;
    if crate::clipboard::is_headless() {
        return Ok(false);
    }
    let resolved = resolve_navigation_path(path);
    verify()?;
    let Ok((navigation, is_file)) = resolved else {
        return Ok(false);
    };
    locate_resolved(&navigation, is_file, None, verify)
}

pub fn try_locate_hwnd(hwnd: i64, path: &str) -> bool {
    try_locate_hwnd_result(hwnd, path).unwrap_or(false)
}

pub fn try_locate_hwnd_result(hwnd: i64, path: &str) -> Result<bool, String> {
    try_locate_hwnd_guarded(hwnd, path, || Ok(()))
}

/// Native menus must keep their accepted HWND and input lifetime. Never search
/// for a replacement dialog; validate after path I/O and before each write.
pub fn try_locate_hwnd_guarded(
    hwnd: i64,
    path: &str,
    verify: impl Fn() -> Result<(), String>,
) -> Result<bool, String> {
    verify()?;
    if crate::clipboard::is_headless() || hwnd == 0 {
        return Ok(false);
    }
    let resolved = resolve_navigation_path(path);
    verify()?;
    let Ok((navigation, is_file)) = resolved else {
        return Ok(false);
    };
    let located = locate_resolved(&navigation, is_file, Some(hwnd), &verify)?;
    // A successful native navigation performs its last lifetime check before
    // activating the destination. That activation may legitimately dismiss or
    // fold the source window and retire its original list-input ownership.
    if !located {
        verify()?;
    }
    Ok(located)
}

pub fn is_file_dialog_hwnd(hwnd: i64) -> bool {
    if crate::clipboard::is_headless() || hwnd == 0 {
        return false;
    }
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::HWND;
        let handle = HWND(hwnd as *mut _);
        is_known_file_dialog(handle)
    }
    #[cfg(not(windows))]
    {
        let _ = hwnd;
        false
    }
}

/// A common file dialog needs recognizable navigation controls. The class
/// alone also matches confirmation and error dialogs.
pub fn is_save_as_hwnd(hwnd: i64) -> bool {
    is_file_dialog_hwnd(hwnd)
}

pub fn resolve_navigation_path(path: &str) -> Result<(PathBuf, bool), String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err("Path is empty".into());
    }
    let normalized = std::path::absolute(trimmed).unwrap_or_else(|_| PathBuf::from(trimmed));
    let is_file = normalized.is_file();
    if is_file {
        if normalized.parent().is_none_or(|p| !p.is_dir()) {
            return Err(format!(
                "Parent directory does not exist: {}",
                normalized.display()
            ));
        }
        Ok((normalized, true))
    } else if normalized.is_dir() {
        Ok((normalized, false))
    } else {
        Err(format!(
            "Directory does not exist: {}",
            normalized.display()
        ))
    }
}

fn locate_resolved(
    path: &Path,
    is_file: bool,
    hwnd: Option<i64>,
    verify: &dyn Fn() -> Result<(), String>,
) -> Result<bool, String> {
    #[cfg(windows)]
    {
        locate_windows(path, is_file, hwnd, verify)
    }
    #[cfg(not(windows))]
    {
        let _ = (path, is_file, hwnd, verify);
        Ok(false)
    }
}

#[cfg(windows)]
fn locate_windows(
    path: &Path,
    is_file: bool,
    hwnd: Option<i64>,
    verify: &dyn Fn() -> Result<(), String>,
) -> Result<bool, String> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow;

    let dialog = if let Some(hwnd) = hwnd {
        let handle = HWND(hwnd as *mut _);
        if !is_known_file_dialog(handle) {
            return Ok(false);
        }
        handle
    } else {
        let Some(dialog) = find_file_dialog() else {
            return Ok(false);
        };
        dialog
    };
    verify()?;
    let located = navigate_dialog_windows(dialog, path, is_file, verify)?;
    verify()?;
    if located {
        let _ = unsafe { SetForegroundWindow(dialog) };
    }
    Ok(located)
}

#[cfg(windows)]
fn navigate_dialog_windows(
    dialog: windows::Win32::Foundation::HWND,
    path: &Path,
    is_file: bool,
    verify: &dyn Fn() -> Result<(), String>,
) -> Result<bool, String> {
    // Navigate through the existing dialog controls from this process. Do not
    // load helper code into the application that owns the dialog.
    if try_filename_box(dialog, path, is_file, verify)?
        || try_modern_address_bar(dialog, path, is_file, verify)?
    {
        return Ok(true);
    }
    if try_toolbar_address_bar(dialog, path, verify)? {
        return Ok(true);
    }
    if !is_file {
        Ok(try_browse_for_folder(dialog, path, verify)?
            || try_tree_view_folder(dialog, path, verify)
            || try_embedded_tree_edit(dialog, path, verify))
    } else {
        Ok(false)
    }
}

#[cfg(windows)]
fn try_filename_box(
    dialog: windows::Win32::Foundation::HWND,
    path: &Path,
    is_file: bool,
    verify: &dyn Fn() -> Result<(), String>,
) -> Result<bool, String> {
    let Some(input) = find_filename_edit(dialog) else { return Ok(false) };
    let text = filename_navigation_text(path, is_file);
    if is_file {
        return write_and_commit(dialog, input, &text, true, verify);
    }
    // The breadcrumb exposes the actual location, independently of the
    // filename used to request navigation. Do not mistake command delivery or
    // reading our own filename write back for a completed folder change.
    if find_dialog_breadcrumb(dialog).is_none() {
        return Ok(false);
    }
    verify()?;
    let original_name = read_window_text(input).ok_or_else(|| crate::i18n::tr(
        "无法读取原文件名，已停止跳转。",
        "The original filename could not be read. Navigation stopped.",
    ).to_string())?;
    if dialog_folder_matches(dialog, path) {
        return Ok(true);
    }
    let navigation = write_and_commit(dialog, input, &text, false, verify)
        .and_then(|_| wait_for_dialog_folder(dialog, path, verify));
    let located = match navigation {
        Ok(located) => located,
        Err(error) => {
            // A failed/slow command must not leave our path in the filename.
            // Restoration has the same guard and never issues another command.
            let _ = restore_dialog_filename(dialog, &original_name, &text, verify);
            return Err(error);
        }
    };
    restore_dialog_filename(dialog, &original_name, &text, verify)?;
    if !located {
        return Err(crate::i18n::tr(
            "文件窗口未确认目录跳转，已保留原文件名；请重试。",
            "The file dialog did not confirm the folder change. The original filename was preserved; please retry.",
        ).into());
    }
    Ok(true)
}

#[cfg(windows)]
fn find_dialog_breadcrumb(
    dialog: windows::Win32::Foundation::HWND,
) -> Option<windows::Win32::Foundation::HWND> {
    let address = find_descendant_by_class(dialog, "Address Band Root")?;
    let breadcrumb = find_descendant_by_class(address, "Breadcrumb Parent")?;
    let toolbar = find_dlg_item_descendant(breadcrumb, IDC_ADDRESS_EDIT)?;
    (class_name(toolbar) == "ToolbarWindow32").then_some(toolbar)
}

fn address_folder_path(text: &str) -> Option<&str> {
    fn is_absolute_windows_path(path: &str) -> bool {
        let bytes = path.as_bytes();
        path.starts_with(r"\\") || (bytes.len() >= 3 && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':' && matches!(bytes[2], b'\\' | b'/'))
    }
    let text = text.trim();
    if is_absolute_windows_path(text) {
        return Some(text);
    }
    // The address label is localized. Parse only an explicit rooted path after
    // its separator; a display name such as "Downloads" cannot prove identity.
    let (_, path) = text.split_once([':', '：'])?;
    let path = path.trim();
    is_absolute_windows_path(path).then_some(path)
}

fn same_navigation_folder(actual: &str, expected: &Path) -> bool {
    fn key(path: &str) -> String {
        let path = path.strip_prefix(r"\\?\UNC\")
            .map(|path| format!(r"\\{path}"))
            .unwrap_or_else(|| path.strip_prefix(r"\\?\").unwrap_or(path).to_string());
        path.replace('/', "\\").trim_end_matches('\\').to_lowercase()
    }
    key(actual) == key(&expected.to_string_lossy())
}

#[cfg(windows)]
fn dialog_folder_matches(dialog: windows::Win32::Foundation::HWND, path: &Path) -> bool {
    find_dialog_breadcrumb(dialog)
        .and_then(|toolbar| read_window_text_with_timeout(toolbar, 100))
        .and_then(|text| address_folder_path(&text).map(|path| path.to_string()))
        .is_some_and(|actual| same_navigation_folder(&actual, path))
}

#[cfg(windows)]
fn wait_for_dialog_folder(
    dialog: windows::Win32::Foundation::HWND,
    path: &Path,
    verify: &dyn Fn() -> Result<(), String>,
) -> Result<bool, String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1500);
    loop {
        verify()?;
        if dialog_folder_matches(dialog, path) {
            return Ok(true);
        }
        if std::time::Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[cfg(windows)]
fn restore_dialog_filename(
    dialog: windows::Win32::Foundation::HWND,
    original: &str,
    navigation: &str,
    verify: &dyn Fn() -> Result<(), String>,
) -> Result<(), String> {
    verify()?;
    let edit = find_filename_edit(dialog).ok_or_else(|| crate::i18n::tr(
        "文件名输入框已变化，无法恢复原文件名。",
        "The filename field changed and the original filename could not be restored.",
    ).to_string())?;
    let current = read_window_text(edit).ok_or_else(|| crate::i18n::tr(
        "无法读取跳转后的文件名。",
        "The filename could not be read after navigation.",
    ).to_string())?;
    if current == original {
        return Ok(());
    }
    if !current.is_empty() && current != navigation {
        return Err(crate::i18n::tr(
            "文件名已发生变化，已保留当前内容；请检查文件窗口。",
            "The filename changed. Its current value was preserved; please check the file dialog.",
        ).into());
    }
    if !set_window_text(edit, original, verify)
        || read_window_text(edit).as_deref() != Some(original)
    {
        return Err(crate::i18n::tr(
            "文件窗口未能恢复原文件名，请检查后重试。",
            "The file dialog could not restore the original filename. Please check it and retry.",
        ).into());
    }
    Ok(())
}

#[cfg(windows)]
fn try_modern_address_bar(
    dialog: windows::Win32::Foundation::HWND,
    path: &Path,
    is_file: bool,
    verify: &dyn Fn() -> Result<(), String>,
) -> Result<bool, String> {
    let Some(edit) = find_modern_address_edit(dialog) else {
        return Ok(false);
    };
    write_and_commit(dialog, edit, &filename_navigation_text(path, is_file), is_file, verify)
}

fn filename_navigation_text(path: &Path, is_file: bool) -> String {
    let text = path.to_string_lossy();
    if is_file { text.into_owned() } else { format!("{}\\", text.trim_end_matches(['\\', '/'])) }
}

#[cfg(windows)]
fn write_and_commit(
    dialog: windows::Win32::Foundation::HWND,
    edit: windows::Win32::Foundation::HWND,
    text: &str,
    is_file: bool,
    verify: &dyn Fn() -> Result<(), String>,
) -> Result<bool, String> {
    verify()?;
    if !set_window_text(edit, text, verify) || read_window_text(edit).as_deref() != Some(text) {
        return Err(crate::i18n::tr("文件窗口未接受目标路径，已停止跳转。", "The file dialog did not accept the path. Navigation stopped.").into());
    }
    verify()?;
    if !commit_folder(dialog, is_file, verify) {
        return Err(crate::i18n::tr("文件窗口暂时繁忙，已停止跳转。", "The file dialog is busy. Navigation stopped.").into());
    }
    Ok(true)
}

#[cfg(windows)]
fn try_toolbar_address_bar(
    dialog: windows::Win32::Foundation::HWND,
    path: &Path,
    verify: &dyn Fn() -> Result<(), String>,
) -> Result<bool, String> {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::PostMessageW;
    let Some(edit) = find_toolbar_address_edit(dialog) else {
        return Ok(false);
    };
    verify()?;
    let text = path.to_string_lossy();
    if !set_window_text(edit, &text, verify) || read_window_text(edit).as_deref() != Some(text.as_ref()) {
        return Err(crate::i18n::tr("文件窗口未接受目标路径，已停止跳转。", "The file dialog did not accept the path. Navigation stopped.").into());
    }
    verify()?;
    unsafe {
        PostMessageW(edit, WM_COMMAND, WPARAM(ID_TOOLBAR_NAVIGATE), LPARAM(0))
            .map_err(|error| error.to_string())?;
    }
    Ok(true)
}

#[cfg(windows)]
fn try_browse_for_folder(
    dialog: windows::Win32::Foundation::HWND,
    path: &Path,
    verify: &dyn Fn() -> Result<(), String>,
) -> Result<bool, String> {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        SendMessageTimeoutW, SMTO_ABORTIFHUNG, SMTO_BLOCK,
    };
    // BFFM_SETSELECTIONW belongs to the old SHBrowseForFolder dialog. A
    // common Open/Save dialog can also contain a tree, and successfully
    // delivering this unsupported message does not mean it navigated.
    let Some(namespace) =
        find_descendant_by_class(dialog, "SHBrowseForFolder ShellNameSpace Control")
    else {
        return Ok(false);
    };
    if find_descendant_by_class(namespace, "SysTreeView32").is_none() {
        return Ok(false);
    }
    let text = path.to_string_lossy();
    let mut wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let mut result = 0usize;
    verify()?;
    let sent = unsafe {
        SendMessageTimeoutW(
            dialog,
            BFFM_SETSELECTIONW,
            WPARAM(1),
            LPARAM(wide.as_mut_ptr() as isize),
            SMTO_ABORTIFHUNG | SMTO_BLOCK,
            200,
            Some(&mut result),
        )
    };
    if sent.0 == 0 {
        Err(crate::i18n::tr("文件窗口暂时繁忙，已停止跳转。", "The file dialog is busy. Navigation stopped.").into())
    } else {
        Ok(true)
    }
}

const TV_FIRST: u32 = 0x1100;
const TVM_EXPAND: u32 = TV_FIRST + 2;
const TVM_GETNEXTITEM: u32 = TV_FIRST + 10;
const TVM_SELECTITEM: u32 = TV_FIRST + 11;
const TVM_GETITEMW: u32 = TV_FIRST + 62;
const TVGN_ROOT: usize = 0;
const TVGN_NEXT: usize = 2;
const TVGN_CHILD: usize = 4;
const TVGN_CARET: usize = 9;
const TVE_EXPAND: usize = 2;
const TVIF_TEXT: u32 = 0x0001;

#[repr(C)]
struct TvItemW {
    mask: u32,
    h_item: isize,
    state: u32,
    state_mask: u32,
    psz_text: *mut u16,
    cch_text_max: i32,
    i_image: i32,
    i_selected_image: i32,
    c_children: i32,
    l_param: isize,
}

fn split_folder_path(path: &Path) -> Vec<String> {
    use std::path::Component;
    let full = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut segs = Vec::new();
    for component in full.components() {
        match component {
            Component::Prefix(prefix) => {
                let text = prefix
                    .as_os_str()
                    .to_string_lossy()
                    .trim_end_matches(['\\', '/'])
                    .to_string();
                if !text.is_empty() {
                    segs.push(text);
                }
            }
            Component::Normal(name) => segs.push(name.to_string_lossy().into_owned()),
            _ => {}
        }
    }
    segs
}

fn tree_segment_matches(item_text: &str, segment: &str) -> bool {
    let trimmed = item_text.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.eq_ignore_ascii_case(segment) {
        return true;
    }
    for alias in folder_segment_aliases(segment) {
        if trimmed.eq_ignore_ascii_case(alias)
            || trimmed
                .to_ascii_lowercase()
                .contains(&alias.to_ascii_lowercase())
        {
            return true;
        }
    }
    if segment.len() >= 2 && segment.as_bytes().get(1) == Some(&b':') {
        return trimmed
            .to_ascii_lowercase()
            .contains(&segment[..2].to_ascii_lowercase());
    }
    false
}

fn folder_segment_aliases(segment: &str) -> &'static [&'static str] {
    match segment.to_ascii_lowercase().as_str() {
        "desktop" => &["Desktop", "桌面"],
        "documents" => &["Documents", "文档"],
        "downloads" => &["Downloads", "下载"],
        "pictures" => &["Pictures", "图片"],
        "music" => &["Music", "音乐"],
        "videos" => &["Videos", "视频"],
        "users" => &["Users", "用户"],
        _ => &[],
    }
}

#[cfg(windows)]
fn try_tree_view_folder(
    dialog: windows::Win32::Foundation::HWND,
    path: &Path,
    verify: &dyn Fn() -> Result<(), String>,
) -> bool {
    let Some(tree) = find_folder_tree_view(dialog) else {
        return false;
    };
    expand_tree_view_to_path(tree, path, verify)
}

#[cfg(windows)]
fn find_folder_tree_view(
    dialog: windows::Win32::Foundation::HWND,
) -> Option<windows::Win32::Foundation::HWND> {
    use windows::Win32::UI::WindowsAndMessaging::GetDlgItem;
    let item_zero = hwnd_or_null(unsafe { GetDlgItem(dialog, 0) });
    if !item_zero.0.is_null() && class_name(item_zero) == "SysTreeView32" {
        return Some(item_zero);
    }
    find_descendant_by_class(dialog, "SysTreeView32")
}

#[cfg(windows)]
fn expand_tree_view_to_path(
    tree: windows::Win32::Foundation::HWND,
    path: &Path,
    verify: &dyn Fn() -> Result<(), String>,
) -> bool {
    use std::thread;
    use std::time::Duration;
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::SendMessageW;
    let segments = split_folder_path(path);
    if segments.is_empty() {
        return false;
    }
    let mut current =
        unsafe { SendMessageW(tree, TVM_GETNEXTITEM, WPARAM(TVGN_ROOT), LPARAM(0)) }.0;
    if current == 0 {
        return false;
    }
    for segment in &segments {
        if verify().is_err() {
            return false;
        }
        unsafe {
            SendMessageW(tree, TVM_EXPAND, WPARAM(TVE_EXPAND), LPARAM(current));
        }
        thread::sleep(Duration::from_millis(40));
        if verify().is_err() {
            return false;
        }
        let Some(child) = find_tree_child_by_name(tree, current, segment) else {
            return false;
        };
        current = child;
    }
    if verify().is_err() {
        return false;
    }
    unsafe {
        SendMessageW(tree, TVM_SELECTITEM, WPARAM(TVGN_CARET), LPARAM(current));
    }
    if verify().is_err() {
        return false;
    }
    unsafe {
        SendMessageW(tree, TVM_EXPAND, WPARAM(TVE_EXPAND), LPARAM(current));
    }
    true
}

#[cfg(windows)]
fn find_tree_child_by_name(
    tree: windows::Win32::Foundation::HWND,
    parent_item: isize,
    name: &str,
) -> Option<isize> {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::SendMessageW;
    let mut child = unsafe {
        SendMessageW(
            tree,
            TVM_GETNEXTITEM,
            WPARAM(TVGN_CHILD),
            LPARAM(parent_item),
        )
    }
    .0;
    while child != 0 {
        if let Some(text) = tree_item_text(tree, child) {
            if tree_segment_matches(&text, name) {
                return Some(child);
            }
        }
        child = unsafe { SendMessageW(tree, TVM_GETNEXTITEM, WPARAM(TVGN_NEXT), LPARAM(child)) }.0;
    }
    None
}

#[cfg(windows)]
fn tree_item_text(tree: windows::Win32::Foundation::HWND, item: isize) -> Option<String> {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::SendMessageW;
    let mut buffer = [0u16; 256];
    let mut tv = TvItemW {
        mask: TVIF_TEXT,
        h_item: item,
        state: 0,
        state_mask: 0,
        psz_text: buffer.as_mut_ptr(),
        cch_text_max: buffer.len() as i32,
        i_image: 0,
        i_selected_image: 0,
        c_children: 0,
        l_param: 0,
    };
    let sent = unsafe {
        SendMessageW(
            tree,
            TVM_GETITEMW,
            WPARAM(0),
            LPARAM(&mut tv as *mut TvItemW as isize),
        )
    };
    if sent.0 == 0 {
        return None;
    }
    let len = buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len());
    Some(String::from_utf16_lossy(&buffer[..len]))
}

#[cfg(windows)]
fn try_embedded_tree_edit(
    dialog: windows::Win32::Foundation::HWND,
    path: &Path,
    verify: &dyn Fn() -> Result<(), String>,
) -> bool {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{GetDlgItem, SendMessageW};
    let container = hwnd_or_null(unsafe { GetDlgItem(dialog, IDC_EMBEDDED_TREE) });
    if container.0.is_null() {
        return false;
    }
    let edit = hwnd_or_null(unsafe { GetDlgItem(container, IDC_ADDRESS_EDIT) });
    if edit.0.is_null() || class_name(edit) != "Edit" {
        return false;
    }
    let folder = path.to_string_lossy().trim_end_matches('\\').to_string();
    if !set_window_text(edit, &folder, verify) {
        return false;
    }
    if verify().is_err() {
        return false;
    }
    unsafe {
        SendMessageW(
            edit,
            EM_SETSEL,
            WPARAM(folder.encode_utf16().count()),
            LPARAM(-1),
        );
    }
    if verify().is_err() {
        return false;
    }
    unsafe {
        SendMessageW(edit, WM_CHAR, WPARAM(b'\\' as usize), LPARAM(0));
    }
    true
}

#[cfg(windows)]
fn find_toolbar_address_edit(
    dialog: windows::Win32::Foundation::HWND,
) -> Option<windows::Win32::Foundation::HWND> {
    for root_class in ["Address Band Root", "ReBarWindow32"] {
        if let Some(root) = find_descendant_by_class(dialog, root_class) {
            if let Some(edit) = find_combo_ex_edit(root) {
                return Some(edit);
            }
        }
    }
    let toolbar = find_descendant_by_class(dialog, "ToolbarWindow32")?;
    let combo = find_descendant_by_class(toolbar, "ComboBoxEx32")?;
    find_combo_ex_edit(combo)
}

#[cfg(windows)]
fn find_combo_ex_edit(
    parent: windows::Win32::Foundation::HWND,
) -> Option<windows::Win32::Foundation::HWND> {
    let combo_ex = if class_name(parent) == "ComboBoxEx32" {
        parent
    } else {
        find_descendant_by_class(parent, "ComboBoxEx32")?
    };
    if let Some(combo) = find_descendant_by_class(combo_ex, "ComboBox") {
        if let Some(edit) = find_descendant_by_class(combo, "Edit") {
            return Some(edit);
        }
    }
    find_descendant_by_class(combo_ex, "Edit").or(Some(combo_ex))
}

#[cfg(windows)]
fn find_descendant_by_class(
    parent: windows::Win32::Foundation::HWND,
    class: &str,
) -> Option<windows::Win32::Foundation::HWND> {
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::EnumChildWindows;
    struct State {
        class: String,
        found: HWND,
    }
    unsafe extern "system" fn proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let state = unsafe { &mut *(lparam.0 as *mut State) };
        if class_name(hwnd).eq_ignore_ascii_case(&state.class) {
            state.found = hwnd;
            return BOOL(0);
        }
        BOOL(1)
    }
    let mut state = State {
        class: class.to_string(),
        found: HWND::default(),
    };
    unsafe {
        let _ = EnumChildWindows(
            parent,
            Some(proc),
            LPARAM(&mut state as *mut State as isize),
        );
    }
    if state.found.0.is_null() {
        None
    } else {
        Some(state.found)
    }
}

#[cfg(windows)]
fn commit_folder(
    dialog: windows::Win32::Foundation::HWND,
    is_file: bool,
    verify: &dyn Fn() -> Result<(), String>,
) -> bool {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{IsWindow, SendMessageTimeoutW, SMTO_ABORTIFHUNG, SMTO_BLOCK};
    if !is_file {
        if verify().is_err() {
            return false;
        }
        let mut result = 0;
        if unsafe { SendMessageTimeoutW(dialog, WM_COMMAND, WPARAM(IDOK), LPARAM(0),
            SMTO_ABORTIFHUNG | SMTO_BLOCK, 500, Some(&mut result)) }.0 == 0 {
            return false;
        }
    }
    unsafe { IsWindow(dialog).as_bool() }
}

#[cfg(windows)]
fn set_window_text(
    hwnd: windows::Win32::Foundation::HWND,
    text: &str,
    verify: &dyn Fn() -> Result<(), String>,
) -> bool {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{SendMessageTimeoutW, SMTO_ABORTIFHUNG, SMTO_BLOCK};
    let mut wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    if verify().is_err() {
        return false;
    }
    let mut result = 0;
    let sent = unsafe {
        SendMessageTimeoutW(
            hwnd,
            WM_SETTEXT,
            WPARAM(0),
            LPARAM(wide.as_mut_ptr() as isize),
            SMTO_ABORTIFHUNG | SMTO_BLOCK,
            500,
            Some(&mut result),
        )
    };
    sent.0 != 0 && result != 0
}

#[cfg(windows)]
fn read_window_text(hwnd: windows::Win32::Foundation::HWND) -> Option<String> {
    read_window_text_with_timeout(hwnd, 500)
}

#[cfg(windows)]
fn read_window_text_with_timeout(hwnd: windows::Win32::Foundation::HWND, timeout_ms: u32) -> Option<String> {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{SendMessageTimeoutW, SMTO_ABORTIFHUNG, SMTO_BLOCK, WM_GETTEXT};
    let mut buffer = vec![0u16; 32768];
    let mut result = 0;
    let sent = unsafe { SendMessageTimeoutW(hwnd, WM_GETTEXT, WPARAM(buffer.len()),
        LPARAM(buffer.as_mut_ptr() as isize), SMTO_ABORTIFHUNG | SMTO_BLOCK, timeout_ms, Some(&mut result)) };
    (sent.0 != 0 && result < buffer.len()).then(|| String::from_utf16_lossy(&buffer[..result]))
}

#[cfg(windows)]
fn find_filename_edit(dialog: windows::Win32::Foundation::HWND) -> Option<windows::Win32::Foundation::HWND> {
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{EnumChildWindows, GetDlgCtrlID, GetParent};
    if let Some(control) = find_dlg_item_descendant(dialog, IDC_FILE_NAME) {
        let edit = resolve_edit(control);
        if class_name(edit) == "Edit" { return Some(edit) }
    }
    unsafe extern "system" fn proc(hwnd: HWND, param: LPARAM) -> BOOL {
        if unsafe { GetDlgCtrlID(hwnd) } == IDC_ADDRESS_EDIT && class_name(hwnd) == "Edit"
            && unsafe { GetParent(hwnd) }.is_ok_and(|parent| class_name(parent) == "ComboBox") {
            unsafe { *(param.0 as *mut HWND) = hwnd; }
            return BOOL(0);
        }
        BOOL(1)
    }
    let mut found = HWND::default();
    unsafe { let _ = EnumChildWindows(dialog, Some(proc), LPARAM(&mut found as *mut HWND as isize)); }
    (!found.0.is_null()).then_some(found)
}

#[cfg(windows)]
fn find_modern_address_edit(
    dialog: windows::Win32::Foundation::HWND,
) -> Option<windows::Win32::Foundation::HWND> {
    use windows::Win32::UI::WindowsAndMessaging::GetDlgItem;
    let container = find_dlg_item_descendant(dialog, IDC_ADDRESS_CONTAINER)?;
    let edit = hwnd_or_null(unsafe { GetDlgItem(container, IDC_ADDRESS_EDIT) });
    if edit.0.is_null() {
        None
    } else {
        Some(edit)
    }
}

#[cfg(windows)]
fn find_dlg_item_descendant(
    parent: windows::Win32::Foundation::HWND,
    id: i32,
) -> Option<windows::Win32::Foundation::HWND> {
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{EnumChildWindows, GetDlgCtrlID, GetDlgItem};
    let direct = hwnd_or_null(unsafe { GetDlgItem(parent, id) });
    if !direct.0.is_null() {
        return Some(direct);
    }
    struct State {
        id: i32,
        found: HWND,
    }
    unsafe extern "system" fn proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let state = unsafe { &mut *(lparam.0 as *mut State) };
        if unsafe { GetDlgCtrlID(hwnd) } == state.id {
            state.found = hwnd;
            return BOOL(0);
        }
        BOOL(1)
    }
    let mut state = State {
        id,
        found: HWND::default(),
    };
    unsafe {
        let _ = EnumChildWindows(
            parent,
            Some(proc),
            LPARAM(&mut state as *mut State as isize),
        );
    }
    if state.found.0.is_null() {
        None
    } else {
        Some(state.found)
    }
}

#[cfg(windows)]
fn hwnd_or_null(
    result: windows::core::Result<windows::Win32::Foundation::HWND>,
) -> windows::Win32::Foundation::HWND {
    result.unwrap_or_default()
}

#[cfg(windows)]
fn resolve_edit(control: windows::Win32::Foundation::HWND) -> windows::Win32::Foundation::HWND {
    if control.0.is_null() || class_name(control) == "Edit" {
        return control;
    }
    find_descendant_by_class(control, "Edit").unwrap_or_default()
}

#[cfg(windows)]
fn find_file_dialog() -> Option<windows::Win32::Foundation::HWND> {
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetForegroundWindow, IsWindowVisible,
    };

    struct State {
        best: HWND,
        fallback: HWND,
    }

    unsafe extern "system" fn proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let state = unsafe { &mut *(lparam.0 as *mut State) };
        if unsafe { !IsWindowVisible(hwnd).as_bool() } {
            return BOOL(1);
        }
        if !is_known_file_dialog(hwnd) {
            return BOOL(1);
        }
        if is_strong_match(hwnd) {
            state.best = hwnd;
            return BOOL(0);
        }
        if state.fallback.0.is_null() {
            state.fallback = hwnd;
        }
        BOOL(1)
    }

    let fg = unsafe { GetForegroundWindow() };
    if is_known_file_dialog(fg) {
        return Some(fg);
    }

    let mut state = State {
        best: HWND::default(),
        fallback: HWND::default(),
    };
    unsafe {
        let _ = EnumWindows(Some(proc), LPARAM(&mut state as *mut State as isize));
    }
    if !state.best.0.is_null() {
        Some(state.best)
    } else if !state.fallback.0.is_null() {
        Some(state.fallback)
    } else {
        None
    }
}

#[cfg(windows)]
fn is_common_dialog(hwnd: windows::Win32::Foundation::HWND) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::IsWindowVisible;
    if hwnd.0.is_null() {
        return false;
    }
    if unsafe { !IsWindowVisible(hwnd).as_bool() } {
        return false;
    }
    class_name(hwnd) == COMMON_DIALOG_CLASS
}

#[cfg(windows)]
fn is_strong_match(hwnd: windows::Win32::Foundation::HWND) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::GetDlgItem;
    if !is_common_dialog(hwnd) {
        return false;
    }
    let file_name = find_filename_edit(hwnd).unwrap_or_default();
    let validate = hwnd_or_null(unsafe { GetDlgItem(hwnd, IDC_VALIDATE_1120) });
    let address = hwnd_or_null(unsafe { GetDlgItem(hwnd, IDC_ADDRESS_CONTAINER) });
    !file_name.0.is_null() || !validate.0.is_null() || !address.0.is_null()
}

#[cfg(windows)]
fn is_known_file_dialog(hwnd: windows::Win32::Foundation::HWND) -> bool {
    is_common_dialog(hwnd) && (is_strong_match(hwnd)
        || find_folder_tree_view(hwnd).is_some()
        || find_toolbar_address_edit(hwnd).is_some())
}

#[cfg(windows)]
fn class_name(hwnd: windows::Win32::Foundation::HWND) -> String {
    use windows::Win32::UI::WindowsAndMessaging::GetClassNameW;
    let mut buf = [0u16; 64];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    if n <= 0 {
        String::new()
    } else {
        String::from_utf16_lossy(&buf[..n as usize])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_dialog_guard_rejects_before_any_lookup_or_write() {
        crate::clipboard::force_headless(true);
        assert_eq!(
            try_locate_hwnd_guarded(0, "", || Err("retired menu".into())).unwrap_err(),
            "retired menu"
        );
        assert!(!try_locate_hwnd_guarded(0, "", || Ok(())).unwrap());
    }

    #[cfg(windows)]
    #[test]
    fn native_dialog_write_and_commit_check_guard_before_sending_messages() {
        use std::cell::Cell;
        use windows::Win32::Foundation::HWND;
        let calls = Cell::new(0);
        let retired = || {
            calls.set(calls.get() + 1);
            Err("retired menu".into())
        };
        assert!(!set_window_text(
            HWND::default(),
            "cancelled path",
            &retired
        ));
        assert!(!commit_folder(HWND::default(), false, &retired));
        assert_eq!(calls.get(), 2);
    }

    #[cfg(windows)]
    #[test]
    fn file_dialog_unsupported_browse_message_is_not_navigation_success() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use windows::core::w;
        use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
        use windows::Win32::UI::WindowsAndMessaging::{
            CallWindowProcW, CreateWindowExW, DefWindowProcW, DestroyWindow,
            GetWindowLongPtrW, SendMessageTimeoutW, SetWindowLongPtrW,
            GWLP_USERDATA, GWLP_WNDPROC, SMTO_ABORTIFHUNG, SMTO_BLOCK,
            WINDOW_EX_STYLE, WNDPROC, WS_POPUP,
        };

        struct Probe {
            original: WNDPROC,
            messages: AtomicUsize,
        }
        unsafe extern "system" fn probe_proc(
            hwnd: HWND,
            message: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            let state = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const Probe;
            if state.is_null() {
                return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
            }
            let state = unsafe { &*state };
            if message == BFFM_SETSELECTIONW {
                state.messages.fetch_add(1, Ordering::SeqCst);
                // Unsupported application messages are normally just ignored.
                return LRESULT(0);
            }
            unsafe { CallWindowProcW(state.original, hwnd, message, wparam, lparam) }
        }
        struct Window(HWND);
        impl Drop for Window {
            fn drop(&mut self) {
                unsafe { let _ = DestroyWindow(self.0); }
            }
        }

        // A hidden, owned test window exercises the actual #32770 message
        // behavior without opening a dialog or changing desktop focus.
        let mut probe = Box::new(Probe {
            original: None,
            messages: AtomicUsize::new(0),
        });
        let window = Window(unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(), w!("#32770"),
                w!("ClipHost unsupported browse message fixture"), WS_POPUP,
                0, 0, 1, 1, None, None, None, None,
            ).unwrap()
        });
        unsafe {
            SetWindowLongPtrW(window.0, GWLP_USERDATA, (&mut *probe as *mut Probe) as isize);
            let original = SetWindowLongPtrW(window.0, GWLP_WNDPROC, probe_proc as *const () as isize);
            assert_ne!(original, 0);
            probe.original = Some(std::mem::transmute::<isize,
                unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT>(original));
        }
        let folder = tempfile::tempdir().unwrap();
        let wide: Vec<u16> = folder.path().to_string_lossy().encode_utf16().chain([0]).collect();
        let mut ignored_result = usize::MAX;
        let delivered = unsafe {
            SendMessageTimeoutW(window.0, BFFM_SETSELECTIONW, WPARAM(1),
                LPARAM(wide.as_ptr() as isize), SMTO_ABORTIFHUNG | SMTO_BLOCK,
                200, Some(&mut ignored_result))
        };
        assert_ne!(delivered.0, 0, "the ignored message must still be delivered");
        assert_eq!(ignored_result, 0);
        assert_eq!(probe.messages.swap(0, Ordering::SeqCst), 1);
        assert!(!try_browse_for_folder(window.0, folder.path(), &|| Ok(())).unwrap());
        assert_eq!(probe.messages.load(Ordering::SeqCst), 0,
            "an unrecognized dialog must not receive a browse-only message");
    }

    #[test]
    fn rejects_empty_and_missing_paths() {
        crate::clipboard::force_headless(true);
        assert!(resolve_navigation_path("").is_err());
        assert!(resolve_navigation_path("D:\\cliphost-tauri-missing-locate-path").is_err());
        assert!(!try_locate(""));
        assert!(!try_locate(r"C:\Windows"));
        assert!(!try_locate_hwnd(0, r"C:\Windows"));
        assert!(!is_file_dialog_hwnd(0));
        assert!(!is_save_as_hwnd(0));
    }

    #[test]
    fn directory_navigation_ends_with_separator_and_files_keep_their_name() {
        assert_eq!(filename_navigation_text(Path::new(r"C:\Folder"), false), "C:\\Folder\\");
        assert_eq!(filename_navigation_text(Path::new(r"C:\"), false), "C:\\");
        assert_eq!(filename_navigation_text(Path::new(r"\\server\share"), false), "\\\\server\\share\\");
        assert_eq!(filename_navigation_text(Path::new(r"C:\Folder\draft.txt"), true), r"C:\Folder\draft.txt");
    }

    #[test]
    fn file_dialog_address_verification_requires_an_explicit_absolute_folder() {
        for (label, expected) in [
            (r"Address: D:\Work\资料", r"D:\Work\资料"),
            (r"地址： D:\Work\资料", r"D:\Work\资料"),
            (r"D:\Work\资料", r"D:\Work\资料"),
            (r"Address: \\server\share\folder", r"\\server\share\folder"),
        ] {
            assert_eq!(address_folder_path(label), Some(expected));
        }
        for label in ["Address: Downloads", "地址: 文档", "Downloads", r"Address: D:relative", ""] {
            assert_eq!(address_folder_path(label), None, "{label}");
        }
        assert!(same_navigation_folder(r"D:\WORK\资料\", Path::new(r"d:/work/资料")));
        assert!(same_navigation_folder(r"D:\Work", Path::new(r"\\?\D:\Work\")));
        assert!(same_navigation_folder(r"\\server\share", Path::new(r"\\?\UNC\server\share\")));
        assert!(!same_navigation_folder(r"D:\Other\Work", Path::new(r"D:\Work")));
    }

    #[test]
    fn resolves_existing_directory() {
        crate::clipboard::force_headless(true);
        let dir = tempfile::tempdir().unwrap();
        let (path, is_file) = resolve_navigation_path(dir.path().to_str().unwrap()).unwrap();
        assert!(!is_file);
        assert!(path.is_dir());
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        let (file_path, is_file) = resolve_navigation_path(file.to_str().unwrap()).unwrap();
        assert!(is_file);
        assert!(file_path.is_file());
        assert!(!try_locate_hwnd(0, dir.path().to_str().unwrap()));
    }

    #[test]
    fn split_folder_path_keeps_drive_then_segments() {
        let segs = split_folder_path(Path::new(r"C:\Users\Public"));
        assert_eq!(segs.first().map(|s| s.as_str()), Some("C:"));
        assert!(segs.iter().any(|s| s.eq_ignore_ascii_case("Users")));
        assert!(segs.iter().any(|s| s.eq_ignore_ascii_case("Public")));
    }

    #[test]
    fn tree_segment_matches_drive_alias_and_localized_names() {
        assert!(tree_segment_matches("Local Disk (C:)", "C:"));
        assert!(tree_segment_matches("桌面", "Desktop"));
        assert!(tree_segment_matches("Downloads", "Downloads"));
        assert!(!tree_segment_matches("", "C:"));
        assert!(!tree_segment_matches("Videos", "Music"));
    }
}
