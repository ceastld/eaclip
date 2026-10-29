use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::Value;

use crate::cf_html::{
    try_extract_fragment_and_source_url, try_parse_source_url,
};
use crate::dto::clip_changed_delta;
use crate::model::{hash_files, hash_text, ClipKind, ClipRecord};
use crate::store::AppStore;

#[path = "image_identity_preparation.rs"]
pub(crate) mod image_identity_preparation;

#[cfg(windows)]
#[path = "clipboard_native_read.rs"]
mod native_read;

#[path = "clipboard_monitor_wait.rs"]
mod monitor_wait;

#[cfg(test)]
#[path = "clipboard_read_safety_tests.rs"]
mod read_safety_tests;

#[cfg(test)]
#[path = "clipboard_suppression_tests.rs"]
mod suppression_tests;

#[cfg(test)]
#[path = "clipboard_dib_tests.rs"]
mod dib_tests;

/// C# `ClipImageStorageRoots` under a data root (`Image` library + `Image/Temp` captures).
#[derive(Debug, Clone)]
pub struct ClipImageDirs {
    pub library: PathBuf,
    pub temp: PathBuf,
}

impl ClipImageDirs {
    pub fn under(data_root: impl AsRef<Path>) -> Self {
        let library = data_root.as_ref().join("Image");
        Self {
            temp: library.join("Temp"),
            library,
        }
    }
}

pub const CLIPBOARD_CHANGE_DEBOUNCE: Duration = Duration::from_millis(180);
pub const HANDLE_SUPPRESS_DURATION: Duration = Duration::from_millis(1000);
/// C# `ClipboardHelper.SaveClipboardImage` retry loop.
const SAVE_CLIPBOARD_IMAGE_ATTEMPTS: u32 = 3;
const SAVE_CLIPBOARD_IMAGE_RETRY_MS: u64 = 100;
const MONITORED_CAPTURE_SAVE_ATTEMPTS: u32 = 3;
const MONITORED_CAPTURE_SAVE_RETRY_DELAY: Duration = Duration::from_millis(250);
const MAX_TEXT_UTF16: usize = 100 * 1024;

const FMT_HTML: &str = "HTML Format";
const FMT_EXCLUDE: &str = "ExcludeClipboardContentFromMonitorProcessing";
const FMT_QQ: &str = "QQ_Unicode_RichEdit_Format";
const FMT_WECHAT: &str = "WeChat_RichEdit_Format";
const FMT_QUICKER_STEPS: &str = "quicker-action-steps";
const FMT_QUICKER_ITEM: &str = "quicker-action-item";
const FMT_QUICKER_ITEM_JSON: &str = "quicker-action-item-json";
const FMT_QUICKER_DRAG: &str = "quicker-action-drag-item";
const FMT_CHROMIUM_SOURCE: &str = "Chromium internal source URL";
const FMT_PNG: &str = "PNG";
const CF_BITMAP: u32 = 2;
const CF_DIB: u32 = 8;
const CF_DIBV5: u32 = 17;
const CF_UNICODETEXT: u32 = 13;
const CF_HDROP: u32 = 15;
const BI_RGB: u32 = 0;
const BI_BITFIELDS: u32 = 3;
const BITMAPINFOHEADER_SIZE: usize = 40;
const BITMAPV5HEADER_SIZE: usize = 124;

pub type NewItemNotification = Box<dyn FnOnce() + Send>;
type NewItemNotificationFactory = Arc<dyn Fn() -> NewItemNotification + Send + Sync>;

#[derive(Clone)]
pub struct CaptureHub {
    suppress_until: Arc<Mutex<Instant>>,
    events: Arc<Mutex<Vec<(String, Value)>>>,
    event_wakeup: Arc<tokio::sync::Notify>,
    on_new_item: Arc<Mutex<Option<NewItemNotificationFactory>>>,
    on_captured: Arc<Mutex<Option<Arc<dyn Fn(ClipRecord) + Send + Sync>>>>,
    on_raw_snapshot: Arc<Mutex<Option<Arc<dyn Fn(u32, &ClipboardSnapshot) + Send + Sync>>>>,
    raw_enabled: Arc<AtomicBool>,
    on_error: Arc<Mutex<Option<Arc<dyn Fn() + Send + Sync>>>>,
}

impl Default for CaptureHub {
    fn default() -> Self {
        Self {
            suppress_until: Arc::new(unsuppressed_slot(Instant::now())),
            events: Arc::new(Mutex::new(Vec::new())),
            event_wakeup: Arc::new(tokio::sync::Notify::new()),
            on_new_item: Arc::new(Mutex::new(None)),
            on_captured: Arc::new(Mutex::new(None)),
            on_raw_snapshot: Arc::new(Mutex::new(None)),
            raw_enabled: Arc::new(AtomicBool::new(false)),
            on_error: Arc::new(Mutex::new(None)),
        }
    }
}

fn unsuppressed_slot(now: Instant) -> Mutex<Instant> {
    // Equality is already unsuppressed; backdating can underflow just after boot.
    Mutex::new(now)
}

impl CaptureHub {
    #[cfg(test)]
    pub(crate) fn with_events_held_for_test<T>(&self, work: impl FnOnce() -> T) -> T {
        let _events = self.events.lock();
        work()
    }
    pub fn suppress(&self, duration: Duration) {
        *self.suppress_until.lock() = Instant::now() + duration;
    }

    pub fn is_suppressed(&self) -> bool {
        Instant::now() < *self.suppress_until.lock()
    }

    pub fn push(&self, event_name: impl Into<String>, data: Value) {
        if crate::hooks::is_input_callback() {
            self.try_push(event_name, data);
            return;
        }
        self.events.lock().push((event_name.into(), data));
        self.event_wakeup.notify_one();
    }

    pub(crate) fn try_push(&self, event_name: impl Into<String>, data: Value) -> bool {
        let Some(mut events) = self.events.try_lock() else { return false; };
        events.push((event_name.into(), data));
        drop(events);
        self.event_wakeup.notify_one();
        true
    }

    /// One event pump owns this notifier. A queued permit also covers events
    /// published while that pump is flushing or has not started waiting yet.
    pub fn event_notifier(&self) -> Arc<tokio::sync::Notify> {
        self.event_wakeup.clone()
    }

    pub fn drain(&self) -> Vec<(String, Value)> {
        self.events.lock().drain(..).collect()
    }

    pub fn set_on_new_item(&self, cb: impl Fn() + Send + Sync + 'static) {
        let cb = Arc::new(cb);
        self.set_on_new_item_factory(move || {
            let cb = cb.clone();
            Box::new(move || cb())
        });
    }

    pub fn set_on_new_item_factory(&self, factory: impl Fn() -> NewItemNotification + Send + Sync + 'static) {
        *self.on_new_item.lock() = Some(Arc::new(factory));
    }

    pub fn prepare_new_item_notification(&self) -> Option<NewItemNotification> {
        let factory = self.on_new_item.lock().clone();
        factory.map(|factory| factory())
    }

    pub fn notify_new_item(&self) {
        let cb = self.prepare_new_item_notification();
        if let Some(cb) = cb {
            cb();
        }
    }

    pub fn set_on_captured(&self, cb: impl Fn(ClipRecord) + Send + Sync + 'static) {
        *self.on_captured.lock() = Some(Arc::new(cb));
    }

    pub fn notify_captured(&self, item: ClipRecord) {
        let cb = self.on_captured.lock().clone();
        if let Some(cb) = cb {
            cb(item);
        }
    }

    pub(crate) fn set_raw_enabled(&self, enabled: bool) {
        self.raw_enabled.store(enabled, Ordering::Relaxed);
    }

    pub(crate) fn set_on_raw_snapshot(&self, cb: impl Fn(u32, &ClipboardSnapshot) + Send + Sync + 'static) {
        *self.on_raw_snapshot.lock() = Some(Arc::new(cb));
    }

    pub(crate) fn notify_raw_snapshot(&self, sequence: u32, snapshot: &ClipboardSnapshot) {
        let cb = self.on_raw_snapshot.lock().clone();
        if let Some(cb) = cb { cb(sequence, snapshot); }
    }

    pub fn set_on_error(&self, cb: impl Fn() + Send + Sync + 'static) {
        *self.on_error.lock() = Some(Arc::new(cb));
    }

    fn notify_error(&self) {
        let cb = self.on_error.lock().clone();
        if let Some(cb) = cb {
            cb();
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ClipboardSnapshot {
    pub exclude_monitor: bool,
    /// Private remote-event identity, independent of elapsed time or content equality.
    pub sync_origin: Option<String>,
    pub qq_rich: Option<String>,
    pub wechat_rich: Option<String>,
    pub html: Option<String>,
    pub chromium_source_url: Option<String>,
    pub text: Option<String>,
    pub quicker_action: Option<String>,
    pub quicker_steps: Option<String>,
    pub files: Vec<String>,
    pub png: Option<Vec<u8>>,
    /// C# `DataFormats.Bitmap` present (including auto-converted DIB/PNG).
    pub has_image_format: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClipboardReadFailure {
    Unavailable,
    // A later image reopen was busy after the first held read completed.
    // Rendering may have advanced this same copy's sequence in that read.
    UnavailableAtSequence(u32),
    Changed,
}

struct ClipboardRead {
    snapshot: Option<ClipboardSnapshot>,
    // Taken while the clipboard is still locked, after delayed rendering.
    sequence: u32,
}

struct PendingMonitoredCopy {
    sequence: u32,
    source_process: String,
    notification: Option<NewItemNotification>,
}

impl PendingMonitoredCopy {
    fn observe(
        pending: &mut Option<Self>,
        sequence: u32,
        hub: &CaptureHub,
        read_source: impl FnOnce() -> String,
    ) {
        if pending.as_ref().is_some_and(|copy| copy.sequence == sequence) { return; }
        let notification = hub.prepare_new_item_notification();
        *pending = Some(Self {
            sequence,
            source_process: crate::file_icon::normalize_exe_key(&read_source()),
            notification,
        });
    }

    fn preserve_rendered_sequence(pending: &mut Option<Self>, error: &ClipboardReadFailure) {
        if let ClipboardReadFailure::UnavailableAtSequence(sequence) = error {
            if let Some(copy) = pending { copy.sequence = *sequence; }
        }
    }
}

pub struct ClipboardMonitor {
    stop: Arc<monitor_wait::StopSignal>,
}

impl ClipboardMonitor {
    pub(crate) fn start(
        store: Arc<Mutex<AppStore>>,
        data_root: PathBuf,
        allow_dup: Arc<AtomicBool>,
        hub: CaptureHub,
        blacklist: Arc<Mutex<String>>,
        collect_quicker: Arc<AtomicBool>,
        ignore_ws: Arc<AtomicBool>,
        monitor_on: Arc<AtomicBool>,
        deferred_hidden: Arc<Mutex<HashSet<String>>>,
        text_process_expr: Arc<parking_lot::Mutex<String>>,
        transform: Arc<dyn Fn(&str, &str) -> Option<String> + Send + Sync>,
    ) -> Self {
        let stop = Arc::new(monitor_wait::StopSignal::new());
        let stop_flag = stop.clone();
        thread::Builder::new()
            .name("cliphost-clipboard".into())
            .spawn(move || {
                run_monitor(
                    stop_flag,
                    store,
                    data_root,
                    allow_dup,
                    hub,
                    blacklist,
                    collect_quicker,
                    ignore_ws,
                    monitor_on,
                    deferred_hidden,
                    text_process_expr,
                    transform,
                )
            })
            .ok();
        Self { stop }
    }
}

impl Drop for ClipboardMonitor {
    fn drop(&mut self) {
        self.stop.signal();
    }
}

use std::sync::atomic::AtomicBool as StdAtomicBool;

static FORCE_HEADLESS: StdAtomicBool = StdAtomicBool::new(false);

pub fn force_headless(value: bool) {
    FORCE_HEADLESS.store(value, Ordering::SeqCst);
}

pub fn is_headless() -> bool {
    if FORCE_HEADLESS.load(Ordering::SeqCst) {
        return true;
    }
    match std::env::var("CLIPHOST_TAURI_HEADLESS") {
        Ok(value) => !matches!(value.as_str(), "0" | "false" | "False"),
        Err(_) => false,
    }
}

pub fn native_dialogs_enabled() -> bool {
    !is_headless() && matches!(std::env::var("CLIPHOST_TAURI_ALLOW_DIALOGS").as_deref(), Ok("1"))
}

#[cfg(test)]
thread_local! {
    static TEST_SNAPSHOT: std::cell::RefCell<Option<ClipboardSnapshot>> = std::cell::RefCell::new(None);
    static TEST_READ_UNAVAILABLE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Read the current clipboard snapshot without inserting a history item.
pub fn peek_snapshot() -> Option<ClipboardSnapshot> {
    #[cfg(test)]
    {
        if let Some(snap) = TEST_SNAPSHOT.with(|cell| cell.borrow().clone()) {
            return Some(snap);
        }
    }
    if is_headless() {
        return None;
    }
    read_snapshot()
}

/// Explicit LAN paste reads only files, bitmap and Unicode text, on its worker.
/// Private application/rich formats are deliberately not requested.
pub fn read_lan_paste() -> Result<ClipboardSnapshot, String> {
    #[cfg(test)]
    if let Some(snapshot) = TEST_SNAPSHOT.with(|cell| cell.borrow().clone()) { return Ok(snapshot); }
    if is_headless() { return Ok(ClipboardSnapshot::default()); }
    #[cfg(windows)]
    { native_read::lan_paste().map_err(|error| match error {
        ClipboardReadFailure::Changed => "剪贴板已变化，请重新粘贴。".into(),
        _ => "暂时无法读取剪贴板，请重试。".into(),
    }) }
    #[cfg(not(windows))]
    { Ok(ClipboardSnapshot::default()) }
}

#[cfg(test)]
pub struct TestSnapshotGuard;

#[cfg(test)]
impl Drop for TestSnapshotGuard {
    fn drop(&mut self) {
        TEST_SNAPSHOT.with(|cell| *cell.borrow_mut() = None);
    }
}

#[cfg(test)]
pub fn install_test_snapshot(snap: ClipboardSnapshot) -> TestSnapshotGuard {
    TEST_SNAPSHOT.with(|cell| *cell.borrow_mut() = Some(snap));
    TestSnapshotGuard
}

#[cfg(test)]
pub struct TestReadUnavailableGuard;

#[cfg(test)]
impl Drop for TestReadUnavailableGuard {
    fn drop(&mut self) {
        TEST_READ_UNAVAILABLE.with(|cell| cell.set(false));
    }
}

/// Simulate C# `Clipboard.GetText()` throwing `ExternalException`.
#[cfg(test)]
pub fn force_clipboard_read_unavailable() -> TestReadUnavailableGuard {
    TEST_READ_UNAVAILABLE.with(|cell| cell.set(true));
    TestReadUnavailableGuard
}

/// C# `Clipboard.GetText()`: locked clipboard throws; missing/whitespace text is empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardTextStatus {
    Text(String),
    Empty,
    Unavailable,
}

/// C# `FileMenuCustomPathActions.TryAddFromClipboard` `Clipboard.GetText` catch vs empty.
pub fn read_clipboard_text_status() -> ClipboardTextStatus {
    #[cfg(test)]
    {
        if TEST_READ_UNAVAILABLE.with(|cell| cell.get()) {
            return ClipboardTextStatus::Unavailable;
        }
    }
    if is_headless() {
        return match peek_snapshot()
            .and_then(|snap| snap.text)
            .filter(|text| !text.trim().is_empty())
        {
            Some(text) => ClipboardTextStatus::Text(text),
            None => ClipboardTextStatus::Empty,
        };
    }
    #[cfg(windows)]
    {
        let text = match native_read::text_bytes() {
            Ok(bytes) => bytes.and_then(|bytes| decode_unicode_text(&bytes)),
            Err(_) => return ClipboardTextStatus::Unavailable,
        };
        match text.filter(|s| !s.trim().is_empty()) {
            Some(text) => ClipboardTextStatus::Text(text),
            None => ClipboardTextStatus::Empty,
        }
    }
    #[cfg(not(windows))]
    {
        ClipboardTextStatus::Empty
    }
}

pub fn suppress_capture(duration: Duration) {
    GLOBAL_SUPPRESS.with_lock(|slot| *slot = Instant::now() + duration);
}

fn is_globally_suppressed() -> bool {
    GLOBAL_SUPPRESS.with_lock(|slot| Instant::now() < *slot)
}

struct SuppressSlot;

impl SuppressSlot {
    fn with_lock<R>(&self, f: impl FnOnce(&mut Instant) -> R) -> R {
        static CELL: std::sync::OnceLock<Mutex<Instant>> = std::sync::OnceLock::new();
        let lock = CELL.get_or_init(|| unsuppressed_slot(Instant::now()));
        f(&mut lock.lock())
    }
}

const GLOBAL_SUPPRESS: SuppressSlot = SuppressSlot;

pub fn capture_once(blobs: &Path) -> Option<ClipRecord> {
    capture_once_with(blobs, false, true, "")
}

pub fn capture_once_with(
    blobs: &Path,
    ignore_white_space: bool,
    collect_quicker: bool,
    text_process_expr: &str,
) -> Option<ClipRecord> {
    capture_once_with_transform(blobs, ignore_white_space, collect_quicker, text_process_expr,
        &crate::text_actions::try_transform_clipboard_text)
}

pub(crate) fn capture_once_with_transform(
    blobs: &Path, ignore_white_space: bool, collect_quicker: bool, text_process_expr: &str,
    transform: &dyn Fn(&str, &str) -> Option<String>,
) -> Option<ClipRecord> {
    capture_with_transform(blobs, ignore_white_space, collect_quicker, text_process_expr, transform, None)
        .ok().and_then(|(_, item)| item)
}

fn capture_with_transform(
    blobs: &Path, ignore_white_space: bool, collect_quicker: bool, text_process_expr: &str,
    transform: &dyn Fn(&str, &str) -> Option<String>, monitored_copy: Option<&PendingMonitoredCopy>,
) -> Result<(u32, Option<ClipRecord>), ClipboardReadFailure> {
    capture_with_raw_observer(blobs, ignore_white_space, collect_quicker, text_process_expr,
        transform, monitored_copy, None, true)
}

fn capture_with_raw_observer(
    blobs: &Path, ignore_white_space: bool, collect_quicker: bool, text_process_expr: &str,
    transform: &dyn Fn(&str, &str) -> Option<String>, monitored_copy: Option<&PendingMonitoredCopy>,
    observer: Option<&CaptureHub>, collect_history: bool,
) -> Result<(u32, Option<ClipRecord>), ClipboardReadFailure> {
    // Manual collection freezes its foreground before potentially delayed reads too.
    let source_process = monitored_copy.map(|copy| copy.source_process.clone())
        .unwrap_or_else(|| crate::file_icon::normalize_exe_key(&foreground_process()));
    #[cfg(windows)]
    if !is_headless() {
        return native_read::capture(blobs, &source_process, ignore_white_space, collect_quicker,
            text_process_expr, transform, monitored_copy, observer, collect_history);
    }
    let read = if let Some(copy) = monitored_copy {
        try_read_snapshot_for_capture(true, Some(copy.sequence))?
    } else {
        ClipboardRead { snapshot: peek_snapshot(), sequence: 0 }
    };
    Ok((read.sequence, read.snapshot.and_then(|mut snap| {
        crate::device_sync::validate_snapshot_origin(&mut snap);
        if let Some(observer) = observer { observer.notify_raw_snapshot(read.sequence, &snap); }
        if !collect_history { return None; }
        if monitored_copy.is_some() && snap.sync_origin.is_some() { return None; }
        item_from_snapshot_with_transform(
        snap,
        blobs,
        &source_process,
        ignore_white_space,
        collect_quicker,
        text_process_expr,
        transform,
    )})))
}

pub fn write_text(text: &str) -> Result<(), String> {
    crate::clipboard_payload::prepare_text(text)?.write()
}

pub fn try_read_text() -> Option<String> {
    if is_headless() {
        return None;
    }
    #[cfg(windows)]
    {
        let text = read_unicode_text();
        text.filter(|s| !s.trim().is_empty())
    }
    #[cfg(not(windows))]
    {
        None
    }
}

pub fn first_existing_path_from_text(text: &str) -> Option<String> {
    crate::file_menu::clipboard_looks_like_existing_path(text)
}

/// Text actions preserve whitespace and a fresh empty selection. Read only text,
/// so an accompanying image does not delay selection replacement with decoding.
pub(crate) fn read_selection_text() -> Option<String> {
    #[cfg(test)]
    if TEST_READ_UNAVAILABLE.with(|cell| cell.get()) {
        return None;
    }
    if is_headless() {
        return peek_snapshot().map(|snapshot| snapshot.text.unwrap_or_default());
    }
    #[cfg(windows)]
    {
        native_read::text_bytes().ok().map(|bytes|
            bytes.and_then(|bytes| decode_unicode_text(&bytes)).unwrap_or_default())
    }
    #[cfg(not(windows))]
    {
        None
    }
}

pub fn try_read_existing_path() -> Option<String> {
    try_read_text().and_then(|text| first_existing_path_from_text(&text))
}

pub fn sequence_number() -> u32 {
    clipboard_sequence()
}

/// C# `GetClipboardSequenceNumber` then `>` (not `!=`) in GetSelectedText / GetSelectedFilesAsync.
pub fn clipboard_sequence_advanced(prev: u32, current: u32) -> bool {
    current > prev
}

/// C# `TextActionRunner.GetSelectedText`: after sequence increases, `Clipboard.GetText()`
/// may be empty (`""`) and still succeeds; timeout is `null`. Locked clipboard keeps waiting.
pub fn selected_text_from_status(status: ClipboardTextStatus) -> Option<String> {
    match status {
        ClipboardTextStatus::Text(text) => Some(text),
        ClipboardTextStatus::Empty => Some(String::new()),
        ClipboardTextStatus::Unavailable => None,
    }
}

pub fn wait_for_new_text(prev: u32, timeout_ms: u32) -> Option<String> {
    let start = Instant::now();
    while start.elapsed().as_millis() < timeout_ms as u128 {
        if clipboard_sequence_advanced(prev, sequence_number()) {
            if let Some(text) = selected_text_from_status(read_clipboard_text_status()) {
                return Some(text);
            }
        }
        thread::sleep(Duration::from_millis(10));
    }
    None
}

/// C# `FileMenuItemWrapper.GetSelectedFilesAsync` waits ~50ms for a new FileDrop.
pub fn wait_for_new_files(prev: u32, timeout_ms: u32) -> Option<Vec<String>> {
    let start = Instant::now();
    while start.elapsed().as_millis() < timeout_ms as u128 {
        if clipboard_sequence_advanced(prev, sequence_number()) {
            return Some(peek_snapshot().map(|snap| snap.files).unwrap_or_default());
        }
        thread::sleep(Duration::from_millis(10));
    }
    None
}

pub fn write_files(paths: &[String]) -> Result<(), String> {
    // C# `ClipboardHelper.CopyFiles`: skip missing paths; empty collection is a no-op.
    let existing = copy_files_existing(paths);
    if existing.is_empty() {
        return Ok(());
    }
    if is_headless() {
        return Ok(());
    }
    crate::clipboard_payload::prepare_text_and_files("", &existing)?.write()
}

/// C# `ClipboardHelper.CopyFiles` only adds paths that `File.Exists || Directory.Exists`.
pub(crate) fn copy_files_existing(paths: &[String]) -> Vec<String> {
    paths
        .iter()
        .filter(|p| {
            let path = Path::new(p.as_str());
            path.is_file() || path.is_dir()
        })
        .cloned()
        .collect()
}

/// C# `ClipItemFactory.CreateMultiItemsDataObject` writes joined text plus FileDrop.
pub fn write_text_and_files(text: &str, paths: &[String]) -> Result<(), String> {
    crate::clipboard_payload::prepare_text_and_files(text, paths)?.write()
}

pub fn write_record(
    item: &ClipRecord,
    plain_only: bool,
    paste_with_new_line: bool,
    standard_paste: bool,
) -> Result<(), String> {
    crate::clipboard_payload::prepare_record(item, plain_only, paste_with_new_line, standard_paste)?.write()
}

pub fn foreground_target() -> (String, String) {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
        process_name_and_class(GetForegroundWindow())
    }
    #[cfg(not(windows))]
    {
        (String::new(), String::new())
    }
}

const WS_EX_NOACTIVATE: u32 = 0x0800_0000;

/// C# `ActiveWindowService.IsExplorerShellChromeRoot`.
pub(crate) fn is_explorer_shell_chrome_root(process_name: &str, class_name: &str) -> bool {
    let key = crate::file_icon::normalize_exe_key(process_name);
    if !key.eq_ignore_ascii_case("explorer.exe") || class_name.is_empty() {
        return false;
    }
    class_name == "Shell_TrayWnd"
        || class_name == "Shell_SecondaryTrayWnd"
        || class_name == "MSTaskSwWClass"
        || class_name.contains("MSTaskList")
        || class_name == "TaskListThumbnailWnd"
        || class_name == "TaskSwitcherWnd"
}

/// C# `ActiveWindowService.TryGetForegroundProcessName` for clip-list process filter.
/// Empty when the hint HWND is `WS_EX_NOACTIVATE`.
pub fn foreground_process_name_for_list_filter() -> String {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowLongW, GWL_EXSTYLE};
        let hint = GetForegroundWindow();
        if hint.0 as isize == 0 {
            return String::new();
        }
        let ex = GetWindowLongW(hint, GWL_EXSTYLE) as u32;
        if ex & WS_EX_NOACTIVATE != 0 {
            return String::new();
        }
        let hwnd = resolve_foreground_root_window(hint);
        crate::file_icon::normalize_exe_key(&process_name_and_class(hwnd).0)
    }
    #[cfg(not(windows))]
    String::new()
}

#[cfg(windows)]
unsafe fn hwnd_no_activate(hwnd: windows::Win32::Foundation::HWND) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowLongW, GWL_EXSTYLE};
    (GetWindowLongW(hwnd, GWL_EXSTYLE) as u32) & WS_EX_NOACTIVATE != 0
}

/// C# `ActiveWindowService.ResolveForegroundRootWindow` (EnumWindows stands in for `activeWindows`).
#[cfg(windows)]
unsafe fn resolve_foreground_root_window(
    hint: windows::Win32::Foundation::HWND,
) -> windows::Win32::Foundation::HWND {
    use windows::Win32::UI::WindowsAndMessaging::{GetAncestor, GetForegroundWindow, GA_ROOT};
    let h = if hint.0 as isize == 0 {
        GetForegroundWindow()
    } else {
        hint
    };
    if hwnd_no_activate(h) {
        return h;
    }
    let root = GetAncestor(h, GA_ROOT);
    let root = if root.0 as isize == 0 { h } else { root };
    let (name, class) = process_name_and_class(root);
    if !is_explorer_shell_chrome_root(&name, &class) {
        return root;
    }
    let event_hwnd = crate::active_window::last_foreground_event_hwnd();
    let history = crate::active_window::foreground_event_history();
    let try_root = |hwnd: isize| unsafe {
        try_non_shell_visible_root(windows::Win32::Foundation::HWND(hwnd as *mut _)).map(|h| h.0 as isize)
    };
    if let Some(resolved) = crate::active_window::resolve_shell_chrome_fallback(
        event_hwnd,
        history,
        try_root,
        None,
    ) {
        return windows::Win32::Foundation::HWND(resolved as *mut _);
    }
    enum_first_non_shell_visible_root().unwrap_or(root)
}

/// C# `ActiveWindowService.Hook_ForegroundWindowChanged`: skip `WS_EX_NOACTIVATE`,
/// then `ResolveForegroundRootWindow` (WinEvent history when Explorer shell chrome).
pub fn resolved_foreground_root_hwnd() -> Option<isize> {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
        let hint = GetForegroundWindow();
        if hint.0 as isize == 0 || hwnd_no_activate(hint) {
            return None;
        }
        let root = resolve_foreground_root_window(hint);
        if root.0 as isize == 0 {
            None
        } else {
            Some(root.0 as isize)
        }
    }
    #[cfg(not(windows))]
    None
}

#[cfg(windows)]
unsafe fn try_non_shell_visible_root(
    any: windows::Win32::Foundation::HWND,
) -> Option<windows::Win32::Foundation::HWND> {
    use windows::Win32::UI::WindowsAndMessaging::{GetAncestor, IsWindowVisible, GA_ROOT};
    if any.0 as isize == 0 || hwnd_no_activate(any) || !IsWindowVisible(any).as_bool() {
        return None;
    }
    let root = GetAncestor(any, GA_ROOT);
    let root = if root.0 as isize == 0 { any } else { root };
    if !IsWindowVisible(root).as_bool() {
        return None;
    }
    let (name, class) = process_name_and_class(root);
    if is_explorer_shell_chrome_root(&name, &class) {
        return None;
    }
    Some(root)
}

#[cfg(windows)]
struct EnumFgState {
    self_pid: u32,
    found: windows::Win32::Foundation::HWND,
}

#[cfg(windows)]
unsafe extern "system" fn enum_fg_proc(
    hwnd: windows::Win32::Foundation::HWND,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::BOOL {
    use windows::Win32::Foundation::BOOL;
    use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
    let state = &mut *(lparam.0 as *mut EnumFgState);
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == 0 || pid == state.self_pid {
        return BOOL(1);
    }
    if let Some(root) = try_non_shell_visible_root(hwnd) {
        state.found = root;
        return BOOL(0);
    }
    BOOL(1)
}

#[cfg(windows)]
unsafe fn enum_first_non_shell_visible_root() -> Option<windows::Win32::Foundation::HWND> {
    use windows::Win32::Foundation::{HWND, LPARAM};
    use windows::Win32::System::Threading::GetCurrentProcessId;
    use windows::Win32::UI::WindowsAndMessaging::EnumWindows;
    let mut state = EnumFgState {
        self_pid: GetCurrentProcessId(),
        found: HWND(std::ptr::null_mut()),
    };
    let _ = EnumWindows(Some(enum_fg_proc), LPARAM(&mut state as *mut EnumFgState as isize));
    if state.found.0 as isize == 0 {
        None
    } else {
        Some(state.found)
    }
}

#[cfg(windows)]
fn process_name_and_class(hwnd: windows::Win32::Foundation::HWND) -> (String, String) {
    unsafe {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::{
            OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        use windows::Win32::UI::WindowsAndMessaging::{GetClassNameW, GetWindowThreadProcessId};
        let mut class_buf = [0u16; 256];
        let class_n = GetClassNameW(hwnd, &mut class_buf);
        let class_name = String::from_utf16_lossy(&class_buf[..class_n as usize]);
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return (String::new(), class_name);
        }
        let proc = match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(p) => p,
            Err(_) => return (String::new(), class_name),
        };
        let mut buf = vec![0u16; 520];
        let mut size = buf.len() as u32;
        let name = if QueryFullProcessImageNameW(
            proc,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut size,
        )
        .is_ok()
        {
            String::from_utf16_lossy(&buf[..size as usize])
                .rsplit(['\\', '/'])
                .next()
                .unwrap_or_default()
                .to_string()
        } else {
            String::new()
        };
        let _ = CloseHandle(proc);
        (name, class_name)
    }
}

/// C# `ClipItemFactory.CreateFromClipboard` default order `01234`.
pub fn item_from_snapshot(snap: ClipboardSnapshot, blobs: &Path, source_process: &str) -> Option<ClipRecord> {
    item_from_snapshot_with(snap, blobs, source_process, false)
}

pub fn item_from_snapshot_with(
    snap: ClipboardSnapshot,
    blobs: &Path,
    source_process: &str,
    ignore_white_space: bool,
) -> Option<ClipRecord> {
    item_from_snapshot_ordered(snap, blobs, source_process, ignore_white_space, true, "")
}

pub fn item_from_snapshot_ordered(
    snap: ClipboardSnapshot,
    blobs: &Path,
    source_process: &str,
    ignore_white_space: bool,
    collect_quicker: bool,
    text_process_expr: &str,
) -> Option<ClipRecord> {
    item_from_snapshot_with_transform(snap, blobs, source_process, ignore_white_space,
        collect_quicker, text_process_expr, &crate::text_actions::try_transform_clipboard_text)
}

pub(crate) fn item_from_snapshot_with_transform(
    snap: ClipboardSnapshot, blobs: &Path, source_process: &str,
    ignore_white_space: bool, collect_quicker: bool, text_process_expr: &str,
    transform: &dyn Fn(&str, &str) -> Option<String>,
) -> Option<ClipRecord> {
    if snap.exclude_monitor {
        return None;
    }
    // C# `ClipSourceProcessNameNormalizer.Normalize` before persist.
    let source_process = crate::file_icon::normalize_exe_key(source_process);
    for bucket in capture_order(&source_process, collect_quicker).chars() {
        if let Some(item) = try_capture_bucket(
            &snap,
            blobs,
            &source_process,
            ignore_white_space,
            text_process_expr,
            bucket,
            transform,
        ) {
            return Some(item);
        }
    }
    None
}

fn capture_order(source_process: &str, collect_quicker: bool) -> String {
    let stem = strip_exe(source_process).to_ascii_lowercase();
    let mut order = if is_im_process(&stem) {
        "40123"
    } else if is_browser_process(&stem) {
        "43012"
    } else {
        "01234"
    }
    .to_string();
    if !collect_quicker {
        order = order.replace('2', "");
    }
    order
}

/// Reads only the next eligible capture bucket. The reader closes its native
/// scope before attempting conversion/preprocessing, so a completed earlier
/// result never requests unrelated delayed-rendered clipboard formats.
fn capture_in_order<T, E>(source_process: &str, collect_quicker: bool,
    mut read_and_accept: impl FnMut(char) -> Result<Option<T>, E>) -> Result<Option<T>, E>
{
    for bucket in capture_order(source_process, collect_quicker).chars() {
        if let Some(item) = read_and_accept(bucket)? { return Ok(Some(item)); }
    }
    Ok(None)
}

fn monitored_kind_allowed(kind: ClipKind, collect_quicker_steps: bool) -> bool {
    collect_quicker_steps || kind != ClipKind::StepClipItem
}

fn is_im_process(stem: &str) -> bool {
    matches!(stem, "qq" | "wechat" | "weixin" | "tim" | "wxwork")
}

pub(crate) fn is_im_process_name(name: &str) -> bool {
    is_im_process(&strip_exe(name).to_ascii_lowercase())
}

fn is_browser_process(stem: &str) -> bool {
    matches!(
        stem,
        "chrome" | "msedge" | "firefox" | "vivaldi" | "360chromex" | "360chrome" | "360se"
    )
}

pub(crate) fn try_capture_bucket(
    snap: &ClipboardSnapshot,
    blobs: &Path,
    source_process: &str,
    ignore_white_space: bool,
    text_process_expr: &str,
    bucket: char,
    transform: &dyn Fn(&str, &str) -> Option<String>,
) -> Option<ClipRecord> {
    match bucket {
        '0' => capture_text_in_order::<std::convert::Infallible>(|candidate| {
            Ok(try_capture_text_candidate(snap, blobs, source_process, ignore_white_space,
                text_process_expr, candidate, transform))
        }).unwrap(),
        '1' => snap.quicker_action.as_ref().filter(|s| !s.is_empty()).and_then(|action| {
            let mut item = ClipRecord::new_text(action.clone(), source_process);
            item.kind = ClipKind::ActionClipItem;
            after_csharp_preprocess(item, text_process_expr, ignore_white_space, transform)
        }),
        '2' => snap.quicker_steps.as_ref().filter(|s| !s.is_empty()).and_then(|steps| {
            let mut item = ClipRecord::new_text(steps.clone(), source_process);
            item.kind = ClipKind::StepClipItem;
            after_csharp_preprocess(item, text_process_expr, ignore_white_space, transform)
        }),
        '3' if !snap.files.is_empty() => {
            let mut item = ClipRecord::new_text(snap.files.join("\n"), source_process);
            item.kind = ClipKind::FileClipItem;
            item.file_paths = snap.files.clone();
            item.content_hash = hash_files(&snap.files);
            match crate::volatile_images::snapshot_volatile_image(&item, blobs) {
                Ok(Some(snapshot)) => item = snapshot,
                Ok(None) => {},
                Err(reason) => tracing::warn!(reason, "Temporary clipboard image snapshot failed; keeping file reference"),
            }
            after_csharp_preprocess(item, text_process_expr, ignore_white_space, transform)
        }
        '4' => snap
            .png
            .as_ref()
            .and_then(|png| {
                if png.len() > 8 && &png[..8] == b"\x89PNG\r\n\x1a\n" {
                    save_png(blobs, png.clone(), source_process)
                } else {
                    None
                }
            })
            .or_else(|| save_clipboard_helper_filedrop_image(snap, blobs, source_process))
            .and_then(|item| after_csharp_preprocess(item, text_process_expr, ignore_white_space, transform)),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TextCaptureCandidate { Rich, Html, Text }

fn capture_text_in_order<E>(mut read_and_accept: impl FnMut(TextCaptureCandidate) -> Result<Option<ClipRecord>, E>)
    -> Result<Option<ClipRecord>, E>
{
    for candidate in [TextCaptureCandidate::Rich, TextCaptureCandidate::Html, TextCaptureCandidate::Text] {
        if let Some(item) = read_and_accept(candidate)? { return Ok(Some(item)); }
    }
    Ok(None)
}

fn try_capture_text_candidate(
    snap: &ClipboardSnapshot, blobs: &Path, source_process: &str, ignore_white_space: bool,
    text_process_expr: &str, candidate: TextCaptureCandidate,
    transform: &dyn Fn(&str, &str) -> Option<String>,
) -> Option<ClipRecord> {
    let item = match candidate {
        TextCaptureCandidate::Rich => {
            let rich = snap.wechat_rich.as_ref().or(snap.qq_rich.as_ref()).filter(|rich| !rich.trim().is_empty())?;
            qq_item(rich, snap.text.as_deref(), source_process, blobs)
        }
        TextCaptureCandidate::Html => html_item(snap.html.as_deref()?, snap.text.as_deref(),
            snap.chromium_source_url.as_deref(), source_process, blobs)?,
        TextCaptureCandidate::Text => {
            let text = snap.text.as_ref().filter(|text| can_collect_text(text, ignore_white_space))?;
            ClipRecord::new_text(text.clone(), source_process)
        }
    };
    after_csharp_preprocess(item, text_process_expr, ignore_white_space, transform)
}

fn after_csharp_preprocess(
    mut item: ClipRecord,
    expression: &str,
    ignore_white_space: bool,
    transform: &dyn Fn(&str, &str) -> Option<String>,
) -> Option<ClipRecord> {
    crate::text_actions::apply_text_clip_preprocess_with(&mut item, expression, ignore_white_space, transform)
        .then_some(item)
}

// C# string.Length counts UTF-16 units; counting bytes rejects Chinese text
// and emoji before the same clipboard collection limit is reached.
fn within_utf16_limit(text: &str, limit: usize) -> bool {
    text.len() <= limit || text.encode_utf16().take(limit + 1).count() <= limit
}

fn can_collect_text(text: &str, ignore_white_space: bool) -> bool {
    if text.is_empty() || !within_utf16_limit(text, MAX_TEXT_UTF16) {
        return false;
    }
    if ignore_white_space && text.trim().is_empty() {
        return false;
    }
    true
}

fn qq_item(rich: &str, text: Option<&str>, source_process: &str, data_root: &Path) -> ClipRecord {
    let envelope = crate::im_rich::extract_envelope(rich).unwrap_or(rich);
    let origin = crate::im_rich::resolve_origin(source_process, envelope);
    let library = ClipImageDirs::under(data_root).library;
    let persisted = crate::im_rich::persist_images_to_library_folder(envelope, origin, &library);
    let segments = crate::im_rich::parse_segments(&persisted, origin);
    let summary = crate::im_rich::build_plain_summary(&segments);
    let body = if !summary.is_empty() {
        summary
    } else {
        text.unwrap_or(persisted.as_str()).to_string()
    };
    let mut item = ClipRecord::new_text(body, source_process);
    item.kind = ClipKind::QQRichTextItem;
    item.qq_rich_text = Some(persisted.clone());
    item.content_hash = hash_text(&format!("qq:{persisted}"));
    item
}

fn html_item(
    html: &str,
    text: Option<&str>,
    chromium_url: Option<&str>,
    source_process: &str,
    blobs: &Path,
) -> Option<ClipRecord> {
    // C# `HtmlClipItem.Text` is `GetText()` only; `CanCollect` is `!IsNullOrEmpty(Text)`.
    let body = text
        .filter(|s| !s.is_empty() && within_utf16_limit(s, MAX_TEXT_UTF16))
        .map(str::to_string)?;
    // The full CF_HTML context is persistent data. Only project the fragment
    // for rich-text detection; WPS tables can rely on a large head/style block.
    let (fragment, src) = try_extract_fragment_and_source_url(html)
        .unwrap_or_else(|| (html.to_string(), try_parse_source_url(html)));
    if let Some((envelope, summary)) = crate::im_rich::try_upgrade_html_to_qq(
        &fragment,
        &body,
        source_process,
        &ClipImageDirs::under(blobs).library,
    ) {
        let mut item = ClipRecord::new_text(summary, source_process);
        item.kind = ClipKind::QQRichTextItem;
        item.html_data = Some(html.to_string());
        item.html_source_url = sanitize_chromium_source_url(chromium_url)
            .or(src.clone().filter(|s| !s.is_empty()));
        item.qq_rich_text = Some(envelope.clone());
        item.content_hash = hash_text(&format!("qq:{envelope}"));
        return Some(item);
    }
    let mut item = ClipRecord::new_text(body, source_process);
    item.kind = ClipKind::HtmlClipItem;
    item.html_data = Some(html.to_string());
    item.html_source_url = sanitize_chromium_source_url(chromium_url)
        .or(src.filter(|s| !s.is_empty()));
    Some(item)
}

fn save_png(data_root: &Path, png: Vec<u8>, source_process: &str) -> Option<ClipRecord> {
    // C# `ImageClipItem.NewImageFileFullName`: `{Image/Temp}/{yyyyMMdd_HHmmss_fff}.png`.
    let temp = ClipImageDirs::under(data_root).temp;
    let name = chrono::Local::now().format("%Y%m%d_%H%M%S_%3f.png").to_string();
    let path = crate::storage_core::resources::publish_bytes_compatible(&temp, &name, &png).ok()?;
    let mut item = ClipRecord::new_text("", source_process);
    item.kind = ClipKind::ImageClipItem;
    item.image_path = Some(path.to_string_lossy().into_owned());
    item.content_hash = crate::model::sha256_hex(&png);
    item.perceptual_hash = crate::model::perceptual_hash_png(&png).unwrap_or_default();
    Some(item)
}

/// C# `ClipboardHelper.SaveClipboardImage` file-drop last resort (`tiff`/`tif` included).
fn save_clipboard_helper_filedrop_image(
    snap: &ClipboardSnapshot,
    blobs: &Path,
    source_process: &str,
) -> Option<ClipRecord> {
    if !snap.has_image_format {
        return None;
    }
    let source = snap
        .files
        .iter()
        .find(|path| is_clipboard_helper_image_file(path))?;
    let src = Path::new(source);
    let temp = ClipImageDirs::under(blobs).temp;
    let name = chrono::Local::now().format("%Y%m%d_%H%M%S_%3f.png").to_string();
    let dest = crate::storage_core::resources::publish_copy_compatible(src, &temp, &name).ok()?;
    let bytes = std::fs::read(&dest).ok()?;
    let mut item = ClipRecord::new_text("", source_process);
    item.kind = ClipKind::ImageClipItem;
    item.image_path = Some(dest.to_string_lossy().into_owned());
    item.content_hash = crate::model::sha256_hex(&bytes);
    item.perceptual_hash = crate::model::perceptual_hash_png(&bytes).unwrap_or_default();
    Some(item)
}

/// C# `ClipboardHelper.IsImageFile` (not `FileClipItem.IsImageFileExtension`).
fn is_clipboard_helper_image_file(path: &str) -> bool {
    matches!(
        Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str(),
        "png" | "jpg" | "jpeg" | "bmp" | "gif" | "tiff" | "tif"
    )
}

pub fn process_is_blacklisted(name: &str, list: &str) -> bool {
    if name.is_empty() || list.trim().is_empty() {
        return false;
    }
    let stem = strip_exe(name);
    if stem.is_empty() {
        return false;
    }
    split_process_names(list)
        .into_iter()
        .map(|s| strip_exe(&s))
        .any(|s| !s.is_empty() && s.eq_ignore_ascii_case(&stem))
}

/// C# `ProcessHelper.SplitNames`: `,` `，` `;` `；`.
pub fn split_process_names(names: &str) -> Vec<String> {
    names
        .split(|c| matches!(c, ',' | '，' | ';' | '；'))
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn strip_exe(name: &str) -> String {
    name.trim()
        .trim_end_matches(".exe")
        .trim_end_matches(".EXE")
        .to_string()
}

fn run_monitor(
    stop: Arc<monitor_wait::StopSignal>,
    store: Arc<Mutex<AppStore>>,
    data_root: PathBuf,
    allow_dup: Arc<AtomicBool>,
    hub: CaptureHub,
    blacklist: Arc<Mutex<String>>,
    collect_quicker: Arc<AtomicBool>,
    ignore_ws: Arc<AtomicBool>,
    monitor_on: Arc<AtomicBool>,
    deferred_hidden: Arc<Mutex<HashSet<String>>>,
    text_process_expr: Arc<parking_lot::Mutex<String>>,
    transform: Arc<dyn Fn(&str, &str) -> Option<String> + Send + Sync>,
) {
    if is_headless() {
        return;
    }
    // Only this worker advances the consumed sequence. WM_CLIPBOARDUPDATE is
    // just a wakeup; advancing last_seq in the listener silently loses copies.
    // Sample before registering, then check immediately to cover setup races.
    let mut last_seq = clipboard_sequence();
    let mut wake = monitor_wait::MonitorWait::new(&stop);
    let mut first_check = true;
    let mut retry_pending = false;
    let mut pending_copy = None;
    while !stop.is_stopped() {
        if !first_check && !wake.wait(&stop, retry_pending) { break; }
        first_check = false;
        retry_pending = false;
        if stop.is_stopped() { break; }
        if hub.is_suppressed() || is_globally_suppressed() {
            last_seq = clipboard_sequence();
            pending_copy = None;
            continue;
        }
        let seq = clipboard_sequence();
        if seq == 0 || seq == last_seq {
            continue;
        }
        if !monitor_on.load(Ordering::Relaxed) && !hub.raw_enabled.load(Ordering::Relaxed) {
            last_seq = seq;
            pending_copy = None;
            continue;
        }
        PendingMonitoredCopy::observe(&mut pending_copy, seq, &hub, foreground_process);
        // C# `Changed.Throttle(180ms)`: capture after the sequence stays unchanged for one quiet period.
        let Some(seq) = wait_until_clipboard_quiet(
            seq,
            CLIPBOARD_CHANGE_DEBOUNCE,
            || stop.is_stopped() || hub.is_suppressed() || is_globally_suppressed()
                || (!monitor_on.load(Ordering::Relaxed) && !hub.raw_enabled.load(Ordering::Relaxed)),
            || {
                let current = clipboard_sequence();
                PendingMonitoredCopy::observe(&mut pending_copy, current, &hub, foreground_process);
                current
            },
            |duration| stop.wait_timeout(duration),
        ) else {
            if stop.is_stopped() { break; }
            last_seq = clipboard_sequence();
            pending_copy = None;
            continue;
        };
        // Match C# preparation: excluded sources must not read/save images or
        // invoke text preprocessing. Consume only this observed quiet copy.
        if pending_copy.as_ref().is_some_and(|copy|
            process_is_blacklisted(&copy.source_process, &blacklist.lock()))
        {
            // Invalidate older automatic-sync work without reading any content
            // from an excluded source. X -> excluded Y -> X remains a new copy.
            hub.notify_raw_snapshot(seq,&ClipboardSnapshot{exclude_monitor:true,..Default::default()});
            last_seq = seq;
            pending_copy = None;
            continue;
        }
        let expr = text_process_expr.lock().clone();
        let capture = capture_with_raw_observer(
            &data_root,
            ignore_ws.load(Ordering::Relaxed),
            collect_quicker.load(Ordering::Relaxed),
            &expr,
            transform.as_ref(),
            pending_copy.as_ref(),
            Some(&hub),
            monitor_on.load(Ordering::Relaxed),
        );
        if let Err(error) = &capture {
            PendingMonitoredCopy::preserve_rendered_sequence(&mut pending_copy, error);
            // Busy/late-rendered reads need another attempt even when the
            // owner does not publish another clipboard notification.
            retry_pending = true;
        }
        let consumed = capture.is_ok();
        let Some(item) = consume_monitored_capture(&mut last_seq, capture) else {
            // A temporary read failure retries the same observed copy with its
            // original notification, even if a new FIFO queue has since armed.
            if consumed {
                pending_copy = None;
            }
            continue;
        };
        let notification = pending_copy.take().and_then(|copy| copy.notification);
        if process_is_blacklisted(&item.source_process_name, &blacklist.lock()) {
            continue;
        }
        if !monitored_kind_allowed(item.kind, collect_quicker.load(Ordering::Relaxed)) {
            continue;
        }
        let allow = allow_dup.load(Ordering::Relaxed);
        let hidden = deferred_hidden.lock().clone();
        persist_monitored_capture_with_notification(&store, &hub, &item, allow, &hidden, notification);
    }
}

fn consume_monitored_capture(
    last_sequence: &mut u32,
    capture: Result<(u32, Option<ClipRecord>), ClipboardReadFailure>,
) -> Option<ClipRecord> {
    match capture {
        Ok((sequence, item)) => {
            // Own writes, excluded formats and unsupported payloads are final
            // skips. A busy read or changed snapshot must remain pending.
            *last_sequence = sequence;
            item
        }
        Err(_) => None,
    }
}

#[cfg(test)]
fn persist_monitored_capture(
    store: &Mutex<AppStore>,
    hub: &CaptureHub,
    item: &ClipRecord,
    allow_duplicate: bool,
    hidden: &HashSet<String>,
) {
    let notification = hub.prepare_new_item_notification();
    persist_monitored_capture_with_notification(store, hub, item, allow_duplicate, hidden, notification);
}

fn persist_monitored_capture_with_notification(
    store: &Mutex<AppStore>,
    hub: &CaptureHub,
    item: &ClipRecord,
    allow_duplicate: bool,
    hidden: &HashSet<String>,
    notification: Option<NewItemNotification>,
) {
    // Wait for an in-flight search/edit instead of dropping the captured item.
    // Release the database lock before callbacks can query the host/store again.
    persist_monitored_capture_with_retry(
        hub,
        item,
        notification,
        |item| {
            if allow_duplicate || item.kind != ClipKind::ImageClipItem {
                return store.lock().persist_capture(item, allow_duplicate, |id| hidden.contains(id));
            }
            let prepared = image_identity_preparation::prepare(store, item)?;
            store.lock().persist_capture_prepared(item, false, |id| hidden.contains(id), &prepared)
        },
        thread::sleep,
    );
}

fn persist_monitored_capture_with_retry<E: std::fmt::Display>(
    hub: &CaptureHub,
    item: &ClipRecord,
    notification: Option<NewItemNotification>,
    mut persist: impl FnMut(&ClipRecord) -> Result<ClipRecord, E>,
    mut sleep: impl FnMut(Duration),
) {
    // Retry the captured record, never re-read a later clipboard or prepare a
    // notification for a different FIFO generation. Bound the extra waiting.
    for attempt in 0..MONITORED_CAPTURE_SAVE_ATTEMPTS {
        match persist(item) {
            Ok(saved) => {
                hub.push(
                    "clipChanged",
                    clip_changed_delta(std::slice::from_ref(&saved), &[]),
                );
                if let Some(notification) = notification { notification(); }
                hub.notify_captured(saved);
                return;
            }
            Err(error) => {
                tracing::warn!(%error, attempt = attempt + 1, "Failed to persist clipboard capture");
                if attempt + 1 < MONITORED_CAPTURE_SAVE_ATTEMPTS {
                    sleep(MONITORED_CAPTURE_SAVE_RETRY_DELAY);
                }
            }
        }
    }
    hub.notify_error();
}

/// C# Rx `Throttle(ClipboardChangeDebounce)`: wait until `seq` is unchanged for `quiet`.
/// Returns `None` when capture should be skipped (suppress / monitor off).
fn wait_until_clipboard_quiet(
    mut seq: u32,
    quiet: Duration,
    should_skip: impl Fn() -> bool,
    mut read_seq: impl FnMut() -> u32,
    sleeper: impl Fn(Duration),
) -> Option<u32> {
    loop {
        sleeper(quiet);
        if should_skip() {
            return None;
        }
        let now = read_seq();
        if now == seq {
            return Some(seq);
        }
        seq = now;
    }
}

#[cfg(windows)]
fn clipboard_sequence() -> u32 {
    unsafe { windows::Win32::System::DataExchange::GetClipboardSequenceNumber() }
}

#[cfg(not(windows))]
fn clipboard_sequence() -> u32 {
    0
}

fn read_snapshot() -> Option<ClipboardSnapshot> {
    read_snapshot_for_capture(false)
}

fn read_snapshot_for_capture(monitor_only: bool) -> Option<ClipboardSnapshot> {
    try_read_snapshot_for_capture(monitor_only, None).ok().and_then(|read| read.snapshot)
}

#[cfg(windows)]
fn try_read_snapshot_for_capture(
    monitor_only: bool, expected_sequence: Option<u32>,
) -> Result<ClipboardRead, ClipboardReadFailure> {
    unsafe { read_snapshot_win(monitor_only, expected_sequence) }
}

#[cfg(not(windows))]
fn try_read_snapshot_for_capture(
    _monitor_only: bool, _expected_sequence: Option<u32>,
) -> Result<ClipboardRead, ClipboardReadFailure> {
    Ok(ClipboardRead { snapshot: None, sequence: 0 })
}

#[cfg(any(windows, test))]
fn skip_clipboard_owner(monitor_only: bool, owner_pid: Option<u32>, own_pid: u32, allow_fixture: bool) -> bool {
    monitor_only && !allow_fixture && owner_pid == Some(own_pid)
}

#[cfg(any(windows, test))]
fn snapshot_read_allowed(
    expected_sequence: u32, sequence: u32, monitor_only: bool, owner_pid: Option<u32>,
) -> Result<bool, ClipboardReadFailure> {
    if sequence != expected_sequence {
        return Err(ClipboardReadFailure::Changed);
    }
    Ok(!skip_clipboard_owner(monitor_only, owner_pid, std::process::id(),
        cfg!(all(feature = "native-acceptance", not(test)))))
}

#[cfg(windows)]
unsafe fn clipboard_owner_pid_open() -> Option<u32> {
    use windows::Win32::{System::DataExchange::GetClipboardOwner, UI::WindowsAndMessaging::GetWindowThreadProcessId};
    GetClipboardOwner().ok().filter(|owner| !owner.is_invalid()).and_then(|owner| {
        let mut pid = 0;
        GetWindowThreadProcessId(owner, Some(&mut pid));
        (pid != 0).then_some(pid)
    })
}

#[cfg(windows)]
unsafe fn read_snapshot_win(
    monitor_only: bool, expected_sequence: Option<u32>,
) -> Result<ClipboardRead, ClipboardReadFailure> {
    native_read::snapshot(monitor_only, expected_sequence)
}

/// C# `ClipboardHelper.SaveClipboardImage`: up to 3 attempts, 100ms apart, reopening the clipboard.
#[cfg(any(windows, test))]
fn retry_save_clipboard_image_with(
    mut snapshot: ClipboardRead,
    mut read: impl FnMut(u32) -> Result<Option<(Option<Vec<u8>>, bool, u32)>, ClipboardReadFailure>,
    mut sleeper: impl FnMut(Duration),
) -> Result<ClipboardRead, ClipboardReadFailure> {
    if snapshot.snapshot.as_ref().is_none_or(|snap|
        snap.exclude_monitor || snap.png.is_some() || !snap.has_image_format)
    {
        return Ok(snapshot);
    }
    let mut read_available = false;
    for _ in 1..SAVE_CLIPBOARD_IMAGE_ATTEMPTS {
        sleeper(Duration::from_millis(SAVE_CLIPBOARD_IMAGE_RETRY_MS));
        let (png, has, sequence) = match read(snapshot.sequence) {
            Ok(Some(image)) => { read_available = true; image }
            Ok(None) => { snapshot.snapshot = None; return Ok(snapshot); }
            Err(ClipboardReadFailure::Unavailable | ClipboardReadFailure::UnavailableAtSequence(_)) => continue,
            Err(error) => return Err(error),
        };
        snapshot.sequence = sequence;
        let snap = snapshot.snapshot.as_mut().unwrap();
        if let Some(png) = png {
            snap.png = Some(png);
            snap.has_image_format = true;
            return Ok(snapshot);
        }
        snap.has_image_format = snap.has_image_format || has;
    }
    if read_available { Ok(snapshot) } else { Err(ClipboardReadFailure::UnavailableAtSequence(snapshot.sequence)) }
}

#[cfg(windows)]
fn image_format_available() -> bool {
    use windows::Win32::System::DataExchange::IsClipboardFormatAvailable;
    unsafe {
        format_available(FMT_PNG)
            || IsClipboardFormatAvailable(CF_BITMAP).is_ok()
            || IsClipboardFormatAvailable(CF_DIB).is_ok()
            || IsClipboardFormatAvailable(CF_DIBV5).is_ok()
    }
}

#[cfg(windows)]
fn format_available(name: &str) -> bool {
    unsafe {
        use windows::core::HSTRING;
        use windows::Win32::System::DataExchange::{IsClipboardFormatAvailable, RegisterClipboardFormatW};
        let fmt = RegisterClipboardFormatW(&HSTRING::from(name));
        fmt != 0 && IsClipboardFormatAvailable(fmt).is_ok()
    }
}

fn read_quicker_action(mut read: impl FnMut(&str) -> Option<Vec<u8>>) -> Option<String> {
    // V2 JSON wins; V1 legacy formats also carry WPF-serialized action objects.
    [FMT_QUICKER_ITEM_JSON, FMT_QUICKER_DRAG, FMT_QUICKER_ITEM].into_iter().find_map(|format| {
        let text = crate::quicker_wire::decode(&read(format)?)?;
        if format == FMT_QUICKER_ITEM_JSON && !text.trim_start().starts_with('{') { return None; }
        crate::quicker_action::action_id_from_payload(&text).ok()?;
        Some(text)
    })
}

#[cfg(test)]
#[test]
fn quicker_wire_prefers_modern_json_and_falls_back_only_to_valid_actions() {
    let raw = include_str!("../tests/fixtures/quicker-action-v2.json");
    let mut requested = Vec::new();
    let selected = read_quicker_action(|format| {
        requested.push(format.to_string());
        Some(if format == FMT_QUICKER_ITEM_JSON { raw.as_bytes().to_vec() } else { b"legacy-action".to_vec() })
    });
    assert_eq!(selected.as_deref(), Some(raw));
    assert_eq!(requested, [FMT_QUICKER_ITEM_JSON]);
    let selected = read_quicker_action(|format| Some(match format {
        FMT_QUICKER_ITEM_JSON => "invalid-json", FMT_QUICKER_DRAG => "{broken", _ => "legacy-id",
    }.as_bytes().to_vec()));
    assert_eq!(selected.as_deref(), Some("legacy-id"));
    assert!(read_quicker_action(|_| Some(b"{\"Title\":\"missing ID\"}".to_vec())).is_none());
}

#[cfg(test)]
#[test]
fn quicker_wire_v1_registered_formats_create_named_action_records() {
    let dir = tempfile::tempdir().unwrap();
    for (format, bytes) in [
        (FMT_QUICKER_ITEM, include_bytes!("../tests/fixtures/quicker-action-v1-wpf.bin").as_slice()),
        (FMT_QUICKER_DRAG, include_bytes!("../tests/fixtures/quicker-drag-v1-wpf.bin").as_slice()),
    ] {
        let payload = read_quicker_action(|requested| (requested == format).then(|| bytes.to_vec())).unwrap();
        let item = item_from_snapshot(ClipboardSnapshot {
            quicker_action: Some(payload), ..ClipboardSnapshot::default()
        }, dir.path(), "Quicker.exe").unwrap();
        assert_eq!(item.kind, ClipKind::ActionClipItem);
        assert_eq!(item.visual_title(), "Rust 动作 🦀");
        assert_eq!(crate::quicker_action::action_id_from_payload(item.body_text.as_deref().unwrap()).unwrap(),
            "08c06820-416c-4463-99b0-18c711634b3a");
        assert!(item.csharp_summary().contains("Rust 动作 🦀"));
    }
}

/// C# WPF `DataObject.GetData(DataFormats.Html)`: UTF-8 CF_HTML, NUL-terminated, not trimmed
/// (trimming would shift StartFragment/EndFragment byte offsets).
/// Shared byte reader for clipboard and immutable native OLE snapshots. Keep
/// first-NUL UTF-8 behavior and all whitespace: CF_HTML offsets count bytes.
pub(crate) fn decode_cf_html_bytes(bytes: &[u8]) -> Option<String> {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    if end == 0 {
        return None;
    }
    let text = String::from_utf8_lossy(&bytes[..end]).into_owned();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// C# `QQRichTextItem.TryReadClipboardFormatText`: UTF-8 envelope first, else UTF-16 envelope,
/// else non-empty UTF-8, else UTF-16.
pub(crate) fn decode_im_clipboard_bytes(bytes: &[u8]) -> Option<String> {
    let utf8 = decode_clipboard_utf8_payload(bytes);
    if crate::im_rich::contains_im_rich_envelope(&utf8) {
        return Some(utf8);
    }
    let utf16 = decode_clipboard_unicode_payload(bytes);
    if crate::im_rich::contains_im_rich_envelope(&utf16) {
        return Some(utf16);
    }
    if !utf8.trim_matches('\0').is_empty() {
        Some(utf8)
    } else if !utf16.is_empty() {
        Some(utf16)
    } else {
        None
    }
}

/// C# `Encoding.UTF8.GetString` on the HGLOBAL bytes (invalid sequences replaced).
fn decode_clipboard_utf8_payload(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// C# `DecodeUnicodeBytes`: even-length UTF-16 LE, then `TrimEnd('\0')`.
fn decode_clipboard_unicode_payload(bytes: &[u8]) -> String {
    if bytes.len() < 2 || bytes.len() % 2 != 0 {
        return String::new();
    }
    let u16s: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&u16s).trim_end_matches('\0').to_string()
}

/// C# `Uri.TryCreate(text, UriKind.Absolute)` after trim / `IsNullOrWhiteSpace`.
pub(crate) fn sanitize_chromium_source_url(raw: Option<&str>) -> Option<String> {
    let text = raw?.trim();
    if text.is_empty() || !is_absolute_uri(text) {
        None
    } else {
        Some(text.to_string())
    }
}

fn is_absolute_uri(text: &str) -> bool {
    let Some(colon) = text.find(':') else {
        return false;
    };
    let scheme = &text[..colon];
    if scheme.is_empty() {
        return false;
    }
    let mut chars = scheme.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_alphabetic()
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

#[cfg(windows)]
fn lock_bytes(handle: windows::Win32::Foundation::HANDLE) -> Option<Vec<u8>> {
    unsafe {
        use windows::Win32::Foundation::HGLOBAL;
        use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
        if handle.is_invalid() {
            return None;
        }
        let hg = HGLOBAL(handle.0);
        let size = GlobalSize(hg);
        if size == 0 || size > 32 * 1024 * 1024 {
            return None;
        }
        let ptr = GlobalLock(hg);
        if ptr.is_null() {
            return None;
        }
        let bytes = std::slice::from_raw_parts(ptr as *const u8, size).to_vec();
        let _ = GlobalUnlock(hg);
        Some(bytes)
    }
}

#[cfg(windows)]
fn decode_clipboard_bytes(bytes: &[u8]) -> Option<String> {
    crate::clipboard_text::decode(bytes)
}

#[cfg(all(test, windows))]
#[test]
fn quicker_wire_wpf_string_is_decoded_without_the_binary_envelope() {
    // Generated by .NET 10 WPF DataObject.SetData + COM IDataObject.GetData(HGLOBAL).
    let wire = include_bytes!("../tests/fixtures/quicker-action-v2-wpf.bin");
    let json = include_str!("../tests/fixtures/quicker-action-v2.json");
    let decoded = decode_clipboard_bytes(wire).unwrap();
    assert_eq!(decoded.trim_end_matches(['\r', '\n']), json.trim_end_matches(['\r', '\n']));
}

/// CF_UNICODETEXT is always NUL-terminated UTF-16 LE. Detecting its encoding
/// from the first character corrupts text starting with Chinese or an emoji.
fn decode_unicode_text(bytes: &[u8]) -> Option<String> {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&c| c != 0)
        .collect();
    String::from_utf16(&units).ok().filter(|s| !s.is_empty())
}

#[cfg(windows)]
fn read_unicode_text() -> Option<String> {
    native_read::text_bytes().ok().flatten().and_then(|bytes| decode_unicode_text(&bytes))
}

#[cfg(windows)]
unsafe fn read_hdrop_handle(hdrop: windows::Win32::UI::Shell::HDROP) -> Vec<String> {
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::Memory::GlobalSize;
    use windows::Win32::UI::Shell::DragQueryFileW;
    let payload_size = GlobalSize(HGLOBAL(hdrop.0));
    // Apply the same payload limit as other clipboard memory formats before allocating.
    if payload_size == 0 || payload_size > 32 * 1024 * 1024 {
        return Vec::new();
    }
    let count = DragQueryFileW(hdrop, 0xFFFF_FFFF, None);
    let mut paths = Vec::new();
    for i in 0..count {
        // The query excludes the terminator. ANSI HDROP input can expand to UTF-16,
        // so its byte size bounds characters, rather than half that byte size.
        let length = DragQueryFileW(hdrop, i, None) as usize;
        if length == 0 || length >= payload_size {
            continue;
        }
        let mut buf = vec![0u16; length + 1];
        let copied = DragQueryFileW(hdrop, i, Some(&mut buf)) as usize;
        if copied == length {
            paths.push(String::from_utf16_lossy(&buf[..copied]));
        }
    }
    paths
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImagePasteTarget {
    FileDropOnly,
    QqImage,
    BitmapAndFile,
}

fn process_stem(name: &str) -> String {
    Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(name)
        .trim()
        .to_ascii_lowercase()
}

pub(crate) fn is_wechat_family(process: &str) -> bool {
    matches!(process_stem(process).as_str(), "wechat" | "weixin" | "wxwork")
}

/// C# `FileClipItem.IsImageFileExtension`: png/jpg/jpeg/bmp/gif/webp.
pub(crate) fn is_image_file_extension(path: &str) -> bool {
    matches!(
        Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str(),
        "png" | "jpg" | "jpeg" | "bmp" | "gif" | "webp"
    )
}

pub(crate) fn file_clip_item_is_single_image(item: &ClipRecord) -> bool {
    item.kind == ClipKind::FileClipItem
        && item.file_paths.len() == 1
        && is_image_file_extension(&item.file_paths[0])
}

/// C# `FileClipItem.SetToDataObject` routes a single image path through `ImageClipItem`.
#[cfg(test)]
fn writes_as_image_clip(item: &ClipRecord) -> bool {
    item.kind == ClipKind::ImageClipItem || file_clip_item_is_single_image(item)
}

/// C# `ClipItem.SetToDataObject` unicode text. `None` means the type does not call `SetText`.
pub(crate) fn set_to_data_object_unicode(
    item: &ClipRecord,
    paste_with_new_line: bool,
    standard_paste: bool,
) -> Option<String> {
    match item.kind {
        ClipKind::ImageClipItem
        | ClipKind::FileClipItem
        | ClipKind::StepClipItem
        | ClipKind::FavoriteClipItem => None,
        ClipKind::LinkClipItem => {
            let title = item.title.as_deref().unwrap_or("");
            let url = item.body_text.as_deref().unwrap_or("");
            Some(if title.is_empty() {
                url.to_string()
            } else {
                format!("{title}\r\n{url}")
            })
        }
        ClipKind::ActionClipItem => crate::quicker_action::action_id_from_payload(item.body_text.as_deref().unwrap_or("")).ok(),
        ClipKind::CmdClipItem => {
            Some(item.body_text.clone().unwrap_or_default())
        }
        _ => Some(crate::paste::transform_text(
            item.body_text
                .as_deref()
                .unwrap_or(item.preview(4000).as_str()),
            paste_with_new_line,
            standard_paste,
        )),
    }
}

/// C# `FileClipItem.SetToDataObject` HDROP: `File.Exists` only (directories skipped).
pub(crate) fn file_clip_existing_paths(item: &ClipRecord) -> Vec<String> {
    item.file_paths
        .iter()
        .filter(|p| Path::new(p).is_file())
        .cloned()
        .collect()
}

pub(crate) fn classify_image_paste_target(process: &str, file_len: u64) -> ImagePasteTarget {
    let stem = process_stem(process);
    if matches!(stem.as_str(), "explorer" | "winword" | "powerpnt") {
        return ImagePasteTarget::FileDropOnly;
    }
    if matches!(stem.as_str(), "qq" | "wechat" | "weixin" | "tim" | "wxwork") {
        return ImagePasteTarget::QqImage;
    }
    if file_len < 2 * 1024 * 1024 {
        ImagePasteTarget::BitmapAndFile
    } else {
        ImagePasteTarget::FileDropOnly
    }
}

pub fn encode_dib_from_path(path: &str) -> Result<Vec<u8>, String> {
    encode_dib_from_bytes(&std::fs::read(path).map_err(|e| e.to_string())?)
}

pub(crate) fn encode_dib_from_bytes(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let img = image::load_from_memory(bytes).map_err(|e| e.to_string())?.to_rgba8();
    let width = i32::try_from(img.width()).map_err(|_| "image is too wide")?;
    let height = i32::try_from(img.height()).map_err(|_| "image is too high")?;
    if width <= 0 || height <= 0 {
        return Err("empty image".into());
    }
    let stride = (width as usize).checked_mul(4).ok_or("image row is too large")?;
    let size = stride.checked_mul(height as usize).and_then(|n| n.checked_add(40)).ok_or("image is too large")?;
    let mut buf = Vec::new();
    buf.try_reserve_exact(size).map_err(|e| e.to_string())?;
    buf.resize(size, 0u8);
    buf[0..4].copy_from_slice(&40u32.to_le_bytes());
    buf[4..8].copy_from_slice(&width.to_le_bytes());
    buf[8..12].copy_from_slice(&height.to_le_bytes());
    buf[12..14].copy_from_slice(&1u16.to_le_bytes());
    buf[14..16].copy_from_slice(&32u16.to_le_bytes());
    for y in 0..height {
        let src_y = (height - 1 - y) as u32;
        for x in 0..width as u32 {
            let pixel = img.get_pixel(x, src_y).0;
            let dest = 40 + y as usize * stride + x as usize * 4;
            buf[dest] = pixel[2];
            buf[dest + 1] = pixel[1];
            buf[dest + 2] = pixel[0];
            buf[dest + 3] = pixel[3];
        }
    }
    Ok(buf)
}

#[cfg(windows)]
pub(crate) fn open_clipboard_retry() -> Result<(), String> {
    native_read::open()
}

#[cfg(windows)]
pub(crate) fn open_clipboard_with_owner(owner: windows::Win32::Foundation::HWND) -> Result<(), String> {
    use windows::Win32::System::DataExchange::OpenClipboard;
    for _ in 0..8 {
        if unsafe { OpenClipboard(owner) }.is_ok() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(25));
    }
    Err("OpenClipboard failed".into())
}

/// C# WinForms `Clipboard.GetImage()` / `DataFormats.Bitmap` fallback. Do not `GlobalLock` CF_BITMAP.
#[cfg(windows)]
fn copy_bitmap_pixels_open(handle: windows::Win32::Foundation::HANDLE) -> Option<(Vec<u8>, u32, u32)> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, DeleteDC, GetDC, GetObjectW, ReleaseDC, BITMAP, HBITMAP, HGDIOBJ,
    };
    if handle.is_invalid() {
        return None;
    }
    let hbmp = HBITMAP(handle.0);
    let mut bmp = BITMAP::default();
    let got = unsafe {
        GetObjectW(
            HGDIOBJ(hbmp.0),
            std::mem::size_of::<BITMAP>() as i32,
            Some((&mut bmp as *mut BITMAP).cast()),
        )
    };
    if got == 0 || bmp.bmWidth <= 0 || bmp.bmHeight == 0 {
        return None;
    }
    let width = bmp.bmWidth;
    let height = bmp.bmHeight.unsigned_abs() as i32;
    if width > 8192 || height > 8192 {
        return None;
    }
    unsafe {
        let screen = GetDC(HWND::default());
        let hdc = CreateCompatibleDC(screen);
        if hdc.is_invalid() {
            let _ = ReleaseDC(HWND::default(), screen);
            return None;
        }
        let pixels = crate::window_thumb::dib_bgra(hdc, hbmp, width, height);
        let _ = DeleteDC(hdc);
        let _ = ReleaseDC(HWND::default(), screen);
        let pixels = pixels?;
        Some((pixels, width as u32, height as u32))
    }
}

fn bgra32_to_png_opaque(bgra: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    let mut rgba = Vec::with_capacity(bgra.len());
    for chunk in bgra.chunks_exact(4) {
        rgba.extend_from_slice(&[chunk[2], chunk[1], chunk[0], 255]);
    }
    let img = image::RgbaImage::from_raw(width, height, rgba)?;
    let mut out = Vec::new();
    let mut cursor = std::io::Cursor::new(&mut out);
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut cursor, image::ImageFormat::Png)
        .ok()?;
    Some(out)
}

/// A packed opaque V5 can contain either ordinary mask-colored pixels plus
/// allocation tail, or WPF's redundant masks. The bytes cannot identify which.
/// Only the native reader may request an independent bitmap for this shape.
fn dibv5_ambiguous_opaque_size(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < BITMAPV5HEADER_SIZE { return None; }
    let word = |offset: usize| u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
    if word(0) != BITMAPV5HEADER_SIZE as u32
        || u16::from_le_bytes(bytes[12..14].try_into().ok()?) != 1
        || u16::from_le_bytes(bytes[14..16].try_into().ok()?) != 32
        || word(16) != BI_BITFIELDS || word(32) != 0
        || word(40) != 0x00FF_0000 || word(44) != 0x0000_FF00 || word(48) != 0x0000_00FF
        || word(52) != 0 || word(56) != 0x7352_4742 || word(112) != 0 || word(116) != 0
    { return None; }
    let width = i32::from_le_bytes(bytes[4..8].try_into().ok()?);
    let height = i32::from_le_bytes(bytes[8..12].try_into().ok()?);
    let pixel_bytes = dib_pixel_bytes(width, height, 32)?;
    if word(20) as usize != pixel_bytes
        || bytes.len() != BITMAPV5HEADER_SIZE.checked_add(pixel_bytes)?.checked_add(12)?
        || bytes.get(BITMAPV5HEADER_SIZE..BITMAPV5HEADER_SIZE + 12)? != &bytes[40..52]
    { return None; }
    Some((width as u32, height.unsigned_abs()))
}

/// No native access occurs here: the caller provides an owned bitmap copy.
/// Ordinary and transparent layouts never invoke the fallback reader. Its
/// dimensions and complete BGRA length must agree before replacing V5 pixels.
fn dibv5_bitmap_fallback<E>(
    bytes: &[u8],
    read_bitmap: impl FnOnce() -> Result<Option<(Vec<u8>, u32, u32)>, E>,
) -> Result<Option<(Vec<u8>, u32, u32)>, E> {
    let Some((width, height)) = dibv5_ambiguous_opaque_size(bytes) else { return Ok(None); };
    Ok(read_bitmap()?.filter(|(pixels, bitmap_width, bitmap_height)|
        *bitmap_width == width && *bitmap_height == height
            && pixels.len() == width as usize * height as usize * 4))
}

pub(crate) fn dibv5_to_png(bytes: Vec<u8>) -> Option<Vec<u8>> {
    // C# `ClipboardAlphaImageReader.ImageFromClipboardDibV5`.
    if bytes.len() < BITMAPV5HEADER_SIZE {
        return None;
    }
    let header = u32::from_le_bytes(bytes[0..4].try_into().ok()?) as usize;
    if header != BITMAPV5HEADER_SIZE {
        return None;
    }
    let width = i32::from_le_bytes(bytes[4..8].try_into().ok()?);
    let height_raw = i32::from_le_bytes(bytes[8..12].try_into().ok()?);
    let planes = u16::from_le_bytes(bytes[12..14].try_into().ok()?);
    let bit_count = u16::from_le_bytes(bytes[14..16].try_into().ok()?);
    let compression = u32::from_le_bytes(bytes[16..20].try_into().ok()?);
    if planes != 1 || bit_count != 32 || compression != BI_BITFIELDS {
        return None;
    }
    let red_mask = u32::from_le_bytes(bytes[40..44].try_into().ok()?);
    let green_mask = u32::from_le_bytes(bytes[44..48].try_into().ok()?);
    let blue_mask = u32::from_le_bytes(bytes[48..52].try_into().ok()?);
    let alpha_mask = u32::from_le_bytes(bytes[52..56].try_into().ok()?);
    if red_mask != 0x00FF_0000 || green_mask != 0x0000_FF00 || blue_mask != 0x0000_00FF
        || !matches!(alpha_mask, 0 | 0xFF00_0000)
    {
        return None;
    }
    let pixel_bytes = dib_pixel_bytes(width, height_raw, 32)?;
    let image_bytes = u32::from_le_bytes(bytes[20..24].try_into().ok()?) as usize;
    if image_bytes != 0 && image_bytes != pixel_bytes {
        return None;
    }
    let colors_used = u32::from_le_bytes(bytes[32..36].try_into().ok()?) as usize;
    let pixel_offset = BITMAPV5HEADER_SIZE.checked_add(colors_used.checked_mul(4)?)?;
    let pixel_end = pixel_offset.checked_add(pixel_bytes)?;
    let color_space = u32::from_le_bytes(bytes[56..60].try_into().ok()?);
    let profile_offset = u32::from_le_bytes(bytes[112..116].try_into().ok()?) as usize;
    let profile_bytes = u32::from_le_bytes(bytes[116..120].try_into().ok()?) as usize;

    if pixel_end > bytes.len() {
        return None;
    }
    if profile_offset != 0 || profile_bytes != 0 {
        // Packed DIB profiles follow the pixels. Never mistake profile bytes
        // for image data or use an out-of-bounds/overlapping profile declaration.
        if !matches!(color_space, 0x4C49_4E4B | 0x4D42_4544) || profile_bytes == 0
            || profile_offset < pixel_end
            || profile_offset.checked_add(profile_bytes)? > bytes.len()
        {
            return None;
        }
    }
    // GlobalSize may exceed the requested size. Extra bytes alone do not
    // establish another pixel start or invalidate a complete standard bitmap.
    dib_pixels_to_png(width, height_raw, 32, alpha_mask != 0, &bytes[pixel_offset..pixel_end])
}

pub(crate) fn dib_to_png(bytes: Vec<u8>) -> Option<Vec<u8>> {
    // C# `ClipboardAlphaImageReader.ImageFromClipboardDib` (16/24/32 bpp, BI_RGB or BI_BITFIELDS).
    if bytes.len() < BITMAPINFOHEADER_SIZE {
        return None;
    }
    let header = u32::from_le_bytes(bytes[0..4].try_into().ok()?) as usize;
    if header != BITMAPINFOHEADER_SIZE {
        return None;
    }
    let width = i32::from_le_bytes(bytes[4..8].try_into().ok()?);
    let height_raw = i32::from_le_bytes(bytes[8..12].try_into().ok()?);
    let planes = u16::from_le_bytes(bytes[12..14].try_into().ok()?);
    let bit_count = u16::from_le_bytes(bytes[14..16].try_into().ok()?);
    let compression = u32::from_le_bytes(bytes[16..20].try_into().ok()?);
    if planes != 1 || (compression != BI_RGB && compression != BI_BITFIELDS) {
        return None;
    }
    let mut pixel_offset = header;
    let keep_alpha = compression == BI_BITFIELDS;
    if compression == BI_BITFIELDS {
        if bytes.len() < BITMAPINFOHEADER_SIZE + 12 {
            return None;
        }
        let red_mask = u32::from_le_bytes(bytes[40..44].try_into().ok()?);
        let green_mask = u32::from_le_bytes(bytes[44..48].try_into().ok()?);
        let blue_mask = u32::from_le_bytes(bytes[48..52].try_into().ok()?);
        if bit_count != 32 || red_mask != 0x00FF_0000 || green_mask != 0x0000_FF00 || blue_mask != 0x0000_00FF {
            return None;
        }
        pixel_offset += 12;
    }
    if !matches!(bit_count, 16 | 24 | 32) {
        return None;
    }
    let colors_used = u32::from_le_bytes(bytes[32..36].try_into().ok()?) as usize;
    pixel_offset = pixel_offset.checked_add(colors_used.checked_mul(4)?)?;
    if pixel_offset > bytes.len() {
        return None;
    }
    dib_pixels_to_png(width, height_raw, bit_count, keep_alpha, &bytes[pixel_offset..])
}

fn dib_pixel_bytes(width: i32, height_raw: i32, bit_count: u16) -> Option<usize> {
    if width <= 0 || width > 8192 {
        return None;
    }
    let height = height_raw.unsigned_abs();
    if height == 0 || height > 8192 {
        return None;
    }
    let width = width as u32;
    let stride = ((((u32::from(bit_count) * width) + 7) / 8 + 3) / 4) * 4;
    (stride as usize).checked_mul(height as usize)
}

fn dib_pixels_to_png(
    width: i32,
    height_raw: i32,
    bit_count: u16,
    keep_alpha: bool,
    pixels: &[u8],
) -> Option<Vec<u8>> {
    let expected = dib_pixel_bytes(width, height_raw, bit_count)?;
    if pixels.len() < expected {
        return None;
    }
    let width = width as u32;
    let height = height_raw.unsigned_abs();
    let stride = expected / height as usize;
    let mut rgba = vec![0u8; (width as usize) * (height as usize) * 4];
    let bottom_up = height_raw > 0;
    for y in 0..height as usize {
        let src_y = if bottom_up {
            height as usize - 1 - y
        } else {
            y
        };
        let row = &pixels[src_y * stride..];
        for x in 0..width as usize {
            let dst = (y * width as usize + x) * 4;
            match bit_count {
                32 => {
                    let i = x * 4;
                    rgba[dst] = row[i + 2];
                    rgba[dst + 1] = row[i + 1];
                    rgba[dst + 2] = row[i];
                    rgba[dst + 3] = if keep_alpha { row[i + 3] } else { 255 };
                }
                24 => {
                    let i = x * 3;
                    rgba[dst] = row[i + 2];
                    rgba[dst + 1] = row[i + 1];
                    rgba[dst + 2] = row[i];
                    rgba[dst + 3] = 255;
                }
                16 => {
                    let i = x * 2;
                    let px = u16::from_le_bytes([row[i], row[i + 1]]);
                    let r5 = (px >> 10) & 0x1F;
                    let g5 = (px >> 5) & 0x1F;
                    let b5 = px & 0x1F;
                    rgba[dst] = ((r5 as u32 * 255) / 31) as u8;
                    rgba[dst + 1] = ((g5 as u32 * 255) / 31) as u8;
                    rgba[dst + 2] = ((b5 as u32 * 255) / 31) as u8;
                    rgba[dst + 3] = 255;
                }
                _ => return None,
            }
        }
    }
    let img = image::RgbaImage::from_raw(width, height, rgba)?;
    let mut out = Vec::new();
    let mut cursor = std::io::Cursor::new(&mut out);
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut cursor, image::ImageFormat::Png)
        .ok()?;
    Some(out)
}

#[cfg(windows)]
fn foreground_process() -> String {
    foreground_target().0
}

#[cfg(not(windows))]
fn foreground_process() -> String {
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn capture_events_wake_before_wait_and_after_a_flush() {
        use std::future::Future;
        use std::task::{Context, Waker};
        let hub = CaptureHub::default();
        let wakeup = hub.event_notifier();
        let mut context = Context::from_waker(Waker::noop());
        hub.push("first", Value::Null);
        hub.push("second", Value::Null);
        assert!(std::pin::pin!(wakeup.notified()).as_mut().poll(&mut context).is_ready());
        assert_eq!(hub.drain().len(), 2, "one permit drains every queued event");

        let mut waiting = std::pin::pin!(wakeup.notified());
        assert!(waiting.as_mut().poll(&mut context).is_pending());
        assert!(hub.try_push("during-flush", Value::Null));
        assert!(waiting.as_mut().poll(&mut context).is_ready());
        assert_eq!(hub.drain()[0].0, "during-flush");

        let mut waiting = std::pin::pin!(wakeup.notified());
        assert!(waiting.as_mut().poll(&mut context).is_pending());
        hub.with_events_held_for_test(|| assert!(!hub.try_push("busy", Value::Null)));
        assert!(waiting.as_mut().poll(&mut context).is_pending(), "a skipped mirror never creates an empty wake loop");
    }

    #[test]
    fn clipboard_sync_observes_original_text_before_history_transform_and_with_history_disabled() {
        force_headless(true);
        let directory = tempdir().unwrap();
        let hub = CaptureHub::default();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let received = observed.clone();
        hub.set_on_raw_snapshot(move |_, snapshot| received.lock().push(snapshot.text.clone().unwrap()));
        let original = "  original\r\n😀\t  ";
        let _snapshot = install_test_snapshot(ClipboardSnapshot { text:Some(original.into()), ..Default::default() });
        let result = capture_with_raw_observer(directory.path(), true, false, "fixture", &|_, _| Some("history transformed".into()), None, Some(&hub), true).unwrap();
        assert_eq!(observed.lock().as_slice(), &[original.to_string()]);
        assert_eq!(result.1.unwrap().body_text.as_deref(), Some("history transformed"));
        let result = capture_with_raw_observer(directory.path(), true, false, "fixture", &|_, _| panic!("history preprocessing must not run"), None, Some(&hub), false).unwrap();
        assert!(result.1.is_none());
        assert_eq!(observed.lock().as_slice(), &[original.to_string(), original.to_string()]);
    }

    #[test]
    fn capture_origin_busy_and_rendered_retry_preserve_source_and_notification() {
        use std::sync::atomic::AtomicUsize;
        let hub = CaptureHub::default();
        let prepared = Arc::new(AtomicUsize::new(0));
        let preparing = prepared.clone();
        hub.set_on_new_item_factory(move || {
            preparing.fetch_add(1, Ordering::Relaxed);
            Box::new(|| {})
        });
        let mut pending = None;
        PendingMonitoredCopy::observe(&mut pending, 7, &hub, || "chrome".into());
        PendingMonitoredCopy::preserve_rendered_sequence(&mut pending, &ClipboardReadFailure::Unavailable);
        PendingMonitoredCopy::observe(&mut pending, 7, &hub, || panic!("a busy retry must not read the new foreground"));
        let result = retry_save_clipboard_image_with(ClipboardRead {
            snapshot: Some(ClipboardSnapshot { has_image_format: true, ..Default::default() }),
            sequence: 8, // The first held read rendered this same copy.
        }, |_| Err(ClipboardReadFailure::Unavailable), |_| {});
        PendingMonitoredCopy::preserve_rendered_sequence(&mut pending, &result.err().unwrap());
        PendingMonitoredCopy::observe(&mut pending, 8, &hub, || panic!("rendering must not replace the original source"));
        let copy = pending.as_ref().unwrap();
        assert_eq!(copy.source_process, "chrome.exe");
        assert_eq!(prepared.load(Ordering::Relaxed), 1, "the same copy keeps its FIFO notification ownership");

        let dir = tempdir().unwrap();
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(1, 1).write_to(&mut png, image::ImageFormat::Png).unwrap();
        let mixed = ClipboardSnapshot { text: Some("image description".into()), png: Some(png.into_inner()), has_image_format: true, ..Default::default() };
        let image = item_from_snapshot(mixed.clone(), dir.path(), &copy.source_process).unwrap();
        assert_eq!(image.kind, ClipKind::ImageClipItem, "browser copy still prefers its image after a switch to Notepad");
        assert_eq!(image.source_process_name, "chrome.exe");
        assert!(process_is_blacklisted(&copy.source_process, "chrome"));
        PendingMonitoredCopy::observe(&mut pending, 9, &hub, || "notepad".into());
        let source = &pending.as_ref().unwrap().source_process;
        let text = item_from_snapshot(mixed, dir.path(), source).unwrap();
        assert_eq!(text.kind, ClipKind::TextClipItem, "a genuinely new non-browser copy uses its own format order");
        assert_eq!(text.source_process_name, "notepad.exe");
        assert!(!process_is_blacklisted(source, "chrome"));
        assert_eq!(prepared.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn capture_origin_quiet_period_updates_only_for_new_copy_and_keeps_unknown_source() {
        use std::cell::Cell;
        let hub = CaptureHub::default();
        let mut pending = None;
        PendingMonitoredCopy::observe(&mut pending, 10, &hub, || "chrome.exe".into());
        let reads = Cell::new(0);
        let sequence = wait_until_clipboard_quiet(10, CLIPBOARD_CHANGE_DEBOUNCE, || false, || {
            let first = reads.replace(reads.get() + 1) == 0;
            PendingMonitoredCopy::observe(&mut pending, 11, &hub, || {
                assert!(first, "a stable sequence must not read a later foreground");
                "WeChat.exe".into()
            });
            11
        }, |_| {}).unwrap();
        assert_eq!(sequence, 11);
        let source = &pending.as_ref().unwrap().source_process;
        assert_eq!(source, "WeChat.exe");
        assert_eq!(capture_order(source, true), "40123");
        assert_eq!(reads.get(), 2);
        PendingMonitoredCopy::observe(&mut pending, 12, &hub, String::new);
        PendingMonitoredCopy::preserve_rendered_sequence(&mut pending, &ClipboardReadFailure::UnavailableAtSequence(13));
        PendingMonitoredCopy::observe(&mut pending, 13, &hub, || panic!("unknown origin must not be filled from a later window"));
        let source = &pending.as_ref().unwrap().source_process;
        assert!(source.is_empty());
        assert_eq!(capture_order(source, true), "01234");
        assert!(!process_is_blacklisted(source, "chrome;notepad"));
        assert!(!skip_clipboard_owner(true, None, std::process::id(), false));
        let text = item_from_snapshot(ClipboardSnapshot { text: Some("unknown owner is eligible".into()), ..Default::default() }, Path::new(""), source).unwrap();
        assert!(text.source_process_name.is_empty());
    }

    #[test]
    fn runtime_alignment_busy_capture_keeps_sequence_and_consumes_rendered_version() {
        let external_pid = std::process::id().wrapping_add(1);
        let mut reads = std::collections::VecDeque::from([
            (10, Err(ClipboardReadFailure::Unavailable)),
            (10, Ok(ClipboardRead {
                snapshot: Some(ClipboardSnapshot { text: Some("first copy".into()), ..Default::default() }),
                sequence: 11, // Delayed rendering advances the version while locked.
            })),
            (12, Ok(ClipboardRead {
                snapshot: Some(ClipboardSnapshot { text: Some("next external copy".into()), ..Default::default() }),
                sequence: 12,
            })),
        ]);
        let attempts = std::cell::Cell::new(0);
        let mut capture = |observed: u32, last: &mut u32| {
            if observed == *last { return None; }
            attempts.set(attempts.get() + 1);
            let (opened_sequence, result) = reads.pop_front().unwrap();
            let result = result.and_then(|read| {
                assert!(snapshot_read_allowed(observed, opened_sequence, true, Some(external_pid))?);
                Ok((read.sequence, read.snapshot.and_then(|snap|
                    item_from_snapshot(snap, Path::new(""), "fixture"))))
            });
            consume_monitored_capture(last, result)
        };
        let mut last = 9;
        assert!(capture(10, &mut last).is_none());
        assert_eq!(last, 9, "a busy clipboard must leave the same copy pending");
        assert_eq!(capture(10, &mut last).unwrap().body_text.as_deref(), Some("first copy"));
        assert_eq!(last, 11, "consume the version after delayed rendering, before unlocking");
        assert!(capture(11, &mut last).is_none());
        assert_eq!(attempts.get(), 2, "the rendered copy must not be captured twice");
        assert_eq!(capture(12, &mut last).unwrap().body_text.as_deref(), Some("next external copy"));
        assert_eq!(last, 12, "a later external copy must remain eligible");
        assert_eq!(snapshot_read_allowed(13, 14, true, Some(external_pid)), Err(ClipboardReadFailure::Changed));

        for snapshot in [
            ClipboardSnapshot { exclude_monitor: true, text: Some("excluded".into()), ..Default::default() },
            ClipboardSnapshot::default(),
        ] {
            let item = item_from_snapshot(snapshot, Path::new(""), "fixture");
            assert!(consume_monitored_capture(&mut last, Ok((15, item))).is_none());
            assert_eq!(last, 15, "excluded and unsupported data are final skips");
        }
    }

    #[test]
    fn runtime_alignment_image_retry_discards_changed_copy_and_rechecks_owner() {
        fn retry(
            monitor_only: bool, observed_sequence: u32, owner: u32, payload_reads: &std::cell::Cell<usize>,
        ) -> Result<ClipboardRead, ClipboardReadFailure> {
            retry_save_clipboard_image_with(ClipboardRead {
                snapshot: Some(ClipboardSnapshot {
                    text: Some("original text".into()), has_image_format: true, ..Default::default()
                }),
                sequence: 7,
            }, |expected_sequence| {
                if !snapshot_read_allowed(expected_sequence, observed_sequence, monitor_only, Some(owner))? {
                    return Ok(None);
                }
                payload_reads.set(payload_reads.get() + 1);
                // Rendering this same image can advance the held-lock token.
                Ok(Some((Some(b"original image".to_vec()), true, 8)))
            }, |delay| assert_eq!(delay, Duration::from_millis(SAVE_CLIPBOARD_IMAGE_RETRY_MS)))
        }
        let own_pid = std::process::id();
        let payload_reads = std::cell::Cell::new(0);
        let changed = retry(true, 8, own_pid.wrapping_add(1), &payload_reads);
        assert!(matches!(changed, Err(ClipboardReadFailure::Changed)));
        assert_eq!(payload_reads.get(), 0, "never append an image from another copy");
        let mut last = 6;
        assert!(consume_monitored_capture(&mut last, changed.map(|read| (read.sequence, None))).is_none());
        assert_eq!(last, 6, "discard the whole old snapshot and keep capture pending");

        let skipped = retry(true, 7, own_pid, &payload_reads).unwrap();
        assert!(skipped.snapshot.is_none(), "an own-process owner must discard the old snapshot");
        assert_eq!(payload_reads.get(), 0);
        assert!(consume_monitored_capture(&mut last, Ok((skipped.sequence, None))).is_none());
        assert_eq!(last, 7, "own writes must be consumed without repeating capture");

        let manual = retry(false, 7, own_pid, &payload_reads).unwrap();
        assert_eq!(manual.sequence, 8);
        let snapshot = manual.snapshot.unwrap();
        assert_eq!(snapshot.text.as_deref(), Some("original text"));
        assert_eq!(snapshot.png.as_deref(), Some(b"original image".as_slice()));
        assert_eq!(payload_reads.get(), 1, "manual reads retain their own-payload access");
    }

    #[cfg(windows)]
    static NATIVE_CLIPBOARD_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn unicode_clipboard_text_preserves_non_ascii_prefix_and_line_breaks() {
        for text in ["中文开头\r\nemoji 🦀", "🦀 emoji first", "Rust\r\n中文", "éclair", "\u{0100}text"] {
            let mut bytes: Vec<u8> = text.encode_utf16().chain(Some(0)).flat_map(u16::to_le_bytes).collect();
            // GlobalSize may include capacity after the terminator, not content.
            bytes.extend_from_slice(&[b'x', 0, 0xff]);
            assert_eq!(decode_unicode_text(&bytes).as_deref(), Some(text));
        }
        assert_eq!(decode_unicode_text(&[0, 0]), None);
        assert_eq!(decode_unicode_text(&[]), None);
        assert_eq!(decode_unicode_text(&[0, 0xd8, 0, 0]), None);
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "writes synthetic text to the Windows clipboard; no windows or input injection"]
    fn native_unicode_clipboard_roundtrip_without_desktop_input() {
        let _guard = NATIVE_CLIPBOARD_TEST_LOCK.lock();
        let text = "中文原生剪贴板回归 🦀\r\nRust Unicode roundtrip\té";
        force_headless(false);
        std::env::set_var("CLIPHOST_TAURI_HEADLESS", "0");
        write_text(text).unwrap();
        let mut owner_pid = 0;
        unsafe {
            let owner = windows::Win32::System::DataExchange::GetClipboardOwner().unwrap();
            assert!(!owner.is_invalid(), "clipboard publication requires an owner HWND");
            windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(owner, Some(&mut owner_pid));
        }
        assert_eq!(owner_pid, std::process::id());
        let snapshot = read_snapshot().expect("read native clipboard");
        assert_eq!(snapshot.text.as_deref(), Some(text));
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "writes synthetic HTML, image and file clipboard formats; run alone"]
    fn native_clipboard_formats_and_missing_files_without_desktop_input() {
        let _guard = NATIVE_CLIPBOARD_TEST_LOCK.lock();
        force_headless(false);
        std::env::set_var("CLIPHOST_TAURI_HEADLESS", "0");
        let dir = tempdir().unwrap();
        let path = dir.path().join("中文测试文件.txt");
        std::fs::write(&path, "synthetic fixture").unwrap();
        let image_path = dir.path().join("image.png");
        image::RgbaImage::from_pixel(3, 2, image::Rgba([20, 40, 60, 255]))
            .save(&image_path).unwrap();

        let mut file = ClipRecord::new_text("", "test");
        file.kind = ClipKind::FileClipItem;
        file.file_paths = vec![path.display().to_string()];
        write_record(&file, false, false, false).unwrap();
        assert_eq!(read_snapshot().unwrap().files, file.file_paths);

        let mut html = ClipRecord::new_text("中文 🦀", "test");
        html.kind = ClipKind::HtmlClipItem;
        html.html_data = Some("<b>中文 🦀</b>".into());
        write_record(&html, false, false, false).unwrap();
        let snapshot = read_snapshot().unwrap();
        assert_eq!(snapshot.text, html.body_text);
        assert!(snapshot.html.unwrap().contains("<b>中文 🦀</b>"));

        // Test deterministic bitmap/file formats, independent of the active app.
        let mut image = ClipRecord::new_text("", "test");
        image.kind = ClipKind::ImageClipItem;
        image.image_path = Some(image_path.display().to_string());
        crate::clipboard_payload::prepare_record_for_process(&image, false, false, false, "chrome.exe")
            .unwrap().write().unwrap();
        let snapshot = read_snapshot().unwrap();
        assert_eq!(snapshot.files, vec![image_path.display().to_string()]);
        let actual = image::load_from_memory(&snapshot.png.unwrap()).unwrap().to_rgba8();
        assert_eq!(actual.dimensions(), (3, 2));
        assert!(actual.pixels().all(|pixel| pixel.0 == [20, 40, 60, 255]));

        let sentinel = "missing file must not erase this clipboard text";
        write_text(sentinel).unwrap();
        file.file_paths = vec![dir.path().join("missing.txt").display().to_string()];
        assert!(write_record(&file, false, false, false).is_err());
        assert_eq!(read_snapshot().unwrap().text.as_deref(), Some(sentinel));
        file.kind = ClipKind::ImageClipItem;
        file.image_path = Some(dir.path().join("missing.png").display().to_string());
        assert!(write_record(&file, false, false, false).is_err());
        assert_eq!(read_snapshot().unwrap().text.as_deref(), Some(sentinel));
        assert!(write_text_and_files("", &file.file_paths).is_err());
        assert_eq!(read_snapshot().unwrap().text.as_deref(), Some(sentinel));
        assert!(write_text_and_files("", &[]).is_err());
        assert_eq!(read_snapshot().unwrap().text.as_deref(), Some(sentinel));

        let mixed = vec![path.display().to_string(), file.file_paths[0].clone()];
        write_text_and_files("mixed text", &mixed).unwrap();
        let snapshot = read_snapshot().unwrap();
        assert_eq!(snapshot.text.as_deref(), Some("mixed text"));
        assert_eq!(snapshot.files, vec![path.display().to_string()]);
    }

    #[test]
    fn monitor_capture_retries_same_snapshot_and_notifies_once_after_success() {
        use std::sync::atomic::AtomicUsize;

        let item = ClipRecord::new_text("synthetic copy before a later clipboard change", "capture-test");
        let original = serde_json::to_value(&item).unwrap();
        let hub = CaptureHub::default();
        let prepared = Arc::new(AtomicUsize::new(0));
        let notified = Arc::new(AtomicUsize::new(0));
        let factory_prepared = prepared.clone();
        let factory_notified = notified.clone();
        hub.set_on_new_item_factory(move || {
            factory_prepared.fetch_add(1, Ordering::Relaxed);
            let notified = factory_notified.clone();
            Box::new(move || { notified.fetch_add(1, Ordering::Relaxed); })
        });
        let captured = Arc::new(Mutex::new(Vec::new()));
        let captured_items = captured.clone();
        hub.set_on_captured(move |item| { captured_items.lock().push(item); });
        let errors = Arc::new(AtomicUsize::new(0));
        let error_count = errors.clone();
        hub.set_on_error(move || { error_count.fetch_add(1, Ordering::Relaxed); });

        let mut attempts = Vec::new();
        let mut waits = Vec::new();
        persist_monitored_capture_with_retry(
            &hub,
            &item,
            hub.prepare_new_item_notification(),
            |capture| {
                assert!(std::ptr::eq(capture, &item), "retry must retain the captured record");
                attempts.push(serde_json::to_value(capture).unwrap());
                assert!(hub.drain().is_empty(), "nothing may publish before commit");
                assert_eq!(notified.load(Ordering::Relaxed), 0);
                assert!(captured.lock().is_empty());
                if attempts.len() == 1 { Err("temporary save failure") } else { Ok(capture.clone()) }
            },
            |delay| { waits.push(delay); },
        );

        assert_eq!(attempts, vec![original.clone(), original]);
        assert_eq!(waits, vec![Duration::from_millis(250)]);
        assert_eq!(prepared.load(Ordering::Relaxed), 1, "the original FIFO notification must be retained");
        assert_eq!(notified.load(Ordering::Relaxed), 1);
        assert_eq!(errors.load(Ordering::Relaxed), 0);
        let captured = captured.lock();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].id, item.id);
        let events = hub.drain();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "clipChanged");
    }

    #[test]
    fn monitor_capture_persistent_failure_reports_error_once_without_success() {
        use std::sync::atomic::AtomicUsize;

        let item = ClipRecord::new_text("synthetic unsaved copy", "capture-test");
        let original = serde_json::to_value(&item).unwrap();
        let hub = CaptureHub::default();
        let notified = Arc::new(AtomicUsize::new(0));
        let notification_count = notified.clone();
        let notification: NewItemNotification = Box::new(move || { notification_count.fetch_add(1, Ordering::Relaxed); });
        let captured = Arc::new(AtomicUsize::new(0));
        let captured_count = captured.clone();
        hub.set_on_captured(move |_| { captured_count.fetch_add(1, Ordering::Relaxed); });
        let errors = Arc::new(AtomicUsize::new(0));
        let error_count = errors.clone();
        hub.set_on_error(move || { error_count.fetch_add(1, Ordering::Relaxed); });

        let mut attempts = Vec::new();
        let mut waits = Vec::new();
        persist_monitored_capture_with_retry(
            &hub,
            &item,
            Some(notification),
            |capture| -> Result<ClipRecord, &str> {
                attempts.push(serde_json::to_value(capture).unwrap());
                Err("persistent save failure")
            },
            |delay| { waits.push(delay); },
        );

        assert_eq!(attempts, vec![original.clone(), original.clone(), original]);
        assert_eq!(waits, vec![Duration::from_millis(250), Duration::from_millis(250)]);
        assert_eq!(notified.load(Ordering::Relaxed), 0, "a failed save must not consume a FIFO notification");
        assert_eq!(captured.load(Ordering::Relaxed), 0);
        assert_eq!(errors.load(Ordering::Relaxed), 1);
        assert!(hub.drain().is_empty(), "a failed save must not emit clipChanged");
    }

    #[test]
    fn monitor_capture_waits_for_busy_store_and_notifies_after_unlock() {
        use std::sync::mpsc;

        let dir = tempdir().unwrap();
        let store = Arc::new(Mutex::new(AppStore::open(&dir.path().join("userdata.db")).unwrap()));
        let item = ClipRecord::new_text("中文 🦀\r\nsecond line", "capture-test");
        let hub = CaptureHub::default();
        let callback_store = store.clone();
        let (captured_tx, captured_rx) = mpsc::channel();
        hub.set_on_captured(move |saved| {
            let db = callback_store.try_lock().expect("callback must run after DB unlock");
            assert_eq!(db.get_clip(&saved.id).unwrap().body_text, saved.body_text);
            captured_tx.send(saved.id).unwrap();
        });

        let busy = store.lock();
        let worker_store = store.clone();
        let worker_hub = hub.clone();
        let worker_item = item.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            started_tx.send(()).unwrap();
            persist_monitored_capture(&worker_store, &worker_hub, &worker_item, false, &HashSet::new());
            done_tx.send(()).unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(done_rx.recv_timeout(Duration::from_millis(100)), Err(mpsc::RecvTimeoutError::Timeout)));
        drop(busy);
        done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        worker.join().unwrap();
        assert_eq!(captured_rx.recv_timeout(Duration::from_secs(1)).unwrap(), item.id);
        assert_eq!(store.lock().get_clip(&item.id).unwrap().body_text, item.body_text);
        let events = hub.drain();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, "clipChanged");
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "writes a prepared synthetic image to the Windows clipboard; no windows or input injection"]
    fn native_prepared_clipboard_cross_thread_snapshot_without_desktop_input() {
        let _guard = NATIVE_CLIPBOARD_TEST_LOCK.lock();
        force_headless(false);
        std::env::set_var("CLIPHOST_TAURI_HEADLESS", "0");
        let dir = tempdir().unwrap();
        let path = dir.path().join("prepared.png");
        image::RgbaImage::from_pixel(2, 2, image::Rgba([20, 40, 60, 255])).save(&path).unwrap();
        let mut item = ClipRecord::new_text("", "test");
        item.kind = ClipKind::ImageClipItem;
        item.image_path = Some(path.display().to_string());
        let prepare_item = item.clone();
        let prepared = std::thread::spawn(move || {
            crate::clipboard_payload::prepare_record_for_process(&prepare_item, false, false, false, "chrome.exe")
        }).join().unwrap().unwrap();
        // Both native allocations must carry the original snapshot even though
        // the source changes before the UI thread submits those allocations.
        image::RgbaImage::from_pixel(2, 2, image::Rgba([200, 100, 50, 255])).save(&path).unwrap();
        prepared.write().unwrap();
        let snapshot = read_snapshot().unwrap();
        assert_eq!(snapshot.files, vec![path.display().to_string()]);
        let png = image::load_from_memory(&snapshot.png.unwrap()).unwrap().to_rgba8();
        assert!(png.pixels().all(|pixel| pixel.0 == [20, 40, 60, 255]));
        open_clipboard_retry().unwrap();
        let dib = unsafe {
            use windows::Win32::System::DataExchange::{GetClipboardData, CloseClipboard};
            let data = GetClipboardData(CF_DIB).ok().and_then(lock_bytes);
            CloseClipboard().unwrap();
            data.unwrap()
        };
        assert_eq!(&dib[40..56], &[60, 40, 20, 255].repeat(4));
        let sentinel = "preparation failure must preserve this text";
        write_text(sentinel).unwrap();
        assert!(write_record(&item, true, false, false).is_err());
        assert_eq!(read_snapshot().unwrap().text.as_deref(), Some(sentinel));
        // Cancelling after allocation also must not clear the clipboard.
        drop(crate::clipboard_payload::prepare_record_for_process(&item, false, false, false, "chrome.exe").unwrap());
        assert_eq!(read_snapshot().unwrap().text.as_deref(), Some(sentinel));
    }

    #[test]
    fn save_clipboard_image_retry_matches_csharp() {
        assert_eq!(SAVE_CLIPBOARD_IMAGE_ATTEMPTS, 3);
        assert_eq!(SAVE_CLIPBOARD_IMAGE_RETRY_MS, 100);
    }

    #[test]
    fn get_selected_text_empty_string_is_success_like_csharp() {
        assert!(!clipboard_sequence_advanced(5, 5));
        assert!(!clipboard_sequence_advanced(5, 4));
        assert!(clipboard_sequence_advanced(5, 6));
        assert_eq!(
            selected_text_from_status(ClipboardTextStatus::Text("hi".into())),
            Some("hi".into())
        );
        assert_eq!(
            selected_text_from_status(ClipboardTextStatus::Empty),
            Some(String::new())
        );
        assert_eq!(selected_text_from_status(ClipboardTextStatus::Unavailable), None);
    }

    #[test]
    fn monitoring_skips_own_clipboard_but_manual_capture_keeps_it() {
        assert!(skip_clipboard_owner(true, Some(42), 42, false));
        assert!(!skip_clipboard_owner(false, Some(42), 42, false));
    }

    #[test]
    fn monitoring_accepts_external_owner_immediately_after_own_write() {
        // No cooldown may survive the ownership change, including a copy
        // arriving during the 180ms quiet period of our own publication.
        let owners = [Some(42), Some(7), Some(42), Some(8)];
        assert_eq!(owners.map(|owner| skip_clipboard_owner(true, owner, 42, false)), [true, false, true, false]);
    }

    #[test]
    fn monitoring_keeps_unknown_owner_and_explicit_native_fixture_eligible() {
        assert!(!skip_clipboard_owner(true, None, 42, false));
        assert!(!skip_clipboard_owner(true, Some(42), 42, true));
    }

    #[test]
    fn clipboard_throttle_waits_for_quiet_sequence_like_csharp() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let reads = AtomicUsize::new(0);
        let sleeps = AtomicUsize::new(0);
        let seqs = [3u32, 3];
        let result = wait_until_clipboard_quiet(
            2,
            CLIPBOARD_CHANGE_DEBOUNCE,
            || false,
            || {
                let i = reads.fetch_add(1, Ordering::SeqCst);
                seqs[i.min(seqs.len() - 1)]
            },
            |d| {
                assert_eq!(d, Duration::from_millis(180));
                sleeps.fetch_add(1, Ordering::SeqCst);
            },
        );
        assert_eq!(result, Some(3));
        assert_eq!(sleeps.load(Ordering::SeqCst), 2);
        assert_eq!(CLIPBOARD_CHANGE_DEBOUNCE, Duration::from_millis(180));
    }

    #[test]
    fn clipboard_throttle_skips_when_suppressed_during_quiet() {
        let result = wait_until_clipboard_quiet(1, Duration::from_millis(1), || true, || 1, |_| {});
        assert!(result.is_none());
    }

    #[test]
    fn html_snapshot_becomes_html_clip_item() {
        let dir = tempdir().unwrap();
        let html = crate::cf_html::build_clipboard_html("<i>world</i>", Some("https://a.test"));
        let item = item_from_snapshot(
            ClipboardSnapshot {
                html: Some(html.clone()),
                text: Some("world".into()),
                ..ClipboardSnapshot::default()
            },
            dir.path(),
            "chrome.exe",
        )
        .unwrap();
        assert_eq!(item.kind, ClipKind::HtmlClipItem);
        assert_eq!(item.html_data.as_deref(), Some(html.as_str()));
        assert_eq!(item.html_source_url.as_deref(), Some("https://a.test"));
        assert_eq!(item.source_process_name, "chrome.exe");
    }

    #[test]
    fn html_preservation_capture_keeps_large_wps_context_exactly() {
        let dir = tempdir().unwrap();
        let raw = crate::cf_html::build_clipboard_html(&format!("<html xmlns:wps='urn:wps'><head><style>/*{}*/</style></head><body>中文😀</body></html>", "x".repeat(110 * 1024)), None);
        let item = item_from_snapshot(ClipboardSnapshot { html: Some(raw.clone()), text: Some("中文😀".into()), ..Default::default() }, dir.path(), "wps.exe").unwrap();
        assert_eq!(item.kind, ClipKind::HtmlClipItem);
        assert_eq!(item.html_data.as_deref(), Some(raw.as_str()));
    }

    #[test]
    fn cf_html_shared_byte_decoder_keeps_offsets_whitespace_and_first_nul() {
        let format = crate::cf_html::build_clipboard_html(" <b>中😀</b> ", Some("https://example.test"));
        let bytes = [format.as_bytes(), &[0], b"ignored allocator padding"].concat();
        assert_eq!(decode_cf_html_bytes(&bytes).as_deref(), Some(format.as_str()));
        assert_eq!(decode_cf_html_bytes(b" \r\n<html>body</html> \0padding").as_deref(),
            Some(" \r\n<html>body</html> "));
        assert!(decode_cf_html_bytes(b"\0padding").is_none());
        assert!(decode_cf_html_bytes(&[]).is_none());
    }

    #[test]
    fn html_snapshot_extracts_utf8_fragment_when_version_header_is_lowercase() {
        let dir = tempdir().unwrap();
        let raw = crate::cf_html::build_clipboard_html("<p>中文</p>", Some("https://a.test/x"))
            .replacen("Version:", "version:", 1);
        let item = item_from_snapshot(
            ClipboardSnapshot {
                html: Some(raw.clone()),
                text: Some("中文".into()),
                ..ClipboardSnapshot::default()
            },
            dir.path(),
            "msedge.exe",
        )
        .unwrap();
        assert_eq!(item.kind, ClipKind::HtmlClipItem);
        assert_eq!(item.html_data.as_deref(), Some(raw.as_str()));
        assert_eq!(item.html_source_url.as_deref(), Some("https://a.test/x"));
    }

    #[test]
    fn chromium_source_url_requires_absolute_uri_like_csharp() {
        assert_eq!(
            sanitize_chromium_source_url(Some(" https://example.com/x ")).as_deref(),
            Some("https://example.com/x")
        );
        assert_eq!(
            sanitize_chromium_source_url(Some("about:blank")).as_deref(),
            Some("about:blank")
        );
        assert!(sanitize_chromium_source_url(Some("not a url")).is_none());
        assert!(sanitize_chromium_source_url(Some("/relative")).is_none());
        assert!(sanitize_chromium_source_url(Some("example.com")).is_none());
        assert!(sanitize_chromium_source_url(Some("   ")).is_none());

        let dir = tempdir().unwrap();
        let html = crate::cf_html::build_clipboard_html("<b>x</b>", None);
        let item = item_from_snapshot(
            ClipboardSnapshot {
                html: Some(html),
                text: Some("x".into()),
                chromium_source_url: Some("not a url".into()),
                ..ClipboardSnapshot::default()
            },
            dir.path(),
            "chrome.exe",
        )
        .unwrap();
        assert!(item.html_source_url.is_none());
    }

    #[test]
    fn im_rich_prefers_utf8_envelope_then_utf16_like_csharp() {
        let utf8 = b"<QQRichEditFormat>hi</QQRichEditFormat>";
        assert_eq!(
            decode_im_clipboard_bytes(utf8).as_deref(),
            Some("<QQRichEditFormat>hi</QQRichEditFormat>")
        );

        let utf16: Vec<u8> = "<WeChatRichEditFormat>x</WeChatRichEditFormat>"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        let decoded = decode_im_clipboard_bytes(&utf16).expect("utf16 envelope");
        assert!(decoded.contains("<WeChatRichEditFormat>"));
        assert!(decoded.contains("</WeChatRichEditFormat>"));
        assert!(!decoded.contains('\0'));
    }

    #[test]
    fn snapshot_normalizes_source_process_like_csharp() {
        let dir = tempdir().unwrap();
        let item = item_from_snapshot(
            ClipboardSnapshot {
                text: Some("hello".into()),
                ..ClipboardSnapshot::default()
            },
            dir.path(),
            "  chrome  ",
        )
        .unwrap();
        assert_eq!(item.source_process_name, "chrome.exe");
    }

    #[test]
    fn explorer_shell_chrome_classes_match_csharp() {
        assert!(is_explorer_shell_chrome_root("explorer", "Shell_TrayWnd"));
        assert!(is_explorer_shell_chrome_root("Explorer.EXE", "Shell_SecondaryTrayWnd"));
        assert!(is_explorer_shell_chrome_root("explorer.exe", "MSTaskSwWClass"));
        assert!(is_explorer_shell_chrome_root("explorer", "MSTaskListWClass"));
        assert!(is_explorer_shell_chrome_root("explorer", "TaskListThumbnailWnd"));
        assert!(is_explorer_shell_chrome_root("explorer", "TaskSwitcherWnd"));
        assert!(!is_explorer_shell_chrome_root("explorer", "CabinetWClass"));
        assert!(!is_explorer_shell_chrome_root("chrome", "Shell_TrayWnd"));
        assert!(!is_explorer_shell_chrome_root("explorer", ""));
        assert!(!is_explorer_shell_chrome_root("explorer", "mstasklistwclass"));
    }

    #[test]
    fn exclude_format_skips_capture() {
        let dir = tempdir().unwrap();
        let item = item_from_snapshot(
            ClipboardSnapshot {
                exclude_monitor: true,
                text: Some("secret".into()),
                ..ClipboardSnapshot::default()
            },
            dir.path(),
            "",
        );
        assert!(item.is_none());
    }

    #[cfg(windows)]
    #[test]
    fn native_file_drop_reader_preserves_long_unicode_paths_and_capture_identity() {
        use windows::Win32::{
            Foundation::{GlobalFree, HGLOBAL},
            System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE},
            UI::Shell::HDROP,
        };
        struct Memory(HGLOBAL);
        impl Drop for Memory {
            fn drop(&mut self) {
                unsafe { let _ = GlobalFree(self.0); }
            }
        }
        // Exercise Shell parsing without opening or changing the system clipboard.
        for wide in [true, false] {
            let expected = if wide {
                vec![
                    r"C:\fixture\short.txt".to_string(),
                    format!(r"C:\fixture\{}文档 🦀.txt", "deep\\".repeat(150)),
                    format!(r"\\?\D:\{}最后一项.txt", "中文🦀目录\\".repeat(130)),
                ]
            } else {
                vec![format!(r"C:\fixture\{}last.txt", "deep\\".repeat(150))]
            };
            assert!(expected.last().unwrap().encode_utf16().count() > 520);
            let mut bytes = vec![0u8; 20];
            bytes[..4].copy_from_slice(&20u32.to_le_bytes());
            bytes[16..20].copy_from_slice(&u32::from(wide).to_le_bytes());
            for path in &expected {
                if wide {
                    bytes.extend(path.encode_utf16().chain([0]).flat_map(u16::to_le_bytes));
                } else {
                    bytes.extend(path.bytes().chain([0]));
                }
            }
            bytes.extend(if wide { vec![0, 0] } else { vec![0] });
            let actual = unsafe {
                let memory = Memory(GlobalAlloc(GMEM_MOVEABLE, bytes.len()).unwrap());
                let pointer = GlobalLock(memory.0);
                assert!(!pointer.is_null());
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer as *mut u8, bytes.len());
                let _ = GlobalUnlock(memory.0);
                read_hdrop_handle(HDROP(memory.0.0))
            };
            assert_eq!(actual, expected, "native file paths must be complete (wide={wide})");
            let dir = tempdir().unwrap();
            let item = item_from_snapshot(
                ClipboardSnapshot { files: actual, ..ClipboardSnapshot::default() },
                dir.path(),
                "explorer.exe",
            ).unwrap();
            assert_eq!(item.kind, ClipKind::FileClipItem);
            assert_eq!(item.file_paths, expected);
            assert_eq!(item.content_hash, hash_files(&expected));
        }
    }

    #[test]
    fn files_collected_when_no_text() {
        let dir = tempdir().unwrap();
        let item = item_from_snapshot(
            ClipboardSnapshot {
                files: vec!["D:\\a.txt".into(), "D:\\b.txt".into()],
                ..ClipboardSnapshot::default()
            },
            dir.path(),
            "explorer.exe",
        )
        .unwrap();
        assert_eq!(item.kind, ClipKind::FileClipItem);
        assert_eq!(item.file_paths.len(), 2);
    }

    #[test]
    fn blacklist_matches_exe_stem() {
        assert!(process_is_blacklisted("WeChat.exe", "wechat,qq"));
        assert!(process_is_blacklisted("qq", "QQ.EXE"));
        assert!(!process_is_blacklisted("chrome.exe", "wechat"));
        assert!(process_is_blacklisted("Notepad", "notepad;chrome"));
        assert!(process_is_blacklisted("notepad.exe", "notepad.exe"));
        assert!(process_is_blacklisted("notepad.exe", "NOTEPAD"));
        assert!(!process_is_blacklisted("", "notepad"));
        assert!(!process_is_blacklisted("notepad", ""));
        assert!(process_is_blacklisted("chrome", "notepad，chrome"));
        assert!(process_is_blacklisted("qq", "wechat；QQ.EXE"));
        assert!(!process_is_blacklisted("chrome.exe", "wechat|chrome"));
    }

    #[test]
    fn file_clip_single_image_routes_like_image_clipitem() {
        let mut file = ClipRecord::new_text(r"D:\shot.png", "t");
        file.kind = ClipKind::FileClipItem;
        file.file_paths = vec![r"D:\shot.png".into()];
        assert!(file_clip_item_is_single_image(&file));
        assert!(writes_as_image_clip(&file));
        file.file_paths = vec![r"D:\notes.txt".into()];
        assert!(!file_clip_item_is_single_image(&file));
        file.file_paths = vec![r"D:\a.png".into(), r"D:\b.png".into()];
        assert!(!file_clip_item_is_single_image(&file));
        assert!(is_image_file_extension(r"C:\x.JPEG"));
        assert!(!is_image_file_extension(r"C:\x.txt"));
    }

    #[test]
    fn copy_files_skips_missing_paths_like_csharp() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("keep.txt");
        std::fs::write(&file, "x").unwrap();
        let folder = dir.path().join("sub");
        std::fs::create_dir(&folder).unwrap();
        let missing = dir.path().join("gone.bin");
        let kept = copy_files_existing(&[
            file.to_string_lossy().into_owned(),
            missing.to_string_lossy().into_owned(),
            folder.to_string_lossy().into_owned(),
            String::new(),
        ]);
        assert_eq!(kept.len(), 2, "{kept:?}");
        assert!(kept.iter().any(|p| p.ends_with("keep.txt")));
        assert!(kept.iter().any(|p| Path::new(p).is_dir()));
        assert!(copy_files_existing(&[missing.to_string_lossy().into_owned()]).is_empty());
        assert!(write_files(&[missing.to_string_lossy().into_owned()]).is_ok());
    }

    #[test]
    fn set_to_data_object_formats_match_csharp() {
        let mut link = ClipRecord::new_text("https://a.test", "t");
        link.kind = ClipKind::LinkClipItem;
        link.title = Some("Docs".into());
        link.body_text = Some("https://a.test".into());
        assert_eq!(
            set_to_data_object_unicode(&link, true, true).as_deref(),
            Some("Docs\r\nhttps://a.test")
        );
        link.title = Some(String::new());
        assert_eq!(
            set_to_data_object_unicode(&link, true, true).as_deref(),
            Some("https://a.test")
        );

        let mut action = ClipRecord::new_text("action-guid", "t");
        action.kind = ClipKind::ActionClipItem;
        assert_eq!(
            set_to_data_object_unicode(&action, true, true).as_deref(),
            Some("action-guid")
        );

        let mut step = ClipRecord::new_text("{\"steps\":[]}", "t");
        step.kind = ClipKind::StepClipItem;
        assert_eq!(set_to_data_object_unicode(&step, false, false), None);

        let mut file = ClipRecord::new_text("files", "t");
        file.kind = ClipKind::FileClipItem;
        file.file_paths = vec!["D:\\missing.txt".into()];
        assert_eq!(set_to_data_object_unicode(&file, false, false), None);
        assert!(file_clip_existing_paths(&file).is_empty());

        let dir = tempdir().unwrap();
        let existing = dir.path().join("a.txt");
        std::fs::write(&existing, b"x").unwrap();
        let folder = dir.path().join("sub");
        std::fs::create_dir(&folder).unwrap();
        file.file_paths = vec![
            existing.to_string_lossy().into_owned(),
            folder.to_string_lossy().into_owned(),
            dir.path().join("nope.txt").to_string_lossy().into_owned(),
        ];
        let hdrop = file_clip_existing_paths(&file);
        assert_eq!(hdrop.len(), 1);
        assert!(hdrop[0].ends_with("a.txt"));
    }

    #[test]
    fn qq_write_plans_html_then_qq_like_csharp_base() {
        let mut item = ClipRecord::new_text("hi", "t");
        item.kind = ClipKind::QQRichTextItem;
        item.html_data = Some("<b>hi</b>".into());
        item.qq_rich_text = Some("<QQRichEditFormat/>".into());
        assert!(item.html_data.is_some());
        assert!(item.qq_rich_text.is_some());
        assert_eq!(item.kind, ClipKind::QQRichTextItem);
    }

    #[test]
    fn qq_envelope_becomes_rich_item_with_summary() {
        let dir = tempdir().unwrap();
        let item = item_from_snapshot(
            ClipboardSnapshot {
                qq_rich: Some(
                    r#"<QQRichEditFormat><EditElement type="0"><![CDATA[from qq]]></EditElement></QQRichEditFormat>"#
                        .into(),
                ),
                text: Some("plain".into()),
                ..ClipboardSnapshot::default()
            },
            dir.path(),
            "QQ.exe",
        )
        .unwrap();
        assert_eq!(item.kind, ClipKind::QQRichTextItem);
        assert_eq!(item.body_text.as_deref(), Some("from qq"));
        assert!(item.qq_rich_text.as_deref().unwrap().contains("QQRichEditFormat"));
    }

    #[test]
    fn qq_capture_persists_local_image_into_library() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("im.png");
        image::RgbaImage::from_pixel(1, 1, image::Rgba([4, 5, 6, 255]))
            .save(&src)
            .unwrap();
        let raw = format!(
            r#"<QQRichEditFormat><EditElement type="3" filepath="{}"></EditElement></QQRichEditFormat>"#,
            src.display()
        );
        let item = item_from_snapshot(
            ClipboardSnapshot {
                qq_rich: Some(raw),
                ..ClipboardSnapshot::default()
            },
            dir.path(),
            "QQ.exe",
        )
        .unwrap();
        let persisted = item.qq_rich_text.as_deref().unwrap();
        assert!(persisted.contains("Image"));
        let dest = dir.path().join("Image").join("im.png");
        assert!(dest.is_file());
        let segs = crate::im_rich::parse_segments(persisted, crate::im_rich::ImRichOrigin::Qq);
        assert_eq!(std::path::Path::new(&segs[0].path), dest.as_path());
    }

    #[test]
    fn png_capture_writes_timestamp_name_under_image_temp() {
        let dir = tempdir().unwrap();
        let item = item_from_snapshot(
            ClipboardSnapshot {
                png: Some(tiny_png()),
                ..ClipboardSnapshot::default()
            },
            dir.path(),
            "chrome.exe",
        )
        .unwrap();
        let path = item.image_path.as_deref().unwrap();
        let path_buf = std::path::Path::new(path);
        assert_eq!(path_buf.parent().unwrap(), dir.path().join("Image").join("Temp"));
        let name = path_buf.file_name().unwrap().to_string_lossy();
        assert!(
            regex::Regex::new(r"^\d{8}_\d{6}_\d{3}\.png$")
                .unwrap()
                .is_match(&name),
            "{name}"
        );
        assert!(path_buf.is_file());
        assert_ne!(item.id, name.trim_end_matches(".png"));
    }

    #[test]
    fn im_filedrop_image_fallback_when_bitmap_present_but_decode_failed() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("shot.jpg");
        std::fs::write(&src, tiny_png()).unwrap();
        let item = item_from_snapshot(
            ClipboardSnapshot {
                files: vec![src.to_string_lossy().into_owned()],
                has_image_format: true,
                ..ClipboardSnapshot::default()
            },
            dir.path(),
            "WeChat.exe",
        )
        .unwrap();
        assert_eq!(item.kind, ClipKind::ImageClipItem);
        let dest = std::path::Path::new(item.image_path.as_deref().unwrap());
        assert_eq!(dest.parent().unwrap(), dir.path().join("Image").join("Temp"));
        assert!(dest.is_file());
    }

    #[test]
    fn im_temporary_filedrop_without_bitmap_is_preserved_as_image() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("shot.png");
        std::fs::write(&src, tiny_png()).unwrap();
        let item = item_from_snapshot(
            ClipboardSnapshot {
                files: vec![src.to_string_lossy().into_owned()],
                ..ClipboardSnapshot::default()
            },
            dir.path(),
            "WeChat.exe",
        )
        .unwrap();
        assert_eq!(item.kind, ClipKind::ImageClipItem);
        assert!(item.file_paths.is_empty());
        std::fs::remove_file(&src).unwrap();
        assert_eq!(std::fs::read(item.image_path.unwrap()).unwrap(), tiny_png());
    }

    #[test]
    fn default_order_temporary_filedrop_is_preserved_before_bitmap_fallback() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("shot.png");
        std::fs::write(&src, tiny_png()).unwrap();
        let item = item_from_snapshot(
            ClipboardSnapshot {
                files: vec![src.to_string_lossy().into_owned()],
                has_image_format: true,
                ..ClipboardSnapshot::default()
            },
            dir.path(),
            "notepad.exe",
        )
        .unwrap();
        assert_eq!(item.kind, ClipKind::ImageClipItem);
        assert!(item.file_paths.is_empty());
        assert_ne!(item.image_path.as_deref(), Some(src.to_string_lossy().as_ref()));
    }

    #[test]
    fn image_paste_target_matches_csharp_window_classes() {
        assert_eq!(
            classify_image_paste_target("explorer.exe", 100),
            ImagePasteTarget::FileDropOnly
        );
        assert_eq!(
            classify_image_paste_target("WINWORD.EXE", 100),
            ImagePasteTarget::FileDropOnly
        );
        assert_eq!(
            classify_image_paste_target("QQ.exe", 100),
            ImagePasteTarget::QqImage
        );
        assert_eq!(
            classify_image_paste_target("WeChat.exe", 100),
            ImagePasteTarget::QqImage
        );
        assert_eq!(
            classify_image_paste_target("POWERPNT.EXE", 100),
            ImagePasteTarget::FileDropOnly
        );
        assert_eq!(
            classify_image_paste_target("Tim.exe", 100),
            ImagePasteTarget::QqImage
        );
        assert_eq!(
            classify_image_paste_target("WXWork.exe", 100),
            ImagePasteTarget::QqImage
        );
        assert_eq!(
            classify_image_paste_target("chrome.exe", 100),
            ImagePasteTarget::BitmapAndFile
        );
        assert_eq!(
            classify_image_paste_target("chrome.exe", 3 * 1024 * 1024),
            ImagePasteTarget::FileDropOnly
        );
        assert!(crate::im_rich::qq_image_envelope("D:\\a.png").contains(r#"filepath="D:\a.png""#));
    }

    fn tiny_png() -> Vec<u8> {
        vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
            0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
            0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
            0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
            0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ]
    }

    #[test]
    fn browser_prefers_files_over_html() {
        let dir = tempdir().unwrap();
        let item = item_from_snapshot(
            ClipboardSnapshot {
                html: Some(crate::cf_html::build_clipboard_html("<b>x</b>", None)),
                text: Some("x".into()),
                files: vec!["D:\\page.pdf".into()],
                ..ClipboardSnapshot::default()
            },
            dir.path(),
            "chrome.exe",
        )
        .unwrap();
        assert_eq!(item.kind, ClipKind::FileClipItem);
    }

    #[test]
    fn qq_prefers_image_over_text() {
        let dir = tempdir().unwrap();
        let item = item_from_snapshot(
            ClipboardSnapshot {
                text: Some("caption".into()),
                png: Some(tiny_png()),
                ..ClipboardSnapshot::default()
            },
            dir.path(),
            "QQ.exe",
        )
        .unwrap();
        assert_eq!(item.kind, ClipKind::ImageClipItem);
    }

    #[test]
    fn image_capture_hashes_match_csharp_imagecomparer() {
        let dir = tempdir().unwrap();
        let png = tiny_png();
        let expected_sha = crate::model::sha256_hex(&png);
        let a = item_from_snapshot(
            ClipboardSnapshot {
                png: Some(png.clone()),
                ..ClipboardSnapshot::default()
            },
            dir.path(),
            "notepad.exe",
        )
        .unwrap();
        let b = item_from_snapshot(
            ClipboardSnapshot {
                png: Some(png),
                ..ClipboardSnapshot::default()
            },
            dir.path(),
            "chrome.exe",
        )
        .unwrap();
        assert_eq!(a.kind, ClipKind::ImageClipItem);
        assert_eq!(a.content_hash, expected_sha);
        assert_eq!(a.content_hash.len(), 64);
        assert_eq!(a.perceptual_hash.len(), 64);
        assert!(a.perceptual_hash.chars().all(|c| c == '0' || c == '1'));
        assert!(a.looks_equal(&b));
        assert!(a.distinct_key().starts_with("image:phash:"));
    }

    #[test]
    fn whitespace_text_collected_unless_ignore_flag() {
        let dir = tempdir().unwrap();
        let snap = ClipboardSnapshot {
            text: Some("   \n".into()),
            ..ClipboardSnapshot::default()
        };
        let kept = item_from_snapshot_with(snap.clone(), dir.path(), "notepad.exe", false);
        assert!(kept.is_some());
        let skipped = item_from_snapshot_with(snap, dir.path(), "notepad.exe", true);
        assert!(skipped.is_none());
    }

    #[test]
    fn html_without_gettext_falls_through_like_csharp_cancollect() {
        let dir = tempdir().unwrap();
        let html = crate::cf_html::build_clipboard_html("<b>only-html</b>", None);
        let skipped = item_from_snapshot(
            ClipboardSnapshot {
                html: Some(html.clone()),
                ..ClipboardSnapshot::default()
            },
            dir.path(),
            "notepad.exe",
        );
        assert!(
            skipped.is_none(),
            "C# HtmlClipItem.CanCollect is !IsNullOrEmpty(GetText())"
        );
        let files = item_from_snapshot(
            ClipboardSnapshot {
                html: Some(html),
                files: vec![r"D:\page.pdf".into()],
                ..ClipboardSnapshot::default()
            },
            dir.path(),
            "notepad.exe",
        )
        .unwrap();
        assert_eq!(files.kind, ClipKind::FileClipItem);
        let ws = item_from_snapshot_with(
            ClipboardSnapshot {
                html: Some(crate::cf_html::build_clipboard_html("<i>x</i>", None)),
                text: Some("   ".into()),
                ..ClipboardSnapshot::default()
            },
            dir.path(),
            "notepad.exe",
            true,
        )
        .unwrap();
        assert_eq!(ws.kind, ClipKind::HtmlClipItem);
        assert_eq!(ws.body_text.as_deref(), Some("   "));
    }

    fn sample_dibv5_bgra(b: u8, g: u8, r: u8, a: u8) -> Vec<u8> {
        let mut bytes = vec![0u8; 128];
        bytes[0..4].copy_from_slice(&124u32.to_le_bytes());
        bytes[4..8].copy_from_slice(&1i32.to_le_bytes());
        bytes[8..12].copy_from_slice(&1i32.to_le_bytes());
        bytes[12..14].copy_from_slice(&1u16.to_le_bytes());
        bytes[14..16].copy_from_slice(&32u16.to_le_bytes());
        bytes[16..20].copy_from_slice(&BI_BITFIELDS.to_le_bytes());
        bytes[40..44].copy_from_slice(&0x00FF_0000u32.to_le_bytes());
        bytes[44..48].copy_from_slice(&0x0000_FF00u32.to_le_bytes());
        bytes[48..52].copy_from_slice(&0x0000_00FFu32.to_le_bytes());
        bytes[52..56].copy_from_slice(&0xFF00_0000u32.to_le_bytes());
        bytes[124] = b;
        bytes[125] = g;
        bytes[126] = r;
        bytes[127] = a;
        bytes
    }

    #[test]
    fn dibv5_bgra_converts_to_png_with_alpha() {
        let png = dibv5_to_png(sample_dibv5_bgra(0, 0, 255, 128)).unwrap();
        let img = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (1, 1));
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0, 128]);
    }

    #[test]
    fn dibv5_rejects_non_bgra_masks() {
        let mut bytes = sample_dibv5_bgra(0, 0, 255, 255);
        bytes[40..44].copy_from_slice(&0x0000_00FFu32.to_le_bytes());
        assert!(dibv5_to_png(bytes).is_none());
    }

    fn sample_dib_header(width: i32, height: i32, bit_count: u16, compression: u32) -> Vec<u8> {
        let mut bytes = vec![0u8; BITMAPINFOHEADER_SIZE];
        bytes[0..4].copy_from_slice(&(BITMAPINFOHEADER_SIZE as u32).to_le_bytes());
        bytes[4..8].copy_from_slice(&width.to_le_bytes());
        bytes[8..12].copy_from_slice(&height.to_le_bytes());
        bytes[12..14].copy_from_slice(&1u16.to_le_bytes());
        bytes[14..16].copy_from_slice(&bit_count.to_le_bytes());
        bytes[16..20].copy_from_slice(&compression.to_le_bytes());
        bytes
    }

    #[test]
    fn dib_24bpp_converts_padded_bgr_and_flips_bottom_up() {
        // C# ImageFromClipboardDib 24bpp: DWORD stride, first scanline is the bottom row.
        let mut bytes = sample_dib_header(1, 2, 24, BI_RGB);
        bytes.extend_from_slice(&[0, 255, 0, 0]); // bottom = green
        bytes.extend_from_slice(&[255, 0, 0, 0]); // top = blue
        let png = dib_to_png(bytes).unwrap();
        let img = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (1, 2));
        assert_eq!(img.get_pixel(0, 0).0, [0, 0, 255, 255]);
        assert_eq!(img.get_pixel(0, 1).0, [0, 255, 0, 255]);
    }

    #[test]
    fn dib_bitfields_32_skips_mask_bytes_and_keeps_alpha() {
        let mut bytes = sample_dib_header(1, 1, 32, BI_BITFIELDS);
        bytes.extend_from_slice(&0x00FF_0000u32.to_le_bytes());
        bytes.extend_from_slice(&0x0000_FF00u32.to_le_bytes());
        bytes.extend_from_slice(&0x0000_00FFu32.to_le_bytes());
        bytes.extend_from_slice(&[0, 0, 255, 128]);
        let png = dib_to_png(bytes).unwrap();
        let img = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0, 128]);
    }

    #[test]
    fn dib_rgb_32_ignores_alpha_byte() {
        let mut bytes = sample_dib_header(1, 1, 32, BI_RGB);
        bytes.extend_from_slice(&[0, 0, 255, 128]);
        let png = dib_to_png(bytes).unwrap();
        let img = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0, 255]);
    }

    #[test]
    fn dib_16bpp_rgb555_converts_to_png() {
        let mut bytes = sample_dib_header(1, 1, 16, BI_RGB);
        // RGB555 red: R=31, G=0, B=0 → 0b0_11111_00000_00000 = 0x7C00, plus 2 pad bytes.
        bytes.extend_from_slice(&0x7C00u16.to_le_bytes());
        bytes.extend_from_slice(&[0, 0]);
        let png = dib_to_png(bytes).unwrap();
        let img = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0, 255]);
    }

    #[test]
    fn dib_rejects_bitfields_when_not_32bpp_bgra_masks() {
        let mut bytes = sample_dib_header(1, 1, 24, BI_BITFIELDS);
        bytes.extend_from_slice(&0x00FF_0000u32.to_le_bytes());
        bytes.extend_from_slice(&0x0000_FF00u32.to_le_bytes());
        bytes.extend_from_slice(&0x0000_00FFu32.to_le_bytes());
        bytes.extend_from_slice(&[0, 0, 255, 0]);
        assert!(dib_to_png(bytes).is_none());
    }

    #[test]
    fn dib_rejects_v5_header_size() {
        let mut bytes = sample_dib_header(1, 1, 32, BI_RGB);
        bytes[0..4].copy_from_slice(&(BITMAPV5HEADER_SIZE as u32).to_le_bytes());
        bytes.extend_from_slice(&[0, 0, 255, 255]);
        assert!(dib_to_png(bytes).is_none());
    }

    #[cfg(windows)]
    #[test]
    fn cf_bitmap_handle_converts_ddb_to_png() {
        use windows::Win32::Foundation::{COLORREF, HANDLE, HWND};
        use windows::Win32::Graphics::Gdi::{
            CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, ReleaseDC,
            SelectObject, SetPixel,
        };
        unsafe {
            let screen = GetDC(HWND::default());
            let hdc = CreateCompatibleDC(screen);
            let hbmp = CreateCompatibleBitmap(screen, 1, 1);
            let old = SelectObject(hdc, hbmp);
            let _ = SetPixel(hdc, 0, 0, COLORREF(0x0000_00FF));
            SelectObject(hdc, old);
            let png = copy_bitmap_pixels_open(HANDLE(hbmp.0))
                .and_then(|(pixels, width, height)| bgra32_to_png_opaque(&pixels, width, height));
            let _ = DeleteObject(hbmp);
            let _ = DeleteDC(hdc);
            let _ = ReleaseDC(HWND::default(), screen);
            let img = image::load_from_memory(&png.unwrap()).unwrap().to_rgba8();
            assert_eq!(img.dimensions(), (1, 1));
            assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0, 255]);
        }
    }

    #[test]
    fn peek_snapshot_uses_test_override_in_headless() {
        force_headless(true);
        let _guard = install_test_snapshot(ClipboardSnapshot {
            text: Some("injected".into()),
            files: vec!["D:\\a.txt".into()],
            ..ClipboardSnapshot::default()
        });
        let snap = peek_snapshot().unwrap();
        assert_eq!(snap.text.as_deref(), Some("injected"));
        assert_eq!(snap.files.len(), 1);
    }

    #[test]
    fn first_existing_path_from_text_trims_quotes_and_lines() {
        let dir = tempdir().unwrap();
        let path = dir.path().to_string_lossy().into_owned();
        let quoted = format!("\"{path}\"\r\nC:\\cliphost-missing-path");
        assert_eq!(first_existing_path_from_text(&quoted), Some(path.clone()));
        assert!(first_existing_path_from_text("").is_none());
        assert!(first_existing_path_from_text("   \r\nnot-a-path").is_none());
        assert!(first_existing_path_from_text(&format!("missing-path\r\n{path}")).is_none());
        assert_eq!(first_existing_path_from_text(&path), Some(path));
    }
}
