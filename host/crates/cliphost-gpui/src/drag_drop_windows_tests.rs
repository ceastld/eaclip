//! Self-owned COM formats/HWNDs only; no clipboard write or global desktop input.
use super::*;
use crate::windows_api as windows;
use windows::{
    core::{Result, HRESULT},
    Win32::{
        Foundation::{BOOL, DV_E_FORMATETC, E_NOTIMPL, S_OK},
        System::{
            Com::{IDataObject_Impl, IEnumFORMATETC, STGMEDIUM_0},
            Memory::{GlobalAlloc, GMEM_MOVEABLE, GMEM_ZEROINIT},
        },
        UI::{
            Shell::SHCreateStdEnumFmtEtc,
            WindowsAndMessaging::{CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WS_POPUP},
        },
    },
};

#[implement(IDataObject)]
struct Fixture {
    formats: Vec<(u16, Vec<u8>)>,
    on_read: Option<Rc<dyn Fn()>>,
    fail_format: Option<u16>,
}
impl IDataObject_Impl for Fixture_Impl {
    fn GetData(&self, format: *const FORMATETC) -> Result<STGMEDIUM> {
        if let Some(on_read) = &self.on_read {
            on_read();
        }
        let format = unsafe { format.as_ref() }.ok_or(windows::core::Error::from(E_POINTER))?;
        if self.fail_format == Some(format.cfFormat) {
            return Err(E_NOTIMPL.into());
        }
        let bytes = &self
            .formats
            .iter()
            .find(|(id, _)| *id == format.cfFormat)
            .ok_or(windows::core::Error::from(DV_E_FORMATETC))?
            .1;
        if format.cfFormat == 2 {
            use windows::Win32::Graphics::Gdi::{
                CreateDIBSection, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS,
            };
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: 40,
                    biWidth: 2,
                    biHeight: -2,
                    biPlanes: 1,
                    biBitCount: 32,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut pixels = std::ptr::null_mut();
            let bitmap =
                unsafe { CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut pixels, None, 0) }?;
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), pixels.cast::<u8>(), 16);
            }
            return Ok(STGMEDIUM {
                tymed: TYMED_GDI.0 as u32,
                u: STGMEDIUM_0 { hBitmap: bitmap },
                pUnkForRelease: std::mem::ManuallyDrop::new(None),
            });
        }
        unsafe {
            let global = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes.len())?;
            let target = GlobalLock(global);
            assert!(!target.is_null());
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
        if unsafe { format.as_ref() }.is_some_and(|f| {
            self.formats.iter().any(|(id, _)| *id == f.cfFormat)
                && f.tymed
                    & (if f.cfFormat == 2 {
                        TYMED_GDI
                    } else {
                        TYMED_HGLOBAL
                    })
                    .0 as u32
                    != 0
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

fn fixture(text: &str, html: &str) -> IDataObject {
    Fixture {
        on_read: None,
        fail_format: None,
        formats: vec![
            (
                13,
                text.encode_utf16()
                    .chain([0])
                    .flat_map(u16::to_le_bytes)
                    .collect(),
            ),
            (registered("HTML Format"), [html.as_bytes(), &[0]].concat()),
            (
                registered("QQ_Unicode_RichEdit_Format"),
                b"<QQRichEditFormat>fixture</QQRichEditFormat>\0".to_vec(),
            ),
        ],
    }
    .into()
}

fn session_for(ticket: DropTicket, source: &IDataObject) -> Rc<IncomingSession> {
    Rc::new(IncomingSession {
        ticket,
        source: source.cast::<IUnknown>().unwrap(),
    })
}

#[test]
fn actual_com_reader_copies_each_medium_and_retains_rich_wire() {
    let source = fixture("中文🦀", " <b>中文🦀</b> ");
    let first = read_object(&source).unwrap();
    let second = read_object(&source).unwrap();
    drop(source);
    assert_eq!(first.text.as_deref(), Some("中文🦀"));
    assert_eq!(
        first.html.as_deref(),
        Some(&b" <b>\xe4\xb8\xad\xe6\x96\x87\xf0\x9f\xa6\x80</b> \0"[..])
    );
    assert_eq!(first.html, second.html);
    assert_ne!(
        first.html.as_ref().unwrap().as_ptr(),
        second.html.as_ref().unwrap().as_ptr()
    );
    assert!(first.qq_rich.unwrap().ends_with(b"</QQRichEditFormat>\0"));
}

struct ActionDecoder;
impl super::super::DropPayloadDecoder for ActionDecoder {
    fn rich_text(&self, _: &str, _: &[u8]) -> std::result::Result<String, String> {
        Err("unexpected rich format in action fixture".into())
    }
    fn quicker_action(&self, bytes: &[u8]) -> std::result::Result<String, String> {
        HostState::decode_native_drop_quicker_action(bytes)
    }
    fn dib_to_png(&self, _: &[u8], _: bool) -> std::result::Result<Vec<u8>, String> {
        Err("unexpected image format in action fixture".into())
    }
}

#[test]
fn dynamic_quicker_action_candidates_preserve_wire_and_skip_first_bad_format() {
    let dynamic = "Quicker.Common.ActionItemDragObject, Quicker.Common, Version=0.0.0.0";
    let wire = include_bytes!("../../cliphost-core/tests/fixtures/quicker-drag-v1-wpf.bin");
    let source: IDataObject = Fixture {
        formats: vec![
            (registered("quicker-action-item-json"), b"{broken".to_vec()),
            (registered(dynamic), wire.to_vec()),
        ],
        on_read: None,
        fail_format: None,
    }
    .into();
    assert!(accepts(&source), "dynamic action format must be admissible");
    let raw = read_object(&source).unwrap();
    drop(source);
    assert!(
        raw.quicker_action.is_none(),
        "wire must not be copied twice"
    );
    assert_eq!(raw.quicker_action_candidates.len(), 2);
    assert_eq!(
        raw.quicker_action_candidates[0].format_name,
        ACTION_FORMATS[0]
    );
    assert_eq!(raw.quicker_action_candidates[1].format_name, dynamic);
    assert_eq!(
        raw.quicker_action_candidates[1].wire.as_ref().unwrap(),
        wire
    );
    let snapshot = raw.into_snapshot(&ActionDecoder).unwrap();
    let action: serde_json::Value =
        serde_json::from_str(snapshot.quicker_action.as_deref().unwrap()).unwrap();
    assert_eq!(action["Id"], "08c06820-416c-4463-99b0-18c711634b3a");
    assert_eq!(action["Title"], "Rust 动作 🦀");
    assert_eq!(action["Description"], "native-v1-fixture");
    assert_eq!(action["Icon"], "fa:Light_Bolt");
}

#[test]
fn quicker_action_read_failure_tries_later_real_wpf_string_without_rebuilding_json() {
    let first = registered("quicker-action-item-json");
    let wire = include_bytes!("../../cliphost-core/tests/fixtures/quicker-action-v2-wpf.bin");
    let expected = include_str!("../../cliphost-core/tests/fixtures/quicker-action-v2.json");
    let source: IDataObject = Fixture {
        formats: vec![
            (first, b"advertised but unavailable".to_vec()),
            (registered("quicker-action-drag-item"), wire.to_vec()),
        ],
        on_read: None,
        fail_format: Some(first),
    }
    .into();
    let raw = read_object(&source).unwrap();
    assert_eq!(raw.quicker_action_candidates.len(), 2);
    assert!(raw.quicker_action_candidates[0].wire.is_err());
    assert_eq!(
        raw.quicker_action_candidates[1].wire.as_ref().unwrap(),
        wire
    );
    let snapshot = raw.into_snapshot(&ActionDecoder).unwrap();
    assert_eq!(
        snapshot
            .quicker_action
            .as_deref()
            .unwrap()
            .trim_end_matches(['\r', '\n']),
        expected.trim_end_matches(['\r', '\n'])
    );
}

#[test]
fn all_bad_quicker_action_candidates_report_error_instead_of_falling_back_to_text() {
    let source: IDataObject = Fixture {
        formats: vec![
            (
                13,
                "ordinary fallback"
                    .encode_utf16()
                    .chain([0])
                    .flat_map(u16::to_le_bytes)
                    .collect(),
            ),
            (registered("quicker-action-item-json"), b"{broken".to_vec()),
            (
                registered("owned-QUICKER-ACTION-invalid-provider"),
                vec![0x96, 0xa7, 0x9e, 0xfd],
            ),
        ],
        on_read: None,
        fail_format: None,
    }
    .into();
    let raw = read_object(&source).unwrap();
    assert_eq!(raw.text.as_deref(), Some("ordinary fallback"));
    let error = raw.into_snapshot(&ActionDecoder).unwrap_err();
    assert!(error.contains("No valid Quicker action drag format"));
    assert!(error.contains("quicker-action-item-json"));
    assert!(error.contains("owned-QUICKER-ACTION-invalid-provider"));
}

#[test]
fn quicker_action_candidates_cannot_bypass_remaining_transfer_budget() {
    let first = b"{broken".to_vec();
    let source: IDataObject = Fixture {
        formats: vec![
            (registered("quicker-action-item-json"), first.clone()),
            (
                registered("quicker-action-drag-item"),
                b"larger than the remaining allowance".to_vec(),
            ),
        ],
        on_read: None,
        fail_format: None,
    }
    .into();
    let mut budget = first.len() + 1;
    assert_eq!(
        read_action_candidates(&source, &mut budget).unwrap_err(),
        TRANSFER_BUDGET_ERROR
    );
    assert_eq!(
        budget, 1,
        "only copied wire consumes the aggregate allowance"
    );
}

#[test]
fn oversized_single_action_candidate_is_skipped_without_consuming_transfer_budget() {
    let wire = include_bytes!("../../cliphost-core/tests/fixtures/quicker-action-v2-wpf.bin");
    let expected = include_str!("../../cliphost-core/tests/fixtures/quicker-action-v2.json");
    let source: IDataObject = Fixture {
        formats: vec![
            (
                registered("quicker-action-item-json"),
                vec![0; MAX_FORMAT_BYTES + 1],
            ),
            (registered("quicker-action-drag-item"), wire.to_vec()),
        ],
        on_read: None,
        fail_format: None,
    }
    .into();
    // Other copied formats may already have consumed the rest of the total
    // allowance. Skipping an ineligible format must still leave this valid
    // candidate exactly enough room without bypassing the total cap.
    let mut budget = wire.len();
    let candidates = read_action_candidates(&source, &mut budget).unwrap();
    drop(source);
    assert_eq!(candidates.len(), 2);
    assert_eq!(candidates[0].wire.as_ref().unwrap_err(), FORMAT_SIZE_ERROR);
    assert_eq!(candidates[1].wire.as_ref().unwrap(), wire);
    assert_eq!(budget, 0);
    let snapshot = RawDrop {
        quicker_action_candidates: candidates,
        ..Default::default()
    }
    .into_snapshot(&ActionDecoder)
    .unwrap();
    assert_eq!(
        snapshot
            .quicker_action
            .as_deref()
            .unwrap()
            .trim_end_matches(['\r', '\n']),
        expected.trim_end_matches(['\r', '\n'])
    );
}

#[test]
fn dynamic_quicker_action_enumeration_is_bounded_and_steps_keep_their_typed_carrier() {
    let ignored = "Quicker.Common.ActionItem2";
    let source: IDataObject = Fixture {
        formats: vec![
            (
                registered("quicker-action-steps"),
                "{\"steps\":[]}"
                    .encode_utf16()
                    .chain([0])
                    .flat_map(u16::to_le_bytes)
                    .collect(),
            ),
            (registered(ignored), b"fixture-action".to_vec()),
        ],
        on_read: None,
        fail_format: None,
    }
    .into();
    let raw = read_object(&source).unwrap();
    assert!(raw.quicker_action_candidates.is_empty());
    assert_eq!(
        raw.into_snapshot(&ActionDecoder)
            .unwrap()
            .quicker_steps
            .as_deref(),
        Some("{\"steps\":[]}")
    );

    let mut formats = (0..MAX_ENUMERATED_ACTION_FORMATS)
        .map(|index| {
            (
                registered(&format!("ClipHost.OwnedActionEnumBoundary.{index}")),
                vec![1],
            )
        })
        .collect::<Vec<_>>();
    let late = registered("owned-quicker-action-beyond-enumeration-budget");
    formats.push((late, b"fixture-action".to_vec()));
    let source: IDataObject = Fixture {
        formats,
        on_read: None,
        fail_format: None,
    }
    .into();
    assert!(!action_formats(&source).iter().any(|(id, _)| *id == late));
    assert!(!accepts(&source));
}

#[test]
fn cf_bitmap_is_read_as_gdi_and_copied_before_source_medium_is_released() {
    let pixel = [160u8, 90, 20, 0];
    let source: IDataObject = Fixture {
        formats: vec![(2, pixel.repeat(4))],
        on_read: None,
        fail_format: None,
    }
    .into();
    let raw = read_object(&source).unwrap();
    drop(source);
    assert!(raw.has_image_format);
    let dib = raw.dib.unwrap();
    assert_eq!(dib.len(), 56);
    assert_eq!(i32::from_le_bytes(dib[4..8].try_into().unwrap()), 2);
    assert_eq!(i32::from_le_bytes(dib[8..12].try_into().unwrap()), -2);
    assert_eq!(&dib[40..44], &pixel);
}

#[test]
fn native_target_rejects_move_only_and_old_page_before_data_is_accepted() {
    let state = OwnerState::new(123);
    state.sync(1, true);
    let seen = Rc::new(RefCell::new(Vec::new()));
    let callback_seen = seen.clone();
    let source = fixture("text", "<b>text</b>");
    let hovering = Rc::new(RefCell::new(Some(session_for(
        state.ticket().unwrap(),
        &source,
    ))));
    let target: IDropTarget = Target {
        files_only: false,
        state: state.clone(),
        on_drop: Rc::new(move |event| {
            callback_seen.borrow_mut().push(event.formats);
            Ok(())
        }),
        on_over: Rc::new(|_| {}),
        on_error: Rc::new(|_| {}),
        hovering: hovering.clone(),
        negotiation_generation: Cell::new(0),
    }
    .into();
    unsafe {
        let mut effect = DROPEFFECT(2); // A source offering only MOVE must not lose data.
        target
            .Drop(
                Some(&source),
                MODIFIERKEYS_FLAGS(0),
                POINTL { x: 1, y: 2 },
                &mut effect,
            )
            .unwrap();
        assert_eq!(effect, DROPEFFECT_NONE);
        assert!(seen.borrow().is_empty());
        *hovering.borrow_mut() = Some(session_for(state.ticket().unwrap(), &source));
        state.sync(2, true);
        let mut effect = DROPEFFECT_COPY;
        target
            .Drop(
                Some(&source),
                MODIFIERKEYS_FLAGS(0),
                POINTL { x: 1, y: 2 },
                &mut effect,
            )
            .unwrap();
        assert_eq!(effect, DROPEFFECT_NONE);
        assert!(seen.borrow().is_empty());
        *hovering.borrow_mut() = Some(session_for(state.ticket().unwrap(), &source));
        let mut effect = DROPEFFECT_COPY;
        target
            .Drop(
                Some(&source),
                MODIFIERKEYS_FLAGS(0),
                POINTL { x: 1, y: 2 },
                &mut effect,
            )
            .unwrap();
        assert_eq!(effect, DROPEFFECT_COPY);
        assert_eq!(seen.borrow().len(), 1);
    }
}

#[test]
fn root_registration_restores_previous_target_and_destruction_retires_owner() {
    unsafe {
        OleInitialize(None).unwrap();
    }
    unsafe {
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!("ClipHost isolated OLE fixture"),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let original: IDropTarget = Target {
        files_only: false,
            state: OwnerState::new(hwnd.0 as isize),
            on_drop: Rc::new(|_| Ok(())),
            on_over: Rc::new(|_| {}),
            on_error: Rc::new(|_| {}),
            hovering: Rc::new(RefCell::new(None)),
            negotiation_generation: Cell::new(0),
        }
        .into();
        RegisterDragDrop(hwnd, &original).unwrap();
        let binding = DragDropWindow::attach(
            hwnd.0 as isize,
            Rc::new(|_| Ok(())),
            Rc::new(|_| {}),
            Rc::new(|_| {}),
        )
        .unwrap();
        assert_eq!(property(hwnd), binding.target.as_ref().unwrap().as_raw());
        drop(binding);
        assert_eq!(property(hwnd), original.as_raw());
        let binding = DragDropWindow::attach(
            hwnd.0 as isize,
            Rc::new(|_| Ok(())),
            Rc::new(|_| {}),
            Rc::new(|_| {}),
        )
        .unwrap();
        binding.sync_session(1, true);
        let old = binding.state.ticket().unwrap();
        DestroyWindow(hwnd).unwrap();
        assert!(!old.is_current());
        drop(binding);
        OleUninitialize();
    }
}

#[test]
fn destroyed_dispatch_window_answers_queued_drag_and_never_runs_the_backend() {
    use std::sync::atomic::{AtomicBool, Ordering};
    unsafe {
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!("ClipHost isolated outgoing OLE fixture"),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let called = Arc::new(AtomicBool::new(false));
        let observed = called.clone();
        let dispatcher = DragOutDispatcher::attach(
            hwnd.0 as isize,
            Arc::new(move |_, _| {
                observed.store(true, Ordering::Release);
                Ok(())
            }),
        )
        .unwrap();
        dispatcher.sync_session(1, true);
        let ticket = dispatcher.shared.owner.ticket().unwrap();
        let (reply, receiver) = mpsc::sync_channel(1);
        dispatcher.shared.queue.lock().unwrap().pending = Some(OutCommand {
            ticket: ticket.clone(),
            payload: OleDragPayload::default(),
            reply,
        });
        PostMessageW(hwnd, DRAG_MESSAGE, WPARAM(0), LPARAM(0)).unwrap();
        DestroyWindow(hwnd).unwrap();
        assert!(receiver.try_recv().unwrap().is_err());
        assert!(!ticket.is_current());
        assert!(!called.load(Ordering::Acquire));
        drop(dispatcher);
    }
}

#[test]
fn accepted_drop_keeps_destination_but_reply_retires_on_same_session_rebind() {
    let state = OwnerState::new(123);
    state.sync(7, true);
    let destination = Rc::new(RefCell::new("folder-a".to_string()));
    let accepted = Rc::new(RefCell::new(None));
    let target_destination = destination.clone();
    let target_accepted = accepted.clone();
    let source = fixture("accepted text", "<b>accepted text</b>");
    let target: IDropTarget = Target {
        files_only: false,
        state: state.clone(),
        on_over: Rc::new(|_| {}),
        on_error: Rc::new(|_| {}),
        hovering: Rc::new(RefCell::new(Some(session_for(
            state.ticket().unwrap(),
            &source,
        )))),
        negotiation_generation: Cell::new(0),
        on_drop: Rc::new(move |event| {
            *target_accepted.borrow_mut() = Some((
                event.ticket,
                target_destination.borrow().clone(),
                event.formats,
            ));
            Ok(())
        }),
    }
    .into();
    unsafe {
        let mut effect = DROPEFFECT_COPY;
        target
            .Drop(
                Some(&source),
                MODIFIERKEYS_FLAGS(0),
                POINTL { x: 1, y: 2 },
                &mut effect,
            )
            .unwrap();
        assert_eq!(effect, DROPEFFECT_COPY);
    }
    state.retire();
    state.sync(7, true); // Same page session, different destination; explicit retire is required.
    *destination.borrow_mut() = "folder-b".into();
    let reply = accepted.borrow_mut().take().unwrap();
    assert!(!reply.0.is_current());
    assert_eq!(reply.1, "folder-a");
    assert_eq!(reply.2.text.as_deref(), Some("accepted text"));
}

#[test]
fn reentrant_data_read_cannot_finish_or_release_a_newer_drag() {
    use std::cell::Cell;
    let state = OwnerState::new(123);
    state.sync(1, true);
    let old = state.ticket().unwrap();
    let hovering = Rc::new(RefCell::new(None));
    let releases = Rc::new(RefCell::new(Vec::<DropTicket>::new()));
    let callback_releases = releases.clone();
    let accepted = Rc::new(Cell::new(false));
    let callback_accepted = accepted.clone();
    let target: IDropTarget = Target {
        files_only: false,
        state: state.clone(),
        hovering: hovering.clone(),
        negotiation_generation: Cell::new(0),
        on_error: Rc::new(|_| {}),
        on_drop: Rc::new(move |_| {
            callback_accepted.set(true);
            Ok(())
        }),
        on_over: Rc::new(move |event| {
            if let DragOverEvent::Leave(ticket) = event {
                callback_releases.borrow_mut().push(ticket)
            }
        }),
    }
    .into();
    let entered = Rc::new(Cell::new(false));
    let input_state = state.clone();
    let input_hover = hovering.clone();
    let later_source = fixture("later source", "<b>later source</b>");
    let later_identity = later_source.cast::<IUnknown>().unwrap();
    let source: IDataObject = Fixture {
        formats: vec![(
            13,
            "new drag during GetData"
                .encode_utf16()
                .chain([0])
                .flat_map(u16::to_le_bytes)
                .collect(),
        )],
        on_read: Some(Rc::new(move || {
            if !entered.replace(true) {
                *input_hover.borrow_mut() = Some(Rc::new(IncomingSession {
                    ticket: input_state.ticket().unwrap(),
                    source: later_identity.clone(),
                }));
            }
        })),
        fail_format: None,
    }
    .into();
    *hovering.borrow_mut() = Some(session_for(old.clone(), &source));
    unsafe {
        let mut effect = DROPEFFECT_COPY;
        target
            .Drop(
                Some(&source),
                MODIFIERKEYS_FLAGS(0),
                POINTL { x: 1, y: 2 },
                &mut effect,
            )
            .unwrap();
        assert_eq!(effect, DROPEFFECT_NONE);
    }
    let newer = hovering.borrow().clone().unwrap();
    assert!(newer.ticket.is_current());
    assert!(!newer.ticket.same_request(&old));
    assert!(!accepted.get());
    assert!(releases.borrow().is_empty());
}

#[test]
fn stamped_com_error_is_discardable_after_the_owner_destination_changes() {
    let state = OwnerState::new(123);
    state.sync(5, true);
    let errors = Rc::new(RefCell::new(Vec::<DropError>::new()));
    let error_sink = errors.clone();
    let source: IDataObject = Fixture {
        formats: vec![(13, vec![65])],
        on_read: None,
        fail_format: None,
    }
    .into();
    let target: IDropTarget = Target {
        files_only: false,
        state: state.clone(),
        hovering: Rc::new(RefCell::new(Some(session_for(
            state.ticket().unwrap(),
            &source,
        )))),
        negotiation_generation: Cell::new(0),
        on_drop: Rc::new(|_| panic!("bad unicode format accepted")),
        on_over: Rc::new(|_| {}),
        on_error: Rc::new(move |error| error_sink.borrow_mut().push(error)),
    }
    .into();
    unsafe {
        let mut effect = DROPEFFECT_COPY;
        target
            .Drop(
                Some(&source),
                MODIFIERKEYS_FLAGS(0),
                POINTL { x: 1, y: 2 },
                &mut effect,
            )
            .unwrap();
        assert_eq!(effect, DROPEFFECT_NONE);
    }
    assert_eq!(errors.borrow().len(), 1);
    assert!(errors.borrow()[0].ticket.is_current());
    state.retire();
    state.sync(5, true);
    assert!(errors
        .borrow()
        .iter()
        .all(|error| !error.ticket.is_current()));
}

fn hidden_outgoing_window() -> HWND {
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!("ClipHost isolated outgoing lifetime"),
            WS_POPUP,
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
    }
}

#[test]
fn rejected_second_start_preserves_the_already_accepted_preparation_ticket() {
    let hwnd = hidden_outgoing_window();
    let dispatcher = DragOutDispatcher::attach(hwnd.0 as isize, Arc::new(|_, _| Ok(()))).unwrap();
    dispatcher.sync_session(1, true);
    let (release, wait) = mpsc::channel();
    let (first, reply) = dispatcher
        .start_ticketed(move || {
            let _ = wait.recv();
            Ok(serde_json::json!({"ok":true}))
        })
        .unwrap();
    let second = dispatcher.start(|| Ok(serde_json::Value::Null));
    let still_current = first.is_current();
    release.send(()).unwrap();
    assert!(reply
        .recv_timeout(std::time::Duration::from_secs(3))
        .unwrap()
        .is_ok());
    assert!(second.is_err());
    assert!(still_current);
    unsafe {
        DestroyWindow(hwnd).unwrap();
    }
}

#[test]
fn destination_rebind_during_preparation_never_enqueues_or_runs_the_native_drag() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let hwnd = hidden_outgoing_window();
    let called = Arc::new(AtomicBool::new(false));
    let backend = called.clone();
    let dispatcher = DragOutDispatcher::attach(
        hwnd.0 as isize,
        Arc::new(move |_, _| {
            backend.store(true, Ordering::Release);
            Ok(())
        }),
    )
    .unwrap();
    dispatcher.sync_session(9, true);
    let shared = dispatcher.shared.clone();
    let (release, wait) = mpsc::channel();
    let (entered, ready) = mpsc::channel();
    let (old, reply) = dispatcher
        .start_ticketed(move || {
            entered.send(()).unwrap();
            let _ = wait.recv();
            shared.run(OleDragPayload::default())?;
            Ok(serde_json::Value::Null)
        })
        .unwrap();
    // Exercise cancellation during preparation, after the worker's initial
    // ticket check, rather than racing cancellation against closure entry.
    ready
        .recv_timeout(std::time::Duration::from_secs(3))
        .expect("drag preparation did not enter before destination rebind");
    dispatcher.cancel();
    dispatcher.sync_session(9, true);
    release.send(()).unwrap();
    assert!(reply
        .recv_timeout(std::time::Duration::from_secs(3))
        .unwrap()
        .is_err());
    assert!(!old.is_current());
    assert!(!called.load(Ordering::Acquire));
    assert!(dispatcher.shared.queue.lock().unwrap().pending.is_none());
    let (next, reply) = dispatcher
        .start_ticketed(|| Ok(serde_json::Value::Null))
        .unwrap();
    assert!(reply
        .recv_timeout(std::time::Duration::from_secs(3))
        .unwrap()
        .is_ok());
    assert!(next.is_current());
    unsafe {
        DestroyWindow(hwnd).unwrap();
    }
}

#[test]
fn stopped_ui_thread_dispatcher_replies_to_queued_work_before_thread_exit() {
    let (send_reply, receive_reply) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let hwnd = hidden_outgoing_window();
        let dispatcher = DragOutDispatcher::attach(
            hwnd.0 as isize,
            Arc::new(|_, _| panic!("retired queued drag ran")),
        )
        .unwrap();
        dispatcher.sync_session(1, true);
        let ticket = dispatcher.shared.owner.ticket().unwrap();
        let (reply, receive) = mpsc::sync_channel(1);
        dispatcher.shared.queue.lock().unwrap().pending = Some(OutCommand {
            ticket,
            payload: OleDragPayload::default(),
            reply,
        });
        send_reply.send(receive).unwrap();
        // RAII shutdown happens on the owning thread before its HWND teardown.
        drop(dispatcher);
        unsafe {
            DestroyWindow(hwnd).unwrap();
        }
    });
    let reply = receive_reply
        .recv_timeout(std::time::Duration::from_secs(3))
        .unwrap();
    assert!(reply
        .recv_timeout(std::time::Duration::from_secs(3))
        .unwrap()
        .is_err());
    worker.join().unwrap();
}

#[test]
fn queued_drag_retires_before_its_posted_message_can_enter_the_backend() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use windows::Win32::UI::WindowsAndMessaging::SendMessageW;
    let hwnd = hidden_outgoing_window();
    let called = Arc::new(AtomicBool::new(false));
    let observed = called.clone();
    let dispatcher = DragOutDispatcher::attach(
        hwnd.0 as isize,
        Arc::new(move |_, _| {
            observed.store(true, Ordering::Release);
            Ok(())
        }),
    )
    .unwrap();
    dispatcher.sync_session(1, true);
    let old = dispatcher.shared.owner.ticket().unwrap();
    let (reply, receive) = mpsc::sync_channel(1);
    dispatcher.shared.queue.lock().unwrap().pending = Some(OutCommand {
        ticket: old.clone(),
        payload: OleDragPayload::default(),
        reply,
    });
    dispatcher.sync_session(1, false);
    dispatcher.sync_session(1, true);
    assert!(receive.try_recv().unwrap().is_err());
    unsafe {
        SendMessageW(hwnd, DRAG_MESSAGE, WPARAM(0), LPARAM(0));
    }
    assert!(!old.is_current());
    assert!(!called.load(Ordering::Acquire));
    unsafe {
        DestroyWindow(hwnd).unwrap();
    }
}
