//! Native OLE drag aligned with C# `BeginOleDragByIds` (no clipboard write).

use std::collections::HashSet;

use crate::clipboard::is_headless;
use crate::model::{ClipKind, ClipRecord};

const FMT_HTML: &str = "HTML Format";
const FMT_QQ: &str = "QQ_Unicode_RichEdit_Format";
const FMT_WECHAT: &str = "WeChat_RichEdit_Format";
const FMT_QUICKER_STEPS: &str = "quicker-action-steps";

#[derive(Debug, Clone, Default)]
pub struct OleDragPayload {
    /// An opaque in-process gesture token; never contains records or paths.
    pub internal_drag_token: Option<String>,
    pub text: String,
    pub files: Vec<String>,
    pub html: Option<String>,
    pub qq_rich: Option<String>,
    pub wechat_rich: Option<String>,
    pub dib: Option<Vec<u8>>,
    pub quicker_steps: Option<String>,
}

impl OleDragPayload {
    pub fn is_empty(&self) -> bool {
        self.internal_drag_token.is_none() && self.text.is_empty()
            && self.files.is_empty()
            && self.html.is_none()
            && self.qq_rich.is_none()
            && self.wechat_rich.is_none()
            && self.dib.is_none()
            && self.quicker_steps.is_none()
    }
}

/// C# `ClipItemFactory.CreateMultiItemsDataObject`: TextClipItem texts joined with
/// `Environment.NewLine`, FileDrop from files/images, first-occurrence distinct.
pub fn multi_items_text_and_files(items: &[ClipRecord]) -> (String, Vec<String>) {
    let texts: Vec<&str> = items
        .iter()
        .filter(|item| is_text_clip_item(item.kind))
        .filter_map(|item| item.body_text.as_deref().filter(|s| !s.is_empty()))
        .collect();
    let mut files = Vec::new();
    let mut seen = HashSet::new();
    for item in items {
        match item.kind {
            ClipKind::FileClipItem => {
                for path in &item.file_paths {
                    let path = path.trim();
                    if path.is_empty() || !seen.insert(path.to_string()) {
                        continue;
                    }
                    files.push(path.to_string());
                }
            }
            ClipKind::ImageClipItem => {
                if let Some(path) = item
                    .image_path
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    if seen.insert(path.to_string()) {
                        files.push(path.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    (texts.join("\r\n"), files)
}

fn is_text_clip_item(kind: ClipKind) -> bool {
    matches!(
        kind,
        ClipKind::TextClipItem | ClipKind::HtmlClipItem | ClipKind::QQRichTextItem
    )
}

pub fn payload_from_items(
    items: &[ClipRecord],
    plain_text: bool,
    standard_paste: bool,
    paste_with_new_line: bool,
    target_process: &str,
) -> Result<OleDragPayload,String> {
    for item in items { crate::clipboard_payload::require_output_projection(item)?; }
    Ok(match items {
        [] => OleDragPayload::default(),
        [item] => payload_from_single(
            item,
            plain_text,
            standard_paste,
            paste_with_new_line,
            target_process,
        ),
        _ => payload_from_multi(items),
    })
}

/// C# `CreateMultiItemsDataObject`: joined TextClipItem text + file/image paths, no extra formats.
fn payload_from_multi(items: &[ClipRecord]) -> OleDragPayload {
    let (text, files) = multi_items_text_and_files(items);
    OleDragPayload {
        text,
        files,
        ..OleDragPayload::default()
    }
}

/// C# `ClipItemBase.GetDragDataObject` → `SetToDataObject`.
fn payload_from_single(
    item: &ClipRecord,
    plain_text: bool,
    standard_paste: bool,
    paste_with_new_line: bool,
    target_process: &str,
) -> OleDragPayload {
    let text =
        crate::clipboard::set_to_data_object_unicode(item, paste_with_new_line, standard_paste)
            .unwrap_or_default();
    let mut payload = OleDragPayload {
        text,
        ..OleDragPayload::default()
    };
    match item.kind {
        ClipKind::HtmlClipItem | ClipKind::QQRichTextItem if !plain_text => {
            if let Some(raw) = item.html_data.as_deref().filter(|s| !s.is_empty()) {
                payload.html = Some(if crate::cf_html::looks_like_complete_format(raw) {
                    raw.to_string()
                } else {
                    crate::cf_html::build_clipboard_html(raw, item.html_source_url.as_deref())
                });
            }
            if item.kind == ClipKind::QQRichTextItem {
                if let Some(raw) = item.qq_rich_text.as_deref().filter(|s| !s.is_empty()) {
                    let wechat = crate::clipboard::is_wechat_family(target_process);
                    let origin = if wechat {
                        crate::im_rich::ImRichOrigin::WeChat
                    } else {
                        crate::im_rich::ImRichOrigin::Qq
                    };
                    let xml = crate::im_rich::rewrite_for_record_origin(
                        raw,
                        &item.source_process_name,
                        origin,
                    );
                    if wechat {
                        payload.wechat_rich = Some(xml);
                    } else {
                        payload.qq_rich = Some(xml);
                    }
                }
            }
        }
        ClipKind::StepClipItem => {
            payload.quicker_steps = item.body_text.clone().filter(|s| !s.is_empty());
        }
        ClipKind::FileClipItem if crate::clipboard::file_clip_item_is_single_image(item) => {
            fill_image_formats(&mut payload, &item.file_paths[0], target_process);
        }
        ClipKind::FileClipItem => {
            payload.files = crate::clipboard::file_clip_existing_paths(item);
        }
        ClipKind::ImageClipItem => {
            if let Some(path) = item.image_path.as_deref().filter(|s| !s.is_empty()) {
                fill_image_formats(&mut payload, path, target_process);
            }
        }
        _ => {}
    }
    payload
}

fn fill_image_formats(payload: &mut OleDragPayload, path: &str, process: &str) {
    let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    match crate::clipboard::classify_image_paste_target(process, len) {
        crate::clipboard::ImagePasteTarget::FileDropOnly => {
            payload.files = vec![path.to_string()];
        }
        crate::clipboard::ImagePasteTarget::QqImage => {
            // C# `SetQQImage` always uses `QQ_Unicode_RichEdit_Format`.
            payload.qq_rich = Some(crate::im_rich::qq_image_envelope(path));
        }
        crate::clipboard::ImagePasteTarget::BitmapAndFile => {
            payload.files = vec![path.to_string()];
            payload.dib = crate::clipboard::encode_dib_from_path(path).ok();
        }
    }
}

/// C# `ImageClipItem.SetToDataObject` uses `SetQQImage` when the paste target is QQ/WeChat/TIM.
pub fn apply_im_image_envelope(payload: &mut OleDragPayload, target_process: &str) {
    if !crate::clipboard::is_im_process_name(target_process) {
        return;
    }
    let Some(path) = payload
        .files
        .iter()
        .find(|p| is_image_file_extension(p))
        .cloned()
        .or_else(|| payload.files.first().cloned())
    else {
        return;
    };
    // C# `SetQQImage` always writes QQ format, including WeChat/WXWork targets.
    payload.files.clear();
    payload.dib = None;
    payload.wechat_rich = None;
    payload.qq_rich = Some(crate::im_rich::qq_image_envelope(&path));
}

fn is_image_file_extension(path: &str) -> bool {
    crate::clipboard::is_image_file_extension(path)
}

pub fn begin_ole_drag(payload: &OleDragPayload) -> Result<(), String> {
    begin_ole_drag_guarded(payload, std::sync::Arc::new(|| Ok(())))
}

pub type OleDragGuard = std::sync::Arc<dyn Fn() -> Result<(), String> + Send + Sync>;

/// DROPEFFECT of the current outgoing OLE drag: the target's live answer
/// while the loop runs, then the final effect (0 = cancelled or refused)
/// once it returns. The drag preview reads it on the same UI thread.
static DROP_EFFECT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

pub fn drop_effect() -> u32 {
    DROP_EFFECT.load(std::sync::atomic::Ordering::Acquire)
}

#[cfg(test)]
fn fixture_directory() -> tempfile::TempDir {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
    std::fs::create_dir_all(&root).unwrap();
    tempfile::Builder::new()
        .prefix("native-ole-fixture-")
        .tempdir_in(root)
        .unwrap()
}

/// Root window/page retirement also cancels OLE's nested message loop.
pub fn begin_ole_drag_guarded(payload: &OleDragPayload, guard: OleDragGuard) -> Result<(), String> {
    guard()?;
    if is_headless() {
        return Ok(());
    }
    if payload.is_empty() {
        return Err("no transferable formats".into());
    }
    #[cfg(windows)]
    {
        win::do_drag_drop(payload, guard)
    }
    #[cfg(not(windows))]
    {
        Err("OLE drag is Windows-only".into())
    }
}

#[cfg(windows)]
mod win {
    use super::{FMT_HTML, FMT_QQ, FMT_QUICKER_STEPS, FMT_WECHAT, OleDragGuard, OleDragPayload};
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Foundation::{
        BOOL, DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, S_OK,
    };
    use windows::Win32::System::Com::{
        DVASPECT_CONTENT, FORMATETC, IDataObject, IDataObject_Impl, IEnumFORMATETC, STGMEDIUM,
        TYMED_HGLOBAL,
    };
    use windows::Win32::System::Memory::{
        GMEM_MOVEABLE, GMEM_ZEROINIT, GlobalAlloc, GlobalLock, GlobalUnlock,
    };
    use windows::Win32::System::Ole::{
        DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE, DoDragDrop, IDropSource, IDropSource_Impl,
        OleInitialize, OleUninitialize,
    };
    use windows::Win32::UI::Shell::DROPFILES;
    use windows::core::{HRESULT, implement};

    const CF_UNICODETEXT: u16 = 13;
    const CF_HDROP: u16 = 15;
    const CF_DIB: u16 = 8;
    const MK_LBUTTON: u32 = 0x0001;

    #[implement(IDropSource)]
    struct DropSource {
        guard: OleDragGuard,
    }

    impl IDropSource_Impl for DropSource_Impl {
        fn QueryContinueDrag(
            &self,
            fescapepressed: BOOL,
            grfkeystate: windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS,
        ) -> HRESULT {
            if (self.guard)().is_err() {
                return DRAGDROP_S_CANCEL;
            }
            tracing::debug!(
                escape = fescapepressed.as_bool(),
                keys = grfkeystate.0,
                "OLE drag continuation"
            );
            if fescapepressed.as_bool() {
                return DRAGDROP_S_CANCEL;
            }
            if grfkeystate.0 & MK_LBUTTON == 0 {
                return DRAGDROP_S_DROP;
            }
            S_OK
        }

        fn GiveFeedback(&self, dweffect: DROPEFFECT) -> HRESULT {
            super::DROP_EFFECT.store(dweffect.0, std::sync::atomic::Ordering::Release);
            DRAGDROP_S_USEDEFAULTCURSORS
        }
    }

    #[implement(IDataObject)]
    struct ClipDataObject {
        internal_drag_token: Option<String>,
        cf_internal: u16,
        guard: OleDragGuard,
        text: String,
        files: Vec<String>,
        html: Option<String>,
        qq_rich: Option<String>,
        wechat_rich: Option<String>,
        dib: Option<Vec<u8>>,
        quicker_steps: Option<String>,
        cf_html: u16,
        cf_qq: u16,
        cf_wechat: u16,
        cf_steps: u16,
    }

    impl ClipDataObject {
        fn fmt(cf: u16) -> FORMATETC {
            FORMATETC {
                cfFormat: cf,
                ptd: std::ptr::null_mut(),
                dwAspect: DVASPECT_CONTENT.0,
                lindex: -1,
                tymed: TYMED_HGLOBAL.0 as u32,
            }
        }

        fn formats(&self) -> Vec<FORMATETC> {
            let mut out = Vec::new();
            if self.internal_drag_token.is_some() && self.cf_internal != 0 {
                out.push(Self::fmt(self.cf_internal));
            }
            if !self.text.is_empty() {
                out.push(Self::fmt(CF_UNICODETEXT));
            }
            if !self.files.is_empty() {
                out.push(Self::fmt(CF_HDROP));
            }
            if self.html.is_some() && self.cf_html != 0 {
                out.push(Self::fmt(self.cf_html));
            }
            if self.qq_rich.is_some() && self.cf_qq != 0 {
                out.push(Self::fmt(self.cf_qq));
            }
            if self.wechat_rich.is_some() && self.cf_wechat != 0 {
                out.push(Self::fmt(self.cf_wechat));
            }
            if self.dib.is_some() {
                out.push(Self::fmt(CF_DIB));
            }
            if self.quicker_steps.is_some() && self.cf_steps != 0 {
                out.push(Self::fmt(self.cf_steps));
            }
            out
        }

        fn validate(&self, fmt: &FORMATETC) -> windows::core::Result<()> {
            if (self.guard)().is_err() {
                return Err(windows::Win32::Foundation::E_ABORT.into());
            }
            use windows::Win32::Foundation::{
                DV_E_DVASPECT, DV_E_FORMATETC, DV_E_LINDEX, DV_E_TYMED,
            };
            let error = if fmt.dwAspect != DVASPECT_CONTENT.0 {
                Some(DV_E_DVASPECT)
            } else if fmt.lindex != -1 {
                Some(DV_E_LINDEX)
            } else if fmt.tymed & TYMED_HGLOBAL.0 as u32 == 0 {
                Some(DV_E_TYMED)
            } else if !self
                .formats()
                .iter()
                .any(|owned| owned.cfFormat == fmt.cfFormat)
            {
                Some(DV_E_FORMATETC)
            } else {
                None
            };
            match error {
                Some(error) => Err(error.into()),
                None => Ok(()),
            }
        }
    }

    impl IDataObject_Impl for ClipDataObject_Impl {
        fn GetData(&self, pformatetcin: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
            let fmt = unsafe { pformatetcin.as_ref() }.ok_or(windows::core::Error::from(
                windows::Win32::Foundation::E_INVALIDARG,
            ))?;
            self.validate(fmt)?;
            let hg = if fmt.cfFormat == self.cf_internal && self.internal_drag_token.is_some() {
                alloc_bytes(self.internal_drag_token.as_ref().unwrap().as_bytes())?
            } else if fmt.cfFormat == CF_UNICODETEXT && !self.text.is_empty() {
                alloc_unicode(&self.text)?
            } else if fmt.cfFormat == CF_HDROP && !self.files.is_empty() {
                alloc_hdrop(&self.files)?
            } else if fmt.cfFormat == self.cf_html {
                if let Some(html) = &self.html {
                    alloc_bytes(html.as_bytes())?
                } else {
                    return Err(windows::core::Error::from(
                        windows::Win32::Foundation::DV_E_FORMATETC,
                    ));
                }
            } else if fmt.cfFormat == self.cf_qq {
                if let Some(xml) = &self.qq_rich {
                    alloc_bytes(xml.as_bytes())?
                } else {
                    return Err(windows::core::Error::from(
                        windows::Win32::Foundation::DV_E_FORMATETC,
                    ));
                }
            } else if fmt.cfFormat == self.cf_wechat {
                if let Some(xml) = &self.wechat_rich {
                    alloc_bytes(xml.as_bytes())?
                } else {
                    return Err(windows::core::Error::from(
                        windows::Win32::Foundation::DV_E_FORMATETC,
                    ));
                }
            } else if fmt.cfFormat == CF_DIB {
                if let Some(dib) = &self.dib {
                    alloc_exact(dib)?
                } else {
                    return Err(windows::core::Error::from(
                        windows::Win32::Foundation::DV_E_FORMATETC,
                    ));
                }
            } else if fmt.cfFormat == self.cf_steps {
                if let Some(steps) = &self.quicker_steps {
                    alloc_unicode(steps)?
                } else {
                    return Err(windows::core::Error::from(
                        windows::Win32::Foundation::DV_E_FORMATETC,
                    ));
                }
            } else {
                return Err(windows::core::Error::from(
                    windows::Win32::Foundation::DV_E_FORMATETC,
                ));
            };
            let mut medium = STGMEDIUM {
                tymed: TYMED_HGLOBAL.0 as u32,
                u: windows::Win32::System::Com::STGMEDIUM_0 { hGlobal: hg },
                pUnkForRelease: std::mem::ManuallyDrop::new(None),
            };
            #[cfg(test)]
            tests::observe_prepared_medium();
            // Preparation can allocate/encode a large payload after validate.
            // A retired source must not hand that prepared medium to a target.
            if (self.guard)().is_err() {
                unsafe {
                    windows::Win32::System::Ole::ReleaseStgMedium(&mut medium);
                }
                return Err(windows::Win32::Foundation::E_ABORT.into());
            }
            Ok(medium)
        }

        fn GetDataHere(
            &self,
            _pformatetc: *const FORMATETC,
            _pmedium: *mut STGMEDIUM,
        ) -> windows::core::Result<()> {
            Err(windows::core::Error::from(
                windows::Win32::Foundation::E_NOTIMPL,
            ))
        }

        fn QueryGetData(&self, pformatetc: *const FORMATETC) -> windows::core::HRESULT {
            let Some(fmt) = (unsafe { pformatetc.as_ref() }) else {
                return windows::Win32::Foundation::E_INVALIDARG;
            };
            self.validate(fmt)
                .map_or_else(|error| error.code(), |_| S_OK)
        }

        fn GetCanonicalFormatEtc(
            &self,
            pformatectin: *const FORMATETC,
            pformatetcout: *mut FORMATETC,
        ) -> windows::core::HRESULT {
            if !pformatetcout.is_null() && !pformatectin.is_null() {
                unsafe {
                    *pformatetcout = *pformatectin;
                    (*pformatetcout).ptd = std::ptr::null_mut();
                }
            }
            windows::Win32::Foundation::DATA_S_SAMEFORMATETC
        }

        fn SetData(
            &self,
            _pformatetc: *const FORMATETC,
            _pmedium: *const STGMEDIUM,
            _frelease: BOOL,
        ) -> windows::core::Result<()> {
            Err(windows::core::Error::from(
                windows::Win32::Foundation::E_NOTIMPL,
            ))
        }

        fn EnumFormatEtc(&self, dwdirection: u32) -> windows::core::Result<IEnumFORMATETC> {
            if dwdirection != 1 {
                return Err(windows::core::Error::from(
                    windows::Win32::Foundation::E_NOTIMPL,
                ));
            }
            let formats = self.formats();
            unsafe { windows::Win32::UI::Shell::SHCreateStdEnumFmtEtc(&formats) }
        }

        fn DAdvise(
            &self,
            _pformatetc: *const FORMATETC,
            _advf: u32,
            _padvsink: Option<&windows::Win32::System::Com::IAdviseSink>,
        ) -> windows::core::Result<u32> {
            Err(windows::core::Error::from(
                windows::Win32::Foundation::OLE_E_ADVISENOTSUPPORTED,
            ))
        }

        fn DUnadvise(&self, _dwconnection: u32) -> windows::core::Result<()> {
            Err(windows::core::Error::from(
                windows::Win32::Foundation::OLE_E_ADVISENOTSUPPORTED,
            ))
        }

        fn EnumDAdvise(&self) -> windows::core::Result<windows::Win32::System::Com::IEnumSTATDATA> {
            Err(windows::core::Error::from(
                windows::Win32::Foundation::OLE_E_ADVISENOTSUPPORTED,
            ))
        }
    }

    fn alloc_unicode(text: &str) -> windows::core::Result<windows::Win32::Foundation::HGLOBAL> {
        let mut wide: Vec<u16> = text.encode_utf16().collect();
        wide.push(0);
        unsafe {
            let hg = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, wide.len() * 2)?;
            let ptr = GlobalLock(hg) as *mut u16;
            if ptr.is_null() {
                let _ = windows::Win32::Foundation::GlobalFree(hg);
                return Err(windows::core::Error::from(
                    windows::Win32::Foundation::E_OUTOFMEMORY,
                ));
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
            let _ = GlobalUnlock(hg);
            Ok(hg)
        }
    }

    fn alloc_bytes(bytes: &[u8]) -> windows::core::Result<windows::Win32::Foundation::HGLOBAL> {
        let mut terminated = bytes.to_vec();
        terminated.push(0);
        alloc_exact(&terminated)
    }

    fn alloc_exact(bytes: &[u8]) -> windows::core::Result<windows::Win32::Foundation::HGLOBAL> {
        unsafe {
            let hg = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes.len())?;
            let ptr = GlobalLock(hg) as *mut u8;
            if ptr.is_null() {
                let _ = windows::Win32::Foundation::GlobalFree(hg);
                return Err(windows::core::Error::from(
                    windows::Win32::Foundation::E_OUTOFMEMORY,
                ));
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
            let _ = GlobalUnlock(hg);
            Ok(hg)
        }
    }

    fn register_format(name: &str) -> u16 {
        use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
        use windows::core::HSTRING;
        unsafe { RegisterClipboardFormatW(&HSTRING::from(name)) as u16 }
    }

    fn alloc_hdrop(paths: &[String]) -> windows::core::Result<windows::Win32::Foundation::HGLOBAL> {
        let mut wide: Vec<u16> = Vec::new();
        for path in paths {
            wide.extend(path.encode_utf16());
            wide.push(0);
        }
        wide.push(0);
        let header_size = std::mem::size_of::<DROPFILES>();
        let total = header_size + wide.len() * 2;
        unsafe {
            let hg = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, total)?;
            let ptr = GlobalLock(hg) as *mut u8;
            if ptr.is_null() {
                let _ = windows::Win32::Foundation::GlobalFree(hg);
                return Err(windows::core::Error::from(
                    windows::Win32::Foundation::E_OUTOFMEMORY,
                ));
            }
            std::ptr::write(
                ptr as *mut DROPFILES,
                DROPFILES {
                    pFiles: header_size as u32,
                    pt: POINT { x: 0, y: 0 },
                    fNC: BOOL(0),
                    fWide: BOOL(1),
                },
            );
            std::ptr::copy_nonoverlapping(
                wide.as_ptr() as *const u8,
                ptr.add(header_size),
                wide.len() * 2,
            );
            let _ = GlobalUnlock(hg);
            Ok(hg)
        }
    }

    #[cfg(test)]
    fn data_object(payload: &OleDragPayload) -> IDataObject {
        data_object_guarded(payload, std::sync::Arc::new(|| Ok(())))
    }

    fn data_object_guarded(payload: &OleDragPayload, guard: OleDragGuard) -> IDataObject {
        let cf_html = register_format(FMT_HTML);
        let cf_qq = register_format(FMT_QQ);
        let cf_wechat = register_format(FMT_WECHAT);
        let cf_steps = register_format(FMT_QUICKER_STEPS);
        ClipDataObject {
            internal_drag_token: payload.internal_drag_token.clone(),
            cf_internal: register_format("ClipHost.InternalDrag.v1"),
            guard,
            text: payload.text.clone(),
            files: payload.files.clone(),
            html: payload.html.clone(),
            qq_rich: payload.qq_rich.clone(),
            wechat_rich: payload.wechat_rich.clone(),
            dib: payload.dib.clone(),
            quicker_steps: payload.quicker_steps.clone(),
            cf_html,
            cf_qq,
            cf_wechat,
            cf_steps,
        }
        .into()
    }

    pub(super) fn do_drag_drop(
        payload: &OleDragPayload,
        guard: OleDragGuard,
    ) -> Result<(), String> {
        do_drag_drop_with_dispatch(payload, guard, |data, source, effect| unsafe {
            DoDragDrop(data, source, DROPEFFECT_COPY, effect)
        })
    }

    fn do_drag_drop_with_dispatch(
        payload: &OleDragPayload,
        guard: OleDragGuard,
        dispatch: impl FnOnce(&IDataObject, &IDropSource, &mut DROPEFFECT) -> HRESULT,
    ) -> Result<(), String> {
        unsafe {
            guard()?;
            OleInitialize(None).map_err(|e| e.to_string())?;
            struct Ole;
            impl Drop for Ole {
                fn drop(&mut self) {
                    unsafe {
                        OleUninitialize();
                    }
                }
            }
            let _ole = Ole;
            let data = data_object_guarded(payload, guard.clone());
            let source: IDropSource = DropSource {
                guard: guard.clone(),
            }
            .into();
            guard()?;
            let mut effect = DROPEFFECT_NONE;
            super::DROP_EFFECT.store(0, std::sync::atomic::Ordering::Release);
            let hr = dispatch(&data, &source, &mut effect);
            let accepted = if hr == DRAGDROP_S_DROP { effect.0 } else { 0 };
            super::DROP_EFFECT.store(accepted, std::sync::atomic::Ordering::Release);
            guard()?;
            if hr == S_OK || hr == DRAGDROP_S_DROP || hr == DRAGDROP_S_CANCEL {
                Ok(())
            } else {
                Err(format!("DoDragDrop failed: {hr:?}"))
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use windows::Win32::Foundation::E_ABORT;
        use windows::Win32::System::Memory::GlobalSize;
        use windows::Win32::System::Ole::ReleaseStgMedium;
        use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};

        thread_local! {
            static PREPARED_MEDIA: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
            static MEDIUM_PREPARED_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
                const { std::cell::RefCell::new(None) };
        }

        pub(super) fn observe_prepared_medium() {
            PREPARED_MEDIA.with(|count| count.set(count.get() + 1));
            let hook = MEDIUM_PREPARED_HOOK.with(|hook| hook.borrow_mut().take());
            if let Some(hook) = hook {
                hook();
            }
        }

        fn reset_medium_preparation(hook: Option<Box<dyn FnOnce()>>) {
            PREPARED_MEDIA.with(|count| count.set(0));
            MEDIUM_PREPARED_HOOK.with(|pending| *pending.borrow_mut() = hook);
        }

        struct Medium(STGMEDIUM);
        impl Drop for Medium {
            fn drop(&mut self) {
                unsafe {
                    ReleaseStgMedium(&mut self.0);
                }
            }
        }
        fn read(data: &IDataObject, format: u16) -> Medium {
            let fmt = ClipDataObject::fmt(format);
            unsafe {
                assert_eq!(data.QueryGetData(&fmt), S_OK);
                let medium = data.GetData(&fmt).unwrap();
                assert_eq!(medium.tymed, TYMED_HGLOBAL.0 as u32);
                Medium(medium)
            }
        }
        fn bytes(medium: &Medium) -> Vec<u8> {
            unsafe {
                let handle = medium.0.u.hGlobal;
                let ptr = GlobalLock(handle).cast::<u8>();
                assert!(!ptr.is_null());
                let bytes = std::slice::from_raw_parts(ptr, GlobalSize(handle)).to_vec();
                let _ = GlobalUnlock(handle);
                bytes
            }
        }
        fn unicode(medium: &Medium) -> String {
            let chars: Vec<_> = bytes(medium)
                .chunks_exact(2)
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .take_while(|c| *c != 0)
                .collect();
            String::from_utf16(&chars).unwrap()
        }

        fn source_guard(current: &Arc<AtomicBool>) -> OleDragGuard {
            let current = current.clone();
            Arc::new(move || {
                if current.load(Ordering::Acquire) {
                    Ok(())
                } else {
                    Err("owned OLE source retired".into())
                }
            })
        }

        fn all_formats_payload() -> (tempfile::TempDir, OleDragPayload) {
            let dir = super::super::fixture_directory();
            let image = dir.path().join("图片🦀.png");
            image::RgbaImage::from_pixel(16, 12, image::Rgba([20, 90, 160, 255]))
                .save(&image)
                .unwrap();
            let path = image.display().to_string();
            let qq = crate::im_rich::qq_image_envelope(&path);
            let wechat =
                crate::im_rich::rewrite_for_origin(&qq, crate::im_rich::ImRichOrigin::WeChat);
            let payload = OleDragPayload {
                text: "中文 🦀\r\nline\ttab".into(),
                files: vec![path.clone()],
                html: Some(crate::cf_html::build_clipboard_html("<b>中文🦀</b>", None)),
                qq_rich: Some(qq),
                wechat_rich: Some(wechat),
                dib: Some(crate::clipboard::encode_dib_from_path(&path).unwrap()),
                quicker_steps: Some("{\"steps\":[\"中文🦀\"]}".into()),
                internal_drag_token: None,
            };
            (dir, payload)
        }

        fn all_formats() -> [u16; 7] {
            [
                CF_UNICODETEXT,
                CF_HDROP,
                register_format(FMT_HTML),
                register_format(FMT_QQ),
                register_format(FMT_WECHAT),
                CF_DIB,
                register_format(FMT_QUICKER_STEPS),
            ]
        }

        #[test]
        fn native_guarded_ole_data_preserves_all_formats_and_independent_mediums() {
            let (_dir, payload) = all_formats_payload();
            let current = Arc::new(AtomicBool::new(true));
            let data = data_object_guarded(&payload, source_guard(&current));
            unsafe {
                let enumerator = data.EnumFormatEtc(1).unwrap();
                let mut formats = [FORMATETC::default(); 8];
                let mut count = 0;
                enumerator
                    .Next(&mut formats, Some(&mut count))
                    .ok()
                    .unwrap();
                assert_eq!(count, 7);
                assert_eq!(
                    formats[..count as usize]
                        .iter()
                        .map(|format| format.cfFormat)
                        .collect::<Vec<_>>(),
                    all_formats()
                );
            }
            for format in all_formats() {
                let first = read(&data, format);
                let second = read(&data, format);
                unsafe {
                    assert_ne!(first.0.u.hGlobal, second.0.u.hGlobal);
                }
                drop(first);
                if format == CF_UNICODETEXT {
                    assert_eq!(unicode(&second), payload.text);
                } else if format == CF_HDROP {
                    unsafe {
                        let handle = HDROP(second.0.u.hGlobal.0);
                        assert_eq!(DragQueryFileW(handle, u32::MAX, None), 1);
                        let len = DragQueryFileW(handle, 0, None);
                        let mut wide = vec![0; len as usize + 1];
                        assert_eq!(DragQueryFileW(handle, 0, Some(&mut wide)), len);
                        assert_eq!(
                            String::from_utf16(&wide[..len as usize]).unwrap(),
                            payload.files[0]
                        );
                    }
                } else if format == CF_DIB {
                    let expected = payload.dib.as_ref().unwrap();
                    assert_eq!(&bytes(&second)[..expected.len()], expected);
                } else if format == register_format(FMT_QUICKER_STEPS) {
                    assert_eq!(unicode(&second), *payload.quicker_steps.as_ref().unwrap());
                } else {
                    let expected = if format == register_format(FMT_HTML) {
                        payload.html.as_ref().unwrap()
                    } else if format == register_format(FMT_QQ) {
                        payload.qq_rich.as_ref().unwrap()
                    } else {
                        payload.wechat_rich.as_ref().unwrap()
                    };
                    let actual = bytes(&second);
                    assert_eq!(&actual[..expected.len()], expected.as_bytes());
                    assert_eq!(actual[expected.len()], 0);
                }
            }
        }

        #[test]
        fn native_guarded_ole_rejects_retired_source_between_negotiation_and_get_data() {
            let (_dir, payload) = all_formats_payload();
            let current = Arc::new(AtomicBool::new(true));
            let data = data_object_guarded(&payload, source_guard(&current));
            for format in all_formats() {
                unsafe {
                    assert_eq!(data.QueryGetData(&ClipDataObject::fmt(format)), S_OK);
                }
            }
            current.store(false, Ordering::Release);
            reset_medium_preparation(None);
            for format in all_formats() {
                let fmt = ClipDataObject::fmt(format);
                unsafe {
                    assert_eq!(data.QueryGetData(&fmt), E_ABORT);
                    assert_eq!(
                        data.GetData(&fmt)
                            .err()
                            .expect("retired source must not return a medium")
                            .code(),
                        E_ABORT
                    );
                }
            }
            assert_eq!(PREPARED_MEDIA.with(|count| count.get()), 0);
        }

        #[test]
        fn native_guarded_ole_rejects_source_retired_while_preparing_each_medium() {
            let (_dir, payload) = all_formats_payload();
            for format in all_formats() {
                let current = Arc::new(AtomicBool::new(true));
                let data = data_object_guarded(&payload, source_guard(&current));
                let retired = current.clone();
                reset_medium_preparation(Some(Box::new(move || {
                    retired.store(false, Ordering::Release);
                })));
                unsafe {
                    assert_eq!(
                        data.GetData(&ClipDataObject::fmt(format))
                            .err()
                            .expect("prepared medium must be released instead of delivered")
                            .code(),
                        E_ABORT
                    );
                }
                assert_eq!(PREPARED_MEDIA.with(|count| count.get()), 1);
                assert!(!current.load(Ordering::Acquire));
            }
        }

        #[test]
        fn native_guarded_ole_source_preserves_escape_and_cancels_before_drop() {
            use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
            let current = Arc::new(AtomicBool::new(true));
            let source: IDropSource = DropSource {
                guard: source_guard(&current),
            }
            .into();
            unsafe {
                assert_eq!(
                    source.QueryContinueDrag(BOOL(0), MODIFIERKEYS_FLAGS(MK_LBUTTON)),
                    S_OK
                );
                assert_eq!(
                    source.QueryContinueDrag(BOOL(1), MODIFIERKEYS_FLAGS(MK_LBUTTON)),
                    DRAGDROP_S_CANCEL
                );
                assert_eq!(
                    source.QueryContinueDrag(BOOL(0), MODIFIERKEYS_FLAGS(0)),
                    DRAGDROP_S_DROP
                );
                assert_eq!(
                    source.GiveFeedback(DROPEFFECT_COPY),
                    DRAGDROP_S_USEDEFAULTCURSORS
                );
                current.store(false, Ordering::Release);
                assert_eq!(
                    source.QueryContinueDrag(BOOL(0), MODIFIERKEYS_FLAGS(MK_LBUTTON)),
                    DRAGDROP_S_CANCEL
                );
                assert_eq!(
                    source.QueryContinueDrag(BOOL(0), MODIFIERKEYS_FLAGS(0)),
                    DRAGDROP_S_CANCEL
                );
            }
        }

        #[test]
        fn native_guarded_ole_owned_sta_rejects_retired_queued_source_before_dispatch() {
            let current = Arc::new(AtomicBool::new(true));
            let guard = source_guard(&current);
            guard().unwrap(); // Accepted while the source is still current.
            let dispatches = Arc::new(AtomicUsize::new(0));
            let dispatched = dispatches.clone();
            let (jobs, queued) = std::sync::mpsc::sync_channel::<(OleDragPayload, OleDragGuard)>(1);
            let (ready, observed) = std::sync::mpsc::sync_channel(1);
            let (resume, wait) = std::sync::mpsc::sync_channel(1);
            let worker = std::thread::Builder::new()
                .name("native-ole-owned-sta-fixture".into())
                .spawn(move || {
                    unsafe {
                        OleInitialize(None).unwrap();
                    }
                    struct OwnedSta;
                    impl Drop for OwnedSta {
                        fn drop(&mut self) {
                            unsafe {
                                OleUninitialize();
                            }
                        }
                    }
                    let _sta = OwnedSta;
                    let (payload, guard) = queued.recv().unwrap();
                    ready.send(()).unwrap();
                    wait.recv().unwrap();
                    do_drag_drop_with_dispatch(&payload, guard, |_, _, _| {
                        dispatched.fetch_add(1, Ordering::AcqRel);
                        DRAGDROP_S_CANCEL
                    })
                })
                .unwrap();
            jobs.send((
                OleDragPayload {
                    text: "queued owned source".into(),
                    ..Default::default()
                },
                guard,
            ))
            .unwrap();
            observed
                .recv_timeout(std::time::Duration::from_secs(3))
                .unwrap();
            current.store(false, Ordering::Release);
            resume.send(()).unwrap();
            assert_eq!(
                worker.join().unwrap().unwrap_err(),
                "owned OLE source retired"
            );
            assert_eq!(dispatches.load(Ordering::Acquire), 0);
        }

        #[test]
        fn native_guarded_ole_checks_prepared_objects_before_native_dispatch() {
            let dispatches = Arc::new(AtomicUsize::new(0));
            let checks = Arc::new(AtomicUsize::new(0));
            let source_checks = checks.clone();
            let result = do_drag_drop_with_dispatch(
                &OleDragPayload {
                    text: "prepared owned source".into(),
                    ..Default::default()
                },
                Arc::new(move || {
                    if source_checks.fetch_add(1, Ordering::AcqRel) == 0 {
                        Ok(())
                    } else {
                        Err("owned OLE source retired while preparing COM objects".into())
                    }
                }),
                |_, _, _| {
                    dispatches.fetch_add(1, Ordering::AcqRel);
                    DRAGDROP_S_CANCEL
                },
            );
            assert_eq!(
                result.unwrap_err(),
                "owned OLE source retired while preparing COM objects"
            );
            assert_eq!(dispatches.load(Ordering::Acquire), 0);
        }

        #[test]
        fn native_private_drag_token_does_not_invent_external_text_or_files() {
            let token="test-owned-gesture";
            let payload=OleDragPayload {internal_drag_token:Some(token.into()),..Default::default()};
            assert!(!payload.is_empty());
            let data=data_object(&payload);
            let format=register_format("ClipHost.InternalDrag.v1");
            let medium=read(&data,format);
            assert!(bytes(&medium).starts_with(token.as_bytes()));
            unsafe {
                assert!(data.QueryGetData(&ClipDataObject::fmt(CF_UNICODETEXT)).is_err());
                assert!(data.QueryGetData(&ClipDataObject::fmt(CF_HDROP)).is_err());
            }
        }

        #[test]
        fn native_guarded_ole_checks_completion_and_keeps_normal_escape_success() {
            use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
            let current = Arc::new(AtomicBool::new(true));
            let payload = OleDragPayload {
                text: "owned source".into(),
                ..Default::default()
            };
            do_drag_drop_with_dispatch(&payload, source_guard(&current), |data, source, _| {
                assert_eq!(unicode(&read(data, CF_UNICODETEXT)), payload.text);
                unsafe { source.QueryContinueDrag(BOOL(1), MODIFIERKEYS_FLAGS(MK_LBUTTON)) }
            })
            .unwrap();
            let result =
                do_drag_drop_with_dispatch(&payload, source_guard(&current), |data, _, _| {
                    assert_eq!(unicode(&read(data, CF_UNICODETEXT)), payload.text);
                    current.store(false, Ordering::Release);
                    DRAGDROP_S_CANCEL
                });
            assert_eq!(result.unwrap_err(), "owned OLE source retired");
        }

        #[test]
        fn native_data_object_delivers_unicode_html_and_file_drop_without_clipboard() {
            let html = crate::cf_html::build_clipboard_html("<b>中文🦀</b>", None);
            let payload = OleDragPayload {
                text: "中文 🦀\r\nline\ttab".into(),
                files: vec![r"D:\文件 甲.txt".into(), r"D:\emoji🦀.png".into()],
                html: Some(html.clone()),
                quicker_steps: Some("{\"steps\":[]}".into()),
                ..Default::default()
            };
            let data = data_object(&payload);
            // Call the COM vtable and Windows' actual format enumerator, not
            // just the payload-building helpers used by the other tests.
            unsafe {
                let enumerator = data.EnumFormatEtc(1).unwrap();
                let mut formats = [FORMATETC::default(); 8];
                let mut count = 0;
                enumerator
                    .Next(&mut formats, Some(&mut count))
                    .ok()
                    .unwrap();
                assert_eq!(count, 4);
                assert_eq!(
                    formats[..count as usize]
                        .iter()
                        .map(|f| f.cfFormat)
                        .collect::<Vec<_>>(),
                    [
                        CF_UNICODETEXT,
                        CF_HDROP,
                        register_format(FMT_HTML),
                        register_format(FMT_QUICKER_STEPS)
                    ]
                );
            }
            let text = read(&data, CF_UNICODETEXT);
            let another = read(&data, CF_UNICODETEXT);
            unsafe {
                assert_ne!(text.0.u.hGlobal, another.0.u.hGlobal);
            }
            drop(text); // Every GetData caller owns an independent STGMEDIUM.
            assert_eq!(unicode(&another), payload.text);
            let html_bytes = bytes(&read(&data, register_format(FMT_HTML)));
            assert_eq!(&html_bytes[..html.len()], html.as_bytes());
            assert_eq!(html_bytes[html.len()], 0);
            assert_eq!(
                unicode(&read(&data, register_format(FMT_QUICKER_STEPS))),
                "{\"steps\":[]}"
            );
            let files = read(&data, CF_HDROP);
            unsafe {
                let drop = HDROP(files.0.u.hGlobal.0);
                assert_eq!(DragQueryFileW(drop, u32::MAX, None), 2);
                for (index, path) in payload.files.iter().enumerate() {
                    let len = DragQueryFileW(drop, index as u32, None);
                    let mut wide = vec![0; len as usize + 1];
                    assert_eq!(DragQueryFileW(drop, index as u32, Some(&mut wide)), len);
                    assert_eq!(String::from_utf16(&wide[..len as usize]).unwrap(), *path);
                }
            }
        }

        #[test]
        fn native_image_data_object_retains_dimensions_pixels_and_file_fallback() {
            let dir = super::super::fixture_directory();
            let path = dir.path().join("image.png");
            image::RgbaImage::from_pixel(16, 12, image::Rgba([20, 90, 160, 255]))
                .save(&path)
                .unwrap();
            let mut item = crate::model::ClipRecord::new_text("", "fixture");
            item.kind = crate::model::ClipKind::ImageClipItem;
            item.image_path = Some(path.display().to_string());
            let payload = super::super::payload_from_items(&[item], false, false, false, "").unwrap();
            let data = data_object(&payload);
            let dib = bytes(&read(&data, CF_DIB));
            assert_eq!(i32::from_le_bytes(dib[4..8].try_into().unwrap()), 16);
            assert_eq!(i32::from_le_bytes(dib[8..12].try_into().unwrap()).abs(), 12);
            assert_eq!(u16::from_le_bytes(dib[14..16].try_into().unwrap()), 32);
            let offset = u32::from_le_bytes(dib[..4].try_into().unwrap()) as usize;
            assert_eq!(&dib[offset..offset + 4], &[160, 90, 20, 255]);
            let files = read(&data, CF_HDROP);
            unsafe {
                assert_eq!(
                    DragQueryFileW(HDROP(files.0.u.hGlobal.0), u32::MAX, None),
                    1
                );
            }
        }

        #[test]
        fn native_format_negotiation_rejects_unsupported_requests_consistently() {
            use windows::Win32::Foundation::{
                DV_E_DVASPECT, DV_E_FORMATETC, DV_E_LINDEX, DV_E_TYMED,
            };
            let data = data_object(&OleDragPayload {
                text: "test".into(),
                ..Default::default()
            });
            let normal = ClipDataObject::fmt(CF_UNICODETEXT);
            for (fmt, expected) in [
                (
                    FORMATETC {
                        dwAspect: 4,
                        ..normal
                    },
                    DV_E_DVASPECT,
                ),
                (
                    FORMATETC {
                        lindex: 0,
                        ..normal
                    },
                    DV_E_LINDEX,
                ),
                (FORMATETC { tymed: 4, ..normal }, DV_E_TYMED),
                (
                    FORMATETC {
                        cfFormat: CF_HDROP,
                        ..normal
                    },
                    DV_E_FORMATETC,
                ),
            ] {
                unsafe {
                    assert_eq!(data.QueryGetData(&fmt), expected);
                    assert_eq!(
                        data.GetData(&fmt)
                            .err()
                            .expect("unsupported format must fail")
                            .code(),
                        expected
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ClipRecord;

    fn payload(items: &[ClipRecord], plain: bool) -> OleDragPayload {
        payload_from_items(items, plain, false, false, "").unwrap()
    }

    #[test]
    fn canonical_core_drag_refuses_before_any_file_probe() {
        let mut item=ClipRecord::new_text("synthetic","fixture");
        item.kind=ClipKind::ImageClipItem;item.storage_revision=Some("core2:synthetic".into());
        item.image_path=Some(r"Z:\offline-root-must-not-be-probed\image.png".into());
        assert!(payload_from_items(&[item],false,false,false,"explorer.exe").is_err());
    }

    #[test]
    fn payload_joins_text_and_files() {
        let text = ClipRecord::new_text("hello", "manual");
        let mut file = ClipRecord::new_text("D:\\a.txt", "manual");
        file.kind = ClipKind::FileClipItem;
        file.file_paths = vec!["D:\\a.txt".into()];
        let drag = payload(&[text, file], false);
        assert_eq!(drag.text, "hello");
        assert_eq!(drag.files, vec!["D:\\a.txt".to_string()]);
        let second = ClipRecord::new_text("world", "manual");
        let joined = payload(&[ClipRecord::new_text("hello", "manual"), second], false);
        assert_eq!(joined.text, "hello\r\nworld");
        assert!(joined.files.is_empty());
        assert!(joined.html.is_none());
    }

    #[test]
    fn single_item_set_to_data_object_formats_match_csharp() {
        let mut html = ClipRecord::new_text("plain", "manual");
        html.kind = ClipKind::HtmlClipItem;
        html.html_data = Some("<b>hi</b>".into());
        let html_payload = payload(std::slice::from_ref(&html), false);
        assert!(
            html_payload
                .html
                .as_ref()
                .unwrap()
                .contains("StartFragment")
        );

        let mut qq = ClipRecord::new_text("qq", "manual");
        qq.kind = ClipKind::QQRichTextItem;
        qq.qq_rich_text = Some("<QQRichEditFormat>x</QQRichEditFormat>".into());
        let skipped = payload(std::slice::from_ref(&qq), true);
        assert!(skipped.html.is_none() && skipped.qq_rich.is_none());
        let qq_payload = payload(std::slice::from_ref(&qq), false);
        assert!(
            qq_payload
                .qq_rich
                .as_ref()
                .unwrap()
                .contains("QQRichEditFormat")
        );

        let multi = payload(&[html, qq], false);
        assert!(multi.html.is_none() && multi.qq_rich.is_none());
        assert!(multi.text.contains("plain"));

        let mut link = ClipRecord::new_text("https://a.test", "manual");
        link.kind = ClipKind::LinkClipItem;
        link.title = Some("Docs".into());
        link.body_text = Some("https://a.test".into());
        assert_eq!(
            payload(std::slice::from_ref(&link), false).text,
            "Docs\r\nhttps://a.test"
        );

        let mut action = ClipRecord::new_text("action-guid", "manual");
        action.kind = ClipKind::ActionClipItem;
        assert_eq!(
            payload(std::slice::from_ref(&action), false).text,
            "action-guid"
        );

        let mut step = ClipRecord::new_text("{\"steps\":[]}", "manual");
        step.kind = ClipKind::StepClipItem;
        let step_payload = payload(std::slice::from_ref(&step), false);
        assert!(step_payload.text.is_empty());
        assert_eq!(
            step_payload.quicker_steps.as_deref(),
            Some("{\"steps\":[]}")
        );
    }

    #[test]
    fn payload_includes_dib_for_small_image() {
        let dir = super::fixture_directory();
        let path = dir.path().join("tiny.png");
        image::RgbaImage::from_pixel(2, 2, image::Rgba([10, 20, 30, 255]))
            .save(&path)
            .unwrap();
        let mut item = ClipRecord::new_text("", "manual");
        item.kind = ClipKind::ImageClipItem;
        item.image_path = Some(path.to_string_lossy().into_owned());
        let drag = payload(std::slice::from_ref(&item), false);
        assert!(drag.dib.as_ref().unwrap().len() > 40);
        assert_eq!(drag.files.len(), 1);
        let explorer = payload_from_items(
            std::slice::from_ref(&item),
            false,
            false,
            false,
            "explorer.exe",
        ).unwrap();
        assert!(explorer.dib.is_none());
        assert_eq!(explorer.files.len(), 1);
        let wechat = payload_from_items(
            std::slice::from_ref(&item),
            false,
            false,
            false,
            "WeChat.exe",
        ).unwrap();
        assert!(wechat.dib.is_none());
        assert!(wechat.files.is_empty());
        assert!(wechat.wechat_rich.is_none());
        assert!(
            wechat
                .qq_rich
                .as_ref()
                .unwrap()
                .contains("QQRichEditFormat")
        );
        let powerpnt = payload_from_items(
            std::slice::from_ref(&item),
            false,
            false,
            false,
            "POWERPNT.EXE",
        ).unwrap();
        assert!(powerpnt.dib.is_none());
        assert_eq!(powerpnt.files.len(), 1);
    }

    #[test]
    fn single_file_clip_skips_directories() {
        let dir = super::fixture_directory();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        let folder = dir.path().join("sub");
        std::fs::create_dir(&folder).unwrap();
        let mut item = ClipRecord::new_text("files", "manual");
        item.kind = ClipKind::FileClipItem;
        item.file_paths = vec![
            file.to_string_lossy().into_owned(),
            folder.to_string_lossy().into_owned(),
        ];
        let drag = payload(std::slice::from_ref(&item), false);
        assert_eq!(drag.files.len(), 1);
        assert!(drag.files[0].ends_with("a.txt"));
        assert!(drag.text.is_empty());
    }

    #[test]
    fn im_target_sets_qq_image_envelope() {
        let mut drag = OleDragPayload {
            files: vec![r"D:\pic.png".into()],
            ..OleDragPayload::default()
        };
        apply_im_image_envelope(&mut drag, "WeChat.exe");
        assert!(drag.files.is_empty());
        assert!(drag.wechat_rich.is_none());
        assert!(
            drag.qq_rich
                .as_deref()
                .unwrap()
                .contains(r#"filepath="D:\pic.png""#)
        );
        let mut skipped = OleDragPayload {
            files: vec![r"D:\pic.png".into()],
            ..OleDragPayload::default()
        };
        apply_im_image_envelope(&mut skipped, "notepad.exe");
        assert!(skipped.qq_rich.is_none());
        assert_eq!(skipped.files.len(), 1);
    }

    #[test]
    fn headless_drag_is_noop() {
        crate::clipboard::force_headless(true);
        std::env::set_var("CLIPHOST_TAURI_HEADLESS", "1");
        begin_ole_drag(&OleDragPayload {
            text: "x".into(),
            ..Default::default()
        })
        .unwrap();
    }

    #[test]
    fn native_drag_entry_rejects_retired_source_before_headless_or_empty_payload() {
        assert_eq!(
            begin_ole_drag_guarded(
                &OleDragPayload::default(),
                std::sync::Arc::new(|| Err("owned OLE source retired before dispatch".into()))
            )
            .unwrap_err(),
            "owned OLE source retired before dispatch"
        );
    }
}
