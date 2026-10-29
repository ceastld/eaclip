//! Independent native OLE receiver for Test-OleDragWorkflow.ps1. No clipboard
//! access, file opening, network listeners or calls into the host drag provider.
#[cfg(not(windows))]
fn main() {
    eprintln!("Windows only");
}

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    win::run()
}

#[cfg(windows)]
mod win {
    use serde_json::{json, Value};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use windows::core::{implement, w};
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINTL, WPARAM};
    use windows::Win32::Graphics::Gdi::HBRUSH;
    use windows::Win32::System::Com::{
        IDataObject, DVASPECT_CONTENT, FORMATETC, STGMEDIUM, TYMED_HGLOBAL,
    };
    use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    use windows::Win32::System::Ole::{
        IDropTarget, IDropTarget_Impl, OleInitialize, OleUninitialize, RegisterDragDrop,
        ReleaseStgMedium, RevokeDragDrop, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE,
    };
    use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
    use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
    use windows::Win32::UI::WindowsAndMessaging::*;

    fn write_json(root: &Path, name: &str, value: &Value) -> std::io::Result<()> {
        let pending = root.join(format!("{name}.pending"));
        std::fs::write(&pending, serde_json::to_vec_pretty(value)?)?;
        std::fs::rename(pending, root.join(name))
    }

    struct Medium(STGMEDIUM);
    impl Drop for Medium {
        fn drop(&mut self) {
            unsafe {
                ReleaseStgMedium(&mut self.0);
            }
        }
    }
    impl Medium {
        fn bytes(&self) -> Result<Vec<u8>, String> {
            unsafe {
                let handle = self.0.u.hGlobal;
                let size = GlobalSize(handle);
                if size == 0 || size > 16 * 1024 * 1024 {
                    return Err("Unexpected fixture size".into());
                }
                let pointer = GlobalLock(handle);
                if pointer.is_null() {
                    return Err("Cannot lock dragged data".into());
                }
                let bytes = std::slice::from_raw_parts(pointer.cast::<u8>(), size).to_vec();
                let _ = GlobalUnlock(handle);
                Ok(bytes)
            }
        }
    }
    fn get(data: &IDataObject, format: u16) -> Result<Option<Medium>, String> {
        let request = FORMATETC {
            cfFormat: format,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
        };
        unsafe {
            if data.QueryGetData(&request).is_err() {
                return Ok(None);
            }
            let medium = Medium(data.GetData(&request).map_err(|e| e.to_string())?);
            if medium.0.tymed != TYMED_HGLOBAL.0 as u32 {
                return Err("Unexpected storage medium".into());
            }
            Ok(Some(medium))
        }
    }

    fn read_drop(data: &IDataObject, html: u16) -> Result<Value, String> {
        let mut result = json!({"effect":"Copy"});
        let mut formats = Vec::new();
        if let Some(medium) = get(data, 13)? {
            let bytes = medium.bytes()?;
            if bytes.len() % 2 != 0 {
                return Err("Invalid UTF-16 byte count".into());
            }
            let units: Vec<u16> = bytes
                .chunks_exact(2)
                .map(|p| u16::from_le_bytes([p[0], p[1]]))
                .take_while(|u| *u != 0)
                .collect();
            result["text"] = json!(String::from_utf16(&units).map_err(|e| e.to_string())?);
            formats.push("UnicodeText");
        }
        if let Some(medium) = get(data, html)? {
            let bytes = medium.bytes()?;
            let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
            result["html"] = json!(std::str::from_utf8(&bytes[..end]).map_err(|e| e.to_string())?);
            result["htmlByteLength"] = json!(end);
            formats.push("HTML Format");
        }
        if let Some(medium) = get(data, 15)? {
            let mut files = Vec::new();
            unsafe {
                let drop = HDROP(medium.0.u.hGlobal.0);
                let count = DragQueryFileW(drop, u32::MAX, None);
                if count > 100 {
                    return Err("Too many fixture files".into());
                }
                for index in 0..count {
                    let len = DragQueryFileW(drop, index, None) as usize;
                    let mut path = vec![0u16; len + 1];
                    DragQueryFileW(drop, index, Some(&mut path));
                    files.push(String::from_utf16(&path[..len]).map_err(|e| e.to_string())?);
                }
            }
            result["files"] = json!(files);
            formats.push("FileDrop");
        }
        if let Some(medium) = get(data, 8)? {
            let bytes = medium.bytes()?;
            if bytes.len() < 40 {
                return Err("Truncated DIB".into());
            }
            result["dib"] = json!({"width":i32::from_le_bytes(bytes[4..8].try_into().unwrap()),
                "height":i32::from_le_bytes(bytes[8..12].try_into().unwrap()),
                "bits":u16::from_le_bytes(bytes[14..16].try_into().unwrap())});
            formats.push("DeviceIndependentBitmap");
        }
        result["formats"] = json!(formats);
        Ok(result)
    }

    fn allow_copy(effect: *mut DROPEFFECT) {
        if let Some(effect) = unsafe { effect.as_mut() } {
            effect.0 &= DROPEFFECT_COPY.0;
        }
    }
    #[implement(IDropTarget)]
    struct Receiver {
        root: PathBuf,
        drop_count: AtomicUsize,
        html: u16,
    }
    impl IDropTarget_Impl for Receiver_Impl {
        fn DragEnter(
            &self,
            _: Option<&IDataObject>,
            _: MODIFIERKEYS_FLAGS,
            _: &POINTL,
            effect: *mut DROPEFFECT,
        ) -> windows::core::Result<()> {
            allow_copy(effect);
            Ok(())
        }
        fn DragOver(
            &self,
            _: MODIFIERKEYS_FLAGS,
            _: &POINTL,
            effect: *mut DROPEFFECT,
        ) -> windows::core::Result<()> {
            allow_copy(effect);
            Ok(())
        }
        fn DragLeave(&self) -> windows::core::Result<()> {
            Ok(())
        }
        fn Drop(
            &self,
            data: Option<&IDataObject>,
            _: MODIFIERKEYS_FLAGS,
            _: &POINTL,
            effect: *mut DROPEFFECT,
        ) -> windows::core::Result<()> {
            let count = self.drop_count.fetch_add(1, Ordering::Relaxed) + 1;
            let mut result = match data
                .ok_or("Missing IDataObject".into())
                .and_then(|data| read_drop(data, self.html))
            {
                Ok(result) => {
                    allow_copy(effect);
                    result
                }
                Err(error) => {
                    if let Some(effect) = unsafe { effect.as_mut() } {
                        *effect = DROPEFFECT_NONE;
                    }
                    json!({"error":error})
                }
            };
            result["count"] = json!(count);
            write_json(&self.root, &format!("drop-{count}.json"), &result).map_err(|e| {
                windows::core::Error::new(windows::Win32::Foundation::E_FAIL, e.to_string())
            })
        }
    }

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        message: u32,
        w: WPARAM,
        l: LPARAM,
    ) -> LRESULT {
        match message {
            WM_TIMER | WM_CLOSE => {
                let _ = RevokeDragDrop(hwnd);
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, message, w, l),
        }
    }

    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        let root = PathBuf::from(
            std::env::args_os()
                .nth(1)
                .ok_or("Output directory required")?,
        )
        .canonicalize()?;
        let temporary = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(5)
            .ok_or("Missing workspace")?
            .join(".tmp")
            .canonicalize()?;
        if !root.starts_with(temporary) || root.join("receiver.json").exists() {
            return Err("Use a fresh workspace .tmp output directory".into());
        }
        unsafe {
            OleInitialize(None)?;
            struct Ole;
            impl Drop for Ole {
                fn drop(&mut self) {
                    unsafe {
                        OleUninitialize();
                    }
                }
            }
            let _ole = Ole;
            let instance = GetModuleHandleW(None)?;
            let class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: instance.into(),
                lpszClassName: w!("ClipHostOleReceiver"),
                hbrBackground: HBRUSH(6usize as *mut _),
                ..Default::default()
            };
            if RegisterClassW(&class) == 0 {
                return Err("Window class registration failed".into());
            }
            let window = CreateWindowExW(
                WS_EX_TOPMOST,
                class.lpszClassName,
                w!("Rust OLE test receiver"),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                1450,
                70,
                390,
                310,
                None,
                None,
                instance,
                None,
            )?;
            let target: IDropTarget = Receiver {
                root: root.clone(),
                drop_count: AtomicUsize::new(0),
                html: RegisterClipboardFormatW(w!("HTML Format")) as u16,
            }
            .into();
            RegisterDragDrop(window, &target)?;
            SetTimer(window, 1, 180_000, None);
            write_json(
                &root,
                "receiver.json",
                &json!({"pid":std::process::id(), "hwnd":window.0 as isize}),
            )?;
            let mut message = MSG::default();
            while GetMessageW(&mut message, None, 0, 0).0 > 0 {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
            drop(target);
            Ok(())
        }
    }
}
