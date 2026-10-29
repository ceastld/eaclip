// Native cases are explicitly ignored: they must create their own non-visible
// window station before any clipboard access, or fail closed. No WinSta0 fallback.
use super::*;
use std::{cell::RefCell, sync::{Arc, atomic::{AtomicUsize, Ordering}, mpsc}, time::Instant};
use windows::{core::{HSTRING, w}, Win32::{Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    System::{DataExchange::{EmptyClipboard, OpenClipboard, SetClipboardData}, LibraryLoader::GetModuleHandleW,
        Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE}},
    UI::WindowsAndMessaging::*}};

#[path = "clipboard_private_station_test.rs"]
mod private_station;

thread_local! { static DECODE_PROBE: RefCell<Option<Box<dyn Fn()>>> = RefCell::new(None); }
pub(super) fn before_decode() { DECODE_PROBE.with(|probe| { if let Some(probe) = probe.borrow().as_ref() { probe(); } }); }

#[test]
#[ignore = "requires create-only non-visible private window station; never accesses WinSta0 clipboard"]
fn clipboard_read_safety_private_station_native_capture() {
    assert_eq!(std::env::var("CLIPHOST_RUN_PRIVATE_CLIPBOARD_TESTS").as_deref(), Ok("1"), "Explicit isolated fixture authorization is required");
    let station = private_station::PrivateStation::create().expect("Private clipboard unavailable; refusing any clipboard access");
    // Destroy this test thread's TLS window before restoring its original
    // desktop, including on assertion failure. Production TLS drops on its owner.
    struct ReaderCleanup;
    impl Drop for ReaderCleanup { fn drop(&mut self) { release_reader_window(); } }
    let _reader_cleanup = ReaderCleanup;
    let fixture_root = std::env::var_os("CLIPHOST_PRIVATE_CLIPBOARD_TEST_ROOT").map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp/rust-clipboard-lock-tests/native"));
    assert!(fixture_root.is_absolute() && fixture_root.components().any(|part| part.as_os_str() == ".tmp"),
        "Native fixture output must remain in an explicit repository .tmp directory");
    std::fs::create_dir_all(&fixture_root).unwrap();
    let owner = Owner::start(station.desktop, false);
    let started = Instant::now();
    let result = capture(&fixture_root, "owned-fixture.exe", true, false, "", &|_, _| None, None, None, true).unwrap();
    let lazy_ms = started.elapsed().as_millis();
    assert_eq!(result.1.unwrap().body_text.as_deref(), Some("owned fixture text"));
    assert_eq!(owner.rendered.load(Ordering::SeqCst), 0, "A completed text capture requested the unrelated delayed DIB");
    assert!(lazy_ms < 1000);
    assert_parallel_readers_exclude_each_other(station.desktop);

    // Negative control reproduces the former eager read: even after text is
    // available, GetClipboardData(DIB) synchronously asks the owner's UI thread.
    let started = Instant::now();
    open_clipboard_retry().unwrap();
    let close = Close::new(Some(CF_DIB));
    let bytes = unsafe { GetClipboardData(CF_DIB).ok().and_then(lock_bytes) }.unwrap();
    let negative_ms = started.elapsed().as_millis();
    assert!(negative_ms >= 10_000 && owner.rendered.load(Ordering::SeqCst) > 0);

    let old_probe = Arc::new(AtomicUsize::new(0));
    install_decode_probe(station.desktop, old_probe.clone());
    assert!(decode_image(bytes, dib_to_png).is_some());
    assert_eq!(old_probe.load(Ordering::SeqCst), 0, "Negative control unexpectedly released the clipboard before encoding");
    drop(close);
    let fixed_probe = Arc::new(AtomicUsize::new(0));
    install_decode_probe(station.desktop, fixed_probe.clone());
    assert!(Session::new(false, None).image_once().unwrap().0.is_some());
    assert!(fixed_probe.load(Ordering::SeqCst) > 0, "Production image decoding still held the clipboard open");
    DECODE_PROBE.with(|probe| *probe.borrow_mut() = None);
    let raw = Arc::new(std::sync::Mutex::new(None));
    let hub = CaptureHub::default();
    hub.set_raw_enabled(true);
    let received = raw.clone();
    hub.set_on_raw_snapshot(move |_, snapshot| {
        *received.lock().unwrap() = Some((snapshot.text.clone(), snapshot.png.is_some()));
    });
    let captured = capture(&fixture_root, "owned-fixture.exe", true, false, "", &|_, _| None, None, Some(&hub), true).unwrap();
    assert_eq!(captured.1.unwrap().kind, ClipKind::TextClipItem);
    assert_eq!(*raw.lock().unwrap(), Some((Some("owned fixture text".into()), true)),
        "History's earlier text selection suppressed the image required by raw sync");
    drop(owner);

    // Exercise the real IM order (40123), rather than mistaking a generic
    // text-first result for proof that a required WeCom image read was skipped.
    let image_owner = Owner::start_image_with_delayed_text(station.desktop);
    let started = Instant::now();
    let captured = capture(&fixture_root, "wxwork.exe", true, false, "", &|_, _| None, None, None, true).unwrap();
    let im_image_ms = started.elapsed().as_millis();
    assert_eq!(captured.1.unwrap().kind, ClipKind::ImageClipItem);
    assert_eq!(image_owner.rendered.load(Ordering::SeqCst), 0,
        "Successful IM image capture requested an unrelated delayed HTML or Unicode payload");
    assert!(im_image_ms < 1000);
    drop(image_owner);

    let excluded = Owner::start(station.desktop, true);
    assert!(capture(&fixture_root, "owned-fixture.exe", true, false, "", &|_, _| None, None, None, true).unwrap().1.is_none());
    assert_eq!(excluded.rendered.load(Ordering::SeqCst), 0, "An excluded copy triggered delayed content rendering");
    std::fs::write(fixture_root.join("result.json"), serde_json::to_vec_pretty(&serde_json::json!({
        "privateStation":station.name,"lazyCaptureMs":lazy_ms,"negativeEagerReadMs":negative_ms,
        "negativeEncodingClipboardAvailable":false,"fixedEncodingClipboardAvailable":true,"excludedRenderRequests":0,
        "rawSyncImagePreservedWithTextHistory":true,"distinctReaderWindowsExcludeParallelRead":true,
        "imImageFirstCaptureMs":im_image_ms,"imImageFirstUnrelatedRenderRequests":0,
    })).unwrap()).unwrap();
}

fn install_decode_probe(desktop: isize, available: Arc<AtomicUsize>) {
    DECODE_PROBE.with(|probe| *probe.borrow_mut() = Some(Box::new(move || {
        let available = available.clone();
        thread::spawn(move || {
            private_station::attach(desktop).unwrap();
            if unsafe { OpenClipboard(reader_window().unwrap()) }.is_ok() {
                available.fetch_add(1, Ordering::SeqCst);
                unsafe { let _ = CloseClipboard(); }
            }
        }).join().unwrap();
    })));
}

fn assert_parallel_readers_exclude_each_other(desktop: isize) {
    open_clipboard_retry().unwrap();
    let close = Close::new(None);
    let hwnd = reader_window().unwrap();
    assert_eq!(reader_window().unwrap(), hwnd, "Reader HWND changed between opens");
    assert_eq!(unsafe { windows::Win32::System::DataExchange::GetOpenClipboardWindow() }.unwrap(), hwnd);
    let (observed, receive) = mpsc::sync_channel(1);
    let (resume, released) = mpsc::sync_channel(1);
    let worker = thread::spawn(move || {
        private_station::attach(desktop).unwrap();
        let other = reader_window().unwrap();
        let opened = unsafe { OpenClipboard(other) }.is_ok();
        if opened { unsafe { CloseClipboard().unwrap(); } }
        observed.send((other.0 as isize, opened)).unwrap();
        released.recv_timeout(Duration::from_secs(5)).unwrap();
        open_clipboard_retry().unwrap();
        assert_eq!(reader_window().unwrap(), other);
        unsafe { CloseClipboard().unwrap(); }
    });
    let (other, opened) = receive.recv_timeout(Duration::from_secs(5)).unwrap();
    drop(close);
    resume.send(()).unwrap();
    worker.join().unwrap();
    assert_ne!(other, hwnd.0 as isize, "Different reader threads shared a window");
    assert!(!opened, "Parallel reader entered an already open clipboard scope");
}

struct Owner {
    hwnd: isize,
    rendered: Arc<AtomicUsize>,
    worker: Option<thread::JoinHandle<()>>,
}
struct OwnerState { rendered: Arc<AtomicUsize>, image_with_delayed_text: bool }
impl Owner {
    fn start(desktop: isize, exclude: bool) -> Self {
        Self::start_payload(desktop, exclude, false)
    }
    fn start_image_with_delayed_text(desktop: isize) -> Self {
        Self::start_payload(desktop, false, true)
    }
    fn start_payload(desktop: isize, exclude: bool, image_with_delayed_text: bool) -> Self {
        let rendered = Arc::new(AtomicUsize::new(0));
        let state = OwnerState { rendered: rendered.clone(), image_with_delayed_text };
        let (ready, receive) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || unsafe {
            private_station::attach(desktop).unwrap();
            let state = Box::new(state);
            let class = HSTRING::from(format!("ClipHostDelayedFixture_{}", uuid::Uuid::new_v4().simple()));
            let instance = GetModuleHandleW(None).unwrap();
            let window_class = WNDCLASSW { lpfnWndProc: Some(owner_proc), hInstance: instance.into(), lpszClassName: windows::core::PCWSTR(class.as_ptr()), ..Default::default() };
            assert_ne!(RegisterClassW(&window_class), 0);
            let hwnd = CreateWindowExW(WINDOW_EX_STYLE::default(), &class, w!("owned clipboard fixture"),
                WINDOW_STYLE::default(), 0, 0, 0, 0, HWND_MESSAGE, None, instance, Some((&*state as *const OwnerState).cast())).unwrap();
            let png = image_with_delayed_text.then(|| bgra32_to_png_opaque(&[10, 20, 30, 255], 1, 1).unwrap());
            OpenClipboard(hwnd).unwrap();
            EmptyClipboard().unwrap();
            if let Some(png) = png {
                let format = windows::Win32::System::DataExchange::RegisterClipboardFormatW(&HSTRING::from(FMT_PNG));
                publish(format, &png);
                let html = windows::Win32::System::DataExchange::RegisterClipboardFormatW(&HSTRING::from(FMT_HTML));
                let _ = SetClipboardData(CF_UNICODETEXT, HANDLE::default());
                let _ = SetClipboardData(html, HANDLE::default());
            } else {
                publish(CF_UNICODETEXT, &"owned fixture text".encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect::<Vec<_>>());
                // NULL is the documented delayed-render advertisement (the generated
                // windows Result wrapper sees its NULL return as an error).
                let _ = SetClipboardData(CF_DIB, HANDLE::default());
            }
            if exclude {
                let format = windows::Win32::System::DataExchange::RegisterClipboardFormatW(&HSTRING::from(FMT_EXCLUDE));
                publish(format, &1u32.to_le_bytes());
            }
            CloseClipboard().unwrap();
            ready.send(hwnd.0 as isize).unwrap();
            let mut message = MSG::default();
            while GetMessageW(&mut message, None, 0, 0).0 > 0 { DispatchMessageW(&message); }
            let _ = UnregisterClassW(&class, instance);
        });
        Self { hwnd: receive.recv_timeout(Duration::from_secs(10)).unwrap(), rendered, worker: Some(worker) }
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        unsafe { let _ = PostMessageW(HWND(self.hwnd as *mut _), WM_CLOSE, WPARAM(0), LPARAM(0)); }
        if let Some(worker) = self.worker.take() { worker.join().unwrap(); }
    }
}
unsafe extern "system" fn owner_proc(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if message == WM_NCCREATE {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, (*(l.0 as *const CREATESTRUCTW)).lpCreateParams as isize);
    }
    match message {
        WM_RENDERFORMAT => {
            let state = &*(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const OwnerState);
            state.rendered.fetch_add(1, Ordering::SeqCst);
            thread::sleep(Duration::from_secs(11));
            if state.image_with_delayed_text {
                let bytes = if w.0 as u32 == CF_UNICODETEXT {
                    "unneeded delayed text".encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect::<Vec<_>>()
                } else { b"<p>unneeded delayed HTML</p>\0".to_vec() };
                publish(w.0 as u32, &bytes);
                return LRESULT(0);
            }
            let mut dib = vec![0u8; 44];
            dib[0..4].copy_from_slice(&40u32.to_le_bytes());
            dib[4..8].copy_from_slice(&1i32.to_le_bytes());
            dib[8..12].copy_from_slice(&1i32.to_le_bytes());
            dib[12..14].copy_from_slice(&1u16.to_le_bytes());
            dib[14..16].copy_from_slice(&32u16.to_le_bytes());
            publish(w.0 as u32, &dib);
            LRESULT(0)
        }
        WM_CLOSE => { let _ = DestroyWindow(hwnd); LRESULT(0) }
        WM_DESTROY => { PostQuitMessage(0); LRESULT(0) }
        _ => DefWindowProcW(hwnd, message, w, l),
    }
}
unsafe fn publish(format: u32, bytes: &[u8]) {
    let memory = GlobalAlloc(GMEM_MOVEABLE, bytes.len()).unwrap();
    let pointer = GlobalLock(memory);
    assert!(!pointer.is_null());
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer.cast(), bytes.len());
    let _ = GlobalUnlock(memory);
    SetClipboardData(format, HANDLE(memory.0)).unwrap();
}
