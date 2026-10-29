//! Root-HWND-only OLE registration and posted outgoing STA dispatch.
use super::{
    wide_drop_files, DragOverEvent, DropError, DropEvent, DropTicket, OwnerState,
    QuickerActionCandidate, RawDrop, MAX_FORMAT_BYTES, MAX_TRANSFER_BYTES,
};
use cliphost_core::{ole_drag::OleDragPayload, HostState};
use std::{
    cell::{Cell, RefCell},
    marker::PhantomData,
    rc::Rc,
    sync::{mpsc, Arc, Mutex},
};
use windows::{
    core::{implement, w, IUnknown, Interface, HSTRING},
    Win32::{
        Foundation::{E_POINTER, HWND, LPARAM, LRESULT, POINTL, WPARAM},
        Graphics::Gdi::{
            CreateCompatibleDC, DeleteDC, GetDIBits, GetObjectW, BITMAP, BITMAPINFO,
            BITMAPINFOHEADER, DIB_RGB_COLORS, HGDIOBJ,
        },
        System::{
            Com::{
                CoTaskMemFree, IDataObject, DATADIR_GET, DVASPECT_CONTENT, FORMATETC, STGMEDIUM,
                TYMED_GDI, TYMED_HGLOBAL,
            },
            DataExchange::{GetClipboardFormatNameW, RegisterClipboardFormatW},
            Memory::{GlobalLock, GlobalSize, GlobalUnlock},
            Ole::{
                IDropTarget, IDropTarget_Impl, OleInitialize, OleUninitialize, RegisterDragDrop,
                ReleaseStgMedium, RevokeDragDrop, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE,
            },
            SystemServices::MODIFIERKEYS_FLAGS,
            Threading::{GetCurrentProcessId, GetCurrentThreadId},
        },
        UI::{
            Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON},
            Shell::{DefSubclassProc, GetWindowSubclass, RemoveWindowSubclass, SetWindowSubclass},
            WindowsAndMessaging::{
                GetPropW, GetWindowThreadProcessId, IsWindowVisible, PostMessageW, WM_APP,
                WM_NCDESTROY, WM_SHOWWINDOW, WM_WINDOWPOSCHANGED,
            },
        },
    },
};
use windows_compat as windows;

type OnDrop = Rc<dyn Fn(DropEvent) -> Result<(), String>>;
type OnDragOver = Rc<dyn Fn(DragOverEvent)>;
type OnError = Rc<dyn Fn(DropError)>;
/// The core guarded IDropSource checks the ticket from QueryContinueDrag too,
/// so hide/destruction cancels a drag that is already inside OLE's modal loop.
pub type NativeDragRunner =
    Arc<dyn Fn(&OleDragPayload, DropTicket) -> Result<(), String> + Send + Sync>;

fn handle(hwnd: isize) -> HWND {
    HWND(hwnd as *mut _)
}
fn validate_sta(hwnd: HWND) -> Result<(), String> {
    let mut process = 0;
    let thread = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process)) };
    if thread == 0
        || thread != unsafe { GetCurrentThreadId() }
        || process != unsafe { GetCurrentProcessId() }
    {
        return Err("OLE window must belong to this process and UI STA.".into());
    }
    Ok(())
}
fn property(hwnd: HWND) -> *mut std::ffi::c_void {
    if validate_sta(hwnd).is_err() {
        return std::ptr::null_mut();
    }
    unsafe { GetPropW(hwnd, w!("OleDropTargetInterface")).0 }
}
fn fmt(id: u16, tymed: u32) -> FORMATETC {
    FORMATETC {
        cfFormat: id,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed,
    }
}
fn registered(name: &str) -> u16 {
    unsafe { RegisterClipboardFormatW(&HSTRING::from(name)) as u16 }
}
fn offered(data: &IDataObject, id: u16, tymed: u32) -> bool {
    id != 0 && unsafe { data.QueryGetData(&fmt(id, tymed)).is_ok() }
}
fn accepts(data: &IDataObject) -> bool {
    [13, 15, 8, 17]
        .into_iter()
        .any(|id| offered(data, id, TYMED_HGLOBAL.0 as u32))
        || offered(data, 2, TYMED_GDI.0 as u32)
        || [
            "ClipHost.InternalDrag.v1",
            "HTML Format",
            "QQ_Unicode_RichEdit_Format",
            "WeChat_RichEdit_Format",
            "PNG",
            "quicker-action-steps",
        ]
        .into_iter()
        .any(|name| offered(data, registered(name), TYMED_HGLOBAL.0 as u32))
        || action_formats(data)
            .into_iter()
            .any(|(id, _)| offered(data, id, TYMED_HGLOBAL.0 as u32))
}
const ACTION_FORMATS: &[&str] = &[
    "quicker-action-item-json",
    "quicker-action-drag-item",
    "quicker-action-item",
    "Quicker.Common.ActionItemDragObject",
    "Quicker.Common.ActionItem",
];
const FORMAT_SIZE_ERROR: &str = "Drag format exceeds the per-format byte limit.";
const TRANSFER_BUDGET_ERROR: &str = "Drag format exceeds the remaining transfer budget.";
const MAX_ENUMERATED_ACTION_FORMATS: usize = 256;

/// Keep the WebView interceptor's fixed priority, bounded enumeration and
/// name predicate. Steps already have a typed carrier in this native reader;
/// they must not be mistaken for an invalid generic action candidate.
fn action_formats(data: &IDataObject) -> Vec<(u16, String)> {
    let mut formats: Vec<_> = ACTION_FORMATS
        .iter()
        .map(|name| (registered(name), (*name).to_string()))
        .filter(|(id, _)| *id != 0)
        .collect();
    let steps = registered("quicker-action-steps");
    if let Ok(enumerator) = unsafe { data.EnumFormatEtc(DATADIR_GET.0 as u32) } {
        for _ in 0..MAX_ENUMERATED_ACTION_FORMATS {
            let mut entry = [FORMATETC::default()];
            let mut fetched = 0;
            let result = unsafe { enumerator.Next(&mut entry, Some(&mut fetched)) };
            if !entry[0].ptd.is_null() {
                unsafe {
                    CoTaskMemFree(Some(entry[0].ptd.cast()));
                }
            }
            if result.is_err() || fetched == 0 {
                break;
            }
            let id = entry[0].cfFormat;
            let mut name = [0u16; 256];
            let len = unsafe { GetClipboardFormatNameW(id as u32, &mut name) };
            let name = String::from_utf16_lossy(&name[..len.max(0) as usize]);
            let lower = name.to_ascii_lowercase();
            if id != steps
                && (lower.contains("quicker-action") || lower.contains("actionitemdrag"))
                && !formats.iter().any(|(known, _)| *known == id)
            {
                formats.push((id, name));
            }
        }
    }
    formats
}

fn read_action_candidates(
    data: &IDataObject,
    budget: &mut usize,
) -> Result<Vec<QuickerActionCandidate>, String> {
    let mut candidates = Vec::new();
    for (id, format_name) in action_formats(data) {
        match bytes(data, id, budget) {
            Ok(None) => {}
            Ok(Some(wire)) => candidates.push(QuickerActionCandidate {
                format_name,
                wire: Ok(wire),
            }),
            // A candidate may fail reading/decoding, as in the WebView, but
            // additional formats can never bypass the aggregate byte limit.
            Err(error) if error == TRANSFER_BUDGET_ERROR => return Err(error),
            Err(error) => candidates.push(QuickerActionCandidate {
                format_name,
                wire: Err(error),
            }),
        }
    }
    Ok(candidates)
}
struct Medium(STGMEDIUM);
impl Drop for Medium {
    fn drop(&mut self) {
        unsafe {
            ReleaseStgMedium(&mut self.0);
        }
    }
}
fn bytes(data: &IDataObject, id: u16, budget: &mut usize) -> Result<Option<Vec<u8>>, String> {
    if !offered(data, id, TYMED_HGLOBAL.0 as u32) {
        return Ok(None);
    }
    let medium = Medium(
        unsafe { data.GetData(&fmt(id, TYMED_HGLOBAL.0 as u32)) }.map_err(|e| e.to_string())?,
    );
    if medium.0.tymed != TYMED_HGLOBAL.0 as u32 {
        return Err("Drag source returned an invalid storage medium.".into());
    }
    unsafe {
        let global = medium.0.u.hGlobal;
        let size = GlobalSize(global);
        if size == 0 {
            return Err("Drag source returned an empty storage medium.".into());
        }
        // An individually ineligible action candidate is never copied, so it
        // cannot consume or exhaust the remaining aggregate allowance.
        if size > MAX_FORMAT_BYTES {
            return Err(FORMAT_SIZE_ERROR.into());
        }
        if size > *budget {
            return Err(TRANSFER_BUDGET_ERROR.into());
        }
        let pointer = GlobalLock(global);
        if pointer.is_null() {
            return Err("Drag format could not be locked.".into());
        }
        let copied = std::slice::from_raw_parts(pointer.cast::<u8>(), size).to_vec();
        let _ = GlobalUnlock(global);
        *budget -= size;
        Ok(Some(copied))
    }
}

/// Copy DDB pixels while the GDI STGMEDIUM still owns its HBITMAP. PNG encoding
/// remains in the background production decoder, and no GDI handle escapes.
fn bitmap_dib(data: &IDataObject, budget: &mut usize) -> Result<Option<Vec<u8>>, String> {
    if !offered(data, 2, TYMED_GDI.0 as u32) {
        return Ok(None);
    }
    let medium =
        Medium(unsafe { data.GetData(&fmt(2, TYMED_GDI.0 as u32)) }.map_err(|e| e.to_string())?);
    if medium.0.tymed != TYMED_GDI.0 as u32 {
        return Err("Invalid bitmap drag medium.".into());
    }
    unsafe {
        let bitmap = medium.0.u.hBitmap;
        let mut description = BITMAP::default();
        if GetObjectW(
            HGDIOBJ(bitmap.0),
            std::mem::size_of::<BITMAP>() as i32,
            Some((&mut description as *mut BITMAP).cast()),
        ) == 0
        {
            return Err("Cannot inspect dragged bitmap.".into());
        }
        let width = description.bmWidth;
        let height = description.bmHeight.unsigned_abs();
        if !(1..=8192).contains(&width) || !(1..=8192).contains(&height) {
            return Err("Dragged bitmap is too large.".into());
        }
        let count = (width as usize)
            .checked_mul(height as usize)
            .and_then(|n| n.checked_mul(4))
            .and_then(|n| n.checked_add(40))
            .ok_or("Dragged bitmap size overflow.")?;
        if count > MAX_FORMAT_BYTES || count > *budget {
            return Err("Dragged bitmap exceeds the transfer budget.".into());
        }
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: 40,
                biWidth: width,
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                ..Default::default()
            },
            ..Default::default()
        };
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            return Err("Cannot allocate bitmap drag context.".into());
        }
        let mut copied = vec![0; count];
        let rows = GetDIBits(
            dc,
            bitmap,
            0,
            height,
            Some(copied[40..].as_mut_ptr().cast()),
            &mut info,
            DIB_RGB_COLORS,
        );
        let _ = DeleteDC(dc);
        if rows != height as i32 {
            return Err("Cannot read complete dragged bitmap.".into());
        }
        // BITMAPINFOHEADER is repr(C), 40 bytes; BI_RGB discards DDB alpha just
        // like the production CF_BITMAP fallback, while DIBV5 keeps alpha.
        copied[..40].copy_from_slice(std::slice::from_raw_parts(
            (&info.bmiHeader as *const BITMAPINFOHEADER).cast(),
            40,
        ));
        *budget -= count;
        Ok(Some(copied))
    }
}
fn read_object(data: &IDataObject) -> Result<RawDrop, String> {
    let mut remaining = MAX_TRANSFER_BYTES;
    if let Some(raw) = bytes(data, registered("ClipHost.InternalDrag.v1"), &mut remaining)? {
        if raw.len() > 256 { return Err("Invalid internal drag token.".into()); }
        let token = String::from_utf8(raw.into_iter().take_while(|value| *value != 0).collect())
            .map_err(|_| "Invalid internal drag token.")?;
        return Ok(RawDrop { internal_drag_token: Some(token), ..Default::default() });
    }
    let text = bytes(data, 13, &mut remaining)?
        .map(|bytes| -> Result<String, String> {
            if bytes.len() % 2 != 0 {
                return Err("Invalid Unicode drag text.".into());
            }
            let chars: Vec<_> = bytes
                .chunks_exact(2)
                .map(|v| u16::from_le_bytes([v[0], v[1]]))
                .take_while(|&v| v != 0)
                .collect();
            String::from_utf16(&chars).map_err(|_| "Invalid Unicode drag text.".into())
        })
        .transpose()?;
    let files = if let Some(raw) = bytes(data, 15, &mut remaining)? {
        // Wide form is what the production object publishes. Legacy ANSI paths
        // use Windows' active code page on the already bounded copied bytes.
        if raw.len() < 20 {
            return Err("Invalid file drag header.".into());
        }
        if u32::from_le_bytes(raw[16..20].try_into().unwrap()) != 0 {
            wide_drop_files(&raw)?
        } else {
            ansi_files(&raw)?
        }
    } else {
        Vec::new()
    };
    let mut raw = RawDrop {
        text,
        files,
        ..Default::default()
    };
    raw.html = bytes(data, registered("HTML Format"), &mut remaining)?;
    raw.chromium_source_url = bytes(
        data,
        registered("Chromium internal source URL"),
        &mut remaining,
    )?;
    raw.qq_rich = bytes(
        data,
        registered("QQ_Unicode_RichEdit_Format"),
        &mut remaining,
    )?;
    raw.wechat_rich = bytes(data, registered("WeChat_RichEdit_Format"), &mut remaining)?;
    raw.quicker_steps = bytes(data, registered("quicker-action-steps"), &mut remaining)?;
    raw.quicker_action_candidates = read_action_candidates(data, &mut remaining)?;
    raw.png = bytes(data, registered("PNG"), &mut remaining)?;
    raw.dib_v5 = bytes(data, 17, &mut remaining)?;
    raw.dib = bytes(data, 8, &mut remaining)?;
    raw.has_image_format = raw.png.is_some()
        || raw.dib_v5.is_some()
        || raw.dib.is_some()
        || offered(data, 2, TYMED_GDI.0 as u32);
    if raw.png.is_none() && raw.dib_v5.is_none() && raw.dib.is_none() {
        raw.dib = bitmap_dib(data, &mut remaining)?;
    }
    if raw.is_empty() {
        return Err("Drag source has no supported transferable content.".into());
    }
    Ok(raw)
}

/// File-line admission requests only CF_HDROP. A provider advertising rich
/// text/images alongside paths must not be asked to render those formats.
fn read_file_object(data: &IDataObject) -> Result<RawDrop, String> {
    let mut remaining = MAX_TRANSFER_BYTES;
    let raw = bytes(data, 15, &mut remaining)?.ok_or("Drag source has no file paths.")?;
    if raw.len() < 20 { return Err("Invalid file drag header.".into()); }
    let files = if u32::from_le_bytes(raw[16..20].try_into().unwrap()) != 0 {
        wide_drop_files(&raw)?
    } else {
        ansi_files(&raw)?
    };
    if files.is_empty() { return Err("Drag source has no file paths.".into()); }
    Ok(RawDrop { files, ..Default::default() })
}
fn ansi_files(bytes: &[u8]) -> Result<Vec<String>, String> {
    use windows::Win32::Globalization::{
        MultiByteToWideChar, CP_ACP, MULTI_BYTE_TO_WIDE_CHAR_FLAGS,
    };
    let offset = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
    if offset < 20 || offset > bytes.len().saturating_sub(2) {
        return Err("Invalid ANSI file drag offset.".into());
    }
    let mut remaining = &bytes[offset..];
    let mut files = Vec::new();
    loop {
        let end = remaining
            .iter()
            .position(|&v| v == 0)
            .ok_or("Incomplete ANSI file drag.")?;
        if end == 0 {
            return Ok(files);
        }
        if files.len() >= super::MAX_DROP_FILES || end > 32767 {
            return Err("ANSI file drag exceeds path budget.".into());
        }
        let length = unsafe {
            MultiByteToWideChar(
                CP_ACP,
                MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0),
                &remaining[..end],
                None,
            )
        };
        if length <= 0 {
            return Err("Cannot decode ANSI dragged path.".into());
        }
        let mut wide = vec![0; length as usize];
        if unsafe {
            MultiByteToWideChar(
                CP_ACP,
                MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0),
                &remaining[..end],
                Some(&mut wide),
            )
        } != length
        {
            return Err("Incomplete ANSI dragged path conversion.".into());
        }
        files.push(String::from_utf16(&wide).map_err(|_| "Invalid dragged file Unicode.")?);
        remaining = &remaining[end + 1..];
    }
}

#[implement(IDropTarget)]
struct Target {
    files_only: bool,
    state: Arc<OwnerState>,
    on_drop: OnDrop,
    on_over: OnDragOver,
    on_error: OnError,
    hovering: Rc<RefCell<Option<Rc<IncomingSession>>>>,
    negotiation_generation: Cell<u64>,
}

/// Retain the source's COM identity on this STA. An interface wrapper for the
/// same source may have a different pointer; canonical IUnknown cannot.
struct IncomingSession {
    ticket: DropTicket,
    source: IUnknown,
}

impl Target {
    fn reset(&self) {
        let previous = self.hovering.borrow_mut().take();
        if let Some(previous) = previous {
            (self.on_over)(DragOverEvent::Leave(previous.ticket.clone()));
        }
    }
    fn session_is_current(&self, session: &Rc<IncomingSession>) -> bool {
        session.ticket.is_current()
            && self
                .hovering
                .borrow()
                .as_ref()
                .is_some_and(|current| Rc::ptr_eq(current, session))
    }
    fn finish(&self, session: &Rc<IncomingSession>) {
        if self
            .hovering
            .borrow()
            .as_ref()
            .is_some_and(|current| Rc::ptr_eq(current, session))
        {
            self.reset();
        }
    }
    fn next_negotiation(&self) -> u64 {
        let generation = self.negotiation_generation.get().wrapping_add(1);
        self.negotiation_generation.set(generation);
        generation
    }
    fn negotiation_is_current(&self, generation: u64, ticket: &DropTicket) -> bool {
        self.negotiation_generation.get() == generation && ticket.is_current()
    }
    fn effect(&self, effect: *mut DROPEFFECT) -> windows::core::Result<()> {
        let out = unsafe { effect.as_mut() }.ok_or(windows::core::Error::from(E_POINTER))?;
        let current = self
            .hovering
            .borrow()
            .as_ref()
            .is_some_and(|session| session.ticket.is_current());
        *out = if current {
            DROPEFFECT(out.0 & DROPEFFECT_COPY.0)
        } else {
            DROPEFFECT_NONE
        };
        Ok(())
    }
}
#[allow(non_snake_case)]
impl IDropTarget_Impl for Target_Impl {
    fn DragEnter(
        &self,
        data: Option<&IDataObject>,
        _: MODIFIERKEYS_FLAGS,
        _: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        if effect.is_null() {
            return Err(E_POINTER.into());
        }
        let allowed = unsafe { (*effect).0 };
        // Reserve before observers/QI/format probing: each can pump messages
        // and enter a later source or leave this still-pending negotiation.
        let generation = self.next_negotiation();
        let ticket = self.state.ticket().ok();
        self.reset();
        unsafe {
            *effect = DROPEFFECT_NONE;
        }
        let Some(ticket) = ticket else {
            return Ok(());
        };
        if allowed & DROPEFFECT_COPY.0 == 0
            || !self.negotiation_is_current(generation, &ticket)
            || !unsafe { IsWindowVisible(handle(self.state.hwnd)).as_bool() }
        {
            return Ok(());
        }
        let Some(data) = data else {
            return Ok(());
        };
        let Ok(source) = data.cast::<IUnknown>() else {
            return Ok(());
        };
        if !self.negotiation_is_current(generation, &ticket) {
            return Ok(());
        }
        let supported = if self.files_only { offered(data, 15, TYMED_HGLOBAL.0 as u32) } else { accepts(data) };
        if !supported || !self.negotiation_is_current(generation, &ticket) {
            return Ok(());
        }
        let session = Rc::new(IncomingSession { ticket, source });
        *self.hovering.borrow_mut() = Some(session.clone());
        (self.on_over)(DragOverEvent::Enter(session.ticket.clone()));
        if self.negotiation_is_current(generation, &session.ticket)
            && self.session_is_current(&session)
        {
            unsafe {
                *effect = DROPEFFECT(allowed & DROPEFFECT_COPY.0);
            }
        }
        Ok(())
    }
    fn DragOver(
        &self,
        _: MODIFIERKEYS_FLAGS,
        _: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        self.effect(effect)
    }
    fn DragLeave(&self) -> windows::core::Result<()> {
        self.next_negotiation();
        self.reset();
        Ok(())
    }
    fn Drop(
        &self,
        data: Option<&IDataObject>,
        key_state: MODIFIERKEYS_FLAGS,
        point: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let out = unsafe { effect.as_mut() }.ok_or(windows::core::Error::from(E_POINTER))?;
        let can_copy = out.0 & DROPEFFECT_COPY.0 != 0;
        *out = DROPEFFECT_NONE;
        // Clone only the Rc while borrowed: IUnknown AddRef/QI may be reentrant.
        let Some(session) = self.hovering.borrow().clone() else {
            return Ok(());
        };
        let Some(data) = data else {
            return Ok(());
        };
        let Ok(source) = data.cast::<IUnknown>() else {
            return Ok(());
        };
        if !self.session_is_current(&session) || source.as_raw() != session.source.as_raw() {
            // A late Drop(A) cannot use B's ticket or release B's current lease.
            return Ok(());
        }
        // Keep the drag-over lease through synchronous GetData and acceptance.
        // Capture immutable destination in on_drop before releasing that lease.
        let result = (|| {
            if !can_copy {
                return Ok(());
            }
            let ticket = session.ticket.clone();
            ticket.verify()?;
            let formats = if self.files_only { read_file_object(data)? } else { read_object(data)? };
            ticket.verify()?;
            if !self.session_is_current(&session) {
                return Err("Drag source left or changed while its data was read.".into());
            }
            (self.on_drop)(DropEvent {
                ticket,
                control:key_state.0 & 0x0008 != 0,
                screen_point: (point.x, point.y),
                formats,
            })?;
            *out = DROPEFFECT_COPY;
            Ok::<_, String>(())
        })();
        let report_error = self.session_is_current(&session);
        self.finish(&session);
        if let Err(error) = result {
            tracing::warn!(%error,"Native drop rejected");
            if report_error && session.ticket.is_current() {
                (self.on_error)(DropError {
                    ticket: session.ticket.clone(),
                    message: error,
                });
            }
        }
        Ok(())
    }
}

/// STA owned. Attach once after GPUI registers its root target. Existing GPUI
/// HTML/file dispatch is replaced here because its string fallback loses the
/// original HTML/IM/OLE formats. Child HWNDs are never enumerated or modified.
pub struct DragDropWindow {
    hwnd: HWND,
    state: Arc<OwnerState>,
    original: Option<IDropTarget>,
    target: Option<IDropTarget>,
    on_over: OnDragOver,
    hook: usize,
    hovering: Rc<RefCell<Option<Rc<IncomingSession>>>>,
    _sta: PhantomData<Rc<()>>,
}
impl DragDropWindow {
    pub fn attach(
        hwnd: isize,
        on_drop: OnDrop,
        on_drag_over: OnDragOver,
        on_error: Rc<dyn Fn(String)>,
    ) -> Result<Self, String> {
        Self::attach_stamped(
            hwnd,
            on_drop,
            on_drag_over,
            Rc::new(move |error| on_error(error.message)),
        )
    }
    /// Carry the original request into queued UI error feedback. The consumer
    /// must verify it at drain time, after any destination/owner replacement.
    pub fn attach_stamped(
        hwnd: isize,
        on_drop: OnDrop,
        on_drag_over: OnDragOver,
        on_error: OnError,
    ) -> Result<Self, String> {
        Self::attach_with_formats(hwnd, on_drop, on_drag_over, on_error, false)
    }

    /// A path-only target. Reject text/image-only providers at DragEnter and
    /// never ask an accepted provider for anything other than CF_HDROP.
    pub fn attach_files(
        hwnd: isize,
        on_drop: OnDrop,
        on_drag_over: OnDragOver,
        on_error: OnError,
    ) -> Result<Self, String> {
        Self::attach_with_formats(hwnd, on_drop, on_drag_over, on_error, true)
    }

    fn attach_with_formats(
        hwnd: isize,
        on_drop: OnDrop,
        on_drag_over: OnDragOver,
        on_error: OnError,
        files_only: bool,
    ) -> Result<Self, String> {
        let hwnd = handle(hwnd);
        validate_sta(hwnd)?;
        let mut existing = 0;
        if unsafe {
            GetWindowSubclass(
                hwnd,
                Some(drop_owner_proc),
                DROP_SUBCLASS,
                Some(&mut existing),
            )
            .as_bool()
        } {
            return Err("Native drop target already attached to this window.".into());
        }
        unsafe { OleInitialize(None) }.map_err(|e| e.to_string())?;
        let result = (|| {
            let state = OwnerState::new(hwnd.0 as isize);
            let pointer = property(hwnd);
            let original = unsafe { IDropTarget::from_raw_borrowed(&pointer) }.cloned();
            let hovering = Rc::new(RefCell::new(None));
            let target: IDropTarget = Target {
                files_only,
                state: state.clone(),
                on_drop,
                on_over: on_drag_over.clone(),
                on_error,
                hovering: hovering.clone(),
                negotiation_generation: Cell::new(0),
            }
            .into();
            if original.is_some() {
                unsafe { RevokeDragDrop(hwnd) }.map_err(|e| e.to_string())?;
            }
            if let Err(error) = unsafe { RegisterDragDrop(hwnd, &target) } {
                if let Some(original) = &original {
                    let _ = unsafe { RegisterDragDrop(hwnd, original) };
                }
                return Err(error.to_string());
            }
            let hook = Box::new(DropHook {
                state: state.clone(),
                on_over: on_drag_over.clone(),
                hovering: hovering.clone(),
            });
            let hook = Box::into_raw(hook) as usize;
            if !unsafe {
                SetWindowSubclass(hwnd, Some(drop_owner_proc), DROP_SUBCLASS, hook).as_bool()
            } {
                unsafe {
                    drop(Box::from_raw(hook as *mut DropHook));
                    let _ = RevokeDragDrop(hwnd);
                }
                if let Some(original) = &original {
                    let _ = unsafe { RegisterDragDrop(hwnd, original) };
                }
                return Err("Cannot attach native drop owner cleanup.".into());
            }
            Ok(Self {
                hwnd,
                state,
                original,
                target: Some(target),
                on_over: on_drag_over,
                hook,
                hovering,
                _sta: PhantomData,
            })
        })();
        if result.is_err() {
            unsafe {
                OleUninitialize();
            }
        }
        result
    }
    pub fn sync_session(&self, session: u64, accepting: bool) {
        self.state.sync(session, accepting);
        let retired = self
            .hovering
            .borrow()
            .as_ref()
            .is_some_and(|session| !session.ticket.is_current());
        if retired {
            let previous = self.hovering.borrow_mut().take();
            if let Some(previous) = previous {
                (self.on_over)(DragOverEvent::Leave(previous.ticket.clone()));
            }
        }
    }
    pub fn cancel(&self) {
        self.state.retire();
        let previous = self.hovering.borrow_mut().take();
        if let Some(previous) = previous {
            (self.on_over)(DragOverEvent::Leave(previous.ticket.clone()));
        }
    }
}
impl Drop for DragDropWindow {
    fn drop(&mut self) {
        self.cancel();
        let mut reference = 0;
        if validate_sta(self.hwnd).is_ok()
            && unsafe {
                GetWindowSubclass(
                    self.hwnd,
                    Some(drop_owner_proc),
                    DROP_SUBCLASS,
                    Some(&mut reference),
                )
                .as_bool()
            }
            && reference == self.hook
            && unsafe {
                RemoveWindowSubclass(self.hwnd, Some(drop_owner_proc), DROP_SUBCLASS).as_bool()
            }
        {
            unsafe {
                drop(Box::from_raw(self.hook as *mut DropHook));
            }
        }
        // Never revoke a replacement/foreign target, including HWND reuse.
        if self
            .target
            .as_ref()
            .is_some_and(|target| property(self.hwnd) == target.as_raw())
        {
            if unsafe { RevokeDragDrop(self.hwnd) }.is_ok() {
                if let Some(original) = &self.original {
                    if let Err(error) = unsafe { RegisterDragDrop(self.hwnd, original) } {
                        tracing::warn!(%error,"Restore native drop target failed");
                    }
                }
            }
        }
        // Release our COM references while the initializing STA apartment is
        // still alive, after OLE's registration has been restored/revoked.
        self.target = None;
        self.original = None;
        unsafe {
            OleUninitialize();
        }
    }
}

const DROP_SUBCLASS: usize = 0xBC18;
struct DropHook {
    state: Arc<OwnerState>,
    on_over: OnDragOver,
    hovering: Rc<RefCell<Option<Rc<IncomingSession>>>>,
}
unsafe extern "system" fn drop_owner_proc(
    hwnd: HWND,
    message: u32,
    wp: WPARAM,
    lp: LPARAM,
    id: usize,
    reference: usize,
) -> LRESULT {
    let hook = &*(reference as *const DropHook);
    let state = hook.state.clone();
    let hovering = hook.hovering.clone();
    let on_over = hook.on_over.clone();
    // Remove/free the subclass-owned box before an external Leave callback can
    // reentrantly drop the binding and try to free the same native reference.
    if message == WM_NCDESTROY {
        let _ = RemoveWindowSubclass(hwnd, Some(drop_owner_proc), id);
        drop(Box::from_raw(reference as *mut DropHook));
    }
    if message == WM_NCDESTROY
        || (message == WM_SHOWWINDOW && wp.0 == 0)
        || (message == WM_WINDOWPOSCHANGED && !IsWindowVisible(hwnd).as_bool())
    {
        state.retire();
        let previous = hovering.borrow_mut().take();
        if let Some(previous) = previous {
            on_over(DragOverEvent::Leave(previous.ticket.clone()));
        }
    }
    DefSubclassProc(hwnd, message, wp, lp)
}

const DRAG_MESSAGE: u32 = WM_APP + 119;
const DRAG_SUBCLASS: usize = 0xBC19;
struct OutCommand {
    ticket: DropTicket,
    payload: OleDragPayload,
    reply: mpsc::SyncSender<Result<(), String>>,
}
#[derive(Default)]
struct OutQueue {
    stopped: bool,
    active: Option<DropTicket>,
    pending: Option<OutCommand>,
}
struct OutShared {
    owner: Arc<OwnerState>,
    thread: u32,
    queue: Mutex<OutQueue>,
    native_drag: NativeDragRunner,
}
impl OutShared {
    fn retire_request(&self) {
        self.owner.retire();
        let pending = self.queue.lock().unwrap().pending.take();
        if let Some(command) = pending {
            let _ = command.reply.send(Err(
                "OLE drag belongs to a retired window or destination.".into()
            ));
        }
    }
    fn cancel(&self) {
        self.owner.retire();
        let mut queue = self.queue.lock().unwrap();
        queue.stopped = true;
        queue.active = None;
        if let Some(command) = queue.pending.take() {
            let _ = command
                .reply
                .send(Err("OLE dispatch window retired.".into()));
        }
    }
    fn run(&self, payload: OleDragPayload) -> Result<(), String> {
        if unsafe { GetCurrentThreadId() } == self.thread {
            return Err("OLE runner cannot wait on its own UI STA.".into());
        }
        let (reply, receiver) = mpsc::sync_channel(1);
        {
            let mut queue = self.queue.lock().unwrap();
            if queue.stopped || queue.pending.is_some() {
                return Err("OLE dispatcher is unavailable.".into());
            }
            let ticket = queue
                .active
                .clone()
                .ok_or("OLE drag has no accepted native gesture.")?;
            ticket.verify()?;
            queue.pending = Some(OutCommand {
                ticket,
                payload,
                reply,
            });
            if let Err(error) =
                unsafe { PostMessageW(handle(self.owner.hwnd), DRAG_MESSAGE, WPARAM(0), LPARAM(0)) }
            {
                queue.pending = None;
                return Err(error.to_string());
            }
        }
        // No timeout: an in-flight native drag owns completion. Destruction
        // cancels queued work and sends a terminal answer rather than executing
        // a late drag after the worker has already reported failure.
        receiver
            .recv()
            .unwrap_or_else(|_| Err("OLE dispatcher closed.".into()))
    }
}
unsafe extern "system" fn outgoing_proc(
    hwnd: HWND,
    message: u32,
    wp: WPARAM,
    lp: LPARAM,
    id: usize,
    reference: usize,
) -> LRESULT {
    // OLE pumps native messages. A nested WM_NCDESTROY can release the
    // subclass reference and the view can drop its dispatcher while this
    // callback is still inside DoDragDrop. Retain a separate reference until
    // this invocation returns; never borrow the subclass-owned raw Arc.
    let pointer = reference as *const OutShared;
    Arc::increment_strong_count(pointer);
    let shared = Arc::from_raw(pointer);
    if message == DRAG_MESSAGE {
        let command = shared.queue.lock().unwrap().pending.take();
        if let Some(command) = command {
            let result = (|| {
                validate_sta(hwnd)?;
                command.ticket.verify()?;
                if !IsWindowVisible(hwnd).as_bool() || GetAsyncKeyState(VK_LBUTTON.0 as i32) >= 0 {
                    return Err("Drag gesture ended before OLE was ready.".into());
                }
                let native_drag = shared.native_drag.clone();
                native_drag(&command.payload, command.ticket.clone())
            })();
            let _ = command.reply.send(result);
        }
        return LRESULT(0);
    }
    if message == WM_NCDESTROY {
        shared.cancel();
        let _ = RemoveWindowSubclass(hwnd, Some(outgoing_proc), id);
        drop(Arc::from_raw(reference as *const OutShared));
    } else if (message == WM_SHOWWINDOW && wp.0 == 0)
        || (message == WM_WINDOWPOSCHANGED && !IsWindowVisible(hwnd).as_bool())
    {
        shared.retire_request();
    }
    DefSubclassProc(hwnd, message, wp, lp)
}

/// Posted DoDragDrop runs on the HWND STA outside GPUI's App borrow. Call
/// start_history on the UI thread; reading/decoding then occurs on its worker.
pub struct DragOutDispatcher {
    shared: Arc<OutShared>,
    reference: usize,
    _sta: PhantomData<Rc<()>>,
}
impl DragOutDispatcher {
    pub fn attach(hwnd: isize, native_drag: NativeDragRunner) -> Result<Self, String> {
        let hwnd = handle(hwnd);
        validate_sta(hwnd)?;
        let mut existing = 0;
        if unsafe {
            GetWindowSubclass(
                hwnd,
                Some(outgoing_proc),
                DRAG_SUBCLASS,
                Some(&mut existing),
            )
            .as_bool()
        } {
            return Err("OLE dispatcher already attached to this window.".into());
        }
        let shared = Arc::new(OutShared {
            owner: OwnerState::new(hwnd.0 as isize),
            thread: unsafe { GetCurrentThreadId() },
            queue: Mutex::new(OutQueue::default()),
            native_drag,
        });
        let reference = Arc::into_raw(shared.clone()) as usize;
        if !unsafe {
            SetWindowSubclass(hwnd, Some(outgoing_proc), DRAG_SUBCLASS, reference).as_bool()
        } {
            unsafe {
                drop(Arc::from_raw(reference as *const OutShared));
            }
            return Err("Cannot attach OLE dispatcher.".into());
        }
        Ok(Self {
            shared,
            reference,
            _sta: PhantomData,
        })
    }
    pub fn install(&self, host: &HostState) {
        let shared = self.shared.clone();
        host.set_ole_drag_ui_runner(move |payload| shared.run(payload));
    }
    pub fn bound_host(&self, host: &HostState) -> HostState {
        let shared = self.shared.clone();
        host.with_ole_drag_ui_runner(move |payload| shared.run(payload))
    }
    pub fn sync_session(&self, session: u64, accepting: bool) {
        self.shared.owner.sync(session, accepting);
        let pending = {
            let mut queue = self.shared.queue.lock().unwrap();
            if queue
                .pending
                .as_ref()
                .is_some_and(|command| !command.ticket.is_current())
            {
                queue.pending.take()
            } else {
                None
            }
        };
        if let Some(command) = pending {
            let _ = command.reply.send(Err(
                "OLE drag belongs to a retired window or destination.".into()
            ));
        }
    }
    /// Retire preparation/queued/active OLE without detaching this root HWND.
    /// Call before rebinding a destination even when the page session is equal.
    /// A queued worker receives an immediate terminal answer; an active modal
    /// loop exits via the core ticket guard. Keep its active slot until that
    /// worker completes so an overlapping drag cannot replace its request.
    pub fn cancel(&self) {
        self.shared.retire_request();
    }
    pub fn start_history(
        &self,
        host: &HostState,
        ids: Vec<String>,
    ) -> Result<mpsc::Receiver<Result<serde_json::Value, String>>, String> {
        let host = self.bound_host(host);
        self.start(move || host.begin_ole_drag(&serde_json::json!({"ids":ids})))
    }

    pub fn start_history_ticketed(
        &self,
        host: &HostState,
        ids: Vec<String>,
    ) -> Result<
        (
            DropTicket,
            mpsc::Receiver<Result<serde_json::Value, String>>,
        ),
        String,
    > {
        let host = self.bound_host(host);
        self.start_ticketed(move || host.begin_ole_drag(&serde_json::json!({"ids":ids})))
    }
    /// For favorites, pass core `begin_favor_ole_drag`; it must share the same
    /// active-session guard/config/output builder as history. No DTO rebuild.
    pub fn start<F>(
        &self,
        operation: F,
    ) -> Result<mpsc::Receiver<Result<serde_json::Value, String>>, String>
    where
        F: FnOnce() -> Result<serde_json::Value, String> + Send + 'static,
    {
        self.start_ticketed(operation).map(|(_, receiver)| receiver)
    }
    /// Use the returned ticket to discard a worker's late success/error after
    /// navigation. Compatibility `start` keeps its original receiver-only API.
    pub fn start_ticketed<F>(
        &self,
        operation: F,
    ) -> Result<
        (
            DropTicket,
            mpsc::Receiver<Result<serde_json::Value, String>>,
        ),
        String,
    >
    where
        F: FnOnce() -> Result<serde_json::Value, String> + Send + 'static,
    {
        validate_sta(handle(self.shared.owner.hwnd))?;
        let ticket = {
            let mut queue = self.shared.queue.lock().unwrap();
            if queue.stopped || queue.active.is_some() || queue.pending.is_some() {
                return Err("A native drag is already preparing or active.".into());
            }
            // A rejected second start must not increment request and cancel
            // the first drag. Ticket allocation belongs to successful admission.
            let ticket = self.shared.owner.ticket()?;
            queue.active = Some(ticket.clone());
            ticket
        };
        let shared = self.shared.clone();
        let reply_ticket = ticket.clone();
        let (reply, receiver) = mpsc::sync_channel(1);
        let spawn = std::thread::Builder::new()
            .name("cliphost-ole-prepare".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    ticket.verify().and_then(|_| operation())
                }))
                .unwrap_or_else(|_| Err("OLE drag preparation failed.".into()));
                shared.queue.lock().unwrap().active = None;
                let _ = reply.send(result);
            });
        if let Err(error) = spawn {
            self.shared.queue.lock().unwrap().active = None;
            return Err(error.to_string());
        }
        Ok((reply_ticket, receiver))
    }
}
impl Drop for DragOutDispatcher {
    fn drop(&mut self) {
        self.shared.cancel();
        let hwnd = handle(self.shared.owner.hwnd);
        let mut reference = 0;
        if validate_sta(hwnd).is_ok()
            && unsafe {
                GetWindowSubclass(
                    hwnd,
                    Some(outgoing_proc),
                    DRAG_SUBCLASS,
                    Some(&mut reference),
                )
                .as_bool()
            }
            && reference == self.reference
            && unsafe { RemoveWindowSubclass(hwnd, Some(outgoing_proc), DRAG_SUBCLASS).as_bool() }
        {
            unsafe {
                drop(Arc::from_raw(self.reference as *const OutShared));
            }
        }
    }
}

#[cfg(test)]
#[path = "drag_drop_windows_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "drag_drop_provider_tests.rs"]
mod provider_tests;
