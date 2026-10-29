//! C# `ClipHostFileDialogs` — native IFileOpenDialog pickers.

use std::path::Path;
#[cfg(all(windows, not(test)))]
use std::path::PathBuf;

pub(crate) const NATIVE_PICKER_RETIRED: &str = "文件选择会话已失效，已停止选择。";

// Blocking workers are reused. Balance every successful COM initialization
// (including S_FALSE) after all dialog interfaces have been dropped, on the
// same thread; otherwise a cancelled picker leaves that worker in an STA.
#[cfg(windows)]
struct StaApartment(std::marker::PhantomData<std::rc::Rc<()>>);

#[cfg(windows)]
impl StaApartment {
    fn initialize() -> windows::core::Result<Self> {
        use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?; }
        Ok(Self(std::marker::PhantomData))
    }
}

#[cfg(windows)]
impl Drop for StaApartment {
    fn drop(&mut self) {
        unsafe { windows::Win32::System::Com::CoUninitialize(); }
    }
}

/// Native Vista+ folder picker (CommonOpenFileDialog: folder + multi-select). Empty when headless.
pub fn pick_directories(initial: Option<&str>, owner_hwnd: i64) -> Result<Vec<String>, String> {
    pick_directories_guarded(initial, owner_hwnd, &|| Ok(()))
}

pub(crate) fn pick_directories_guarded(
    initial: Option<&str>, owner_hwnd: i64,
    verify: &impl Fn() -> Result<(), String>,
) -> Result<Vec<String>, String> {
    pick_open_guarded(
        initial,
        owner_hwnd,
        OpenPickOptions {
            title: "选择文件夹",
            folders: true,
            multi: true,
            files_only: false,
            filters: &[],
        },
        verify,
    )
}

/// Native OpenFileDialog Multiselect — C# `ClipHostFileDialogs.PickFiles`.
pub fn pick_files(initial: Option<&str>, owner_hwnd: i64) -> Result<Vec<String>, String> {
    pick_files_guarded(initial, owner_hwnd, &|| Ok(()))
}

pub(crate) fn pick_files_guarded(
    initial: Option<&str>, owner_hwnd: i64,
    verify: &impl Fn() -> Result<(), String>,
) -> Result<Vec<String>, String> {
    pick_open_guarded(
        initial,
        owner_hwnd,
        OpenPickOptions {
            title: "选择文件",
            folders: false,
            multi: true,
            files_only: true,
            filters: &[],
        },
        verify,
    )
}

/// Native OpenFileDialog single image — C# `ClipHostFileDialogs.PickImageFile`.
pub fn pick_image_file(initial: Option<&str>, owner_hwnd: i64) -> Result<Option<String>, String> {
    pick_image_file_guarded(initial, owner_hwnd, &|| Ok(()))
}

pub(crate) fn pick_image_file_guarded(
    initial: Option<&str>, owner_hwnd: i64,
    verify: &impl Fn() -> Result<(), String>,
) -> Result<Option<String>, String> {
    Ok(pick_open_guarded(
        initial,
        owner_hwnd,
        OpenPickOptions {
            title: "选择背景图片",
            folders: false,
            multi: false,
            files_only: true,
            filters: &[
                FileFilter {
                    name: "图片文件",
                    spec: "*.png;*.jpg;*.jpeg;*.gif;*.bmp;*.webp",
                },
                FileFilter {
                    name: "所有文件",
                    spec: "*.*",
                },
            ],
        },
        verify,
    )?
    .into_iter()
    .next())
}

/// Native FolderBrowserDialog — C# `ClipHostFileDialogs` / loopback `pick-folder`.
pub fn pick_folder(initial: Option<&str>, owner_hwnd: i64) -> Result<Option<String>, String> {
    pick_folder_guarded(initial, owner_hwnd, &|| Ok(()))
}

pub(crate) fn pick_folder_guarded(
    initial: Option<&str>, owner_hwnd: i64,
    verify: &impl Fn() -> Result<(), String>,
) -> Result<Option<String>, String> {
    verify()?;
    #[cfg(test)]
    {
        // Unit tests use the explicit provider even for the legacy browser;
        // they must never fall through to SHBrowseForFolderW.
        return Ok(pick_open_guarded(initial, owner_hwnd, OpenPickOptions {
            title: "选择文件夹", folders: true, multi: false, files_only: false, filters: &[],
        }, verify)?.into_iter().next());
    }
    #[cfg(not(test))]
    {
        if crate::clipboard::is_headless() || !crate::clipboard::native_dialogs_enabled() {
            verify()?;
            return Ok(None);
        }
        #[cfg(windows)]
        { pick_folder_windows(initial, owner_hwnd, verify) }
        #[cfg(not(windows))]
        { let _ = (initial, owner_hwnd); verify()?; Ok(None) }
    }
}

pub(crate) fn dialogs_enabled() -> bool {
    // An explicit thread-local test provider is the only way a headless unit
    // test can exercise Host API error propagation; it cannot reach native UI.
    #[cfg(test)]
    if PICKER_TEST_PROVIDER.with(|provider| provider.borrow().is_some()) {
        return true;
    }
    crate::clipboard::native_dialogs_enabled()
}

struct FileFilter {
    name: &'static str,
    spec: &'static str,
}

#[derive(Clone, Copy)]
struct OpenPickOptions {
    title: &'static str,
    folders: bool,
    multi: bool,
    files_only: bool,
    filters: &'static [FileFilter],
}

fn pick_open_guarded(
    initial: Option<&str>, owner_hwnd: i64, options: OpenPickOptions,
    verify: &impl Fn() -> Result<(), String>,
) -> Result<Vec<String>, String> {
    verify()?;
    #[cfg(test)]
    {
        // Every unit-test selection is explicit. A missing, wrong or unknown
        // stub never falls through to COM.Show, even if dialogs are enabled.
        let result = PICKER_TEST_PROVIDER.with(|provider| {
            let mut provider = provider.borrow_mut();
            let Some(provider) = provider.as_mut() else { return Ok(Vec::new()); };
            let requested = if options.folders {
                if options.multi { PickerTestKind::Directories } else { PickerTestKind::Folder }
            }
                else if options.multi { PickerTestKind::Files } else { PickerTestKind::Image };
            if provider.kind != requested || requested == PickerTestKind::Unknown {
                return Err("Unknown native picker test provider.".into());
            }
            (provider.run)(initial, owner_hwnd, options, verify)
        });
        verify()?;
        result
    }
    #[cfg(not(test))]
    {
        if crate::clipboard::is_headless() || !crate::clipboard::native_dialogs_enabled() {
            verify()?;
            return Ok(Vec::new());
        }
        #[cfg(windows)]
        { pick_open_windows(initial, owner_hwnd, options, verify) }
        #[cfg(not(windows))]
        { let _ = (initial, owner_hwnd, options); verify()?; Ok(Vec::new()) }
    }
}

/// The prepared dialog never leaves its worker. Rechecking even unsuccessful
/// prepare/Show/results prevents a retired operation becoming this page's cancel.
fn run_guarded_open<T>(
    verify: &impl Fn() -> Result<(), String>,
    prepare: impl FnOnce() -> Result<T, String>,
    show: impl FnOnce(&T) -> Result<bool, String>,
    read: impl FnOnce(&T) -> Result<Vec<String>, String>,
) -> Result<Vec<String>, String> {
    verify()?;
    let prepared = prepare();
    verify()?;
    let prepared = prepared?;
    verify()?;
    let shown = show(&prepared);
    verify()?;
    if !shown? { return Ok(Vec::new()); }
    let paths = read(&prepared);
    verify()?;
    paths
}

#[cfg(windows)]
fn dialog_show_result(result: windows::core::Result<()>) -> Result<bool, String> {
    use windows::Win32::Foundation::ERROR_CANCELLED;
    match result {
        Ok(()) => Ok(true),
        Err(error) if error.code() == windows::core::HRESULT::from_win32(ERROR_CANCELLED.0) => Ok(false),
        Err(error) => Err(format!("无法显示文件选择框：{error}")),
    }
}

#[cfg(all(windows, not(test)))]
fn pick_open_windows(
    initial: Option<&str>, owner_hwnd: i64, options: OpenPickOptions,
    verify: &impl Fn() -> Result<(), String>,
) -> Result<Vec<String>, String> {
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
    use windows::Win32::UI::Shell::{
        FileOpenDialog, IFileOpenDialog, FOS_ALLOWMULTISELECT, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS,
    };
    use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;

    struct PreparedDialog {
        // Struct fields drop in declaration order: release COM before the STA.
        dialog: IFileOpenDialog,
        _apartment: StaApartment,
    }
    run_guarded_open(verify, || unsafe {
        let apartment = StaApartment::initialize().map_err(|error| format!("无法初始化文件选择框：{error}"))?;
        let dialog = CoCreateInstance::<_, IFileOpenDialog>(&FileOpenDialog, None, CLSCTX_ALL)
            .map_err(|error| format!("无法创建文件选择框：{error}"))?;
        let mut flags = dialog.GetOptions().map_err(|error| format!("无法读取文件选择框选项：{error}"))?;
        flags |= FOS_FORCEFILESYSTEM;
        if options.folders {
            flags |= FOS_PICKFOLDERS;
        }
        if options.multi {
            flags |= FOS_ALLOWMULTISELECT;
        }
        dialog.SetOptions(flags).map_err(|error| format!("无法设置文件选择框选项：{error}"))?;
        let title = HSTRING::from(options.title);
        dialog.SetTitle(&title).map_err(|error| format!("无法设置文件选择框标题：{error}"))?;
        apply_initial(&dialog, initial);
        if !options.filters.is_empty() {
            let mut names: Vec<Vec<u16>> = options
                .filters
                .iter()
                .map(|f| f.name.encode_utf16().chain([0]).collect())
                .collect();
            let mut specs: Vec<Vec<u16>> = options
                .filters
                .iter()
                .map(|f| f.spec.encode_utf16().chain([0]).collect())
                .collect();
            let specs_api: Vec<COMDLG_FILTERSPEC> = names
                .iter_mut()
                .zip(specs.iter_mut())
                .map(|(name, spec)| COMDLG_FILTERSPEC {
                    pszName: PCWSTR(name.as_mut_ptr()),
                    pszSpec: PCWSTR(spec.as_mut_ptr()),
                })
                .collect();
            dialog.SetFileTypes(&specs_api).map_err(|error| format!("无法设置文件选择框类型：{error}"))?;
        }
        Ok(PreparedDialog { dialog, _apartment: apartment })
    }, |prepared| unsafe {
        let owner = HWND(owner_hwnd as *mut _);
        verify()?;
        dialog_show_result(prepared.dialog.Show(owner))
    }, |prepared| unsafe {
        verify()?;
        let results = prepared.dialog.GetResults();
        verify()?;
        let results = results.map_err(|error| format!("无法读取文件选择结果：{error}"))?;
        collect_shell_item_paths(&results, options.folders, options.files_only, verify)
    })
}

#[cfg(all(windows, not(test)))]
fn apply_initial(dialog: &windows::Win32::UI::Shell::IFileOpenDialog, initial: Option<&str>) {
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::UI::Shell::{IShellItem, SHCreateItemFromParsingName};
    let Some(start) = initial.map(str::trim).filter(|s| !s.is_empty()) else {
        return;
    };
    let full = std::path::absolute(start).unwrap_or_else(|_| PathBuf::from(start));
    if full.is_dir() {
        let mut wide: Vec<u16> = full.to_string_lossy().encode_utf16().chain([0]).collect();
        if let Ok(item) =
            unsafe { SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(wide.as_mut_ptr()), None) }
        {
            let _ = unsafe { dialog.SetFolder(&item) };
        }
        return;
    }
    if let Some(parent) = full.parent().filter(|p| p.is_dir()) {
        let mut wide: Vec<u16> = parent.to_string_lossy().encode_utf16().chain([0]).collect();
        if let Ok(item) =
            unsafe { SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(wide.as_mut_ptr()), None) }
        {
            let _ = unsafe { dialog.SetFolder(&item) };
        }
    }
    if full.is_file() {
        if let Some(name) = full.file_name() {
            let _ = unsafe { dialog.SetFileName(&HSTRING::from(name.to_string_lossy().as_ref())) };
        }
    }
}

#[cfg(all(windows, not(test)))]
fn collect_shell_item_paths(
    results: &windows::Win32::UI::Shell::IShellItemArray,
    allow_dirs: bool,
    files_only: bool,
    verify: &impl Fn() -> Result<(), String>,
) -> Result<Vec<String>, String> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::SIGDN_FILESYSPATH;
    struct ShellPathName(windows::core::PWSTR);
    impl Drop for ShellPathName {
        fn drop(&mut self) { unsafe { CoTaskMemFree(Some(self.0.0 as *const _)); } }
    }
    verify()?;
    let count = unsafe { results.GetCount() };
    verify()?;
    let count = count.map_err(|error| format!("无法读取文件选择结果数量：{error}"))?;
    let mut paths = Vec::new();
    for index in 0..count {
        verify()?;
        let item = unsafe { results.GetItemAt(index) };
        verify()?;
        let item = item.map_err(|error| format!("无法读取第 {} 个文件选择结果：{error}", index + 1))?;
        let name = unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) }.map(ShellPathName);
        verify()?;
        let name = name.map_err(|error| format!("无法读取第 {} 个所选路径：{error}", index + 1))?;
        if name.0.is_null() {
            return Err(format!("第 {} 个文件选择结果没有有效路径。", index + 1));
        }
        let raw = unsafe { name.0.to_string() };
        drop(name);
        verify()?;
        let raw = raw.map_err(|error| format!("无法解码第 {} 个所选路径：{error}", index + 1))?;
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(format!("第 {} 个文件选择结果没有有效路径。", index + 1));
        }
        let normalized = std::path::absolute(trimmed)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| trimmed.to_string());
        let path = Path::new(&normalized);
        if allow_dirs && path.is_dir() {
            paths.push(normalized);
        } else if files_only && path.is_file() {
            paths.push(normalized);
        }
        verify()?;
    }
    verify()?;
    Ok(paths)
}

#[cfg(all(windows, not(test)))]
fn pick_folder_windows(
    initial: Option<&str>, owner_hwnd: i64,
    verify: &impl Fn() -> Result<(), String>,
) -> Result<Option<String>, String> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::Shell::{
        ILFree, SHBrowseForFolderW, SHGetPathFromIDListW, BIF_NEWDIALOGSTYLE, BIF_RETURNONLYFSDIRS,
        BROWSEINFOW,
    };
    use windows::Win32::UI::WindowsAndMessaging::SendMessageW;

    const BFFM_INITIALIZED: u32 = 1;
    const BFFM_SETSELECTIONW: u32 = 0x0400 + 103;

    struct FolderSelection(*mut windows::Win32::UI::Shell::Common::ITEMIDLIST);
    impl Drop for FolderSelection {
        fn drop(&mut self) {
            if !self.0.is_null() { unsafe { ILFree(Some(self.0)); } }
        }
    }

    unsafe extern "system" fn callback(hwnd: HWND, msg: u32, _lparam: LPARAM, data: LPARAM) -> i32 {
        if msg == BFFM_INITIALIZED && data.0 != 0 {
            unsafe {
                SendMessageW(hwnd, BFFM_SETSELECTIONW, WPARAM(1), data);
            }
        }
        0
    }

    unsafe {
        verify()?;
        let apartment = StaApartment::initialize().map_err(|error| format!("无法初始化文件夹选择框：{error}"));
        verify()?;
        let _apartment = apartment?;
        let title: Vec<u16> = "选择文件夹".encode_utf16().chain([0]).collect();
        let mut display = [0u16; 260];
        let mut initial_wide: Vec<u16> = Vec::new();
        if let Some(start) = initial.map(str::trim).filter(|s| !s.is_empty()) {
            let full = std::path::absolute(start).unwrap_or_else(|_| PathBuf::from(start));
            if full.is_dir() {
                initial_wide = full.to_string_lossy().encode_utf16().chain([0]).collect();
            }
        }
        let info = BROWSEINFOW {
            hwndOwner: HWND(owner_hwnd as *mut _),
            pidlRoot: std::ptr::null_mut(),
            pszDisplayName: windows::core::PWSTR(display.as_mut_ptr()),
            lpszTitle: PCWSTR(title.as_ptr()),
            ulFlags: BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE,
            lpfn: Some(callback),
            lParam: if initial_wide.is_empty() {
                LPARAM(0)
            } else {
                LPARAM(initial_wide.as_ptr() as isize)
            },
            iImage: 0,
        };
        verify()?;
        let selection = FolderSelection(SHBrowseForFolderW(&info));
        verify()?;
        if selection.0.is_null() {
            return Ok(None);
        }
        let mut path_buf = [0u16; 260];
        let ok = SHGetPathFromIDListW(selection.0, &mut path_buf).as_bool();
        verify()?;
        if !ok {
            return Err("无法读取所选文件夹的路径。".into());
        }
        let len = path_buf.iter().position(|c| *c == 0).unwrap_or(path_buf.len());
        let raw = String::from_utf16_lossy(&path_buf[..len]);
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err("所选文件夹没有有效路径。".into());
        }
        let path = std::path::absolute(trimmed)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| trimmed.to_string());
        verify()?;
        Ok(Some(path))
    }
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum PickerTestKind { Files, Image, Directories, Folder, Unknown }

#[cfg(test)]
type PickerTestRunner = dyn FnMut(Option<&str>, i64, OpenPickOptions, &dyn Fn() -> Result<(), String>) -> Result<Vec<String>, String>;

#[cfg(test)]
struct PickerTestProvider { kind: PickerTestKind, run: Box<PickerTestRunner> }

#[cfg(test)]
thread_local! {
    static PICKER_TEST_PROVIDER: std::cell::RefCell<Option<PickerTestProvider>> = std::cell::RefCell::new(None);
}

#[cfg(test)]
fn with_picker_test_provider<T>(
    kind: PickerTestKind,
    run_provider: impl FnMut(Option<&str>, i64, OpenPickOptions, &dyn Fn() -> Result<(), String>) -> Result<Vec<String>, String> + 'static,
    run: impl FnOnce() -> T,
) -> T {
    struct Restore(Option<PickerTestProvider>);
    impl Drop for Restore { fn drop(&mut self) { PICKER_TEST_PROVIDER.with(|provider| *provider.borrow_mut() = self.0.take()); } }
    let previous = PICKER_TEST_PROVIDER.with(|provider| provider.borrow_mut().replace(PickerTestProvider { kind, run: Box::new(run_provider) }));
    let _restore = Restore(previous);
    run()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{host::HostState, paths::AppPaths};
    use serde_json::{json, Value};
    use std::sync::{Arc, atomic::{AtomicBool, AtomicUsize, Ordering}};

    struct PickerFixture {
        _root: tempfile::TempDir,
        host: HostState,
        active: Arc<AtomicBool>,
        image: String,
    }

    fn picker_fixture() -> PickerFixture {
        crate::clipboard::force_headless(true);
        let temporary = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
        std::fs::create_dir_all(&temporary).unwrap();
        let root = tempfile::Builder::new().prefix("native-picker-").tempdir_in(temporary).unwrap();
        let host = HostState::open(AppPaths::from_data_root(root.path().join("data")), 29065).unwrap();
        host.set_window_runtime_ready(true);
        host.set_host_hwnd(100);
        host.set_main_hwnd(200);
        host.show_web_route("/main/lan");
        host.set_input_runner(|_| panic!("picker stub must not dispatch real input"));
        host.set_clipboard_ui_runner(|_| panic!("picker must not write clipboard"));
        let image = root.path().join("owned-image.PNG");
        std::fs::write(&image, b"owned image bytes").unwrap();
        PickerFixture { _root: root, host, active: Arc::new(AtomicBool::new(true)), image: image.to_string_lossy().into_owned() }
    }

    fn invoke_native(f: &PickerFixture, image: bool, source: isize) -> Result<Value, String> {
        let active = f.active.clone();
        let host = f.host.clone();
        let current = move || active.load(Ordering::SeqCst) && host.native_caption_drag_allowed("main", 200);
        let params = json!({"initialPath":f._root.path().to_string_lossy(),"sourceWindowHandle":100});
        if image { f.host.pick_image_file_native_guarded(&params, source, current) }
        else { f.host.pick_files_native_guarded(&params, source, current) }
    }

    fn with_scripted_picker<T>(
        f: &PickerFixture, image: bool, phase: &'static str,
        prepared: Arc<AtomicUsize>, shown: Arc<AtomicUsize>, read: Arc<AtomicUsize>, released: Arc<AtomicUsize>,
        run: impl FnOnce() -> T,
    ) -> T {
        struct OwnedDialog(Arc<AtomicUsize>);
        impl Drop for OwnedDialog { fn drop(&mut self) { self.0.fetch_add(1, Ordering::SeqCst); } }
        let active = f.active.clone();
        let host = f.host.clone();
        let paths = vec![f.image.clone()];
        let initial = f._root.path().to_string_lossy().into_owned();
        with_picker_test_provider(if image { PickerTestKind::Image } else { PickerTestKind::Files }, move |start, owner, options, verify| {
            assert_eq!(owner, 200, "dialog owner must be main, never clipboard");
            assert_eq!(start, Some(initial.as_str()));
            assert!(!options.folders && options.files_only);
            assert_eq!(options.multi, !image);
            assert_eq!(options.filters.len(), if image { 2 } else { 0 });
            assert_eq!(options.title, if image { "选择背景图片" } else { "选择文件" });
            run_guarded_open(&|| verify(), || {
                prepared.fetch_add(1, Ordering::SeqCst);
                if matches!(phase, "prepare" | "prepare-fail-retired") { active.store(false, Ordering::SeqCst); }
                if phase == "core-hide-reopen" { host.mark_main_hidden(); host.show_web_route("/main/lan"); }
                if phase == "core-root" { host.set_main_hwnd(201); }
                if matches!(phase, "prepare-fail" | "prepare-fail-retired") {
                    Err("picker preparation failed".into())
                } else { Ok(OwnedDialog(released.clone())) }
            }, |_| {
                verify()?;
                shown.fetch_add(1, Ordering::SeqCst);
                if matches!(phase, "show-return" | "show-cancel-retired" | "show-fail-retired") { active.store(false, Ordering::SeqCst); }
                if matches!(phase, "show-fail" | "show-fail-retired") { return Err("picker display failed".into()); }
                Ok(!matches!(phase, "cancel" | "show-cancel-retired"))
            }, |_| {
                read.fetch_add(1, Ordering::SeqCst);
                if matches!(phase, "results" | "results-fail-retired") { active.store(false, Ordering::SeqCst); }
                if matches!(phase, "results-fail" | "results-fail-retired") { return Err("picker results failed".into()); }
                Ok(paths.clone())
            })
        }, run)
    }

    fn picker_counts() -> [Arc<AtomicUsize>; 4] { std::array::from_fn(|_| Arc::new(AtomicUsize::new(0))) }

    #[test]
    fn native_picker_rejects_clipboard_hidden_or_stale_main_before_provider() {
        for image in [false, true] {
            for phase in ["zero", "clipboard", "wrong-root", "hidden", "page"] {
                let f = picker_fixture();
                let counts = picker_counts();
                if phase == "hidden" { f.host.mark_main_hidden(); }
                if phase == "page" { f.active.store(false, Ordering::SeqCst); }
                let source = match phase { "zero" => 0, "clipboard" => 100, "wrong-root" => 201, _ => 200 };
                let result = with_scripted_picker(&f, image, "success", counts[0].clone(), counts[1].clone(), counts[2].clone(), counts[3].clone(), || invoke_native(&f, image, source));
                assert_eq!(result.unwrap_err(), NATIVE_PICKER_RETIRED, "{phase}");
                assert!(counts.iter().all(|count| count.load(Ordering::SeqCst) == 0));
            }
        }
    }

    #[test]
    fn native_picker_rechecks_prepare_show_return_and_results_and_releases_dialog() {
        for image in [false, true] {
            for phase in ["prepare", "core-hide-reopen", "core-root", "show-return", "show-cancel-retired", "results"] {
                let f = picker_fixture();
                let counts = picker_counts();
                let result = with_scripted_picker(&f, image, phase, counts[0].clone(), counts[1].clone(), counts[2].clone(), counts[3].clone(), || invoke_native(&f, image, 200));
                assert_eq!(result.unwrap_err(), NATIVE_PICKER_RETIRED, "{phase}");
                assert_eq!(counts[0].load(Ordering::SeqCst), 1);
                assert_eq!(counts[1].load(Ordering::SeqCst), usize::from(matches!(phase,"show-return"|"show-cancel-retired"|"results")));
                assert_eq!(counts[2].load(Ordering::SeqCst), usize::from(phase == "results"));
                assert_eq!(counts[3].load(Ordering::SeqCst), 1);
                assert!(!f._root.path().join("data/WindowBackground").exists());
            }
        }
    }

    #[test]
    fn native_picker_preserves_success_and_cancel_payloads() {
        for image in [false, true] {
            for phase in ["success", "cancel"] {
                let f = picker_fixture();
                let counts = picker_counts();
                let result = with_scripted_picker(&f, image, phase, counts[0].clone(), counts[1].clone(), counts[2].clone(), counts[3].clone(), || invoke_native(&f, image, 200)).unwrap();
                assert_eq!(result["cancelled"], phase != "success");
                if phase != "success" {
                    assert_eq!(if image { result["path"].clone() } else { result["paths"].clone() }, if image { Value::Null } else { json!([]) });
                } else if image {
                    let path = Path::new(result["path"].as_str().unwrap());
                    assert!(path.starts_with(f._root.path().join("data/WindowBackground")));
                    assert_eq!(path.extension().unwrap(), "png");
                    assert_eq!(std::fs::read(path).unwrap(), b"owned image bytes");
                    let original = crate::window_background::stage_copy(&f._root.path().join("data"), Path::new(&f.image)).unwrap();
                    assert_eq!(path, original);
                } else { assert_eq!(result["paths"], json!([f.image])); }
                assert_eq!(counts[3].load(Ordering::SeqCst), 1);
            }
        }
    }

    #[test]
    fn native_picker_prepare_show_and_read_failures_are_errors_and_retirement_takes_priority() {
        for image in [false, true] {
            for (phase, failure, show_count, read_count, release_count) in [
                ("prepare-fail", "picker preparation failed", 0, 0, 0),
                ("show-fail", "picker display failed", 1, 0, 1),
                ("results-fail", "picker results failed", 1, 1, 1),
                ("prepare-fail-retired", NATIVE_PICKER_RETIRED, 0, 0, 0),
                ("show-fail-retired", NATIVE_PICKER_RETIRED, 1, 0, 1),
                ("results-fail-retired", NATIVE_PICKER_RETIRED, 1, 1, 1),
            ] {
                let f = picker_fixture();
                let counts = picker_counts();
                let result = with_scripted_picker(&f, image, phase, counts[0].clone(), counts[1].clone(), counts[2].clone(), counts[3].clone(), || invoke_native(&f, image, 200));
                assert_eq!(result.unwrap_err(), failure, "{phase}");
                for (count, expected) in counts.iter().zip([1, show_count, read_count, release_count]) {
                    assert_eq!(count.load(Ordering::SeqCst), expected, "{phase}");
                }
                assert!(!f._root.path().join("data/WindowBackground").exists());
            }
        }
    }

    #[test]
    fn public_picker_apis_propagate_provider_failure_and_keep_cancel_payloads() {
        let f = picker_fixture();
        for kind in [PickerTestKind::Files, PickerTestKind::Image, PickerTestKind::Directories, PickerTestKind::Folder] {
            for failed in [false, true] {
                let result = with_picker_test_provider(kind, move |_, owner, _, _| {
                    assert_eq!(owner, 100);
                    if failed { Err("native picker failed".into()) } else { Ok(Vec::new()) }
                }, || match kind {
                    PickerTestKind::Files => f.host.pick_files(&json!({})),
                    PickerTestKind::Image => f.host.pick_image_file(&json!({})),
                    PickerTestKind::Directories => f.host.pick_directories(&json!({})),
                    PickerTestKind::Folder => f.host.pick_folder(&json!({})),
                    PickerTestKind::Unknown => unreachable!(),
                });
                if failed { assert_eq!(result.unwrap_err(), "native picker failed"); }
                else {
                    let expected = if matches!(kind, PickerTestKind::Image | PickerTestKind::Folder) {
                        json!({"path":null,"cancelled":true})
                    } else { json!({"paths":[],"cancelled":true}) };
                    assert_eq!(result.unwrap(), expected);
                }
            }
        }
    }

    #[test]
    fn folder_and_directory_picker_guards_reject_a_retired_session_before_provider() {
        let called = Arc::new(AtomicBool::new(false));
        for kind in [PickerTestKind::Folder, PickerTestKind::Directories] {
            let provider_called = called.clone();
            let result = with_picker_test_provider(kind, move |_,_,_,_| {
                provider_called.store(true, Ordering::SeqCst);
                Ok(Vec::new())
            }, || {
                let verify = || Err(NATIVE_PICKER_RETIRED.into());
                if kind == PickerTestKind::Folder { pick_folder_guarded(None, 200, &verify).map(|path| path.into_iter().collect()) }
                else { pick_directories_guarded(None, 200, &verify) }
            });
            assert_eq!(result.unwrap_err(), NATIVE_PICKER_RETIRED);
            assert!(!called.load(Ordering::SeqCst));
        }
    }

    #[test]
    #[cfg(windows)]
    fn native_dialog_show_only_treats_windows_cancel_as_cancellation() {
        use windows::core::{Error, HRESULT};
        use windows::Win32::Foundation::{ERROR_CANCELLED, E_ACCESSDENIED, RPC_E_CHANGED_MODE};
        assert_eq!(dialog_show_result(Ok(())).unwrap(), true);
        assert_eq!(dialog_show_result(Err(Error::from_hresult(HRESULT::from_win32(ERROR_CANCELLED.0)))).unwrap(), false);
        for code in [E_ACCESSDENIED, RPC_E_CHANGED_MODE] {
            let error = dialog_show_result(Err(Error::from_hresult(code))).unwrap_err();
            assert!(error.contains("无法显示文件选择框"));
            assert!(error.contains(&format!("{:08X}", code.0 as u32)), "{error}");
        }
    }

    #[test]
    fn native_picker_missing_wrong_and_unknown_stubs_never_fall_through_to_show() {
        let f = picker_fixture();
        assert_eq!(invoke_native(&f, false, 200).unwrap(), json!({"paths":[],"cancelled":true}));
        assert_eq!(invoke_native(&f, true, 200).unwrap(), json!({"path":null,"cancelled":true}));
        for kind in [PickerTestKind::Unknown, PickerTestKind::Image] {
            let result = with_picker_test_provider(kind, |_,_,_,_| panic!("unknown/mismatched provider cannot execute"), || invoke_native(&f, false, 200));
            assert_eq!(result.unwrap_err(), "Unknown native picker test provider.");
        }
        assert_eq!(invoke_native(&f, false, 200).unwrap()["cancelled"], true, "stub scope must restore its predecessor");
        assert_eq!(f.host.pick_files(&json!({})).unwrap(), json!({"paths":[],"cancelled":true}));
        assert_eq!(f.host.pick_image_file(&json!({})).unwrap(), json!({"path":null,"cancelled":true}));
    }

    #[test]
    fn main_folder_picker_keeps_the_settings_owner_and_rejects_retired_returns() {
        for phase in ["selected", "cancelled", "failed", "retired", "hide-reopen"] {
            let f = picker_fixture();
            let active = f.active.clone();
            let host = f.host.clone();
            let initial = f._root.path().to_string_lossy().into_owned();
            let selected = initial.clone();
            let params = json!({"initialPath":initial,"sourceWindowHandle":100});
            let result = with_picker_test_provider(PickerTestKind::Folder, move |start, owner, options, verify| {
                assert_eq!(owner, 200, "folder dialog belongs to settings, not clipboard");
                assert_eq!(start, Some(selected.as_str()));
                assert!(options.folders && !options.multi);
                verify()?;
                if phase == "retired" { active.store(false, Ordering::SeqCst); }
                if phase == "hide-reopen" { host.mark_main_hidden(); host.show_web_route("/main/lan"); }
                verify()?;
                match phase {
                    "failed" => Err("folder picker failed".into()),
                    "cancelled" => Ok(Vec::new()),
                    _ => Ok(vec![selected.clone()]),
                }
            }, || f.host.pick_folder_native_guarded(&params, 200, || f.active.load(Ordering::SeqCst)));
            match phase {
                "selected" => assert_eq!(result.unwrap(), json!({"path":initial,"cancelled":false})),
                "cancelled" => assert_eq!(result.unwrap(), json!({"path":null,"cancelled":true})),
                "failed" => assert_eq!(result.unwrap_err(), "folder picker failed"),
                _ => assert_eq!(result.unwrap_err(), NATIVE_PICKER_RETIRED),
            }
        }
        let f = picker_fixture();
        for owner in [0, 100, 201] {
            with_picker_test_provider(PickerTestKind::Folder, |_,_,_,_| panic!("invalid owner cannot open a dialog"), || {
                assert_eq!(f.host.pick_folder_native_guarded(&json!({}), owner, || true).unwrap_err(), NATIVE_PICKER_RETIRED);
            });
        }
    }

    #[test]
    fn native_image_picker_rechecks_after_source_read_and_before_managed_publication() {
        use crate::window_background::{with_stage_test_hook, StageTestPhase};
        for phase in [StageTestPhase::AfterRead, StageTestPhase::BeforeWrite] {
            let f = picker_fixture();
            let counts = picker_counts();
            let active = f.active.clone();
            let result = with_scripted_picker(&f, true, "success", counts[0].clone(), counts[1].clone(), counts[2].clone(), counts[3].clone(), || {
                with_stage_test_hook(move |current| { if current == phase { active.store(false, Ordering::SeqCst); } }, || invoke_native(&f, true, 200))
            });
            assert_eq!(result.unwrap_err(), NATIVE_PICKER_RETIRED);
            let directory = f._root.path().join("data/WindowBackground");
            assert_eq!(std::fs::read_dir(directory).map(|items| items.count()).unwrap_or(0), 0);
            assert_eq!(std::fs::read(&f.image).unwrap(), b"owned image bytes");
            assert_eq!(counts[3].load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    #[cfg(windows)]
    fn dialog_apartment_balances_nested_initialization_on_reusable_workers() {
        std::thread::spawn(|| unsafe {
            use windows::Win32::System::Com::*;
            let first = StaApartment::initialize().unwrap();
            let second = StaApartment::initialize().unwrap(); // S_FALSE also needs uninitialization.
            drop(second);
            let mut kind = APTTYPE::default();
            let mut qualifier = APTTYPEQUALIFIER::default();
            CoGetApartmentType(&mut kind, &mut qualifier).unwrap();
            assert!(kind == APTTYPE_STA || kind == APTTYPE_MAINSTA);
            drop(first);
            // A leaked STA reference would reject reuse as MTA.
            let initialized = CoInitializeEx(None, COINIT_MULTITHREADED);
            if initialized.is_ok() { CoUninitialize(); }
            initialized.ok().unwrap();
        }).join().unwrap();
    }

    #[test]
    #[cfg(windows)]
    fn rejected_dialog_apartment_preserves_the_callers_com_initialization() {
        std::thread::spawn(|| unsafe {
            use windows::Win32::System::Com::*;
            CoInitializeEx(None, COINIT_MULTITHREADED).ok().unwrap();
            let rejected = StaApartment::initialize().is_err();
            let mut kind = APTTYPE::default();
            let mut qualifier = APTTYPEQUALIFIER::default();
            let state = CoGetApartmentType(&mut kind, &mut qualifier);
            CoUninitialize();
            assert!(rejected);
            state.unwrap();
            assert_eq!(kind, APTTYPE_MTA);
        }).join().unwrap();
    }

    #[test]
    fn headless_native_picks_are_empty() {
        crate::clipboard::force_headless(true);
        assert!(pick_directories(None, 0).unwrap().is_empty());
        assert!(pick_directories(Some(r"C:\Windows"), 0).unwrap().is_empty());
        assert!(pick_files(None, 0).unwrap().is_empty());
        assert!(pick_files(Some(r"C:\Windows"), 0).unwrap().is_empty());
        assert!(pick_image_file(None, 0).unwrap().is_none());
        assert!(pick_image_file(Some(r"C:\Windows\explorer.exe"), 0).unwrap().is_none());
        assert!(pick_folder(None, 0).unwrap().is_none());
        assert!(pick_folder(Some(r"C:\Windows"), 0).unwrap().is_none());
    }
}
