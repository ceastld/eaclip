//! Shell file icons aligned with C# `HandleFileIcon` / `FavorIconShellPath` / `ShellIconPngExporter`.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use parking_lot::Mutex;
use regex::Regex;

use crate::pin_overlay::resolve_running_exe;

const SPEC_NOT_SHELL: &str = "spec is not a shell/file icon path.";
const ARGS_REQUIRED: &str = "path, proc, or spec is required.";

fn cache() -> &'static crate::file_icon_cache::State {
    static CACHE: OnceLock<crate::file_icon_cache::State> = OnceLock::new();
    CACHE.get_or_init(crate::file_icon_cache::State::default)
}

pub fn png_bytes(path: Option<&str>, proc: Option<&str>, spec: Option<&str>) -> Result<Vec<u8>, String> {
    let target = resolve_target(path, proc, spec)?;
    png_from_target(&target)
}

pub fn png_from_target(target: &str) -> Result<Vec<u8>, String> {
    cache().get_or_load(png_cache_key(target), || {
        extract_png(target).ok_or_else(|| "no icon".to_string())
    })
}

/// Native rows draw the icon at its device pixel size. Resampling the 32px
/// Shell icon to 16/20/24px blurs it, so pick the module frame closest to
/// `size` and keep its straight alpha. Falls back to the 32px Shell icon.
pub fn png_from_target_sized(target: &str, size: u32) -> Result<Vec<u8>, String> {
    let size = size.clamp(16, 256);
    cache().get_or_load(format!("{}|{size}px", png_cache_key(target)), || {
        extract_png_sized(target, size)
            .or_else(|| extract_png(target))
            .ok_or_else(|| "no icon".to_string())
    })
}

fn extract_png_sized(path: &str, size: u32) -> Option<Vec<u8>> {
    #[cfg(windows)]
    {
        let (module, index) = parse_module_icon_spec(path)
            .unwrap_or_else(|| (PathBuf::from(path), 0));
        if !module.is_file() {
            return None;
        }
        extract_module_icon_sized(&module, index, size)
    }
    #[cfg(not(windows))]
    {
        let _ = (path, size);
        None
    }
}

fn resolve_target(path: Option<&str>, proc: Option<&str>, spec: Option<&str>) -> Result<String, String> {
    if let Some(proc) = proc.map(str::trim).filter(|s| !s.is_empty()) {
        return resolve_proc_exe(proc).ok_or_else(|| "process exe not found".into());
    }
    if let Some(spec) = spec.map(str::trim).filter(|s| !s.is_empty()) {
        return try_get_shell_path(spec).ok_or_else(|| SPEC_NOT_SHELL.into());
    }
    if let Some(path) = path.map(str::trim).filter(|s| !s.is_empty()) {
        return Ok(try_get_shell_path(path).unwrap_or_else(|| path.to_string()));
    }
    Err(ARGS_REQUIRED.into())
}

/// C# `FavorIconShellPath.TryGetShellPath`.
pub fn try_get_shell_path(raw_icon: &str) -> Option<String> {
    let mut candidate = raw_icon.trim().to_string();
    if candidate.is_empty() {
        return None;
    }
    if candidate.get(..5).is_some_and(|prefix| prefix.eq_ignore_ascii_case("icon:")) {
        candidate = candidate[5..].trim().to_string();
    }
    if candidate.is_empty() {
        return None;
    }
    let lower = candidate.to_ascii_lowercase();
    if lower.starts_with("fa:") || lower.starts_with("text:") || lower.starts_with("url:") {
        return None;
    }
    if is_font_awesome(&candidate) {
        return None;
    }
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return None;
    }
    Some(expand_environment_variables(&candidate))
}

fn is_font_awesome(raw: &str) -> bool {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    let re = PATTERN.get_or_init(|| {
        Regex::new(r"(?i)^(?:fa:)?(?:(?:Light|Solid|Regular|Brands)_\w+)(?::[^:]+)?$").expect("fa spec")
    });
    re.is_match(raw.trim())
}

/// C# `Environment.ExpandEnvironmentVariables` for `%NAME%` tokens.
fn expand_environment_variables(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '%' {
            if let Some(rel) = chars[i + 1..].iter().position(|c| *c == '%') {
                let name: String = chars[i + 1..i + 1 + rel].iter().collect();
                if !name.is_empty() {
                    if let Ok(value) = std::env::var(&name) {
                        out.push_str(&value);
                        i += name.chars().count() + 2;
                        continue;
                    }
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// C# `ProcessIconMetaDataService.TryGetExePath` runtime branch (no PATH lookup).
pub fn try_runtime_exe(proc: &str) -> Option<String> {
    resolve_proc_exe(proc)
}

fn resolve_proc_exe(proc: &str) -> Option<String> {
    let key = normalize_exe_key(proc);
    if key.is_empty() {
        return None;
    }
    let short = Path::new(&key)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(&key);
    let path = resolve_running_exe(short)?;
    if !path.is_file() {
        return None;
    }
    let image_name = path.file_name()?.to_str()?;
    if !normalize_exe_key(image_name).eq_ignore_ascii_case(&key) {
        return None;
    }
    Some(path.to_string_lossy().into_owned())
}

pub fn normalize_exe_key(process_name: &str) -> String {
    let trimmed = process_name.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.to_ascii_lowercase().ends_with(".exe") {
        trimmed.to_string()
    } else {
        format!("{trimmed}.exe")
    }
}

fn png_cache_key(spec: &str) -> String {
    if spec == "." || spec == ".." {
        return "$dir|true".into();
    }
    if is_association_only_spec(spec) {
        return format!("{}|true", spec.to_ascii_lowercase());
    }
    if Path::new(spec).is_dir() {
        return "$dir|true".into();
    }
    spec.to_ascii_lowercase()
}

fn is_association_only_spec(file_path: &str) -> bool {
    if file_path == "." || file_path == ".." {
        return true;
    }
    file_path.starts_with('.')
        && !file_path.contains(['\\', '/'])
        && !file_path.contains(':')
}

fn extract_png(path: &str) -> Option<Vec<u8>> {
    #[cfg(windows)]
    {
        if let Some((module, index)) = parse_module_icon_spec(path) {
            if let Some(png) = extract_module_icon(&module, index) {
                return Some(png);
            }
        }
        if path == "." || path == ".." {
            return sh_get_icon("folder", true, true);
        }
        if path.starts_with('.') && !path.contains(['\\', '/']) {
            return sh_get_icon(path, false, true);
        }
        let as_path = Path::new(path);
        if as_path.is_file() || as_path.is_dir() {
            return sh_get_icon(path, as_path.is_dir(), false);
        }
        None
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        None
    }
}

fn parse_module_icon_spec(raw: &str) -> Option<(PathBuf, i32)> {
    let split = raw.rfind(',')?;
    if split == 0 || split + 1 >= raw.len() {
        return None;
    }
    let path_candidate = raw[..split].trim();
    let index: i32 = raw[split + 1..].trim().parse().ok()?;
    let path = PathBuf::from(path_candidate);
    path.is_file().then_some((path, index))
}

#[cfg(windows)]
fn extract_module_icon(module: &Path, index: i32) -> Option<Vec<u8>> {
    use windows::core::PCWSTR;
    use windows::Win32::UI::Shell::ExtractIconExW;
    use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, HICON};

    let wide: Vec<u16> = module
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let mut large = HICON::default();
        let mut small = HICON::default();
        let extracted = ExtractIconExW(
            PCWSTR(wide.as_ptr()),
            index,
            Some(&mut large),
            Some(&mut small),
            1,
        );
        if extracted == 0 {
            return None;
        }
        let handle = if !large.is_invalid() { large } else { small };
        let png = if handle.is_invalid() {
            None
        } else {
            icon_to_png(handle)
        };
        if !large.is_invalid() {
            let _ = DestroyIcon(large);
        }
        if !small.is_invalid() {
            let _ = DestroyIcon(small);
        }
        png
    }
}

#[cfg(windows)]
fn extract_module_icon_sized(module: &Path, index: i32, size: u32) -> Option<Vec<u8>> {
    use windows::core::PCWSTR;
    use windows::Win32::UI::Shell::SHDefExtractIconW;
    use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, HICON};

    let wide: Vec<u16> = module
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let mut icon = HICON::default();
        // Low word: large icon size. The Shell picks the nearest frame and
        // scales only when the module has no frame of that size.
        SHDefExtractIconW(PCWSTR(wide.as_ptr()), index, 0, Some(&mut icon), None, size).ok().ok()?;
        if icon.is_invalid() {
            return None;
        }
        let png = icon_to_straight_png(icon);
        let _ = DestroyIcon(icon);
        png
    }
}

/// Reads the icon's own color/mask bitmaps instead of drawing it onto an
/// uninitialized surface, so translucent edges stay unpremultiplied.
#[cfg(windows)]
unsafe fn icon_to_straight_png(icon: windows::Win32::UI::WindowsAndMessaging::HICON) -> Option<Vec<u8>> {
    use windows::Win32::Graphics::Gdi::{CreateCompatibleDC, DeleteDC, DeleteObject, GetObjectW, BITMAP};
    use windows::Win32::UI::WindowsAndMessaging::{GetIconInfo, ICONINFO};

    let mut info = ICONINFO::default();
    GetIconInfo(icon, &mut info).ok()?;
    let color = info.hbmColor;
    let mask = info.hbmMask;
    let result = (|| {
        if color.is_invalid() {
            return None;
        }
        let mut bitmap = BITMAP::default();
        let read = GetObjectW(
            color,
            std::mem::size_of::<BITMAP>() as i32,
            Some((&mut bitmap as *mut BITMAP).cast()),
        );
        let (width, height) = (bitmap.bmWidth, bitmap.bmHeight);
        if read == 0 || width <= 0 || height <= 0 || width > 256 || height > 256 {
            return None;
        }
        let dc = CreateCompatibleDC(None);
        let mut pixels = crate::window_thumb::dib_bgra(dc, color, width, height);
        if let Some(pixels) = pixels.as_mut().filter(|p| p.chunks_exact(4).all(|px| px[3] == 0)) {
            // Legacy icons carry no alpha: the AND mask is black where opaque.
            let mask = (!mask.is_invalid())
                .then(|| crate::window_thumb::dib_bgra(dc, mask, width, height))
                .flatten();
            for (index, px) in pixels.chunks_exact_mut(4).enumerate() {
                let transparent = mask.as_ref().is_some_and(|m| m[index * 4] != 0);
                px[3] = if transparent { 0 } else { 255 };
            }
        }
        let _ = DeleteDC(dc);
        bgra_to_png(&pixels?, width as u32, height as u32)
    })();
    if !color.is_invalid() {
        let _ = DeleteObject(color);
    }
    if !mask.is_invalid() {
        let _ = DeleteObject(mask);
    }
    result
}

#[cfg(windows)]
fn sh_get_icon(path: &str, directory: bool, use_file_attributes: bool) -> Option<Vec<u8>> {
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};
    use windows::Win32::UI::Shell::{
        SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGFI_USEFILEATTRIBUTES,
    };
    use windows::Win32::UI::WindowsAndMessaging::DestroyIcon;

    // Concurrent cold Shell lookups can return success with an empty HICON.
    // Keep this lock independent of the cache and the host/window state.
    #[cfg(test)]
    tests::before_shell_lookup();
    static SHELL_LOOKUP: Mutex<()> = Mutex::new(());
    let _lookup = SHELL_LOOKUP.lock();

    // SHGetFileInfoW requires COM on the calling worker. Keep the apartment
    // alive until the returned icon has been converted and destroyed.
    #[cfg(test)]
    tests::observe_icon_stage("COM initialization");
    let _apartment = crate::explorer::ShellApartment::initialize().ok()?;
    #[cfg(test)]
    tests::observe_shell_apartment();

    let wide: Vec<u16> = Path::new(path)
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let attrs = if directory {
        FILE_ATTRIBUTE_DIRECTORY
    } else {
        FILE_ATTRIBUTE_NORMAL
    };
    let mut flags = SHGFI_ICON | SHGFI_LARGEICON;
    if use_file_attributes {
        flags |= SHGFI_USEFILEATTRIBUTES;
    }
    unsafe {
        let mut info = SHFILEINFOW::default();
        #[cfg(test)]
        tests::observe_icon_stage("SHGetFileInfoW");
        let ok = SHGetFileInfoW(
            PCWSTR(wide.as_ptr()),
            attrs,
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            flags,
        );
        if ok == 0 || info.hIcon.is_invalid() {
            #[cfg(test)]
            eprintln!("SHGetFileInfoW failure: path={path:?}, result={ok}, icon={:?}, last_error={:?}",
                info.hIcon, windows::Win32::Foundation::GetLastError());
            return None;
        }
        let png = icon_to_png(info.hIcon);
        let _ = DestroyIcon(info.hIcon);
        png
    }
}

#[cfg(windows)]
unsafe fn icon_to_png(icon: windows::Win32::UI::WindowsAndMessaging::HICON) -> Option<Vec<u8>> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, ReleaseDC, SelectObject,
    };
    use windows::Win32::UI::WindowsAndMessaging::{DrawIconEx, DI_NORMAL};

    const SIZE: i32 = 32;
    #[cfg(test)]
    tests::observe_icon_stage("bitmap conversion");
    let screen = GetDC(HWND::default());
    let mem = CreateCompatibleDC(screen);
    let bmp = CreateCompatibleBitmap(screen, SIZE, SIZE);
    let old = SelectObject(mem, bmp);
    let _ = DrawIconEx(mem, 0, 0, icon, SIZE, SIZE, 0, None, DI_NORMAL);
    // GetDIBits requires the bitmap to be detached from its drawing DC.
    SelectObject(mem, old);
    let pixels = crate::window_thumb::dib_bgra(mem, bmp, SIZE, SIZE);
    let _ = DeleteObject(bmp);
    let _ = DeleteDC(mem);
    let _ = ReleaseDC(HWND::default(), screen);
    #[cfg(test)]
    tests::observe_icon_stage(if pixels.is_some() { "PNG encoding" } else { "GetDIBits" });
    let pixels = pixels?;
    bgra_to_png(&pixels, SIZE as u32, SIZE as u32)
}

fn bgra_to_png(bgra: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    let mut rgba = Vec::with_capacity(bgra.len());
    for chunk in bgra.chunks_exact(4) {
        rgba.extend_from_slice(&[chunk[2], chunk[1], chunk[0], chunk[3]]);
    }
    let img = image::RgbaImage::from_raw(width, height, rgba)?;
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).ok()?;
    Some(out.into_inner())
}

#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[cfg(windows)]
    thread_local! {
        static SHELL_APARTMENT_READY: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
        static ICON_STAGE: std::cell::Cell<&'static str> = const { std::cell::Cell::new("not started") };
        static BEFORE_SHELL: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    }

    #[cfg(windows)]
    pub(crate) fn set_lookup_hook(callback: impl FnOnce() + 'static) {
        BEFORE_SHELL.with(|slot| *slot.borrow_mut() = Some(Box::new(callback)));
    }

    #[cfg(windows)]
    pub(super) fn before_shell_lookup() {
        let callback = BEFORE_SHELL.with(|slot| slot.borrow_mut().take());
        if let Some(callback) = callback { callback(); }
    }

    #[cfg(windows)]
    pub(super) fn observe_icon_stage(stage: &'static str) {
        ICON_STAGE.with(|slot| slot.set(stage));
    }

    #[cfg(windows)]
    pub(super) fn observe_shell_apartment() {
        use windows::Win32::System::Com::{CoGetApartmentType, APTTYPE, APTTYPEQUALIFIER};
        let mut apartment = APTTYPE::default();
        let mut qualifier = APTTYPEQUALIFIER::default();
        let ready = unsafe { CoGetApartmentType(&mut apartment, &mut qualifier).is_ok() };
        SHELL_APARTMENT_READY.with(|slot| slot.set(Some(ready)));
    }

    #[cfg(windows)]
    #[test]
    fn shell_icon_initializes_worker_com_and_preserves_callers_apartment() {
        use windows::Win32::Foundation::S_OK;
        use windows::Win32::System::Com::{CoGetApartmentType, CoInitializeEx, CoUninitialize,
            APTTYPE, APTTYPEQUALIFIER, APTTYPE_MTA, APTTYPEQUALIFIER_IMPLICIT_MTA,
            COINIT_MULTITHREADED, COINIT_APARTMENTTHREADED};
        // Keep another MTA alive so implicit inheritance is deterministic,
        // rather than depending on overlap with unrelated parallel tests.
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
                let mut original_apartment = APTTYPE::default();
                let mut original_qualifier = APTTYPEQUALIFIER::default();
                CoGetApartmentType(&mut original_apartment, &mut original_qualifier).unwrap();
                let png = sh_get_icon(".txt", false, true).unwrap_or_else(|| panic!(
                    "shell association icon failed: caller={caller:?}, stage={}", ICON_STAGE.with(|slot| slot.get())));
                assert!(png.starts_with(b"\x89PNG"));
                assert_eq!(SHELL_APARTMENT_READY.with(|slot| slot.get()), Some(true),
                    "SHGetFileInfoW must run with COM initialized");
                let mut apartment = APTTYPE::default();
                let mut qualifier = APTTYPEQUALIFIER::default();
                CoGetApartmentType(&mut apartment, &mut qualifier).unwrap();
                assert_eq!((apartment, qualifier), (original_apartment, original_qualifier),
                    "icon extraction changed the caller's COM apartment");
                if caller.is_none() {
                    assert_eq!((apartment, qualifier), (APTTYPE_MTA, APTTYPEQUALIFIER_IMPLICIT_MTA),
                        "an uninitialized caller may inherit the process MTA, but no explicit apartment");
                } else {
                    assert_ne!(qualifier, APTTYPEQUALIFIER_IMPLICIT_MTA,
                        "icon extraction removed the caller's explicit COM initialization");
                    CoUninitialize();
                }
                // S_FALSE would reveal an extra explicit MTA count; a leaked STA
                // would reject this initialization with RPC_E_CHANGED_MODE.
                assert_eq!(CoInitializeEx(None, COINIT_MULTITHREADED), S_OK,
                    "icon extraction left an explicit COM initialization count");
                CoUninitialize();
            }).join().unwrap();
        }
        drop(stop);
        owner.join().unwrap();
    }

    #[test]
    fn missing_args_error_matches_csharp() {
        let err = png_bytes(None, None, None).unwrap_err();
        assert_eq!(err, ARGS_REQUIRED);
    }

    #[test]
    fn favor_icon_shell_path_matches_csharp() {
        assert_eq!(try_get_shell_path("icon:.txt").as_deref(), Some(".txt"));
        assert_eq!(try_get_shell_path("fa:Light_Text"), None);
        assert_eq!(try_get_shell_path("Light_File"), None);
        assert_eq!(try_get_shell_path("text:abc"), None);
        assert_eq!(try_get_shell_path("url:https://example"), None);
        assert_eq!(try_get_shell_path("https://example.com/a.png"), None);
        assert_eq!(
            png_bytes(None, None, Some("fa:Light_Text")).unwrap_err(),
            SPEC_NOT_SHELL
        );
        std::env::set_var("CLIPHOST_ICON_ENV_PROBE", "C:\\Windows");
        let expanded = try_get_shell_path("%CLIPHOST_ICON_ENV_PROBE%\\notepad.exe").unwrap();
        assert!(expanded.contains("Windows"), "{expanded}");
        std::env::remove_var("CLIPHOST_ICON_ENV_PROBE");
    }

    #[test]
    #[cfg(windows)]
    fn icon_pixels_are_read_after_bitmap_is_deselected() {
        use windows::Win32::UI::WindowsAndMessaging::{LoadIconW, IDI_APPLICATION};
        crate::window_thumb::tests::take_bitmap_selected();
        // Shared system icon; this test neither creates a window nor captures the desktop.
        let icon = unsafe { LoadIconW(None, IDI_APPLICATION) }.unwrap();
        let png = unsafe { icon_to_png(icon) }.expect("stock icon pixels");
        let selected = crate::window_thumb::tests::take_bitmap_selected();
        let image = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(image.dimensions(), (32, 32));
        assert!(image.pixels().any(|pixel| pixel[3] > 0));
        assert_eq!(selected, Some(false), "GetDIBits requires an unselected bitmap");
    }

    #[test]
    #[cfg(windows)]
    fn sized_module_icons_match_device_pixels_with_straight_alpha() {
        let explorer = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into()) + "\\explorer.exe";
        for size in [16, 20, 24, 40] {
            let png = extract_png_sized(&explorer, size).expect("explorer icon frame");
            let image = image::load_from_memory(&png).unwrap().to_rgba8();
            assert_eq!(image.dimensions(), (size, size));
            assert!(image.pixels().any(|px| px[3] == 0), "corners stay transparent");
            assert!(image.pixels().any(|px| px[3] == 255), "artwork stays opaque");
        }
        assert_eq!(png_from_target_sized(&explorer, 8).map(|png| {
            image::load_from_memory(&png).unwrap().to_rgba8().dimensions()
        }), Ok((16, 16)), "requests clamp to the smallest real frame");
    }

    #[test]
    #[cfg(windows)]
    fn parallel_shell_icons_do_not_return_empty() {
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(16));
        let workers: Vec<_> = (0..16).map(|worker| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                let spec = [".txt", ".文本", ".cliphost_fixture"][worker % 3];
                let png = sh_get_icon(spec, false, true);
                let valid = png.as_deref().and_then(|data| image::load_from_memory(data).ok()).is_some();
                (worker, spec, valid, ICON_STAGE.with(|slot| slot.get()))
            })
        }).collect();
        let failures: Vec<_> = workers.into_iter().map(|worker| worker.join().unwrap())
            .filter(|(_, _, valid, _)| !valid).collect();
        assert!(failures.is_empty(), "parallel Shell failures: {failures:?}");
    }

    #[test]
    #[cfg(windows)]
    fn icon_cache_coalesces_actual_shell_requests_and_preserves_png() {
        use std::sync::{Arc, Barrier, atomic::{AtomicUsize, Ordering}};
        let spec = format!(".cliphost_cache_{}", uuid::Uuid::new_v4().simple());
        let misses = Arc::new(Barrier::new(8));
        let calls = Arc::new(AtomicUsize::new(0));
        let workers: Vec<_> = (0..8).map(|_| {
            let spec = spec.clone();
            let misses = misses.clone();
            let calls = calls.clone();
            std::thread::spawn(move || {
                crate::file_icon_cache::set_miss_hook(move || { misses.wait(); });
                set_lookup_hook(move || { calls.fetch_add(1, Ordering::SeqCst); });
                png_from_target(&spec).expect("native association icon")
            })
        }).collect();
        let pngs: Vec<_> = workers.into_iter().map(|worker| worker.join().unwrap()).collect();
        assert!(pngs.iter().all(|png| png == &pngs[0]));
        assert_eq!(calls.load(Ordering::SeqCst), 1, "same-key public requests must extract only once");
        let image = image::load_from_memory(&pngs[0]).unwrap().to_rgba8();
        assert_eq!(image.dimensions(), (32, 32));
        assert!(image.pixels().any(|pixel| pixel[3] > 0));
        assert_eq!(png_bytes(None, None, Some(&format!("icon:{spec}"))).unwrap(), pngs[0]);
    }

    #[test]
    #[cfg(windows)]
    fn association_spec_yields_png() {
        let png = png_bytes(None, None, Some(".txt")).unwrap_or_else(|error| panic!(
            ".txt association icon: {error}, stage={}", ICON_STAGE.with(|slot| slot.get())));
        assert!(png.starts_with(b"\x89PNG"), "len={}", png.len());
        let via_icon = png_bytes(None, None, Some("icon:.txt")).expect("icon:.txt");
        assert!(via_icon.starts_with(b"\x89PNG"));
    }
}
