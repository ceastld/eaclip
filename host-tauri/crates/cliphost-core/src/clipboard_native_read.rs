//! Foreign clipboard handles never escape an open scope. Decoding and capture
//! policy run only after CloseClipboard; a slow renderer is still a Windows call
//! and cannot be cancelled by merely timing out a Rust thread.
use super::*;
use windows::Win32::{Foundation::HANDLE, System::DataExchange::{CloseClipboard, GetClipboardData}};

// OpenClipboard associates the lock with this stable, thread-owned HWND. NULL
// cannot distinguish readers in this process and also hides them from passive
// GetOpenClipboardWindow diagnostics. STATIC needs no custom message callback.
struct ReaderWindow(windows::Win32::Foundation::HWND);
impl Drop for ReaderWindow {
    fn drop(&mut self) {
        unsafe { let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(self.0); }
    }
}
thread_local! {
    static READER_WINDOW: std::cell::RefCell<Option<ReaderWindow>> = const { std::cell::RefCell::new(None) };
}

fn reader_window() -> Result<windows::Win32::Foundation::HWND, String> {
    use windows::{core::w, Win32::UI::WindowsAndMessaging::{CreateWindowExW, HWND_MESSAGE, WINDOW_EX_STYLE, WINDOW_STYLE}};
    READER_WINDOW.with(|slot| {
        let mut window = slot.borrow_mut();
        if window.is_none() {
            let hwnd = unsafe { CreateWindowExW(WINDOW_EX_STYLE::default(), w!("STATIC"),
                w!("ClipHost Clipboard Reader"), WINDOW_STYLE::default(), 0, 0, 0, 0,
                HWND_MESSAGE, None, None, None) }
                .map_err(|error| format!("Cannot create clipboard reader: {error}"))?;
            *window = Some(ReaderWindow(hwnd));
        }
        Ok(window.as_ref().unwrap().0)
    })
}

pub(super) fn open() -> Result<(), String> {
    open_clipboard_with_owner(reader_window()?)
}

#[cfg(test)]
fn release_reader_window() {
    READER_WINDOW.with(|slot| { slot.borrow_mut().take(); });
}

struct Close {
    started: Instant,
    owner_pid: Option<u32>,
    format: Option<u32>,
}
impl Close {
    fn new(format: Option<u32>) -> Self {
        Self { started: Instant::now(), owner_pid: unsafe { clipboard_owner_pid_open() }, format }
    }
}
impl Drop for Close {
    fn drop(&mut self) {
        unsafe { let _ = CloseClipboard(); }
        let elapsed = self.started.elapsed();
        if elapsed >= Duration::from_millis(100) {
            tracing::warn!(owner_pid = self.owner_pid.unwrap_or(0),
                requested_format = self.format.unwrap_or(0), held_ms = elapsed.as_millis() as u64,
                "Clipboard read scope returned slowly; timing logged after return, not a live blocked-call sample");
        }
    }
}

struct Session {
    sequence: Option<u32>,
    monitor_only: bool,
    skipped: bool,
    files_read: bool,
    image_read: bool,
    text_read: bool,
}

impl Session {
    fn new(monitor_only: bool, expected: Option<u32>) -> Self {
        Self { sequence: expected, monitor_only, skipped: false, files_read: false, image_read: false, text_read: false }
    }

    // The callback may query formats or copy native memory, never decode, encode,
    // access files, invoke business callbacks, or retain a borrowed native handle.
    fn copy<T>(&mut self, format: Option<u32>, read: impl FnOnce() -> T) -> Result<Option<T>, ClipboardReadFailure> {
        open_clipboard_retry().map_err(|_| self.sequence.map(ClipboardReadFailure::UnavailableAtSequence)
            .unwrap_or(ClipboardReadFailure::Unavailable))?;
        let _close = Close::new(format);
        let sequence = clipboard_sequence();
        let allowed = snapshot_read_allowed(self.sequence.unwrap_or(sequence), sequence,
            self.monitor_only, unsafe { clipboard_owner_pid_open() })?;
        if !allowed { self.skipped = true; return Ok(None); }
        let copied = read();
        // Delayed rendering may advance this same copy's sequence while locked.
        self.sequence = Some(clipboard_sequence());
        Ok(Some(copied))
    }

    fn registered(&mut self, name: &str) -> Result<Option<Vec<u8>>, ClipboardReadFailure> {
        let format = unsafe { windows::Win32::System::DataExchange::RegisterClipboardFormatW(&windows::core::HSTRING::from(name)) };
        if format == 0 { return Ok(None); }
        self.standard(format)
    }

    fn standard(&mut self, format: u32) -> Result<Option<Vec<u8>>, ClipboardReadFailure> {
        self.copy(Some(format), || unsafe {
            windows::Win32::System::DataExchange::IsClipboardFormatAvailable(format).ok()?;
            GetClipboardData(format).ok().and_then(lock_bytes)
        }).map(Option::flatten)
    }

    fn preamble(&mut self) -> Result<ClipboardSnapshot, ClipboardReadFailure> {
        let mut snapshot = ClipboardSnapshot::default();
        snapshot.exclude_monitor = self.copy(None, || format_available(FMT_EXCLUDE))?.unwrap_or(false);
        #[cfg(all(feature = "native-acceptance", not(test)))]
        {
            snapshot.exclude_monitor = !crate::native_acceptance::capture_allowed(
                self.registered("ClipHost.NativeCaptureFixture")?.as_deref());
        }
        // Excluded monitored copies must not request even a delayed content format.
        if self.skipped || (self.monitor_only && snapshot.exclude_monitor) { return Ok(snapshot); }
        snapshot.sync_origin = self.registered(crate::clipboard_sync::ORIGIN_FORMAT)?
            .and_then(|bytes| decode_clipboard_bytes(&bytes));
        Ok(snapshot)
    }

    fn text(&mut self, snapshot: &mut ClipboardSnapshot) -> Result<(), ClipboardReadFailure> {
        if !self.text_read {
            self.text_read = true;
            snapshot.text = self.standard(CF_UNICODETEXT)?.and_then(|bytes| decode_unicode_text(&bytes));
        }
        Ok(())
    }

    fn files(&mut self, snapshot: &mut ClipboardSnapshot) -> Result<(), ClipboardReadFailure> {
        if !self.files_read {
            self.files_read = true;
            snapshot.files = self.standard(CF_HDROP)?.map(|bytes| decode_hdrop_bytes(&bytes)).unwrap_or_default();
        }
        Ok(())
    }

    fn image_once(&mut self) -> Result<(Option<Vec<u8>>, bool, u32), ClipboardReadFailure> {
        let mut png = self.registered(FMT_PNG)?;
        if png.is_none() {
            if let Some(bytes) = self.standard(CF_DIBV5)? {
                let bitmap = dibv5_bitmap_fallback(&bytes, || {
                    self.copy(Some(CF_BITMAP), || unsafe {
                        GetClipboardData(CF_BITMAP).ok().and_then(copy_bitmap_pixels)
                    }).map(Option::flatten)
                })?;
                png = match bitmap {
                    Some(bitmap) => decode_image(bitmap,
                        |(pixels, width, height)| bgra32_to_png_opaque(&pixels, width, height)),
                    None => decode_image(bytes, dibv5_to_png),
                };
            }
        }
        if png.is_none() { png = self.standard(CF_DIB)?.and_then(|bytes| decode_image(bytes, dib_to_png)); }
        if png.is_none() {
            let bitmap = self.copy(Some(CF_BITMAP), || unsafe { GetClipboardData(CF_BITMAP).ok().and_then(copy_bitmap_pixels) })?.flatten();
            png = bitmap.and_then(|bitmap| decode_image(bitmap,
                |(pixels, width, height)| bgra32_to_png_opaque(&pixels, width, height)));
        }
        let has_image = png.is_some() || self.copy(None, image_format_available)?.unwrap_or(false);
        Ok((png, has_image, self.sequence.unwrap_or_default()))
    }

    fn image(&mut self, snapshot: &mut ClipboardSnapshot) -> Result<(), ClipboardReadFailure> {
        if self.image_read { return Ok(()); }
        self.image_read = true;
        let (png, has_image, sequence) = self.image_once()?;
        let initial = ClipboardRead { snapshot: Some(ClipboardSnapshot {
            png, has_image_format: has_image, ..Default::default()
        }), sequence };
        let read = retry_save_clipboard_image_with(initial, |expected| {
            self.sequence = Some(expected);
            self.image_once().map(Some)
        }, thread::sleep)?;
        let image = read.snapshot.unwrap();
        snapshot.png = image.png;
        snapshot.has_image_format = image.has_image_format;
        Ok(())
    }

    fn quicker_action(&mut self, snapshot: &mut ClipboardSnapshot) -> Result<(), ClipboardReadFailure> {
        let mut failure = None;
        snapshot.quicker_action = read_quicker_action(|format| {
            if failure.is_some() { return None; }
            match self.registered(format) {
                Ok(bytes) => bytes,
                Err(error) => { failure = Some(error); None }
            }
        });
        failure.map_or(Ok(()), Err)
    }

    fn bucket(&mut self, bucket: char, snapshot: &mut ClipboardSnapshot) -> Result<(), ClipboardReadFailure> {
        match bucket {
            '1' => self.quicker_action(snapshot)?,
            '2' => snapshot.quicker_steps = self.registered(FMT_QUICKER_STEPS)?.and_then(|bytes| decode_clipboard_bytes(&bytes)),
            '3' => self.files(snapshot)?,
            '4' => {
                self.image(snapshot)?;
                if snapshot.png.as_ref().is_none_or(|bytes| !bytes.starts_with(b"\x89PNG\r\n\x1a\n")) && snapshot.has_image_format {
                    self.files(snapshot)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn text_candidate(&mut self, candidate: TextCaptureCandidate, snapshot: &mut ClipboardSnapshot)
        -> Result<(), ClipboardReadFailure>
    {
        match candidate {
            TextCaptureCandidate::Rich => {
                snapshot.wechat_rich = self.registered(FMT_WECHAT)?.and_then(|bytes| decode_im_clipboard_bytes(&bytes));
                // The factory's existing WeChat.or(QQ) priority also preserves a
                // present-but-empty WeChat payload; do not silently promote QQ.
                if snapshot.wechat_rich.is_none() {
                    snapshot.qq_rich = self.registered(FMT_QQ)?.and_then(|bytes| decode_im_clipboard_bytes(&bytes));
                }
                if snapshot.wechat_rich.as_ref().or(snapshot.qq_rich.as_ref()).is_some_and(|rich| !rich.trim().is_empty()) {
                    self.text(snapshot)?;
                }
            }
            TextCaptureCandidate::Html => {
                snapshot.html = self.registered(FMT_HTML)?.and_then(|bytes| decode_cf_html_bytes(&bytes));
                if snapshot.html.is_some() {
                    self.text(snapshot)?;
                    if snapshot.text.as_ref().is_some_and(|text| within_utf16_limit(text, MAX_TEXT_UTF16) && !text.is_empty()) {
                        snapshot.chromium_source_url = self.registered(FMT_CHROMIUM_SOURCE)?
                            .and_then(|bytes| decode_cf_html_bytes(&bytes))
                            .and_then(|text| sanitize_chromium_source_url(Some(&text)));
                    }
                }
            }
            TextCaptureCandidate::Text => self.text(snapshot)?,
        }
        Ok(())
    }

    fn sync_snapshot(&mut self, snapshot: &mut ClipboardSnapshot) -> Result<(), ClipboardReadFailure> {
        // Presence suffices to exclude non-text/image payloads from sync. Never
        // force private Quicker renderers when history collection is disabled.
        let (action, steps) = self.copy(None, || (
            [FMT_QUICKER_ITEM_JSON, FMT_QUICKER_DRAG, FMT_QUICKER_ITEM].into_iter().any(format_available),
            format_available(FMT_QUICKER_STEPS)))?.unwrap_or_default();
        if action && snapshot.quicker_action.is_none() { snapshot.quicker_action = Some(String::new()); }
        if steps && snapshot.quicker_steps.is_none() { snapshot.quicker_steps = Some(String::new()); }
        if action || steps { return Ok(()); }
        self.files(snapshot)?;
        if !snapshot.files.is_empty() { return Ok(()); }
        self.image(snapshot)?;
        if snapshot.png.is_none() { self.text(snapshot)?; }
        Ok(())
    }
}

pub(super) fn snapshot(monitor_only: bool, expected: Option<u32>) -> Result<ClipboardRead, ClipboardReadFailure> {
    let mut session = Session::new(monitor_only, expected);
    let mut snapshot = session.preamble()?;
    if !session.skipped && !(monitor_only && snapshot.exclude_monitor) {
        snapshot.wechat_rich = session.registered(FMT_WECHAT)?.and_then(|bytes| decode_im_clipboard_bytes(&bytes));
        snapshot.qq_rich = session.registered(FMT_QQ)?.and_then(|bytes| decode_im_clipboard_bytes(&bytes));
        snapshot.html = session.registered(FMT_HTML)?.and_then(|bytes| decode_cf_html_bytes(&bytes));
        snapshot.chromium_source_url = session.registered(FMT_CHROMIUM_SOURCE)?
            .and_then(|bytes| decode_cf_html_bytes(&bytes)).and_then(|text| sanitize_chromium_source_url(Some(&text)));
        session.text(&mut snapshot)?;
        session.quicker_action(&mut snapshot)?;
        session.bucket('2', &mut snapshot)?;
        session.files(&mut snapshot)?;
        session.image(&mut snapshot)?;
    }
    Ok(ClipboardRead { snapshot: (!session.skipped).then_some(snapshot), sequence: session.sequence.unwrap_or_default() })
}

pub(super) fn text_bytes() -> Result<Option<Vec<u8>>, ClipboardReadFailure> {
    Session::new(false, None).standard(CF_UNICODETEXT)
}

pub(super) fn lan_paste() -> Result<ClipboardSnapshot, ClipboardReadFailure> {
    let mut session = Session::new(false, None);
    let mut snapshot = ClipboardSnapshot::default();
    session.files(&mut snapshot)?;
    if snapshot.files.is_empty() { session.image(&mut snapshot)?; }
    session.text(&mut snapshot)?;
    Ok(snapshot)
}

pub(super) fn capture(
    blobs: &Path, source_process: &str, ignore_white_space: bool, collect_quicker: bool,
    text_process_expr: &str, transform: &dyn Fn(&str, &str) -> Option<String>,
    monitored_copy: Option<&PendingMonitoredCopy>, observer: Option<&CaptureHub>, collect_history: bool,
) -> Result<(u32, Option<ClipRecord>), ClipboardReadFailure> {
    let mut session = Session::new(monitored_copy.is_some(), monitored_copy.map(|copy| copy.sequence));
    let mut snapshot = session.preamble()?;
    if session.skipped { return Ok((session.sequence.unwrap_or_default(), None)); }
    if crate::device_sync::origin_needs_validation(&snapshot) {
        // Only our new content-bound marker needs this bounded sync payload
        // check. No HTML, IM rich text, files' contents or OLE formats are read.
        session.sync_snapshot(&mut snapshot)?;
        crate::device_sync::validate_snapshot_origin(&mut snapshot);
    }
    if snapshot.exclude_monitor || (monitored_copy.is_some() && snapshot.sync_origin.is_some()) {
        if let Some(observer) = observer { observer.notify_raw_snapshot(session.sequence.unwrap_or_default(), &snapshot); }
        return Ok((session.sequence.unwrap_or_default(), None));
    }
    let item = if collect_history {
        capture_in_order(source_process, collect_quicker, |bucket| {
            if bucket == '0' {
                return capture_text_in_order(|candidate| {
                    session.text_candidate(candidate, &mut snapshot)?;
                    Ok::<_, ClipboardReadFailure>(try_capture_text_candidate(&snapshot, blobs, source_process,
                        ignore_white_space, text_process_expr, candidate, transform))
                });
            }
            session.bucket(bucket, &mut snapshot)?;
            Ok::<_, ClipboardReadFailure>(try_capture_bucket(&snapshot, blobs, source_process,
                ignore_white_space, text_process_expr, bucket, transform))
        })?
    } else { None };
    if observer.is_some_and(|hub| hub.raw_enabled.load(Ordering::Relaxed)) {
        session.sync_snapshot(&mut snapshot)?;
    }
    if let Some(observer) = observer { observer.notify_raw_snapshot(session.sequence.unwrap_or_default(), &snapshot); }
    Ok((session.sequence.unwrap_or_default(), item))
}

// DragQueryFile handles both ANSI and Unicode DROPFILES. Give it our own copied
// allocation after closing the clipboard instead of retaining the foreign HDROP.
fn decode_hdrop_bytes(bytes: &[u8]) -> Vec<String> {
    use windows::Win32::{Foundation::{GlobalFree, HGLOBAL}, System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE}, UI::Shell::HDROP};
    unsafe {
        let Ok(memory) = GlobalAlloc(GMEM_MOVEABLE, bytes.len()) else { return Vec::new(); };
        struct Free(HGLOBAL);
        impl Drop for Free { fn drop(&mut self) { unsafe { let _ = GlobalFree(self.0); } } }
        let _free = Free(memory);
        let destination = GlobalLock(memory);
        if destination.is_null() { return Vec::new(); }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), destination.cast(), bytes.len());
        let _ = GlobalUnlock(memory);
        read_hdrop_handle(HDROP(memory.0))
    }
}

fn copy_bitmap_pixels(handle: HANDLE) -> Option<(Vec<u8>, u32, u32)> {
    copy_bitmap_pixels_open(handle)
}

fn decode_image<T>(owned: T, decode: impl FnOnce(T) -> Option<Vec<u8>>) -> Option<Vec<u8>> {
    #[cfg(test)]
    tests::before_decode();
    decode(owned)
}

#[cfg(test)]
#[path = "clipboard_native_read_tests.rs"]
mod tests;
