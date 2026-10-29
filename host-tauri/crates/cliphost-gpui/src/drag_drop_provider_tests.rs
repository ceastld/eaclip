//! Acceptance boundary using our registered root target and an owned COM
//! provider. Only the COM gesture is scripted; no desktop input, DoDragDrop,
//! system clipboard, production host/store, or third-party receiver is used.
use super::*;
use self::windows::{
    core::{Result, HRESULT},
    Win32::{
        Foundation::{BOOL, DV_E_FORMATETC, E_NOTIMPL, S_OK},
        System::{
            Com::{IDataObject_Impl, IEnumFORMATETC, STGMEDIUM_0},
            Memory::{GlobalAlloc, GMEM_MOVEABLE, GMEM_ZEROINIT},
            Ole::{DROPEFFECT_LINK, DROPEFFECT_MOVE},
        },
        UI::{
            Shell::SHCreateStdEnumFmtEtc,
            WindowsAndMessaging::{
                CreateWindowExW, DestroyWindow, ShowWindow, SW_SHOWNOACTIVATE, WS_EX_NOACTIVATE,
                WS_EX_TOOLWINDOW, WS_POPUP,
            },
        },
    },
};

#[implement(IDataObject)]
struct Provider {
    formats: Vec<(u16, Vec<u8>)>,
    reads: Rc<RefCell<Vec<u16>>>,
    before_read: Option<Rc<dyn Fn()>>,
    before_query: Option<Rc<dyn Fn()>>,
}
impl IDataObject_Impl for Provider_Impl {
    fn GetData(&self, format: *const FORMATETC) -> Result<STGMEDIUM> {
        let format = unsafe { format.as_ref() }.ok_or(windows::core::Error::from(E_POINTER))?;
        if self.QueryGetData(format).is_err() {
            return Err(DV_E_FORMATETC.into());
        }
        if let Some(before_read) = &self.before_read {
            before_read();
        }
        self.reads.borrow_mut().push(format.cfFormat);
        let bytes = &self
            .formats
            .iter()
            .find(|(id, _)| *id == format.cfFormat)
            .unwrap()
            .1;
        unsafe {
            let global = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes.len())?;
            let target = GlobalLock(global);
            if target.is_null() {
                let _ = windows::Win32::Foundation::GlobalFree(global);
                return Err(windows::core::Error::from_win32());
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), target.cast(), bytes.len());
            let _ = GlobalUnlock(global);
            Ok(STGMEDIUM {
                tymed: TYMED_HGLOBAL.0 as u32,
                u: STGMEDIUM_0 { hGlobal: global },
                pUnkForRelease: std::mem::ManuallyDrop::new(None),
            })
        }
    }
    fn GetDataHere(&self, _: *const FORMATETC, _: *mut STGMEDIUM) -> Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn QueryGetData(&self, format: *const FORMATETC) -> HRESULT {
        if let Some(before_query) = &self.before_query {
            before_query();
        }
        if unsafe { format.as_ref() }.is_some_and(|format| {
            format.dwAspect == DVASPECT_CONTENT.0
                && format.lindex == -1
                && format.tymed & TYMED_HGLOBAL.0 as u32 != 0
                && self.formats.iter().any(|(id, _)| *id == format.cfFormat)
        }) {
            S_OK
        } else {
            DV_E_FORMATETC
        }
    }
    fn GetCanonicalFormatEtc(&self, _: *const FORMATETC, _: *mut FORMATETC) -> HRESULT {
        E_NOTIMPL
    }
    fn SetData(&self, _: *const FORMATETC, _: *const STGMEDIUM, _: BOOL) -> Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn EnumFormatEtc(&self, _: u32) -> Result<IEnumFORMATETC> {
        unsafe {
            SHCreateStdEnumFmtEtc(
                &self
                    .formats
                    .iter()
                    .map(|(id, _)| fmt(*id, TYMED_HGLOBAL.0 as u32))
                    .collect::<Vec<_>>(),
            )
        }
    }
    fn DAdvise(
        &self,
        _: *const FORMATETC,
        _: u32,
        _: Option<&windows::Win32::System::Com::IAdviseSink>,
    ) -> Result<u32> {
        Err(E_NOTIMPL.into())
    }
    fn DUnadvise(&self, _: u32) -> Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn EnumDAdvise(&self) -> Result<windows::Win32::System::Com::IEnumSTATDATA> {
        Err(E_NOTIMPL.into())
    }
}

struct Root(HWND);
impl Root {
    fn new() -> Self {
        unsafe {
            let hwnd = CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                w!("STATIC"),
                w!("ClipHost self-owned OLE provider acceptance"),
                WS_POPUP,
                -32000,
                -32000,
                1,
                1,
                None,
                None,
                None,
                None,
            )
            .unwrap();
            // Off-screen and no-activate, but visible to the target's actual
            // admission check. We never move the cursor to this window.
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            assert!(IsWindowVisible(hwnd).as_bool());
            Self(hwnd)
        }
    }
    fn target(&self) -> IDropTarget {
        let pointer = property(self.0);
        assert!(!pointer.is_null(), "our root was not registered with OLE");
        unsafe { IDropTarget::from_raw_borrowed(&pointer).unwrap().clone() }
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        unsafe {
            DestroyWindow(self.0).unwrap();
        }
    }
}

fn wide(text: &str) -> Vec<u8> {
    text.encode_utf16()
        .chain([0])
        .flat_map(u16::to_le_bytes)
        .collect()
}
fn files(paths: &[&str]) -> Vec<u8> {
    let mut bytes = vec![0; 20];
    bytes[..4].copy_from_slice(&20u32.to_le_bytes());
    bytes[16..20].copy_from_slice(&1u32.to_le_bytes());
    for path in paths {
        bytes.extend(wide(path));
    }
    bytes.extend([0; 2]);
    bytes
}
fn provider(
    formats: Vec<(u16, Vec<u8>)>,
    before_read: Option<Rc<dyn Fn()>>,
) -> (IDataObject, Rc<RefCell<Vec<u16>>>) {
    let reads = Rc::new(RefCell::new(Vec::new()));
    (
        Provider {
            formats,
            reads: reads.clone(),
            before_read,
            before_query: None,
        }
        .into(),
        reads,
    )
}
fn provider_with_query(
    formats: Vec<(u16, Vec<u8>)>,
    before_query: Rc<dyn Fn()>,
) -> (IDataObject, Rc<RefCell<Vec<u16>>>) {
    let reads = Rc::new(RefCell::new(Vec::new()));
    (
        Provider {
            formats,
            reads: reads.clone(),
            before_read: None,
            before_query: Some(before_query),
        }
        .into(),
        reads,
    )
}
fn enter(target: &IDropTarget, source: &IDataObject, allowed: DROPEFFECT) -> DROPEFFECT {
    let mut effect = allowed;
    unsafe {
        target
            .DragEnter(
                Some(source),
                MODIFIERKEYS_FLAGS(0),
                POINTL {
                    x: -32000,
                    y: -32000,
                },
                &mut effect,
            )
            .unwrap();
    }
    effect
}
fn drop_at(target: &IDropTarget, source: &IDataObject, allowed: DROPEFFECT) -> DROPEFFECT {
    let mut effect = allowed;
    unsafe {
        target
            .Drop(
                Some(source),
                MODIFIERKEYS_FLAGS(0),
                POINTL { x: 123, y: 456 },
                &mut effect,
            )
            .unwrap();
    }
    effect
}

#[test]
fn file_line_drop_registered_target_reads_only_hdrop_and_never_accepts_move() {
    std::thread::spawn(|| {
        let root = Root::new();
        let received = Rc::new(RefCell::new(Vec::new()));
        let collected = received.clone();
        let binding = DragDropWindow::attach_files(
            root.0.0 as isize,
            Rc::new(move |event| { collected.borrow_mut().push(event.formats); Ok(()) }),
            Rc::new(|_| {}),
            Rc::new(|error| panic!("unexpected file-line drop error: {}", error.message)),
        ).unwrap();
        binding.sync_session(1, true);
        let target = root.target();
        let (source, reads) = provider(vec![
            (13, wide("do not read unrelated text")),
            (15, files(&[r"C:\fixture\folder"])),
            (registered("PNG"), b"do not decode unrelated image".to_vec()),
        ], None);
        assert_eq!(enter(&target, &source, DROPEFFECT_COPY), DROPEFFECT_COPY);
        assert!(reads.borrow().is_empty(), "enter negotiates formats without requesting contents");
        assert_eq!(drop_at(&target, &source, DROPEFFECT_COPY), DROPEFFECT_COPY);
        assert_eq!(*reads.borrow(), vec![15]);
        assert_eq!(received.borrow()[0].files, vec![r"C:\fixture\folder"]);
        assert!(received.borrow()[0].text.is_none());
        let (text, text_reads) = provider(vec![(13, wide(r"C:\fixture\folder"))], None);
        assert_eq!(enter(&target, &text, DROPEFFECT_COPY), DROPEFFECT_NONE);
        assert_eq!(drop_at(&target, &text, DROPEFFECT_COPY), DROPEFFECT_NONE);
        assert!(text_reads.borrow().is_empty(), "text that resembles a path is not CF_HDROP");
        assert_eq!(enter(&target, &source, DROPEFFECT_MOVE), DROPEFFECT_NONE);
        assert_eq!(drop_at(&target, &source, DROPEFFECT_MOVE), DROPEFFECT_NONE);
        assert_eq!(enter(&target, &source, DROPEFFECT_COPY), DROPEFFECT_COPY);
        binding.cancel();
        assert_eq!(drop_at(&target, &source, DROPEFFECT_COPY), DROPEFFECT_NONE);
        assert_eq!(received.borrow().len(), 1, "retired surfaces cannot receive another path offer");
    }).join().unwrap();
}

#[test]
fn owned_provider_registered_target_preserves_rich_files_and_captured_surface() {
    std::thread::spawn(|| {
        for surface in ["history", "favorites", "panel"] {
            let root = Root::new();
            let destination = Rc::new(RefCell::new(format!("{surface}/original")));
            let captured = Rc::new(RefCell::new(Vec::new()));
            let lease = Rc::new(RefCell::new(None::<DropTicket>));
            let received = captured.clone();
            let folder = destination.clone();
            let dropping_lease = lease.clone();
            let dragging_lease = lease.clone();
            let binding = DragDropWindow::attach_stamped(
                root.0.0 as isize,
                Rc::new(move |event| {
                    assert!(dropping_lease.borrow().as_ref().unwrap().same_request(&event.ticket));
                    received.borrow_mut().push((folder.borrow().clone(), event));
                    Ok(())
                }),
                Rc::new(move |event| match event {
                    DragOverEvent::Enter(ticket) => *dragging_lease.borrow_mut() = Some(ticket),
                    DragOverEvent::Leave(ticket) => {
                        assert!(dragging_lease.borrow().as_ref().unwrap().same_request(&ticket));
                        *dragging_lease.borrow_mut() = None;
                    }
                }),
                Rc::new(|error| panic!("unexpected provider error: {}", error.message)),
            ).unwrap();
            binding.sync_session(7, true);
            let target = root.target();
            let html = b"Version:1.0\r\nSourceURL:https://fixture.invalid/\r\n<table><tr><td><b>structure</b><img src=\"file:///D:/fixture/a.png\"></td></tr></table>\0".to_vec();
            let qq = wide("<QQRichEditFormat><EditElement type=\"0\" text=\"中文🦀\"/><EditElement type=\"1\" filepath=\"D:\\fixture\\a.png\"/><EditElement type=\"3\" filepath=\"D:\\fixture\\file.txt\"/></QQRichEditFormat>");
            let wechat = b"<WeChatRichEditFormat><img path=\"D:\\fixture\\b.png\"/></WeChatRichEditFormat>\0".to_vec();
            let steps = wide("{\"steps\":[{\"type\":\"fixture\",\"data\":\"original\"}]}");
            let paths = [r"D:\fixture\中文🦀.png", r"D:\fixture\folder", r"D:\fixture\中文🦀.png"];
            let png = [b"\x89PNG\r\n\x1a\n".as_slice(), b"raw owned image wire"].concat();
            let (source, reads) = provider(vec![
                (13, wide("原文\r\n\t🦀")), (15, files(&paths)),
                (registered("HTML Format"), html.clone()),
                (registered("QQ_Unicode_RichEdit_Format"), qq.clone()),
                (registered("WeChat_RichEdit_Format"), wechat.clone()),
                (registered("quicker-action-steps"), steps.clone()),
                (registered("PNG"), png.clone()),
            ], None);
            assert_eq!(enter(&target, &source, DROPEFFECT(DROPEFFECT_COPY.0 | DROPEFFECT_MOVE.0 | DROPEFFECT_LINK.0)), DROPEFFECT_COPY);
            let mut over = DROPEFFECT(DROPEFFECT_COPY.0 | DROPEFFECT_MOVE.0);
            unsafe { target.DragOver(MODIFIERKEYS_FLAGS(0), POINTL { x: 123, y: 456 }, &mut over).unwrap(); }
            assert_eq!(over, DROPEFFECT_COPY);
            assert_eq!(drop_at(&target, &source, over), DROPEFFECT_COPY);
            drop(source); // all HGLOBALs have already been released by reader
            assert!(lease.borrow().is_none());
            let (accepted_folder, event) = captured.borrow_mut().pop().unwrap();
            *destination.borrow_mut() = format!("{surface}/later");
            binding.sync_session(7, false);
            binding.sync_session(7, true);
            assert_eq!(accepted_folder, format!("{surface}/original"));
            assert!(!event.ticket.is_current(), "old UI completion must retire on navigation");
            assert_eq!(event.screen_point, (123, 456));
            assert_eq!(event.formats.text.as_deref(), Some("原文\r\n\t🦀"));
            assert_eq!(event.formats.files, paths);
            assert_eq!(event.formats.html, Some(html));
            assert_eq!(event.formats.qq_rich, Some(qq));
            assert_eq!(event.formats.wechat_rich, Some(wechat));
            assert_eq!(event.formats.quicker_steps, Some(steps));
            assert_eq!(event.formats.png, Some(png));
            assert!(event.formats.has_image_format);
            assert_eq!(reads.borrow().len(), 7, "QueryGetData must not eagerly read payloads");
            drop(target);
            drop(binding);
        }
    }).join().unwrap();
}

#[test]
fn owned_provider_move_only_and_rejected_queue_never_report_copy_success() {
    std::thread::spawn(|| {
        let root = Root::new();
        let errors = Rc::new(RefCell::new(Vec::new()));
        let seen_errors = errors.clone();
        let accepted = Rc::new(std::cell::Cell::new(0));
        let attempts = accepted.clone();
        let binding = DragDropWindow::attach_stamped(
            root.0 .0 as isize,
            Rc::new(move |_| {
                attempts.set(attempts.get() + 1);
                Err("owned acceptance queue full".into())
            }),
            Rc::new(|_| {}),
            Rc::new(move |error| seen_errors.borrow_mut().push(error)),
        )
        .unwrap();
        binding.sync_session(1, true);
        let target = root.target();
        let (source, reads) = provider(vec![(13, wide("owned text"))], None);
        assert_eq!(enter(&target, &source, DROPEFFECT_MOVE), DROPEFFECT_NONE);
        assert_eq!(drop_at(&target, &source, DROPEFFECT_MOVE), DROPEFFECT_NONE);
        assert_eq!(accepted.get(), 0);
        assert!(reads.borrow().is_empty());
        assert_eq!(enter(&target, &source, DROPEFFECT_COPY), DROPEFFECT_COPY);
        assert_eq!(drop_at(&target, &source, DROPEFFECT_COPY), DROPEFFECT_NONE);
        assert_eq!(accepted.get(), 1);
        let error = errors.borrow_mut().pop().unwrap();
        assert!(error.ticket.is_current());
        assert_eq!(error.message, "owned acceptance queue full");
        binding.sync_session(2, true);
        assert!(!error.ticket.is_current());
    })
    .join()
    .unwrap();
}

#[test]
fn owned_provider_session_retirement_during_getdata_does_not_accept_old_drop() {
    std::thread::spawn(|| {
        let root = Root::new();
        let accepted = Rc::new(std::cell::Cell::new(0));
        let attempts = accepted.clone();
        let errors = Rc::new(std::cell::Cell::new(0));
        let failures = errors.clone();
        let binding = DragDropWindow::attach_stamped(
            root.0 .0 as isize,
            Rc::new(move |_| {
                attempts.set(attempts.get() + 1);
                Ok(())
            }),
            Rc::new(|_| {}),
            Rc::new(move |_| failures.set(failures.get() + 1)),
        )
        .unwrap();
        binding.sync_session(12, true);
        let target = root.target();
        let owner = binding.state.clone();
        let (source, _) = provider(
            vec![(13, wide("late body"))],
            Some(Rc::new(move || owner.sync(13, true))),
        );
        assert_eq!(enter(&target, &source, DROPEFFECT_COPY), DROPEFFECT_COPY);
        assert_eq!(drop_at(&target, &source, DROPEFFECT_COPY), DROPEFFECT_NONE);
        assert_eq!(accepted.get(), 0);
        assert_eq!(
            errors.get(),
            0,
            "stale provider errors must not reach newer UI session"
        );
        let (new_source, _) = provider(vec![(13, wide("current body"))], None);
        assert_eq!(
            enter(&target, &new_source, DROPEFFECT_COPY),
            DROPEFFECT_COPY
        );
        assert_eq!(
            drop_at(&target, &new_source, DROPEFFECT_COPY),
            DROPEFFECT_COPY
        );
        assert_eq!(accepted.get(), 1);
    })
    .join()
    .unwrap();
}

#[test]
fn owned_provider_late_drop_cannot_borrow_the_replacement_provider_session() {
    std::thread::spawn(|| {
        let root = Root::new();
        let captured = Rc::new(RefCell::new(Vec::new()));
        let received = captured.clone();
        let lease = Rc::new(RefCell::new(None::<DropTicket>));
        let dragging = lease.clone();
        let errors = Rc::new(std::cell::Cell::new(0));
        let failures = errors.clone();
        let binding = DragDropWindow::attach_stamped(
            root.0 .0 as isize,
            Rc::new(move |event| {
                received.borrow_mut().push(event);
                Ok(())
            }),
            Rc::new(move |event| match event {
                DragOverEvent::Enter(ticket) => *dragging.borrow_mut() = Some(ticket),
                DragOverEvent::Leave(ticket) => {
                    assert!(dragging.borrow().as_ref().unwrap().same_request(&ticket));
                    *dragging.borrow_mut() = None;
                }
            }),
            Rc::new(move |_| failures.set(failures.get() + 1)),
        )
        .unwrap();
        binding.sync_session(5, true);
        let target = root.target();
        let (old_source, old_reads) = provider(vec![(13, wide("old provider"))], None);
        let (current_source, current_reads) = provider(vec![(13, wide("current provider"))], None);
        assert_eq!(
            enter(&target, &old_source, DROPEFFECT_COPY),
            DROPEFFECT_COPY
        );
        let old_ticket = lease.borrow().as_ref().unwrap().clone();
        assert_eq!(
            enter(&target, &current_source, DROPEFFECT_COPY),
            DROPEFFECT_COPY
        );
        let current_ticket = lease.borrow().as_ref().unwrap().clone();
        assert!(!old_ticket.is_current());
        assert_eq!(
            drop_at(&target, &old_source, DROPEFFECT_COPY),
            DROPEFFECT_NONE
        );
        assert!(
            old_reads.borrow().is_empty(),
            "the stale provider must never be read"
        );
        assert!(captured.borrow().is_empty());
        assert!(lease
            .borrow()
            .as_ref()
            .unwrap()
            .same_request(&current_ticket));
        assert!(current_ticket.is_current());
        assert_eq!(
            errors.get(),
            0,
            "a stale provider cannot post an error as the newer provider"
        );
        let mut effect = DROPEFFECT_COPY;
        unsafe {
            target
                .DragOver(MODIFIERKEYS_FLAGS(0), POINTL { x: 0, y: 0 }, &mut effect)
                .unwrap();
        }
        assert_eq!(effect, DROPEFFECT_COPY);
        // A separately retained interface to the same canonical COM object
        // is still the same provider; raw wrapper address is not the identity.
        let same_source = current_source
            .cast::<windows::core::IUnknown>()
            .unwrap()
            .cast::<IDataObject>()
            .unwrap();
        assert_eq!(drop_at(&target, &same_source, effect), DROPEFFECT_COPY);
        let event = captured.borrow_mut().pop().unwrap();
        assert!(event.ticket.same_request(&current_ticket));
        assert_eq!(event.formats.text.as_deref(), Some("current provider"));
        assert_eq!(current_reads.borrow().len(), 1);
        assert!(lease.borrow().is_none());
    })
    .join()
    .unwrap();
}

#[path = "drag_drop_provider_reentry_tests.rs"]
mod reentry;
