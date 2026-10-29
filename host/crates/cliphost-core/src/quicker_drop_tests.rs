use super::*;
use std::cell::RefCell;
use windows::core::HRESULT;
use windows::Win32::Foundation::{DV_E_FORMATETC, E_NOTIMPL, S_OK};
use windows::Win32::System::Com::{
    IAdviseSink, IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA, STGMEDIUM_0,
};
use windows::Win32::System::Memory::{GlobalAlloc, GMEM_MOVEABLE};
use windows::Win32::System::Ole::DROPEFFECT_MOVE;

#[implement(IDropTarget)]
struct Original(Rc<RefCell<Vec<&'static str>>>);
#[allow(non_snake_case)]
impl IDropTarget_Impl for Original_Impl {
    fn DragEnter(
        &self,
        _: Option<&IDataObject>,
        _: MODIFIERKEYS_FLAGS,
        _: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> Result<()> {
        self.0.borrow_mut().push("enter");
        unsafe {
            *effect = DROPEFFECT_MOVE;
        }
        Ok(())
    }
    fn DragOver(&self, _: MODIFIERKEYS_FLAGS, _: &POINTL, effect: *mut DROPEFFECT) -> Result<()> {
        self.0.borrow_mut().push("over");
        unsafe {
            *effect = DROPEFFECT_MOVE;
        }
        Ok(())
    }
    fn DragLeave(&self) -> Result<()> {
        self.0.borrow_mut().push("leave");
        Ok(())
    }
    fn Drop(
        &self,
        _: Option<&IDataObject>,
        _: MODIFIERKEYS_FLAGS,
        _: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> Result<()> {
        self.0.borrow_mut().push("drop");
        unsafe {
            *effect = DROPEFFECT_MOVE;
        }
        Ok(())
    }
}

#[implement(IDataObject)]
struct Data {
    format: u16,
    bytes: Vec<u8>,
    reads: Rc<Cell<usize>>,
}
#[allow(non_snake_case)]
impl IDataObject_Impl for Data_Impl {
    fn GetData(&self, requested: *const FORMATETC) -> Result<STGMEDIUM> {
        self.QueryGetData(requested).ok()?;
        self.reads.set(self.reads.get() + 1);
        unsafe {
            let memory = GlobalAlloc(GMEM_MOVEABLE, self.bytes.len())?;
            let address = GlobalLock(memory);
            assert!(!address.is_null());
            std::ptr::copy_nonoverlapping(self.bytes.as_ptr(), address.cast(), self.bytes.len());
            let _ = GlobalUnlock(memory);
            Ok(STGMEDIUM {
                tymed: TYMED_HGLOBAL.0 as u32,
                u: STGMEDIUM_0 { hGlobal: memory },
                pUnkForRelease: std::mem::ManuallyDrop::new(None),
            })
        }
    }
    fn QueryGetData(&self, requested: *const FORMATETC) -> HRESULT {
        if unsafe { requested.as_ref() }
            .is_some_and(|f| f.cfFormat == self.format && f.tymed & TYMED_HGLOBAL.0 as u32 != 0)
        {
            S_OK
        } else {
            DV_E_FORMATETC
        }
    }
    fn EnumFormatEtc(&self, _: u32) -> Result<IEnumFORMATETC> {
        unsafe { windows::Win32::UI::Shell::SHCreateStdEnumFmtEtc(&[format(self.format)]) }
    }
    fn GetDataHere(&self, _: *const FORMATETC, _: *mut STGMEDIUM) -> Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn GetCanonicalFormatEtc(&self, _: *const FORMATETC, _: *mut FORMATETC) -> HRESULT {
        E_NOTIMPL
    }
    fn SetData(&self, _: *const FORMATETC, _: *const STGMEDIUM, _: BOOL) -> Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn DAdvise(&self, _: *const FORMATETC, _: u32, _: Option<&IAdviseSink>) -> Result<u32> {
        Err(E_NOTIMPL.into())
    }
    fn DUnadvise(&self, _: u32) -> Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn EnumDAdvise(&self) -> Result<IEnumSTATDATA> {
        Err(E_NOTIMPL.into())
    }
}

fn data(name: &str, bytes: &[u8]) -> (IDataObject, Rc<Cell<usize>>) {
    let reads = Rc::new(Cell::new(0));
    (
        Data {
            format: unsafe { RegisterClipboardFormatW(&HSTRING::from(name)) as u16 },
            bytes: bytes.to_vec(),
            reads: reads.clone(),
        }
        .into(),
        reads,
    )
}
fn fixture() -> (
    IDropTarget,
    Rc<RefCell<Vec<&'static str>>>,
    Rc<RefCell<Vec<std::result::Result<String, String>>>>,
) {
    let calls = Rc::new(RefCell::new(Vec::new()));
    let drops = Rc::new(RefCell::new(Vec::new()));
    let sink = drops.clone();
    let target = Target {
        original: Original(calls.clone()).into(),
        on_drop: Rc::new(move |value| {
            let result = value.as_ref().map(|_| ()).map_err(Clone::clone);
            sink.borrow_mut().push(value);
            result
        }),
        on_hover: Rc::new(|_| {}),
        quicker: Cell::new(false),
        forwarding: Cell::new(false),
    }
    .into();
    (target, calls, drops)
}

#[test]
fn quicker_drop_native_copy_cancel_invalid_and_forwarding() {
    let (target, calls, drops) = fixture();
    let pt = POINTL { x: 37, y: 48 };
    let keys = MODIFIERKEYS_FLAGS(0);
    let mut effect = DROPEFFECT(3);
    let (action, reads) = data(
        "quicker-action-item",
        include_bytes!("../tests/fixtures/quicker-action-v1-wpf.bin"),
    );
    unsafe {
        target.DragEnter(&action, keys, pt, &mut effect).unwrap();
        assert_eq!(effect, DROPEFFECT_COPY);
        target.DragOver(keys, pt, &mut effect).unwrap();
        assert_eq!(reads.get(), 0, "hover only probes format availability");
        target.DragLeave().unwrap();
        assert!(drops.borrow().is_empty());
        effect = DROPEFFECT(3);
        target.DragEnter(&action, keys, pt, &mut effect).unwrap();
        target.Drop(&action, keys, pt, &mut effect).unwrap();
        assert_eq!(effect, DROPEFFECT_COPY);
        assert_eq!(reads.get(), 1);
        assert!(drops.borrow()[0].as_ref().unwrap().contains("Rust 动作"));
        assert!(calls.borrow().is_empty());
        effect = DROPEFFECT_MOVE;
        target.DragEnter(&action, keys, pt, &mut effect).unwrap();
        target.Drop(&action, keys, pt, &mut effect).unwrap();
        assert_eq!(effect, DROPEFFECT_NONE);
        assert_eq!(
            drops.borrow().len(),
            1,
            "move-only sources must not be consumed"
        );
        let (bad, _) = data("quicker-action-item", b"{broken");
        effect = DROPEFFECT(3);
        target.DragEnter(&bad, keys, pt, &mut effect).unwrap();
        target.Drop(&bad, keys, pt, &mut effect).unwrap();
        assert_eq!(effect, DROPEFFECT_NONE);
        assert!(
            drops.borrow()[1].is_err(),
            "invalid payload reaches the visible error handler"
        );
        effect = DROPEFFECT(3);
        target.DragEnter(None, keys, pt, &mut effect).unwrap();
        target.DragOver(keys, pt, &mut effect).unwrap();
        target.Drop(None, keys, pt, &mut effect).unwrap();
        assert_eq!(effect, DROPEFFECT_MOVE);
        target.DragEnter(None, keys, pt, &mut effect).unwrap();
        target.DragLeave().unwrap();
        assert_eq!(
            &*calls.borrow(),
            &["enter", "over", "drop", "enter", "leave"]
        );
    }
}

#[test]
fn quicker_drop_native_assembly_qualified_format_is_decoded() {
    let (action, _) = data(
        "Quicker.Common.ActionItemDragObject, Quicker.Common, Version=0.0.0.0",
        include_bytes!("../tests/fixtures/quicker-drag-v1-wpf.bin"),
    );
    assert!(has_action(&action));
    assert!(action_payload(&action).unwrap().contains("Rust 动作"));
}

#[test]
fn quicker_drop_native_registration_retains_and_restores_original() {
    use windows::Win32::UI::WindowsAndMessaging::*;
    struct Apartment;
    impl Drop for Apartment { fn drop(&mut self) { unsafe { OleUninitialize(); } } }
    unsafe { OleInitialize(None).unwrap(); }
    let _apartment = Apartment;
    struct Window(HWND);
    impl Drop for Window {
        fn drop(&mut self) {
            unsafe {
                let _ = RevokeDragDrop(self.0);
                let _ = DestroyWindow(self.0);
            }
        }
    }
    // Hidden HWNDs only: no desktop input, focus, global clipboard, or network.
    let root = Window(unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!("owned root"),
            WINDOW_STYLE(0),
            0,
            0,
            1,
            1,
            None,
            None,
            None,
            None,
        )
        .unwrap()
    });
    let child = Window(unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!("owned child"),
            WS_CHILD,
            0,
            0,
            1,
            1,
            root.0,
            None,
            None,
            None,
        )
        .unwrap()
    });
    let mut bindings = Bindings::new(Rc::new(|_| Ok(())), Rc::new(|_| {})).unwrap();
    let original: IDropTarget = Original(Rc::new(RefCell::new(Vec::new()))).into();
    unsafe {
        RegisterDragDrop(child.0, &original).unwrap();
    }
    assert_eq!(owned_target_pointer(child.0), original.as_raw());
    assert_eq!(bindings.refresh(root.0 .0 as isize), 1);
    assert_ne!(owned_target_pointer(child.0), original.as_raw());
    assert_eq!(bindings.refresh(root.0 .0 as isize), 1);
    drop(bindings);
    assert_eq!(owned_target_pointer(child.0), original.as_raw());
    unsafe {
        RevokeDragDrop(child.0).unwrap();
    }
}
