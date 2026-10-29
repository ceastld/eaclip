//! Owned Windows paste target, without a clipboard listener or background capture.
//! cargo build -p cliphost-core --example storage_core_acceptance_receiver
//! storage_core_acceptance_receiver.exe --root <compiled-repo>/.tmp/<run>/receiver --nonce <32hex>
//! The supervisor owns clipboard backup/restoration. This process never writes the clipboard.
//! Only ClipHost.NativeFixture (ASCII nonce, optionally NUL terminated) is accepted.

#[cfg(not(windows))]
fn main() {
    eprintln!("Windows is required");
    std::process::exit(2);
}

#[cfg(windows)]
fn main() {
    if let Err(code) = native::run() {
        // Errors are fixed categories, never paths, clipboard text, or native error detail.
        eprintln!("{code}");
        std::process::exit(2);
    }
}

#[cfg(windows)]
mod native {
    use serde::Serialize;
    use sha2::{Digest, Sha256};
    use std::{
        cell::{Cell, RefCell},
        fs::{self, OpenOptions},
        io::Write,
        os::windows::fs::MetadataExt,
        path::{Component, Path, PathBuf},
    };
    use windows::{
        core::{w, HSTRING},
        Win32::{
            Foundation::{HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, WPARAM},
            Graphics::Gdi::HBRUSH,
            System::{
                DataExchange::{
                    CloseClipboard, EnumClipboardFormats, GetClipboardData,
                    GetClipboardSequenceNumber, IsClipboardFormatAvailable, OpenClipboard,
                    RegisterClipboardFormatW,
                },
                LibraryLoader::GetModuleHandleW,
                Memory::{GlobalLock, GlobalSize, GlobalUnlock},
            },
            UI::{
                Input::KeyboardAndMouse::{GetKeyState, SetFocus, VK_CONTROL},
                WindowsAndMessaging::*,
            },
        },
    };

    type Result<T> = std::result::Result<T, &'static str>;
    const EDIT_ID: usize = 100;
    const PASTE_ID: usize = 101;
    const TEXT_FORMAT: u32 = 13;
    const DIB_FORMAT: u32 = 8;
    const DIBV5_FORMAT: u32 = 17;
    const MAX_IMAGE_BYTES: usize = 64 * 1024 * 1024;
    const MAX_TEXT_BYTES: usize = 8 * 1024 * 1024;
    const BAD_ATTRIBUTES: u32 = 0x400 | 0x1000 | 0x40000 | 0x400000;

    thread_local! {
        static APP: RefCell<Option<App>> = const { RefCell::new(None) };
        // The standard EDIT sends synchronous notifications while presenting verified text.
        // Keep its original procedure outside APP so those messages never reborrow APP.
        static EDIT_ORIGINAL: Cell<isize> = const { Cell::new(0) };
    }

    #[derive(Serialize, Default)]
    #[serde(rename_all = "camelCase")]
    struct Receipt {
        schema: u32,
        status: &'static str,
        pid: u32,
        hwnd: isize,
        edit_hwnd: isize,
        paste_button_hwnd: isize,
        nonce_sha256: String,
        paste_requests: u64,
        accepted_pastes: u64,
        button_requests: u64,
        accepted_button_reads: u64,
        rejected_requests: u64,
        receipt_write_failures: u64,
        last_trigger: &'static str,
        last_result: &'static str,
        last_clipboard_sequence: Option<u32>,
        last_formats: Vec<u32>,
        last_text: Option<TextReceipt>,
        last_images: Vec<ImageReceipt>,
    }

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct TextReceipt {
        utf8_sha256: String,
        utf8_bytes: usize,
        utf16_code_units: usize,
        unicode_scalars: usize,
    }

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct ImageReceipt {
        format: &'static str,
        allocation_bytes: usize,
        allocation_sha256: String,
        // PNG length ends after IEND; allocation padding is recorded separately above.
        content_bytes: Option<usize>,
        content_sha256: Option<String>,
        width: u32,
        height: u32,
    }

    struct App {
        root: PathBuf,
        nonce: String,
        marker: u32,
        png: u32,
        status_hwnd: isize,
        receipt: Receipt,
    }

    struct Paste {
        sequence: u32,
        formats: Vec<u32>,
        text: Option<String>,
        text_receipt: Option<TextReceipt>,
        images: Vec<ImageReceipt>,
    }

    fn sha(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    fn nonce_valid(nonce: &str) -> bool {
        nonce.len() == 32 && nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
    }

    fn marker_matches(bytes: &[u8], nonce: &str) -> bool {
        nonce_valid(nonce) && bytes.split(|byte| *byte == 0).next() == Some(nonce.as_bytes())
    }

    fn inspect_existing(path: &Path) -> Result<()> {
        if !path.is_absolute() {
            return Err("receiver_root_not_absolute");
        }
        let mut current = PathBuf::new();
        for part in path.components() {
            match part {
                Component::ParentDir | Component::CurDir => return Err("receiver_root_traversal"),
                Component::Prefix(_) => {
                    current.push(part.as_os_str());
                    // D: is drive-relative, and \\?\D: is not a complete
                    // verbatim root. Do not inspect either until RootDir
                    // contributes the following backslash.
                    continue;
                }
                _ => current.push(part.as_os_str()),
            }
            match fs::symlink_metadata(&current) {
                Ok(metadata) => {
                    if metadata.file_attributes() & BAD_ATTRIBUTES != 0
                        || metadata.file_type().is_symlink()
                    {
                        return Err("receiver_root_unsafe_attributes");
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(_) => return Err("receiver_root_metadata"),
            }
        }
        Ok(())
    }

    fn guarded_root(path: &Path) -> Result<PathBuf> {
        let scratch = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .ok_or("receiver_compiled_root")?
            .join(".tmp");
        if !path.is_absolute() || !path.starts_with(&scratch) || path == scratch {
            return Err("receiver_root_outside_compiled_scratch");
        }
        if path.components().any(|part| match part {
            Component::Normal(value) => value.to_string_lossy().contains(':'),
            Component::ParentDir | Component::CurDir => true,
            Component::Prefix(prefix) => !matches!(prefix.kind(), std::path::Prefix::Disk(_)),
            _ => false,
        }) {
            return Err("receiver_root_invalid_components");
        }
        inspect_existing(&scratch)?;
        inspect_existing(path)?;
        let approved = scratch
            .canonicalize()
            .map_err(|_| "receiver_scratch_missing")?;
        fs::create_dir_all(path).map_err(|_| "receiver_root_create")?;
        inspect_existing(path)?;
        let actual = path.canonicalize().map_err(|_| "receiver_root_canonical")?;
        if actual == approved || !actual.starts_with(approved) {
            return Err("receiver_root_canonical_escape");
        }
        Ok(actual)
    }

    fn write_receipt(app: &App, initial: bool) -> Result<()> {
        inspect_existing(&app.root)?;
        let destination = app.root.join("receiver-state.json");
        inspect_existing(&destination)?;
        let bytes = serde_json::to_vec_pretty(&app.receipt).map_err(|_| "receiver_receipt_json")?;
        if initial {
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(destination)
                .map_err(|_| "receiver_receipt_already_exists_or_unwritable")?;
            output
                .write_all(&bytes)
                .map_err(|_| "receiver_receipt_write")?;
            output.sync_all().map_err(|_| "receiver_receipt_sync")?;
        } else {
            let mut stage =
                tempfile::NamedTempFile::new_in(&app.root).map_err(|_| "receiver_receipt_stage")?;
            stage
                .write_all(&bytes)
                .map_err(|_| "receiver_receipt_write")?;
            stage
                .as_file()
                .sync_all()
                .map_err(|_| "receiver_receipt_sync")?;
            stage
                .persist(destination)
                .map_err(|_| "receiver_receipt_publish")?;
        }
        Ok(())
    }

    unsafe fn format_bytes(format: u32, limit: usize) -> Result<Vec<u8>> {
        let handle: HANDLE =
            GetClipboardData(format).map_err(|_| "clipboard_format_unavailable")?;
        let memory = HGLOBAL(handle.0);
        let size = GlobalSize(memory);
        if size == 0 || size > limit {
            return Err("clipboard_format_size");
        }
        let pointer = GlobalLock(memory);
        if pointer.is_null() {
            return Err("clipboard_format_lock");
        }
        let bytes = std::slice::from_raw_parts(pointer.cast::<u8>(), size).to_vec();
        let _ = GlobalUnlock(memory);
        Ok(bytes)
    }

    fn decode_text(bytes: &[u8]) -> Result<(String, TextReceipt)> {
        if bytes.len() % 2 != 0 {
            return Err("clipboard_unicode_odd_size");
        }
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|value| u16::from_le_bytes([value[0], value[1]]))
            .take_while(|value| *value != 0)
            .collect();
        if units.len() * 2 == bytes.len() {
            return Err("clipboard_unicode_no_terminator");
        }
        let text = String::from_utf16(&units).map_err(|_| "clipboard_unicode_invalid")?;
        let receipt = TextReceipt {
            utf8_sha256: sha(text.as_bytes()),
            utf8_bytes: text.len(),
            utf16_code_units: units.len(),
            unicode_scalars: text.chars().count(),
        };
        Ok((text, receipt))
    }

    fn dimensions_valid(width: u32, height: u32) -> bool {
        width > 0
            && height > 0
            && width <= 32768
            && height <= 32768
            && u64::from(width) * u64::from(height) <= 100_000_000
    }

    fn png_dimensions(bytes: &[u8]) -> Result<(u32, u32, usize)> {
        if bytes.len() < 33
            || &bytes[..8] != b"\x89PNG\r\n\x1a\n"
            || bytes[8..12] != 13u32.to_be_bytes()
            || &bytes[12..16] != b"IHDR"
        {
            return Err("clipboard_png_header");
        }
        let width = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
        let height = u32::from_be_bytes(bytes[20..24].try_into().unwrap());
        if !dimensions_valid(width, height) {
            return Err("clipboard_image_dimensions");
        }
        let mut offset = 8usize;
        while offset.checked_add(12).is_some_and(|end| end <= bytes.len()) {
            let length = u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
            let end = offset
                .checked_add(12)
                .and_then(|value| value.checked_add(length))
                .filter(|end| *end <= bytes.len())
                .ok_or("clipboard_png_chunk")?;
            if &bytes[offset + 4..offset + 8] == b"IEND" {
                if length != 0 {
                    return Err("clipboard_png_iend");
                }
                return Ok((width, height, end));
            }
            offset = end;
        }
        Err("clipboard_png_no_iend")
    }

    fn dib_dimensions(bytes: &[u8]) -> Result<(u32, u32)> {
        if bytes.len() < 12 {
            return Err("clipboard_dib_header");
        }
        let header = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        let (width, height) = if header == 12 {
            (
                u16::from_le_bytes(bytes[4..6].try_into().unwrap()) as u32,
                u16::from_le_bytes(bytes[6..8].try_into().unwrap()) as u32,
            )
        } else if header >= 40 && header <= bytes.len() {
            let width = i32::from_le_bytes(bytes[4..8].try_into().unwrap());
            let height = i32::from_le_bytes(bytes[8..12].try_into().unwrap());
            if width <= 0 || height == i32::MIN {
                return Err("clipboard_dib_dimensions");
            }
            (width as u32, height.unsigned_abs())
        } else {
            return Err("clipboard_dib_header");
        };
        if !dimensions_valid(width, height) {
            return Err("clipboard_image_dimensions");
        }
        Ok((width, height))
    }

    unsafe fn read_marked_clipboard(
        owner: HWND,
        marker: u32,
        png: u32,
        nonce: &str,
    ) -> Result<Paste> {
        OpenClipboard(owner).map_err(|_| "clipboard_busy")?;
        struct Close;
        impl Drop for Close {
            fn drop(&mut self) {
                unsafe {
                    let _ = CloseClipboard();
                }
            }
        }
        let _close = Close;
        // Do not enumerate formats or request any content until this small explicit marker matches.
        if IsClipboardFormatAvailable(marker).is_err()
            || !marker_matches(&format_bytes(marker, 128)?, nonce)
        {
            return Err("clipboard_fixture_marker_mismatch");
        }
        let mut formats = Vec::new();
        let mut current = 0;
        loop {
            current = EnumClipboardFormats(current);
            if current == 0 {
                break;
            }
            if formats.len() >= 256 {
                return Err("clipboard_too_many_formats");
            }
            formats.push(current);
        }
        let (text, text_receipt) = if IsClipboardFormatAvailable(TEXT_FORMAT).is_ok() {
            let (text, receipt) = decode_text(&format_bytes(TEXT_FORMAT, MAX_TEXT_BYTES)?)?;
            (Some(text), Some(receipt))
        } else {
            (None, None)
        };
        let mut images = Vec::new();
        for (format, name) in [
            (png, "PNG"),
            (DIB_FORMAT, "CF_DIB"),
            (DIBV5_FORMAT, "CF_DIBV5"),
        ] {
            if IsClipboardFormatAvailable(format).is_err() {
                continue;
            }
            let bytes = format_bytes(format, MAX_IMAGE_BYTES)?;
            let (width, height, content_bytes) = if format == png {
                let (width, height, length) = png_dimensions(&bytes)?;
                (width, height, Some(length))
            } else {
                let (width, height) = dib_dimensions(&bytes)?;
                (width, height, None)
            };
            images.push(ImageReceipt {
                format: name,
                allocation_bytes: bytes.len(),
                allocation_sha256: sha(&bytes),
                content_bytes,
                content_sha256: content_bytes.map(|length| sha(&bytes[..length])),
                width,
                height,
            });
        }
        if text.is_none() && images.is_empty() {
            return Err("clipboard_no_supported_test_payload");
        }
        // No CF_HDROP, OLE object, filenames, URLs, or unknown format bytes are read or executed.
        Ok(Paste {
            sequence: GetClipboardSequenceNumber(),
            formats,
            text,
            text_receipt,
            images,
        })
    }

    fn receive(trigger: &'static str, from_button: bool) {
        APP.with(|slot| {
            let mut slot = slot.borrow_mut();
            let Some(app) = slot.as_mut() else {
                return;
            };
            if from_button {
                app.receipt.button_requests += 1;
            } else {
                app.receipt.paste_requests += 1;
            }
            app.receipt.last_trigger = trigger;
            app.receipt.last_text = None;
            app.receipt.last_images.clear();
            app.receipt.last_formats.clear();
            app.receipt.last_clipboard_sequence = None;
            let owner = HWND(app.receipt.hwnd as *mut _);
            let result = unsafe { read_marked_clipboard(owner, app.marker, app.png, &app.nonce) };
            let status = match result {
                Ok(paste) => {
                    if from_button {
                        app.receipt.accepted_button_reads += 1;
                    } else {
                        app.receipt.accepted_pastes += 1;
                    }
                    app.receipt.last_result = "accepted";
                    app.receipt.last_clipboard_sequence = Some(paste.sequence);
                    app.receipt.last_formats = paste.formats;
                    app.receipt.last_text = paste.text_receipt;
                    app.receipt.last_images = paste.images;
                    if let Some(text) = paste.text {
                        // A WM_PASTE / actual Ctrl+V reached our EDIT. Present the bytes copied
                        // under the verified marker lock; never let EDIT re-open a raced clipboard.
                        unsafe {
                            let _ = SetWindowTextW(
                                HWND(app.receipt.edit_hwnd as *mut _),
                                &HSTRING::from(text),
                            );
                        }
                    }
                    if let Some(image) = app.receipt.last_images.first() {
                        format!(
                            "图片 {}x{}；已接收 {} 次粘贴",
                            image.width, image.height, app.receipt.accepted_pastes
                        )
                    } else {
                        format!("文字；已接收 {} 次粘贴", app.receipt.accepted_pastes)
                    }
                }
                Err(code) => {
                    app.receipt.rejected_requests += 1;
                    app.receipt.last_result = code;
                    "已拒绝：测试标记缺失、不同或载荷不可用".to_owned()
                }
            };
            let mut status = status;
            if write_receipt(app, false).is_err() {
                app.receipt.receipt_write_failures += 1;
                status = "回执写入失败".to_owned();
            }
            unsafe {
                let _ = SetWindowTextW(HWND(app.status_hwnd as *mut _), &HSTRING::from(status));
            }
        });
    }

    unsafe extern "system" fn edit_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        if msg == WM_PASTE {
            receive("edit_wm_paste", false);
            return LRESULT(0);
        }
        if msg == WM_KEYDOWN && wp.0 == b'V' as usize && GetKeyState(VK_CONTROL.0 as i32) < 0 {
            receive("edit_ctrl_v", false);
            return LRESULT(0);
        }
        if msg == WM_CHAR && wp.0 == 0x16 {
            return LRESULT(0);
        }
        let original = EDIT_ORIGINAL.with(Cell::get);
        if original == 0 {
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        let callback: WNDPROC = std::mem::transmute(original);
        CallWindowProcW(callback, hwnd, msg, wp, lp)
    }

    unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        match msg {
            WM_COMMAND
                if wp.0 & 0xffff == PASTE_ID && (wp.0 >> 16) & 0xffff == BN_CLICKED as usize =>
            {
                receive("paste_button", true);
                LRESULT(0)
            }
            WM_PASTE => {
                receive("window_wm_paste", false);
                LRESULT(0)
            }
            WM_DESTROY => {
                APP.with(|slot| {
                    if let Some(app) = slot.borrow_mut().as_mut() {
                        app.receipt.status = "closed";
                        let _ = write_receipt(app, false);
                    }
                });
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }

    pub fn run() -> Result<()> {
        let mut args = std::env::args_os().skip(1);
        let (mut root, mut nonce) = (None, None);
        while let Some(arg) = args.next() {
            match arg.to_str() {
                Some("--root") if root.is_none() => {
                    root = Some(PathBuf::from(args.next().ok_or("receiver_root_required")?))
                }
                Some("--nonce") if nonce.is_none() => {
                    nonce = Some(
                        args.next()
                            .and_then(|value| value.into_string().ok())
                            .ok_or("receiver_nonce_required")?,
                    )
                }
                _ => return Err("usage_receiver_root_and_32hex_nonce_required"),
            }
        }
        let nonce = nonce
            .filter(|value| nonce_valid(value))
            .ok_or("receiver_nonce_invalid")?;
        let root = guarded_root(&root.ok_or("receiver_root_required")?)?;
        // Claim this run before registering/showing any window; never overwrite another run's receipt.
        let receipt = Receipt {
            schema: 1,
            status: "starting",
            pid: std::process::id(),
            nonce_sha256: sha(nonce.as_bytes()),
            last_result: "none",
            last_trigger: "none",
            ..Default::default()
        };
        let mut app = App {
            root,
            nonce,
            marker: 0,
            png: 0,
            status_hwnd: 0,
            receipt,
        };
        write_receipt(&app, true)?;
        unsafe {
            app.marker = RegisterClipboardFormatW(w!("ClipHost.NativeFixture"));
            app.png = RegisterClipboardFormatW(w!("PNG"));
            if app.marker == 0 || app.png == 0 {
                return Err("receiver_format_registration");
            }
            let instance = GetModuleHandleW(None).map_err(|_| "receiver_module")?;
            let class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: instance.into(),
                lpszClassName: w!("StorageCoreAcceptanceReceiver"),
                hbrBackground: HBRUSH(6usize as *mut _),
                ..Default::default()
            };
            if RegisterClassW(&class) == 0 {
                return Err("receiver_class_registration");
            }
            let title = HSTRING::from(format!(
                "Storage Core Acceptance Receiver {}",
                &app.nonce[..8]
            ));
            let window = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class.lpszClassName,
                &title,
                WS_OVERLAPPEDWINDOW,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                720,
                440,
                None,
                None,
                instance,
                None,
            )
            .map_err(|_| "receiver_window_create")?;
            let edit = CreateWindowExW(
                WS_EX_CLIENTEDGE,
                w!("EDIT"),
                w!(""),
                WS_CHILD
                    | WS_VISIBLE
                    | WS_TABSTOP
                    | WS_VSCROLL
                    | WINDOW_STYLE((ES_MULTILINE | ES_AUTOVSCROLL | ES_WANTRETURN) as u32),
                16,
                16,
                670,
                260,
                window,
                HMENU(EDIT_ID as *mut _),
                instance,
                None,
            )
            .map_err(|_| "receiver_edit_create")?;
            let button = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("BUTTON"),
                w!("粘贴测试图片"),
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_PUSHBUTTON as u32),
                16,
                292,
                180,
                36,
                window,
                HMENU(PASTE_ID as *mut _),
                instance,
                None,
            )
            .map_err(|_| "receiver_button_create")?;
            let status = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                w!("等待带 nonce 标记的测试粘贴"),
                WS_CHILD | WS_VISIBLE,
                16,
                346,
                670,
                32,
                window,
                None,
                instance,
                None,
            )
            .map_err(|_| "receiver_status_create")?;
            app.receipt.hwnd = window.0 as isize;
            app.receipt.edit_hwnd = edit.0 as isize;
            app.receipt.paste_button_hwnd = button.0 as isize;
            app.status_hwnd = status.0 as isize;
            APP.with(|slot| *slot.borrow_mut() = Some(app));
            let original = SetWindowLongPtrW(edit, GWLP_WNDPROC, edit_proc as *const () as isize);
            if original == 0 {
                return Err("receiver_edit_subclass");
            }
            EDIT_ORIGINAL.with(|slot| slot.set(original));
            APP.with(|slot| {
                let mut slot = slot.borrow_mut();
                let app = slot.as_mut().unwrap();
                app.receipt.status = "ready";
                write_receipt(app, false)
            })?;
            let _ = ShowWindow(window, SW_SHOWNORMAL);
            let _ = SetFocus(edit);
            let mut message = MSG::default();
            loop {
                let result = GetMessageW(&mut message, None, 0, 0).0;
                if result == -1 {
                    return Err("receiver_message_loop");
                }
                if result == 0 {
                    break;
                }
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn canonical_repository_scratch_root_can_write_initial_receipt_without_gui() {
            let scratch = Path::new(env!("CARGO_MANIFEST_DIR"))
                .ancestors()
                .nth(3)
                .unwrap()
                .join(".tmp");
            let fixture = tempfile::Builder::new()
                .prefix("storage-core-receiver-preflight-")
                .tempdir_in(&scratch)
                .expect("receiver fixture in the compiled repository .tmp");
            let canonical = fixture.path().canonicalize().unwrap();
            assert!(matches!(
                canonical.components().next(),
                Some(Component::Prefix(prefix))
                    if matches!(prefix.kind(), std::path::Prefix::VerbatimDisk(_))
            ));
            inspect_existing(fixture.path()).unwrap();
            inspect_existing(&canonical).unwrap();
            let root = guarded_root(fixture.path()).unwrap();
            assert_eq!(root, canonical);
            inspect_existing(&root).unwrap();
            let nonce = "0123456789abcdef0123456789abcdef";
            let app = App {
                root,
                nonce: nonce.into(),
                marker: 0,
                png: 0,
                status_hwnd: 0,
                receipt: Receipt {
                    schema: 1,
                    status: "starting",
                    pid: std::process::id(),
                    nonce_sha256: sha(nonce.as_bytes()),
                    ..Default::default()
                },
            };
            write_receipt(&app, true).unwrap();
            let receipt: serde_json::Value =
                serde_json::from_slice(&fs::read(app.root.join("receiver-state.json")).unwrap())
                    .unwrap();
            assert_eq!(receipt["status"], "starting");
            assert_eq!(receipt["nonceSha256"], sha(nonce.as_bytes()));
            assert_eq!(receipt["hwnd"], 0);
            assert!(
                write_receipt(&app, true).is_err(),
                "receipt remains create_new"
            );
            assert!(inspect_existing(Path::new("D:")).is_err());
        }

        #[test]
        fn marker_requires_the_exact_valid_run() {
            let nonce = "0123456789abcdef0123456789abcdef";
            assert!(marker_matches(nonce.as_bytes(), nonce));
            assert!(marker_matches(format!("{nonce}\0").as_bytes(), nonce));
            assert!(!marker_matches(b"another fixture\0", nonce));
            assert!(!marker_matches(nonce.as_bytes(), ""));
        }

        #[test]
        fn text_hashes_exact_unicode_without_terminator_or_allocation_padding() {
            let text = "Synthetic 接收器\r\nline two 🦀";
            let bytes: Vec<u8> = text
                .encode_utf16()
                .chain([0, 0])
                .flat_map(u16::to_le_bytes)
                .collect();
            let (actual, receipt) = decode_text(&bytes).unwrap();
            assert_eq!(actual, text);
            assert_eq!(receipt.utf8_sha256, sha(text.as_bytes()));
            assert_eq!(receipt.utf16_code_units, text.encode_utf16().count());
            assert!(decode_text(&[b'x', 0]).is_err());
            assert!(decode_text(&[0]).is_err());
        }

        #[test]
        fn image_headers_are_bounded_and_do_not_parse_unknown_payloads() {
            let mut dib = vec![0u8; 40];
            dib[..4].copy_from_slice(&40u32.to_le_bytes());
            dib[4..8].copy_from_slice(&2i32.to_le_bytes());
            dib[8..12].copy_from_slice(&(-3i32).to_le_bytes());
            assert_eq!(dib_dimensions(&dib).unwrap(), (2, 3));
            dib[4..8].copy_from_slice(&i32::MAX.to_le_bytes());
            assert!(dib_dimensions(&dib).is_err());
            assert!(png_dimensions(b"not a PNG").is_err());
        }
    }
}
