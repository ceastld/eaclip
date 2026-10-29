//! One reusable native hover surface; no additional WebView or per-hover HWND.
use serde::Deserialize;

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    pub action: String,
    pub ticket: u64,
    #[serde(default)]
    pub anchor: [f64; 4],
    pub image_src: Option<String>,
    pub text: Option<String>,
    pub meta_line: Option<String>,
    #[serde(default)]
    pub dark: bool,
    #[serde(default)]
    pub force: bool,
    /// GPUI warm row switch (2026-10-24): keep the visible preview until this
    /// content is ready, then glide to the new row. Tauri never sets it.
    #[serde(default)]
    pub keep: bool,
    /// GPUI side lock (2026-10-24): pick the preview side from the room beside
    /// the owner, keep it for a warm session, and move it at most once for an
    /// image that would be unusably small. Tauri keeps the per-image rule.
    #[serde(default)]
    pub side_lock: bool,
}

#[cfg(windows)]
#[path = "hover_surface.rs"]
mod surface;
#[cfg(windows)]
#[path = "hover_style.rs"]
mod style;
#[cfg(windows)]
#[path = "hover_frames.rs"]
mod frames;
#[cfg(windows)]
#[path = "hover_text_links.rs"]
mod text_links;
#[cfg(windows)]
#[path = "hover_video.rs"]
mod video;
#[cfg(windows)]
#[path = "hover_video_player.rs"]
mod video_player;
#[cfg(windows)]
pub use win::Service;
#[cfg(not(windows))]
pub struct Service;
#[cfg(not(windows))]
impl Service {
    pub fn new() -> Result<Self, String> {
        Ok(Self)
    }
    pub fn hide_owner(&self, _owner: isize) {}
    pub async fn request(
        &self,
        _owner: isize,
        _scale: f64,
        _request: Request,
        _host: cliphost_core::HostState,
    ) -> Result<(), String> {
        Err("Native hover is unavailable".into())
    }
}

#[cfg(windows)]
mod win {
    use super::{surface::Surface, Request};
    use std::{
        collections::{HashMap, HashSet, VecDeque},
        future::Future,
        io::Read,
        path::PathBuf,
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };
    use crate::windows_api as windows;
    use windows::{
        core::w,
        Win32::{
            Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
            Graphics::{
                Dwm::{DwmSetWindowAttribute, DWMWA_CLOAK},
                Gdi::*,
            },
            System::LibraryLoader::GetModuleHandleW,
            UI::{
                HiDpi::{SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2},
                WindowsAndMessaging::*,
            },
        },
    };

    #[cfg(test)]
    mod test_runtime {
        use std::{future::Future, sync::OnceLock};
        fn runtime() -> &'static tokio::runtime::Runtime {
            static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
            RUNTIME.get_or_init(|| tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2).enable_all().build().expect("hover test runtime"))
        }
        pub fn block_on<F: Future>(future: F) -> F::Output { runtime().block_on(future) }
        pub fn spawn<F>(future: F) -> tokio::task::JoinHandle<F::Output>
        where F: Future + Send + 'static, F::Output: Send + 'static {
            runtime().spawn(future)
        }
    }

    const UPDATE: u32 = WM_APP + 71;
    const INSTANCE_PROPERTY: windows::core::PCWSTR = w!("ClipHost.NativeHoverService");
    const OWNER_INSTANCE_PROPERTY: windows::core::PCWSTR = w!("ClipHost.NativeHoverOwnerService");
    const OWNER_POPUP_PROPERTY: windows::core::PCWSTR = w!("ClipHost.NativeHoverPreview");
    // All production binds go through Service. Serialize comparison/removal
    // with replacement binds; GetProp followed by RemoveProp is not atomic.
    static OWNER_LINK_LOCK: Mutex<()> = Mutex::new(());
    const CACHE_BYTES: usize = 32 * 1024 * 1024;
    const TEXT_FLAGS: DRAW_TEXT_FORMAT =
        DRAW_TEXT_FORMAT(DT_WORDBREAK.0 | DT_NOPREFIX.0 | DT_EDITCONTROL.0);
    use super::frames::Picture as Bitmap;
    struct Cache {
        entries: VecDeque<(String, Arc<Bitmap>)>,
        bytes: usize,
    }
    struct ImageInput {
        file: PathBuf,
        cached: Option<Arc<Bitmap>>,
    }
    struct Content {
        image: Option<Arc<Bitmap>>,
        video: Option<VideoContent>,
        text: Vec<u16>,
        dark: bool,
        opener: Option<LinkOpener>,
        truncated: bool,
    }
    impl Content {
        fn media_dimensions(&self) -> Option<(u32, u32)> {
            self.image.as_ref().map(|image| (image.width, image.height)).or_else(|| {
                let frame = self.video.as_ref()?.player.snapshot().frame;
                Some((frame.width, frame.height))
            })
        }
    }
    struct VideoContent {
        player: Arc<super::video_player::Player>,
        name: String,
    }
    impl Drop for VideoContent {
        fn drop(&mut self) { self.player.stop(); }
    }
    type LinkOpener = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;
    #[derive(Clone)]
    struct Presentation {
        content: Arc<Content>,
        owner: isize,
        anchor: RECT,
        /// Painted menu columns may occupy only part of a transparent owner.
        beside: Option<RECT>,
        scale: f64,
        generation: u64,
        glide: bool,
        side_lock: bool,
    }
    enum Pending {
        Show(Presentation),
        Hide,
        Leave { generation: u64, previous_generation: u64, owner: isize },
    }
    #[derive(Default)]
    struct Control {
        generation: u64,
        owner: isize,
        tickets: HashMap<isize, u64>,
        pending: Option<Pending>,
        closed: bool,
        live_hwnd: isize,
        ui_thread: u32,
        ui_exited: bool,
        destroying: bool,
        terminal_error: Option<String>,
        owners: HashSet<isize>,
        /// Shown content at or below this generation stays valid while a
        /// `keep` request loads; any hide or ordinary request clears it.
        keep_floor: Option<u64>,
    }
    struct Shared {
        control: Mutex<Control>,
        cache: Mutex<Cache>,
        stopped: tokio::sync::Notify,
        #[cfg(test)]
        created_hwnd: std::sync::atomic::AtomicIsize,
        #[cfg(test)]
        before_cache: Mutex<Option<Arc<DecodePause>>>,
        #[cfg(test)]
        before_dispatch: Mutex<Option<Arc<DecodePause>>>,
        #[cfg(test)]
        painted: std::sync::atomic::AtomicU64,
        #[cfg(test)]
        animation_ticks: std::sync::atomic::AtomicU64,
    }
    pub struct Service {
        shared: Arc<Shared>,
        hwnd: isize,
        decoder: Arc<tokio::sync::Semaphore>,
        resolver: Arc<tokio::sync::Semaphore>,
        worker: Mutex<Option<std::thread::JoinHandle<()>>>,
    }
    #[derive(Default)]
    struct Render {
        shown: Option<Presentation>,
        offset: i32,
        wheel_remainder: i32,
        max_offset: i32,
        pad: i32,
        image_size: (i32, i32),
        leaving: Option<Instant>,
        frame: usize,
        animation_started: Option<Instant>,
        /// Last wheel scroll of tooltip text; drives the overlay scrollbar.
        scrolled: Option<Instant>,
        pressed_link: Option<(u64, String)>,
        pressed_video: Option<u64>,
        video_revision: u64,
        /// Warm switch: started, from and to window positions.
        glide: Option<(Instant, (i32, i32), (i32, i32))>,
        /// Fade-in start for replaced content.
        fade: Option<Instant>,
    }
    const GLIDE: Duration = Duration::from_millis(140);
    const FADE: Duration = Duration::from_millis(110);
    impl Render {
        fn clear(&mut self) {
            if let Some(video) = self.shown.as_ref().and_then(|show| show.content.video.as_ref()) {
                video.player.stop();
            }
            self.shown = None;
            self.pressed_link = None;
            self.pressed_video = None;
        }
        fn scroll(&mut self, delta: i16) -> bool {
            if self.max_offset <= 0 {
                self.wheel_remainder = 0;
                return false;
            }
            // Keep the existing 60 pixels per full detent, but respond to
            // partial detents too. Retain subpixel input across wheel messages.
            let total = delta as i32 * 60 + self.wheel_remainder;
            self.wheel_remainder = total % 120;
            let before = self.offset;
            self.offset = self.offset.saturating_sub(total / 120).clamp(0, self.max_offset);
            // Outward input at a boundary must not delay reversing direction.
            if (self.offset == 0 && delta > 0)
                || (self.offset == self.max_offset && delta < 0)
            {
                self.wheel_remainder = 0;
            }
            before != self.offset
        }
    }
    struct Ui {
        shared: Arc<Shared>,
        render: Mutex<Render>,
        surface: Mutex<Surface>,
        /// Owner and side (true = right) of the current warm session.
        side_lock: Mutex<Option<(isize, bool)>>,
        #[cfg(test)]
        before_present: Mutex<Option<Arc<std::sync::Barrier>>>,
    }
    #[cfg(test)]
    #[derive(Default)]
    struct DecodePause {
        reached: std::sync::atomic::AtomicBool,
        released: Mutex<bool>,
        wake: std::sync::Condvar,
    }
    #[cfg(test)]
    impl DecodePause {
        fn pause(&self) {
            self.reached.store(true, std::sync::atomic::Ordering::SeqCst);
            let _guard = self.wake.wait_while(self.released.lock().unwrap(), |released| !*released).unwrap();
        }
        fn release(&self) {
            *self.released.lock().unwrap() = true;
            self.wake.notify_all();
        }
    }

    fn text(value: &str) -> Vec<u16> {
        value
            .chars()
            .take(8192)
            .collect::<String>()
            .encode_utf16()
            .collect()
    }
    fn text_truncated(value: &str) -> bool { value.chars().nth(8192).is_some() }
    fn link_opener(host: cliphost_core::HostState) -> LinkOpener {
        Arc::new(move |uri| host.shell_open_uri(&serde_json::json!({ "uri": uri })).map(|_| ()))
    }
    fn handle(value: isize) -> HWND {
        HWND(value as *mut _)
    }
    #[cfg(test)]
    fn rgb(dark: bool) -> COLORREF {
        if dark {
            COLORREF(0x001b1b1b)
        } else {
            COLORREF(0x00fafafa)
        }
    }
    /// Shown content may outlive its request generation while a warm `keep`
    /// replacement loads. Request admission still uses `current`.
    fn shown_valid(shared: &Shared, generation: u64) -> bool {
        let state = shared.control.lock().unwrap();
        !state.closed
            && (state.generation == generation || state.keep_floor.is_some_and(|floor| generation <= floor))
    }
    fn motion_enabled() -> bool {
        let mut enabled = windows::Win32::Foundation::BOOL(1);
        let _ = unsafe {
            SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION, 0,
                Some((&mut enabled as *mut windows::Win32::Foundation::BOOL).cast()),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0))
        };
        enabled.as_bool()
    }
    fn current(shared: &Shared, generation: u64) -> bool {
        let state = shared.control.lock().unwrap();
        !state.closed && state.generation == generation
    }
    fn closed_error(state: &Control) -> String {
        state.terminal_error.clone().unwrap_or_else(|| "Native hover service is closed".into())
    }
    fn request_current(shared: &Shared, generation: u64) -> Result<bool, String> {
        let state = shared.control.lock().unwrap();
        if state.closed { Err(closed_error(&state)) } else { Ok(state.generation == generation) }
    }
    async fn while_open<T>(shared: &Shared, future: impl Future<Output = T>) -> Result<T, String> {
        // Register before checking closed so a close between the check and
        // select cannot strand a semaphore/decoder waiter.
        let stopped = shared.stopped.notified();
        tokio::pin!(stopped);
        stopped.as_mut().enable();
        {
            let state = shared.control.lock().unwrap();
            if state.closed { return Err(closed_error(&state)); }
        }
        tokio::select! {
            biased;
            _ = &mut stopped => Err(closed_error(&shared.control.lock().unwrap())),
            value = future => Ok(value),
        }
    }
    fn retire_locked(state: &mut Control, error: Option<String>) {
        if !state.closed { state.generation += 1; }
        state.closed = true;
        state.owner = 0;
        state.pending = None;
        state.tickets.clear();
        if state.terminal_error.is_none() { state.terminal_error = error; }
    }
    fn instance(shared: &Shared) -> windows::Win32::Foundation::HANDLE {
        windows::Win32::Foundation::HANDLE(shared as *const _ as *mut _)
    }
    fn unlink_owners(shared: &Shared, popup: HWND, owners: Vec<isize>) {
        let _links = OWNER_LINK_LOCK.lock().unwrap();
        for owner in owners {
            unsafe {
                let owner = handle(owner);
                let mut pid = 0;
                if GetWindowThreadProcessId(owner, Some(&mut pid)) != 0
                    && pid == std::process::id()
                    && GetPropW(owner, OWNER_POPUP_PROPERTY).0 == popup.0
                    && GetPropW(owner, OWNER_INSTANCE_PROPERTY) == instance(shared) {
                    let _ = RemovePropW(owner, OWNER_POPUP_PROPERTY);
                    let _ = RemovePropW(owner, OWNER_INSTANCE_PROPERTY);
                }
            }
        }
    }
    fn retire_ui(shared: &Shared, popup: HWND, error: Option<String>) {
        let owners = {
            let mut state = shared.control.lock().unwrap();
            retire_locked(&mut state, error);
            state.live_hwnd = 0;
            state.destroying = true;
            state.owners.drain().collect()
        };
        shared.stopped.notify_waiters();
        let mut cache = shared.cache.lock().unwrap();
        cache.entries.clear();
        cache.bytes = 0;
        drop(cache);
        // Do this before DestroyWindow can recycle the popup's numeric HWND.
        unlink_owners(shared, popup, owners);
    }
    unsafe fn owns_dispatch_window(shared: &Shared, state: &Control, hwnd: HWND) -> bool {
        let mut pid = 0;
        state.live_hwnd == hwnd.0 as isize && state.live_hwnd != 0
            && GetWindowThreadProcessId(hwnd, Some(&mut pid)) == state.ui_thread
            && pid == std::process::id()
            && GetPropW(hwnd, INSTANCE_PROPERTY) == instance(shared)
    }
    struct UiWindow {
        hwnd: HWND,
        ui: Option<Box<Ui>>,
    }
    impl Drop for UiWindow {
        fn drop(&mut self) {
            let Some(ui) = self.ui.take() else { return; };
            let shared = ui.shared.clone();
            retire_ui(&shared, self.hwnd, None);
            unsafe {
                let mut pid = 0;
                // The native Ui pointer is only valid while this Box is alive.
                // Never destroy an HWND that has since been replaced/reused.
                if GetWindowThreadProcessId(self.hwnd, Some(&mut pid))
                    == windows::Win32::System::Threading::GetCurrentThreadId()
                    && pid == std::process::id()
                    && GetWindowLongPtrW(self.hwnd, GWLP_USERDATA) == &*ui as *const Ui as isize {
                    if let Err(error) = DestroyWindow(self.hwnd) {
                        // Prevent a dangling wndproc pointer even if native
                        // destruction unexpectedly fails during thread teardown.
                        SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, 0);
                        let _ = RemovePropW(self.hwnd, INSTANCE_PROPERTY);
                        let mut state = shared.control.lock().unwrap();
                        state.terminal_error = Some(format!("Destroying hover window failed: {error}"));
                        tracing::error!(%error, "Hover UI window finalizer failed");
                    }
                }
            }
            drop(ui); // Surface restores/deletes GDI resources after HWND teardown.
            shared.control.lock().unwrap().ui_exited = true;
        }
    }

    fn image_key(file: &PathBuf, dark: bool) -> Result<String, String> {
        let metadata = std::fs::metadata(file).map_err(|e| e.to_string())?;
        if metadata.len() > 64 * 1024 * 1024 {
            return Err("Hover image exceeds the decode budget".into());
        }
        Ok(format!("{}:{}:{}:{:?}", dark, file.display(), metadata.len(), metadata.modified().ok()))
    }
    fn cached_image(shared: &Shared, key: &str) -> Option<Arc<Bitmap>> {
        let mut cache = shared.cache.lock().unwrap();
        let index = cache.entries.iter().position(|entry| entry.0 == key)?;
        let entry = cache.entries.remove(index).unwrap();
        let image = entry.1.clone();
        cache.entries.push_front(entry);
        Some(image)
    }
    fn cache_image(shared: &Shared, key: String, image: &Arc<Bitmap>, generation: u64) -> Result<bool, String> {
        // Publishing a decoded cache entry and retiring the UI must be ordered.
        // The short control/cache critical section never spans I/O or decode.
        let state = shared.control.lock().unwrap();
        if state.closed { return Err(closed_error(&state)); }
        if state.generation != generation { return Ok(false); }
        store_cache(&mut shared.cache.lock().unwrap(), key, image);
        Ok(true)
    }
    fn store_cache(cache: &mut Cache, key: String, image: &Arc<Bitmap>) {
        if let Some(index) = cache.entries.iter().position(|entry| entry.0 == key) {
            let (_, old) = cache.entries.remove(index).unwrap();
            cache.bytes -= old.byte_len();
        }
        while cache.bytes + image.byte_len() > CACHE_BYTES {
            let Some((_, old)) = cache.entries.pop_back() else { break; };
            cache.bytes -= old.byte_len();
        }
        cache.bytes += image.byte_len();
        cache.entries.push_front((key, image.clone()));
    }

    fn shared_state() -> Arc<Shared> {
        Arc::new(Shared {
            control: Mutex::new(Control::default()),
            cache: Mutex::new(Cache { entries: VecDeque::new(), bytes: 0 }),
            stopped: tokio::sync::Notify::new(),
            #[cfg(test)]
            created_hwnd: std::sync::atomic::AtomicIsize::new(0),
            #[cfg(test)]
            before_cache: Mutex::new(None),
            #[cfg(test)]
            before_dispatch: Mutex::new(None),
            #[cfg(test)]
            painted: std::sync::atomic::AtomicU64::new(0),
            #[cfg(test)]
            animation_ticks: std::sync::atomic::AtomicU64::new(0),
        })
    }

    // Runs only on its own native UI thread. Tests can inject an invalid
    // GetMessage filter to exercise the real -1 error path without OS input.
    unsafe fn run_ui(
        shared: Arc<Shared>,
        ready: std::sync::mpsc::SyncSender<Result<isize, String>>,
        message_filter: isize,
    ) {
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let module = GetModuleHandleW(None).unwrap_or_default();
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: module.into(),
            lpszClassName: w!("ClipHostHoverPopup"),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&class);
        let ui = Box::new(Ui {
            shared: shared.clone(), render: Mutex::new(Render::default()),
            surface: Mutex::new(Surface::default()), side_lock: Mutex::new(None),
            #[cfg(test)]
            before_present: Mutex::new(None),
        });
        let hwnd = match CreateWindowExW(
            WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_LAYERED,
            class.lpszClassName, w!("ClipHost hover preview"), WS_POPUP,
            0, 0, 1, 1, None, None, module,
            Some(&*ui as *const Ui as *const _),
        ) {
            Ok(hwnd) => hwnd,
            Err(error) => {
                retire_ui(&shared, HWND::default(), Some(error.to_string()));
                drop(ui);
                shared.control.lock().unwrap().ui_exited = true;
                let _ = ready.send(Err(error.to_string()));
                return;
            }
        };
        // Established immediately after creation: every subsequent early return,
        // WM_QUIT, GetMessage failure or unwind destroys HWND before dropping Ui.
        let _window = UiWindow { hwnd, ui: Some(ui) };
        #[cfg(test)]
        shared.created_hwnd.store(hwnd.0 as isize, std::sync::atomic::Ordering::SeqCst);
        if let Err(error) = SetPropW(hwnd, INSTANCE_PROPERTY, instance(&shared)) {
            retire_ui(&shared, hwnd, Some(error.to_string()));
            let _ = ready.send(Err(error.to_string()));
            return;
        }
        {
            let mut state = shared.control.lock().unwrap();
            state.live_hwnd = hwnd.0 as isize;
            state.ui_thread = windows::Win32::System::Threading::GetCurrentThreadId();
        }
        if cfg!(test) || (cfg!(feature = "native-acceptance")
            && std::env::var("CLIPHOST_NATIVE_CLOAK_WINDOWS").as_deref() == Ok("1")) {
            let cloak = windows::Win32::Foundation::BOOL(1);
            let _ = DwmSetWindowAttribute(hwnd, DWMWA_CLOAK, &cloak as *const _ as *const _, 4);
        }
        if ready.send(Ok(hwnd.0 as isize)).is_err() {
            retire_ui(&shared, hwnd, Some("Hover UI readiness receiver was closed".into()));
            return;
        }
        let mut message = MSG::default();
        loop {
            let result = GetMessageW(&mut message, handle(message_filter), 0, 0).0;
            if result <= 0 {
                let error = (result == -1).then(|| format!("Hover GetMessage failed: {:?}", windows::Win32::Foundation::GetLastError()));
                retire_ui(&shared, hwnd, error);
                break;
            }
            let _ = TranslateMessage(&message);
            #[cfg(test)]
            if let Some(pause) = shared.before_dispatch.lock().unwrap().take() { pause.pause(); }
            DispatchMessageW(&message);
        }
    }

    impl Service {
        pub fn new() -> Result<Self, String> {
            let shared = shared_state();
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            let ui_shared = shared.clone();
            let worker = std::thread::Builder::new()
                .name("cliphost-hover-ui".into())
                .spawn(move || unsafe { run_ui(ui_shared, tx, 0); })
                .map_err(|error| error.to_string())?;
            let hwnd = rx
                .recv_timeout(Duration::from_secs(3))
                .map_err(|error| error.to_string())??;
            let service = Self {
                shared,
                hwnd,
                decoder: Arc::new(tokio::sync::Semaphore::new(1)),
                resolver: Arc::new(tokio::sync::Semaphore::new(1)),
                worker: Mutex::new(Some(worker)),
            };
            if !service.is_alive() { return Err(closed_error(&service.shared.control.lock().unwrap())); }
            Ok(service)
        }

        pub fn is_alive(&self) -> bool {
            let state = self.shared.control.lock().unwrap();
            !state.closed && unsafe { owns_dispatch_window(&self.shared, &state, handle(self.hwnd)) }
        }
        pub fn bind_owner(&self, owner: isize) -> Result<(), String> {
            let _links = OWNER_LINK_LOCK.lock().unwrap();
            let mut state = self.shared.control.lock().unwrap();
            if state.closed { return Err(closed_error(&state)); }
            if !unsafe { owns_dispatch_window(&self.shared, &state, handle(self.hwnd)) } {
                return Err("Native hover UI window is unavailable".into());
            }
            let hwnd = handle(owner);
            let mut owner_pid = 0;
            if owner == 0 || owner == self.hwnd
                || unsafe { GetWindowThreadProcessId(hwnd, Some(&mut owner_pid)) }
                    != unsafe { windows::Win32::System::Threading::GetCurrentThreadId() }
                || owner_pid != std::process::id() {
                return Err("Hover link must be bound on its current-process owner UI thread".into());
            }
            let (previous_popup, previous_instance) = unsafe {
                (GetPropW(hwnd, OWNER_POPUP_PROPERTY), GetPropW(hwnd, OWNER_INSTANCE_PROPERTY))
            };
            let registered = cliphost_core::host_hit_test::register_native_hover_popup(owner, self.hwnd)
                .and_then(|_| unsafe { SetPropW(hwnd, OWNER_INSTANCE_PROPERTY, instance(&self.shared)) }
                    .map_err(|error| error.to_string()));
            if let Err(error) = registered {
                // Both properties are one bind transaction. Roll back without
                // changing a later service's link; replacement binds use this lock.
                unsafe {
                    if previous_popup.0.is_null() { let _ = RemovePropW(hwnd, OWNER_POPUP_PROPERTY); }
                    else { let _ = SetPropW(hwnd, OWNER_POPUP_PROPERTY, previous_popup); }
                    if previous_instance.0.is_null() { let _ = RemovePropW(hwnd, OWNER_INSTANCE_PROPERTY); }
                    else { let _ = SetPropW(hwnd, OWNER_INSTANCE_PROPERTY, previous_instance); }
                }
                return Err(error);
            }
            state.owners.insert(owner);
            Ok(())
        }
        pub fn hide_owner(&self, owner: isize) {
            if let Err(error) = self.hide_owner_checked(owner) {
                tracing::debug!(%error, "Hover owner hide was retired");
            }
        }
        pub fn hide_owner_checked(&self, owner: isize) -> Result<(), String> {
            let mut state = self.shared.control.lock().unwrap();
            if state.closed { return Err(closed_error(&state)); }
            if state.owner != owner { return Ok(()); }
            state.generation += 1;
            state.owner = 0;
            state.pending = Some(Pending::Hide);
            drop(state);
            self.wake()
        }
        fn post(&self, message: u32, allow_closed: bool) -> Result<(), String> {
            let mut state = self.shared.control.lock().unwrap();
            if state.closed && !allow_closed { return Err(closed_error(&state)); }
            let hwnd = handle(self.hwnd);
            if !unsafe { owns_dispatch_window(&self.shared, &state, hwnd) } {
                let error = "Native hover UI window is unavailable".to_string();
                retire_locked(&mut state, Some(error.clone()));
                drop(state);
                self.shared.stopped.notify_waiters();
                return Err(error);
            }
            // Post is asynchronous; holding control across identity validation
            // prevents finalization and HWND reuse between validation and post.
            let result = unsafe { PostMessageW(hwnd, message, WPARAM(0), LPARAM(0)) }
                .map_err(|error| format!("Hover message dispatch failed: {error}"));
            if let Err(error) = &result {
                retire_locked(&mut state, Some(error.clone()));
                // Best effort wake-up of the same still-owned UI thread. If
                // its queue is full, the next queued message observes closed
                // and destroys the native window in its own wndproc.
                let _ = unsafe { PostThreadMessageW(state.ui_thread, WM_QUIT, WPARAM(0), LPARAM(0)) };
                drop(state);
                self.shared.stopped.notify_waiters();
            }
            result
        }
        fn wake(&self) -> Result<(), String> {
            self.post(UPDATE, false)
        }

        pub fn shutdown(&self) {
            // Keep the legacy idempotent teardown API, with a checked variant
            // for callers that need a dispatch error. Do not join on owner UI.
            {
                let state = self.shared.control.lock().unwrap();
                if state.ui_exited || state.live_hwnd == 0 { return; }
            }
            if let Err(error) = self.shutdown_checked() {
                tracing::warn!(%error, "Hover UI shutdown dispatch failed");
            }
        }
        pub fn shutdown_checked(&self) -> Result<(), String> {
            let mut state = self.shared.control.lock().unwrap();
            if state.ui_exited || state.live_hwnd == 0 { return Err(closed_error(&state)); }
            retire_locked(&mut state, None);
            drop(state);
            self.shared.stopped.notify_waiters();
            // A failed earlier post does not suppress a retry while this exact
            // HWND/instance still lives. A replacement instance never receives it.
            self.post(WM_CLOSE, true)
        }

        pub async fn request(
            &self,
            owner: isize,
            scale: f64,
            request: Request,
            host: cliphost_core::HostState,
        ) -> Result<(), String> {
            self.request_inner(owner, scale, request, host, None, None).await
        }

        /// GPUI captures the client-to-screen anchor on the owning UI thread
        /// before its hover delay; the worker uses that immutable rectangle.
        pub async fn request_screen(
            &self,
            owner: isize,
            scale: f64,
            request: Request,
            host: cliphost_core::HostState,
            anchor: [i32; 4],
        ) -> Result<(), String> {
            self.request_inner(owner, scale, request, host, Some(anchor), None).await
        }

        /// Keep the native owner for lifetime and input routing, while placing
        /// the preview outside the painted columns of a transparent menu.
        pub async fn request_screen_beside(
            &self,
            owner: isize,
            scale: f64,
            request: Request,
            host: cliphost_core::HostState,
            anchor: [i32; 4],
            beside: Option<[i32; 4]>,
        ) -> Result<(), String> {
            self.request_inner(owner, scale, request, host, Some(anchor), beside).await
        }

        /// Decode into the shared cache during the hover delay so the preview
        /// paints as soon as it is due. Never touches the visible popup or the
        /// request generation; skipped while the decoder is busy.
        pub async fn prefetch_image(&self, source: String, dark: bool, host: cliphost_core::HostState) {
            let Ok(permit) = self.decoder.clone().try_acquire_owned() else { return; };
            let shared = self.shared.clone();
            let _ = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                let open = || !shared.control.lock().unwrap().closed;
                if !open() { return; }
                let Ok(file) = image_file(&host, &source) else { return; };
                let Ok(key) = image_key(&file, dark) else { return; };
                if cached_image(&shared, &key).is_some() { return; }
                let mut bytes = Vec::new();
                let Ok(reader) = std::fs::File::open(&file) else { return; };
                if reader.take(64 * 1024 * 1024 + 1).read_to_end(&mut bytes).is_err()
                    || bytes.len() > 64 * 1024 * 1024 { return; }
                let Ok(Some(image)) = super::frames::decode(&bytes, dark, open) else { return; };
                let state = shared.control.lock().unwrap();
                if state.closed { return; }
                store_cache(&mut shared.cache.lock().unwrap(), key, &Arc::new(image));
            }).await;
        }
        async fn request_inner(
            &self,
            owner: isize,
            scale: f64,
            request: Request,
            host: cliphost_core::HostState,
            captured_anchor: Option<[i32; 4]>,
            captured_beside: Option<[i32; 4]>,
        ) -> Result<(), String> {
            let beside = captured_beside.map(|[left, top, right, bottom]| {
                if right <= left || bottom <= top {
                    return Err("Invalid captured hover menu bounds".to_owned());
                }
                Ok(RECT { left, top, right, bottom })
            }).transpose()?;
            let generation = {
                let mut state = self.shared.control.lock().unwrap();
                if state.closed {
                    return Err(closed_error(&state));
                }
                if request.ticket <= *state.tickets.get(&owner).unwrap_or(&0) {
                    return Ok(());
                }
                state.tickets.insert(owner, request.ticket);
                if request.action == "hide" {
                    if state.owner != owner {
                        return Ok(());
                    }
                    // Cancel pending decode immediately. Visible previews can survive
                    // crossing the gap into the popup; forced lifecycle hides cannot.
                    // Coalesced leaves still refer to the same rendered item.
                    // A newer image/text request has its own generation, even
                    // when the UI has not yet removed the previous content.
                    let previous_generation = match state.pending.as_ref() {
                        Some(Pending::Leave { previous_generation, .. }) => *previous_generation,
                        _ => state.generation,
                    };
                    state.generation += 1;
                    state.keep_floor = None;
                    state.pending = Some(if request.force {
                        Pending::Hide
                    } else {
                        Pending::Leave { generation: state.generation, previous_generation, owner }
                    });
                    drop(state);
                    return self.wake();
                }
                if !matches!(request.action.as_str(), "image" | "text" | "video") {
                    return Err("Unknown hover request".into());
                }
                unsafe {
                    if !IsWindowVisible(handle(owner)).as_bool() {
                        return Ok(());
                    }
                }
                let keep = request.keep && state.owner == owner;
                state.keep_floor = keep.then_some(state.generation);
                state.generation += 1;
                state.owner = owner;
                // Remove the previous item immediately; never show it during a
                // new image load, except a warm switch that replaces it in place.
                if !keep {
                    state.pending = Some(Pending::Hide);
                }
                state.generation
            };
            self.wake()?;
            let anchor = if let Some([left, top, right, bottom]) = captured_anchor {
                if right <= left || bottom <= top { return Err("Invalid captured hover anchor".into()); }
                RECT { left, top, right, bottom }
            } else { unsafe {
                let mut origin = POINT::default();
                if !ClientToScreen(handle(owner), &mut origin).as_bool() {
                    return Err("Hover owner is unavailable".into());
                }
                let a = request.anchor;
                if !a.iter().all(|v| v.is_finite()) || a[2] <= 0.0 || a[3] <= 0.0 {
                    return Err("Invalid hover anchor".into());
                }
                RECT {
                    left: origin.x + (a[0] * scale).round() as i32,
                    top: origin.y + (a[1] * scale).round() as i32,
                    right: origin.x + ((a[0] + a[2]) * scale).round() as i32,
                    bottom: origin.y + ((a[1] + a[3]) * scale).round() as i32,
                }
            } };
            let shared = self.shared.clone();
            if request.action == "text" {
                let mut state = shared.control.lock().unwrap();
                if state.closed { return Err(closed_error(&state)); }
                if state.generation != generation {
                    return Ok(());
                }
                state.pending = Some(Pending::Show(Presentation {
                    content: Arc::new(Content {
                        image: None,
                        video: None,
                        text: text(request.text.as_deref().unwrap_or("")),
                        dark: request.dark,
                        opener: Some(link_opener(host.clone())),
                        truncated: text_truncated(request.text.as_deref().unwrap_or("")),
                    }),
                    owner,
                    anchor,
                    beside,
                    scale,
                    generation,
                    glide: request.keep, side_lock: request.side_lock,
                }));
                drop(state);
                return self.wake();
            }
            if request.action == "video" {
                let permit = while_open(&shared, self.decoder.clone().acquire_owned()).await?.map_err(|error| error.to_string())?;
                if !request_current(&shared, generation)? { return Ok(()); }
                let source = request.image_src.as_deref().ok_or("Missing hover video")?.to_owned();
                let video_shared = shared.clone();
                let prepared = while_open(&shared, tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    if !request_current(&video_shared, generation)? { return Ok(None); }
                    let file = video_file(&source)?;
                    let active_shared = video_shared.clone();
                    // A leave ticket can retain this same popup while crossing
                    // the gap, so generation is checked on publication; ongoing
                    // playback is explicitly stopped when its render retires.
                    let active = Arc::new(move || {
                        let state = active_shared.control.lock().unwrap();
                        !state.closed && state.owner == owner
                            && unsafe { IsWindowVisible(handle(owner)).as_bool() }
                    });
                    let player = super::video_player::Player::open(file.clone(), active)?;
                    if !request_current(&video_shared, generation)? { player.stop(); return Ok(None); }
                    let snapshot = player.snapshot();
                    let name = file.file_name().unwrap_or_default().to_string_lossy().into_owned();
                    let caption = video_caption(&name, &snapshot);
                    Ok::<_, String>(Some((VideoContent { player, name }, caption)))
                })).await?.map_err(|error| error.to_string())?;
                // Unsupported codecs and inaccessible files still show a useful
                // file tooltip instead of leaving an empty/stale popup.
                let content = match prepared {
                    Ok(Some((video, caption))) => Content {
                        image: None, video: Some(video), text: text(&caption),
                        dark: request.dark, opener: None, truncated: false,
                    },
                    Ok(None) => return Ok(()),
                    Err(error) => Content {
                        image: None, video: None,
                        text: text(&format!("{}\n{}\n{error}",
                            request.image_src.as_deref().unwrap_or_default(),
                            cliphost_core::i18n::tr("无法预览此视频", "Video preview unavailable"))),
                        dark: request.dark, opener: None, truncated: false,
                    },
                };
                let mut state = shared.control.lock().unwrap();
                if state.closed { return Err(closed_error(&state)); }
                if state.generation != generation { return Ok(()); }
                state.pending = Some(Pending::Show(Presentation {
                    content: Arc::new(content), owner, anchor, beside, scale, generation, glide: request.keep, side_lock: request.side_lock,
                }));
                drop(state);
                return self.wake();
            }
            // Revision lookup is separately bounded and runs off the UI thread.
            // Cached pixels must not wait behind an unrelated expensive decode.
            let permit = while_open(&shared, self.resolver.clone().acquire_owned()).await?.map_err(|e| e.to_string())?;
            if !request_current(&shared, generation)? { return Ok(()); }
            let resolver_shared = shared.clone();
            let opener = link_opener(host.clone());
            let source = request.image_src.as_deref().ok_or("Missing hover image")?.to_owned();
            let dark = request.dark;
            let input = while_open(&shared, tokio::task::spawn_blocking(move || {
                let _permit = permit;
                if !request_current(&resolver_shared, generation)? { return Ok(None); }
                let file = image_file(&host, &source)?;
                let key = image_key(&file, dark)?;
                if !request_current(&resolver_shared, generation)? { return Ok(None); }
                let cached = cached_image(&resolver_shared, &key);
                Ok::<_, String>(Some(ImageInput { file, cached }))
            })).await?.map_err(|error| error.to_string())??;
            let Some(input) = input else { return Ok(()); };
            if !request_current(&shared, generation)? { return Ok(()); }
            let image = if let Some(image) = input.cached {
                image
            } else {
                // Only cold file reads and decodes use this gate. Queued stale
                // requests occupy futures, not blocking-pool threads.
                let permit = while_open(&shared, self.decoder.clone().acquire_owned()).await?.map_err(|e| e.to_string())?;
                if !request_current(&shared, generation)? { return Ok(()); }
                let decoder_shared = shared.clone();
                let image = while_open(&shared, tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    if !request_current(&decoder_shared, generation)? { return Ok(None); }
                    // A completed older decode or file revision may have changed
                    // while this cold request waited. Validate again at dequeue.
                    let key = image_key(&input.file, dark)?;
                    if !request_current(&decoder_shared, generation)? { return Ok(None); }
                    if let Some(image) = cached_image(&decoder_shared, &key) {
                        return Ok(Some(image));
                    }
                    let mut bytes = Vec::new();
                    std::fs::File::open(&input.file).map_err(|e| e.to_string())?
                        .take(64 * 1024 * 1024 + 1).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
                    if bytes.len() > 64 * 1024 * 1024 {
                        return Err("Hover image exceeds the decode budget".into());
                    }
                    if !request_current(&decoder_shared, generation)? { return Ok(None); }
                    let Some(image) = super::frames::decode(&bytes, dark, || current(&decoder_shared, generation))? else {
                        return Ok(None);
                    };
                    let image = Arc::new(image);
                    #[cfg(test)]
                    if let Some(pause) = decoder_shared.before_cache.lock().unwrap().take() { pause.pause(); }
                    if !request_current(&decoder_shared, generation)? { return Ok(None); }
                    // Never hold the cache mutex over file I/O or decoding.
                    if !cache_image(&decoder_shared, key, &image, generation)? { return Ok(None); }
                    Ok::<_, String>(Some(image))
                })).await?.map_err(|error| error.to_string())??;
                let Some(image) = image else { return Ok(()); };
                image
            };
            let mut state = shared.control.lock().unwrap();
            if state.closed { return Err(closed_error(&state)); }
            if state.generation != generation { return Ok(()); }
            state.pending = Some(Pending::Show(Presentation {
                content: Arc::new(Content {
                    image: Some(image),
                    video: None,
                    text: text(request.meta_line.as_deref().unwrap_or("")),
                    dark,
                    opener: Some(opener),
                    truncated: text_truncated(request.meta_line.as_deref().unwrap_or("")),
                }),
                owner,
                anchor,
                beside,
                scale,
                generation,
                glide: request.keep, side_lock: request.side_lock,
            }));
            drop(state);
            self.wake()
        }
    }
    impl Drop for Service {
        fn drop(&mut self) {
            self.shutdown();
            // Dropping JoinHandle detaches rather than synchronously waiting
            // on a popup that may currently be messaging this owner UI thread.
            let _ = self.worker.get_mut().unwrap().take();
        }
    }

    fn video_file(source: &str) -> Result<PathBuf, String> {
        use std::os::windows::fs::MetadataExt;
        let file = PathBuf::from(cliphost_core::shell_path::normalize_local_shell_path(source)?);
        let extension = file.extension().and_then(|value| value.to_str()).unwrap_or_default().to_ascii_lowercase();
        if !matches!(extension.as_str(), "mp4" | "m4v" | "mov" | "mkv" | "webm" | "avi" | "wmv" | "mpeg" | "mpg" | "m2ts" | "ts") {
            return Err("Unsupported video file type".into());
        }
        let metadata = file.metadata().map_err(|error| error.to_string())?;
        if !metadata.is_file() || metadata.len() == 0 { return Err("Video file is unavailable".into()); }
        // Hover must never hydrate a cloud-only placeholder.
        if metadata.file_attributes() & (0x1000 | 0x40000 | 0x400000) != 0 {
            return Err(cliphost_core::i18n::tr("请先将视频下载到本机", "Download this video locally first").into());
        }
        Ok(file)
    }
    fn video_time(time: Duration) -> String {
        let seconds = time.as_secs();
        if seconds >= 3600 { format!("{}:{:02}:{:02}", seconds / 3600, seconds / 60 % 60, seconds % 60) }
        else { format!("{}:{:02}", seconds / 60, seconds % 60) }
    }
    fn video_caption(name: &str, snapshot: &super::video_player::Snapshot) -> String {
        use cliphost_core::i18n::tr;
        let action = if snapshot.error.is_some() { tr("播放失败", "Playback failed") }
            else if snapshot.ended { tr("重新播放", "Replay") }
            else if snapshot.playing { tr("暂停", "Pause") }
            else { tr("播放", "Play") };
        let duration = snapshot.duration.map(|duration| format!(" / {}", video_time(duration))).unwrap_or_default();
        // Break long names for a narrow side popup, retaining the full path in
        // the underlying file row's original tooltip/projection.
        let name: String = name.chars().take(80).collect();
        format!("{name}\n{action} · {} · {}{duration}", tr("静音", "Muted"), video_time(snapshot.frame.timestamp))
    }
    fn image_file(host: &cliphost_core::HostState, source: &str) -> Result<PathBuf, String> {
        let url = url::Url::parse(source)
            .or_else(|_| {
                url::Url::parse(&format!("http://127.0.0.1:{}{}", host.api_port(), source))
            })
            .map_err(|e| e.to_string())?;
        let path = url
            .path()
            .strip_prefix("/__tauri-cliphost")
            .unwrap_or(url.path());
        if path == "/api/clip/local-image" {
            let local = url
                .query_pairs()
                .find(|(name, _)| name == "path")
                .map(|(_, value)| value.into_owned())
                .ok_or("Missing image path")?;
            return cliphost_core::shell_path::local_image_content_path(&local)
                .map(PathBuf::from)
                .ok_or("Hover image unavailable".into());
        }
        let pieces: Vec<_> = path.split('/').collect();
        if pieces.len() == 6 && pieces[1..3] == ["api", "clip"] && pieces[5] == "image" {
            let id = percent_decode(pieces[4])?;
            return match pieces[3] {
                "items" => host.hover_image_file(&id, false),
                "favorites" => host.hover_image_file(&id, true),
                _ => Err("Unsupported hover image".into()),
            };
        }
        Err("Unsupported hover image".into())
    }
    fn percent_decode(value: &str) -> Result<String, String> {
        let url =
            url::Url::parse(&format!("http://localhost/?id={value}")).map_err(|e| e.to_string())?;
        url.query_pairs()
            .next()
            .map(|(_, v)| v.into_owned())
            .ok_or("Missing image id".into())
    }
    #[cfg(test)]
    fn decode(bytes: &[u8], dark: bool) -> Result<Bitmap, String> {
        super::frames::decode(bytes, dark, || true)?.ok_or("Hover decode cancelled".into())
    }
    fn contains(rect: RECT, point: POINT) -> bool {
        point.x >= rect.left && point.x < rect.right && point.y >= rect.top && point.y < rect.bottom
    }
    fn placement(anchor: RECT, work: RECT, width: i32, height: i32, gap: i32) -> (i32, i32) {
        let x = if anchor.right + gap + width <= work.right {
            anchor.right + gap
        } else if anchor.left - gap - width >= work.left {
            anchor.left - gap - width
        } else {
            anchor.left
        };
        let y = if x == anchor.left && anchor.top - gap - height >= work.top {
            anchor.top - gap - height
        } else {
            anchor.top
        };
        (
            x.clamp(work.left, (work.right - width).max(work.left)),
            y.clamp(work.top, (work.bottom - height).max(work.top)),
        )
    }
    struct ImageSide {
        right: bool,
        edge: i32,
        width: i32,
    }
    impl ImageSide {
        fn position(&self, anchor: RECT, work: RECT, width: i32, height: i32) -> (i32, i32) {
            let x = if self.right { self.edge } else { self.edge - width };
            (x, anchor.top.clamp(work.top, (work.bottom - height).max(work.top)))
        }
    }
    fn image_side(owner: RECT, work: RECT, desired_width: i32, gap: i32) -> Option<ImageSide> {
        let right_edge = (owner.right + gap).max(work.left);
        let left_edge = (owner.left - gap).min(work.right);
        let right_width = (work.right - right_edge).max(0);
        let left_width = (left_edge - work.left).max(0);
        let right = if right_width >= desired_width { true }
            else if left_width >= desired_width { false }
            else { right_width >= left_width };
        let (edge, width) = if right { (right_edge, right_width) } else { (left_edge, left_width) };
        (width > 0).then_some(ImageSide { right, edge, width })
    }
    /// Side lock thresholds in DIP; see `Request::side_lock`.
    const COMFORT_DIP: f64 = 480.0;
    const USABLE_DIP: f64 = 240.0;
    const SWITCH_GAIN: f64 = 1.6;
    /// Physical room (right, left) between the owner's gap and the work edge.
    fn side_rooms(owner: RECT, work: RECT, gap: i32) -> (i32, i32) {
        ((work.right - owner.right - gap).max(0), (owner.left - gap - work.left).max(0))
    }
    /// A new session: right when it is comfortable or not the smaller side,
    /// independent of the image, so a session never starts on a cramped side.
    fn default_right(rooms: (i32, i32), scale: f64) -> bool {
        f64::from(rooms.0) >= COMFORT_DIP * scale || rooms.0 >= rooms.1
    }
    fn side_at(owner: RECT, work: RECT, gap: i32, right: bool) -> Option<ImageSide> {
        let (edge, width) = if right {
            let edge = (owner.right + gap).max(work.left);
            (edge, (work.right - edge).max(0))
        } else {
            let edge = (owner.left - gap).min(work.right);
            (edge, (edge - work.left).max(0))
        };
        (width > 0).then_some(ImageSide { right, edge, width })
    }
    unsafe fn placement_owner(show: &Presentation) -> Option<RECT> {
        if let Some(beside) = show.beside {
            return Some(beside);
        }
        let mut owner = RECT::default();
        GetWindowRect(handle(show.owner), &mut owner).ok().map(|_| owner)
    }
    /// Text also shrinks beside a menu; it must never use the generic tooltip
    /// fallback that may cover the anchor when both sides are occupied.
    fn menu_text_side(owner: RECT, work: RECT, gap: i32, scale: f64, locked: Option<bool>) -> Option<ImageSide> {
        let right = locked.unwrap_or_else(|| default_right(side_rooms(owner, work, gap), scale));
        side_at(owner, work, gap, right).or_else(|| side_at(owner, work, gap, !right))
    }
    fn beside_glide_safe(beside: Option<RECT>, from: RECT, to: (i32, i32), width: i32) -> bool {
        beside.is_none_or(|beside| {
            (from.left >= beside.right && to.0 >= beside.right)
                || (from.left.saturating_add(width) <= beside.left
                    && to.0.saturating_add(width) <= beside.left)
        })
    }
    /// Keep the locked side unless this image would be narrower than the
    /// usable width there and at least SWITCH_GAIN larger on the other side.
    #[allow(clippy::too_many_arguments)]
    fn locked_image_right(owner: RECT, work: RECT, gap: i32, scale: f64, locked: Option<bool>,
        image: (u32, u32), pad: i32, meta: i32, max_w: i32, max_h: i32) -> bool {
        let rooms = side_rooms(owner, work, gap);
        let mut right = locked.unwrap_or_else(|| default_right(rooms, scale));
        let fit = |room: i32| {
            let ratio = ((room.min(max_w) - 2 * pad).max(1) as f64 / f64::from(image.0.max(1)))
                .min((max_h - 2 * pad - meta).max(1) as f64 / f64::from(image.1.max(1)))
                .min(1.0);
            (f64::from(image.0) * ratio, f64::from(image.1) * ratio)
        };
        let (here, there) = if right { (fit(rooms.0), fit(rooms.1)) } else { (fit(rooms.1), fit(rooms.0)) };
        if here.0 < USABLE_DIP * scale && there.0 * there.1 >= here.0 * here.1 * SWITCH_GAIN * SWITCH_GAIN {
            right = !right;
        }
        right
    }
    unsafe fn clear_presentation(hwnd: HWND, ui: &Ui) {
        ui.render.lock().unwrap().clear();
        let _ = KillTimer(hwnd, 1);
        let _ = KillTimer(hwnd, 2);
        let _ = KillTimer(hwnd, 3);
        let _ = KillTimer(hwnd, 4);
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
    unsafe fn measure(
        content: &Content,
        max_width: i32,
        px: i32,
        surface: &mut Surface,
    ) -> (i32, i32) {
        if content.text.is_empty() {
            return (px, px);
        }
        if let Some(measured) = surface.measure_links(&content.text, max_width, px, content.truncated) {
            return measured;
        }
        let Ok(dc) = surface.ensure_font(px) else {
            return (px, px);
        };
        let mut natural = px;
        for line in content.text.split(|c| *c == 10) {
            let mut size = windows::Win32::Foundation::SIZE::default();
            let _ = GetTextExtentPoint32W(dc, line, &mut size);
            natural = natural.max(size.cx);
        }
        let width = natural.min(max_width).max(px);
        let mut rect = RECT {
            right: width,
            ..Default::default()
        };
        surface.draw_text(&content.text, &mut rect, TEXT_FLAGS | DT_CALCRECT);
        (width, rect.bottom.max(px))
    }
    unsafe fn measure_image_caption(
        content: &Content,
        max_width: i32,
        px: i32,
        surface: &mut Surface,
    ) -> Option<(i32, i32)> {
        if content.video.is_some() {
            // Two stable lines: filename and play/pause + muted/time. The
            // draw path ellipsizes each line, so long names cannot suppress
            // the preview or resize it on every timestamp update.
            return Some((max_width.min(px * 23), px * 3));
        }
        if content.text.is_empty() {
            return Some((0, 0));
        }
        if let Some(measured) = surface.measure_links(&content.text, max_width, px, content.truncated) {
            return Some(measured);
        }
        surface.ensure_font(px).ok()?;
        let mut rect = RECT { right: max_width, ..Default::default() };
        surface.draw_text(&content.text, &mut rect, TEXT_FLAGS | DT_CALCRECT);
        // DT_CALCRECT can widen the rectangle for an unbreakable word. It
        // cannot be displayed completely in this side's available space.
        (rect.right > 0 && rect.right <= max_width && rect.bottom > 0)
            .then_some((rect.right, rect.bottom))
    }
    unsafe fn apply(hwnd: HWND, ui: &Ui) {
        let pending = ui.shared.control.lock().unwrap().pending.take();
        match pending {
            Some(Pending::Hide) => {
                ui.render.lock().unwrap().clear();
                let _ = KillTimer(hwnd, 1);
                let _ = KillTimer(hwnd, 2);
                let _ = KillTimer(hwnd, 3);
                let _ = KillTimer(hwnd, 4);
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
            Some(Pending::Leave { generation, previous_generation, owner }) => {
                if !current(&ui.shared, generation) { return; }
                let mut render = ui.render.lock().unwrap();
                if render.shown.as_ref().is_some_and(|show|
                    show.owner == owner && show.generation == previous_generation)
                {
                    render.shown.as_mut().unwrap().generation = generation;
                    render.leaving = Some(Instant::now());
                    render.pressed_link = None;
                    render.pressed_video = None;
                    drop(render);
                    // The retained content now belongs to this leave ticket;
                    // refresh the paint/hit generation together.
                    draw(hwnd, ui, HDC::default());
                } else {
                    // A leave can replace the queued Hide/Show for a newer
                    // request. It must never revive the old rendered content.
                    render.clear();
                    drop(render);
                    let _ = KillTimer(hwnd, 1);
                    let _ = KillTimer(hwnd, 2);
                    let _ = KillTimer(hwnd, 3);
                    let _ = ShowWindow(hwnd, SW_HIDE);
                }
            }
            Some(Pending::Show(show)) => {
                if !current(&ui.shared, show.generation)
                    || !IsWindowVisible(handle(show.owner)).as_bool()
                {
                    return;
                }
                let monitor = MonitorFromRect(&show.anchor, MONITOR_DEFAULTTONEAREST);
                let mut info = MONITORINFO {
                    cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                    ..Default::default()
                };
                if !GetMonitorInfoW(monitor, &mut info).as_bool() {
                    return;
                }
                let work = info.rcWork;
                let shadow = super::style::shadow_padding(show.scale);
                let content_pad = (if show.content.media_dimensions().is_some() { 12.0 } else { 10.0 }
                    * show.scale).round() as i32;
                let pad = content_pad + shadow;
                let px = (14.0 * show.scale).round() as i32;
                let mut max_w = (work.right - work.left - 16)
                    .max(40)
                    .min((960.0 * show.scale) as i32);
                let max_h = (work.bottom - work.top - 16)
                    .max(40)
                    .min((760.0 * show.scale) as i32);
                let gap = (8.0 * show.scale).round() as i32;
                let locked = (show.side_lock && show.glide)
                    .then(|| *ui.side_lock.lock().unwrap())
                    .flatten()
                    .filter(|(owner, _)| *owner == show.owner)
                    .map(|(_, right)| right);
                let side = if let Some((image_width, image_height)) = show.content.media_dimensions() {
                    let Some(owner) = placement_owner(&show) else {
                        clear_presentation(hwnd, ui);
                        return;
                    };
                    let Some((caption_width, _)) = measure_image_caption(&show.content,
                        (max_w - 2 * pad).max(1), px, &mut ui.surface.lock().unwrap()) else {
                        clear_presentation(hwnd, ui);
                        return;
                    };
                    let desired = (image_width as i32).max(caption_width)
                        .saturating_add(2 * pad).min(max_w);
                    let chosen = if show.side_lock {
                        let right = locked_image_right(owner, work, gap, show.scale, locked,
                            (image_width, image_height), pad, px * 3, max_w, max_h);
                        side_at(owner, work, gap, right)
                    } else {
                        image_side(owner, work, desired, gap)
                    };
                    let Some(side) = chosen
                        .filter(|side| side.width > 2 * pad) else {
                        // Never fall back over the list when both sides are
                        // occupied. Also clear an older preview whose queued
                        // Hide may have been superseded by this Show.
                        clear_presentation(hwnd, ui);
                        return;
                    };
                    max_w = max_w.min(side.width);
                    Some(side)
                } else if show.beside.is_some() {
                    let Some(side) = placement_owner(&show)
                        .and_then(|owner| menu_text_side(owner, work, gap, show.scale, locked))
                        .filter(|side| side.width > 2 * pad) else {
                        clear_presentation(hwnd, ui);
                        return;
                    };
                    max_w = max_w.min(side.width);
                    Some(side)
                } else { None };
                let (width, height, image_size, max_offset) =
                    if let Some((image_width, image_height)) = show.content.media_dimensions() {
                        let Some((caption_width, caption_height)) = measure_image_caption(&show.content,
                            (max_w - 2 * pad).max(1), px, &mut ui.surface.lock().unwrap()) else {
                            clear_presentation(hwnd, ui);
                            return;
                        };
                        let meta = if caption_height == 0 { 0 } else { caption_height + content_pad };
                        if max_h <= 2 * pad + meta {
                            clear_presentation(hwnd, ui);
                            return;
                        }
                        let ratio = ((max_w - 2 * pad).max(1) as f64 / image_width as f64)
                            .min((max_h - 2 * pad - meta).max(1) as f64 / image_height as f64)
                            .min(1.0);
                        let w = (image_width as f64 * ratio).round().max(1.0) as i32;
                        let h = (image_height as f64 * ratio).round().max(1.0) as i32;
                        // Keep the measured caption width even for a small or
                        // height-limited image, so drawing cannot wrap it again.
                        (w.max(caption_width) + 2 * pad, h + 2 * pad + meta, (w, h), 0)
                    } else {
                        let max_width = max_w.min((520.0 * show.scale) as i32);
                        let (body_width, measured) =
                            measure(
                                &show.content,
                                (max_width - 2 * pad).max(1),
                                px,
                                &mut ui.surface.lock().unwrap(),
                            );
                        let width = body_width + 2 * pad;
                        let height = (measured + 2 * pad).min(max_h).max(2 * pad + px);
                        (width, height, (0, 0), (measured - height + 2 * pad).max(0))
                    };
                // Tests pause after real layout and before native publication.
                #[cfg(test)]
                if let Some(gate) = ui.before_present.lock().unwrap().take() {
                    gate.wait();
                    gate.wait();
                }
                // Layout can outlive cancellation or a newer owner's request.
                // Recheck before publishing render state or showing the HWND;
                // never hold the request mutex over GDI or native window calls.
                if !current(&ui.shared, show.generation)
                    || !IsWindowVisible(handle(show.owner)).as_bool()
                {
                    return;
                }
                let (x, y) = if side.is_some() {
                    // Layout may pump owner messages. Recheck its current
                    // bounds so a move cannot put the preview over the list.
                    let locked_right = side.as_ref().map(|side| side.right)
                        .filter(|_| show.side_lock || show.beside.is_some());
                    let fresh_side = placement_owner(&show)
                        .and_then(|owner| match locked_right {
                            Some(right) => side_at(owner, work, gap, right),
                            None => image_side(owner, work, width, gap),
                        })
                        .filter(|side| side.width >= width);
                    let Some(side) = fresh_side else {
                        clear_presentation(hwnd, ui);
                        return;
                    };
                    if show.side_lock {
                        *ui.side_lock.lock().unwrap() = Some((show.owner, side.right));
                    }
                    side.position(show.anchor, work, width, height)
                } else if show.side_lock {
                    // Text follows the same session side beside the owner.
                    let mut owner = RECT::default();
                    match GetWindowRect(handle(show.owner), &mut owner) {
                        Ok(()) => {
                            let rooms = side_rooms(owner, work, gap);
                            let mut right = locked.unwrap_or_else(|| default_right(rooms, show.scale));
                            let (here, there) = if right { rooms } else { (rooms.1, rooms.0) };
                            if here < width && there >= width {
                                right = !right;
                            }
                            match side_at(owner, work, gap, right).filter(|side| side.width >= width) {
                                Some(side) => {
                                    *ui.side_lock.lock().unwrap() = Some((show.owner, right));
                                    side.position(show.anchor, work, width, height)
                                }
                                None => placement(show.anchor, work, width, height, gap),
                            }
                        }
                        Err(_) => placement(show.anchor, work, width, height, gap),
                    }
                } else {
                    placement(show.anchor, work, width, height, gap)
                };
                let owner = show.owner;
                let generation = show.generation;
                let mut from = RECT::default();
                let glide = (show.glide && IsWindowVisible(hwnd).as_bool() && motion_enabled()
                    && GetWindowRect(hwnd, &mut from).is_ok() && (from.left, from.top) != (x, y)
                    && beside_glide_safe(show.beside, from, (x, y), width))
                    .then(|| (Instant::now(), (from.left, from.top), (x, y)));
                let fade = (show.glide && IsWindowVisible(hwnd).as_bool() && motion_enabled())
                    .then(Instant::now);
                ui.render.lock().unwrap().clear();
                *ui.render.lock().unwrap() = Render {
                    shown: Some(show),
                    offset: 0,
                    wheel_remainder: 0,
                    max_offset,
                    pad,
                    image_size,
                    leaving: None,
                    frame: 0,
                    animation_started: Some(Instant::now()),
                    scrolled: None,
                    pressed_link: None,
                    pressed_video: None,
                    video_revision: 0,
                    glide,
                    fade,
                };

                SetWindowLongPtrW(hwnd, GWLP_HWNDPARENT, owner);

                let (start_x, start_y) = glide.map_or((x, y), |(_, from, _)| from);
                let _ = SetWindowPos(hwnd, HWND_TOPMOST, start_x, start_y, width, height,
                    SWP_NOACTIVATE | SWP_NOOWNERZORDER);
                if glide.is_some() || fade.is_some() {
                    SetTimer(hwnd, 4, 10, None);
                }

                // A layered window has no displayed surface until its first
                // UpdateLayeredWindow. Publish before ShowWindow rather than
                // waiting for WM_PAINT to bootstrap that surface.
                draw(hwnd, ui, HDC::default());
                if !ui.render.lock().unwrap().shown.as_ref().is_some_and(|show| show.generation == generation) {
                    return;
                }
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);

                SetTimer(hwnd, 1, 50, None);
                animate(hwnd, ui);
            }
            None => {}
        }
    }
    /// Ease the window toward its warm-switch target and finish the fade.
    unsafe fn glide_tick(hwnd: HWND, ui: &Ui) {
        let mut render = ui.render.lock().unwrap();
        let mut running = false;
        if let Some((started, from, to)) = render.glide {
            let t = (started.elapsed().as_secs_f64() / GLIDE.as_secs_f64()).min(1.0);
            let eased = 1.0 - (1.0 - t).powi(3);
            let x = from.0 + ((to.0 - from.0) as f64 * eased).round() as i32;
            let y = from.1 + ((to.1 - from.1) as f64 * eased).round() as i32;
            let _ = SetWindowPos(hwnd, None, x, y, 0, 0,
                SWP_NOACTIVATE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOOWNERZORDER);
            if t >= 1.0 { render.glide = None; } else { running = true; }
        }
        let fading = render.fade.is_some_and(|started| started.elapsed() < FADE);
        if !fading { render.fade = None; }
        drop(render);
        draw(hwnd, ui, HDC::default());
        if !running && !fading { let _ = KillTimer(hwnd, 4); }
    }
    unsafe fn animate(hwnd: HWND, ui: &Ui) {
        let mut render = ui.render.lock().unwrap();
        if let Some(video) = render.shown.as_ref().and_then(|show| show.content.video.as_ref()) {
            let snapshot = video.player.snapshot();
            let valid = render.shown.as_ref().is_some_and(|show|
                IsWindowVisible(hwnd).as_bool() && IsWindowVisible(handle(show.owner)).as_bool()
                    && shown_valid(&ui.shared, show.generation));
            let changed = render.video_revision != snapshot.revision;
            render.video_revision = snapshot.revision;
            drop(render);
            if !valid { clear_presentation(hwnd, ui); return; }
            if changed { draw(hwnd, ui, HDC::default()); }
            if snapshot.playing { SetTimer(hwnd, 2, 33, None); }
            else { let _ = KillTimer(hwnd, 2); }
            return;
        }
        let result = render.shown.as_ref().and_then(|show| {
            if !IsWindowVisible(hwnd).as_bool() || !IsWindowVisible(handle(show.owner)).as_bool()
                || !shown_valid(&ui.shared, show.generation) { return None; }
            let image = show.content.image.as_ref()?;
            let elapsed = render.animation_started?.elapsed();
            Some(image.frame_at(elapsed))
        });
        let Some((frame, next)) = result else { let _ = KillTimer(hwnd, 2); return; };
        let changed = render.frame != frame;
        render.frame = frame;
        drop(render);
        if changed { draw(hwnd, ui, HDC::default()); }
        if let Some(delay) = next {
            let milliseconds = delay.as_nanos().div_ceil(1_000_000).clamp(10, i32::MAX as u128) as u32;
            SetTimer(hwnd, 2, milliseconds, None);
        } else { let _ = KillTimer(hwnd, 2); }
    }
    /// Schedule the next overlay-scrollbar repaint (fade or idle deadline).
    unsafe fn scrollbar_tick(hwnd: HWND, ui: &Ui) {
        let next = {
            let render = ui.render.lock().unwrap();
            render.shown.as_ref().and(render.scrolled).and_then(|at| {
                super::style::scrollbar_visibility(at.elapsed().as_secs_f64() * 1000.0).1
            })
        };
        match next {
            Some(milliseconds) => {
                SetTimer(hwnd, 3, milliseconds.max(10), None);
            }
            None => {
                let _ = KillTimer(hwnd, 3);
            }
        }
    }
    unsafe fn paint(hwnd: HWND, ui: &Ui) {
        let mut ps = PAINTSTRUCT::default();
        let _ = BeginPaint(hwnd, &mut ps);
        draw(hwnd, ui, HDC::default());
        let _ = EndPaint(hwnd, &ps);
    }
    unsafe fn link_at(hwnd: HWND, ui: &Ui, point: POINT) -> Option<(u64, String, LinkOpener, isize)> {
        if !IsWindowVisible(hwnd).as_bool() { return None; }
        let render = ui.render.lock().unwrap();
        let show = render.shown.as_ref()?;
        if !shown_valid(&ui.shared, show.generation) || !IsWindowVisible(handle(show.owner)).as_bool() {
            return None;
        }
        let uri = ui.surface.lock().unwrap().hit_link(show.generation, point.x, point.y)?;
        Some((show.generation, uri, show.content.opener.clone()?, show.owner))
    }
    fn mouse_point(lp: LPARAM) -> POINT {
        POINT { x: lp.0 as i16 as i32, y: (lp.0 >> 16) as i16 as i32 }
    }
    unsafe fn video_at(hwnd: HWND, ui: &Ui, point: POINT) -> Option<(u64, Arc<super::video_player::Player>)> {
        if !IsWindowVisible(hwnd).as_bool() { return None; }
        let render = ui.render.lock().unwrap();
        let show = render.shown.as_ref()?;
        if !shown_valid(&ui.shared, show.generation) || !IsWindowVisible(handle(show.owner)).as_bool() { return None; }
        let mut bounds = RECT::default();
        GetClientRect(hwnd, &mut bounds).ok()?;
        let pad = render.pad;
        if !contains(RECT { left: pad, top: pad, right: bounds.right - pad, bottom: bounds.bottom - pad }, point) { return None; }
        Some((show.generation, show.content.video.as_ref()?.player.clone()))
    }
    unsafe fn press_video(hwnd: HWND, ui: &Ui, point: POINT) {
        let pressed = video_at(hwnd, ui, point).map(|(generation, _)| generation);
        ui.render.lock().unwrap().pressed_video = pressed;
    }
    unsafe fn release_video(hwnd: HWND, ui: &Ui, point: POINT) {
        let pressed = ui.render.lock().unwrap().pressed_video.take();
        let Some((generation, player)) = video_at(hwnd, ui, point) else { return; };
        if pressed != Some(generation) { return; }
        player.toggle();
        draw(hwnd, ui, HDC::default());
        animate(hwnd, ui);
    }
    unsafe fn press_link(hwnd: HWND, ui: &Ui, point: POINT) {
        let pressed = link_at(hwnd, ui, point).map(|(generation, uri, _, _)| (generation, uri));
        ui.render.lock().unwrap().pressed_link = pressed;
    }
    unsafe fn release_link(hwnd: HWND, ui: &Ui, point: POINT) {
        let pressed = ui.render.lock().unwrap().pressed_link.take();
        let Some((generation, uri, opener, owner)) = link_at(hwnd, ui, point) else { return; };
        if pressed != Some((generation, uri.clone())) { return; }
        let shared = ui.shared.clone();
        // Shell execution may message other applications and must not run in
        // the popup dispatcher, or under render/surface/request mutexes.
        let _ = std::thread::Builder::new().name("hover-open-uri".into()).spawn(move || {
            // Cancellation while the accepted click was queued still wins.
            if current(&shared, generation) && unsafe { IsWindowVisible(handle(owner)).as_bool() } {
                if let Err(error) = opener(&uri) { eprintln!("Opening hover URL failed: {error}"); }
            }
        });
    }
    unsafe fn draw(hwnd: HWND, ui: &Ui, dc: HDC) {
        let mut bounds = RECT::default();
        let _ = GetClientRect(hwnd, &mut bounds);

        let mut render = ui.render.lock().unwrap();

        if let Some(show) = &render.shown {
            let video_snapshot = show.content.video.as_ref().map(|video| video.player.snapshot());
            let mut surface = ui.surface.lock().unwrap();
            let px = (14.0 * show.scale).round() as i32;
            let Ok(buffer) = surface.prepare_frame(bounds.right, bounds.bottom, px,
                show.scale, show.content.dark) else {
                return;
            };
            let pad = render.pad;
            let content_bounds = RECT {
                left: pad, top: pad, right: bounds.right - pad, bottom: bounds.bottom - pad,
            };
            let _ = IntersectClipRect(buffer, content_bounds.left, content_bounds.top,
                content_bounds.right, content_bounds.bottom);
            SetBkMode(buffer, TRANSPARENT);
            SetTextColor(
                buffer,
                if show.content.dark {
                    COLORREF(0x00f0f0f0)
                } else {
                    COLORREF(0x001c1c1c)
                },
            );
            let media = video_snapshot.as_ref().map(|snapshot|
                (snapshot.frame.width, snapshot.frame.height, snapshot.frame.pixels.as_slice()))
                .or_else(|| show.content.image.as_ref().map(|image|
                    (image.width, image.height, image.pixels_at(render.frame))));
            let top = if let Some((source_width, source_height, pixels)) = media {
                let bmi = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: source_width as i32,
                        biHeight: -(source_height as i32),
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: BI_RGB.0,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                SetStretchBltMode(buffer, HALFTONE);
                let _ = SetBrushOrgEx(buffer, 0, 0, None);
                StretchDIBits(
                    buffer,
                    (bounds.right - render.image_size.0) / 2,
                    pad,
                    render.image_size.0,
                    render.image_size.1,
                    0,
                    0,
                    source_width as i32,
                    source_height as i32,
                    Some(pixels.as_ptr() as *const _),
                    &bmi,
                    DIB_RGB_COLORS,
                    SRCCOPY,
                );
                if video_snapshot.as_ref().is_some_and(|snapshot| !snapshot.playing) {
                    let center_x = bounds.right / 2;
                    let center_y = pad + render.image_size.1 / 2;
                    let radius = (24.0 * show.scale).round() as i32;
                    let brush = SelectObject(buffer, GetStockObject(BLACK_BRUSH));
                    let pen = SelectObject(buffer, GetStockObject(NULL_PEN));
                    let _ = Ellipse(buffer, center_x - radius, center_y - radius, center_x + radius, center_y + radius);
                    SelectObject(buffer, GetStockObject(WHITE_BRUSH));
                    let unit = (9.0 * show.scale).round() as i32;
                    let points = [POINT { x: center_x - unit / 2, y: center_y - unit },
                        POINT { x: center_x - unit / 2, y: center_y + unit },
                        POINT { x: center_x + unit, y: center_y }];
                    let _ = Polygon(buffer, &points);
                    SelectObject(buffer, brush);
                    SelectObject(buffer, pen);
                }

                pad + render.image_size.1 + pad - super::style::shadow_padding(show.scale)
            } else {
                pad - render.offset
            };
            let mut rect = RECT {
                left: pad,
                top,
                right: bounds.right - pad,
                bottom: bounds.bottom - pad + render.max_offset,
            };
            // An image without metadata has no text box; don't pass its inverted
            // rectangle to DrawTextW (DT_WORDBREAK can hang on it).
            if let (Some(video), Some(snapshot)) = (&show.content.video, &video_snapshot) {
                let caption = video_caption(&video.name, snapshot);
                for (index, line) in caption.lines().take(2).enumerate() {
                    let mut line_rect = RECT { top: top + index as i32 * px * 3 / 2,
                        bottom: top + (index as i32 + 1) * px * 3 / 2, ..rect };
                    surface.draw_text(&text(line), &mut line_rect, DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX);
                }
            } else if !show.content.text.is_empty() && !surface.draw_links(&show.content.text,
                bounds, rect, content_bounds, px, show.content.dark, show.generation, show.content.truncated) {
                surface.draw_text(&show.content.text, &mut rect, TEXT_FLAGS);
            }

            surface.finish_content(content_bounds);
            if show.content.image.is_none() && render.max_offset > 0 {
                if let Some(scrolled) = render.scrolled {
                    let (shown, _) = super::style::scrollbar_visibility(
                        scrolled.elapsed().as_secs_f64() * 1000.0,
                    );
                    let scale = show.scale;
                    let shadow = super::style::shadow_padding(scale) as f64;
                    let inset = super::style::SCROLLBAR_INSET_DIP * scale;
                    let width = super::style::SCROLLBAR_THUMB_DIP * scale;
                    let right = bounds.right as f64 - shadow - inset;
                    if let Some((top, height)) = super::style::scroll_thumb(
                        shadow + inset,
                        bounds.bottom as f64 - shadow - inset,
                        (bounds.bottom - 2 * pad) as f64,
                        render.max_offset as f64,
                        render.offset as f64,
                        super::style::SCROLLBAR_MIN_THUMB_DIP * scale,
                    ) {
                        let ink = if show.content.dark { [240, 240, 240] } else { [28, 28, 28] };
                        surface.blend_capsule(
                            (right - width, top, right, top + height),
                            ink,
                            super::style::SCROLLBAR_ALPHA * shown,
                        );
                    }
                }
            }
            if dc.is_invalid() {
                if !shown_valid(&ui.shared, show.generation) || !IsWindowVisible(handle(show.owner)).as_bool() {
                    surface.clear_link_hit();
                    drop(surface);
                    render.clear();
                    render.pressed_link = None;
                    drop(render);
                    clear_presentation(hwnd, ui);
                    return;
                }
                let alpha = render.fade.map_or(255, |started| {
                    let t = (started.elapsed().as_secs_f64() / FADE.as_secs_f64()).min(1.0);
                    (255.0 * (0.4 + 0.6 * t)).round() as u8
                });
                let blend = BLENDFUNCTION {
                    BlendOp: AC_SRC_OVER as u8, BlendFlags: 0,
                    SourceConstantAlpha: alpha, AlphaFormat: AC_SRC_ALPHA as u8,
                };
                let size = windows::Win32::Foundation::SIZE { cx: bounds.right, cy: bounds.bottom };
                if let Err(error) = UpdateLayeredWindow(hwnd, None, None, Some(&size), buffer,
                    Some(&POINT::default()), COLORREF(0), Some(&blend), ULW_ALPHA)
                {
                    tracing::warn!(%error, "Could not present hover popup");
                    #[cfg(test)]
                    eprintln!("Hover layered presentation failed: {error}; bounds={bounds:?}");
                    surface.clear_link_hit();
                    drop(surface);
                    render.clear();
                    render.pressed_link = None;
                    drop(render);
                    clear_presentation(hwnd, ui);
                    return;
                }
                surface.commit_link_hit();
                #[cfg(test)]
                ui.shared.painted.store(show.generation, std::sync::atomic::Ordering::SeqCst);
            } else {
                // PrintWindow captures the same buffer without changing the
                // popup's presentation or position.
                let _ = BitBlt(dc, 0, 0, bounds.right, bounds.bottom, buffer, 0, 0, SRCCOPY);
            }

        }
        drop(render);
    }
    unsafe extern "system" fn wndproc(hwnd: HWND, message: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        if message == WM_NCCREATE {
            let cs = &*(lp.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
        }
        let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Ui;
        if !pointer.is_null() {
            let ui = &*pointer;
            if message == WM_NCDESTROY {
                retire_ui(&ui.shared, hwnd, None);
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                if GetPropW(hwnd, INSTANCE_PROPERTY) == instance(&ui.shared) {
                    let _ = RemovePropW(hwnd, INSTANCE_PROPERTY);
                }
                return DefWindowProcW(hwnd, message, wp, lp);
            }
            let close = {
                let mut state = ui.shared.control.lock().unwrap();
                let close = message != WM_DESTROY && state.closed && !state.destroying;
                if close { state.destroying = true; }
                close
            };
            if close {
                // A queue-full PostMessage failure still leaves queued messages.
                // Their next dispatch closes this exact instance on its UI STA.
                // DestroyWindow sends synchronous teardown messages; prevent
                // those reentrant messages from requesting destruction again.
                if let Err(error) = DestroyWindow(hwnd) {
                    ui.shared.control.lock().unwrap().terminal_error = Some(format!("Destroying hover window failed: {error}"));
                    PostQuitMessage(1);
                }
                return LRESULT(0);
            }
            match message {
                UPDATE => {
                    apply(hwnd, ui);
                    return LRESULT(0);
                }
                WM_ERASEBKGND => return LRESULT(1),
                WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
                WM_SETCURSOR => {
                    let mut point = POINT::default();
                    if GetCursorPos(&mut point).is_ok() {
                        let _ = ScreenToClient(hwnd, &mut point);
                        let cursor = if link_at(hwnd, ui, point).is_some() || video_at(hwnd, ui, point).is_some() { IDC_HAND } else { IDC_ARROW };
                        if let Ok(cursor) = LoadCursorW(None, cursor) {
                            SetCursor(cursor);
                            return LRESULT(1);
                        }
                    }
                }
                WM_LBUTTONDOWN => {
                    press_video(hwnd, ui, mouse_point(lp));
                    press_link(hwnd, ui, mouse_point(lp));
                    return LRESULT(0);
                }
                WM_LBUTTONUP => {
                    release_video(hwnd, ui, mouse_point(lp));
                    release_link(hwnd, ui, mouse_point(lp));
                    return LRESULT(0);
                }
                WM_CANCELMODE | WM_CAPTURECHANGED | WM_RBUTTONDOWN | WM_MBUTTONDOWN => {
                    let mut render = ui.render.lock().unwrap();
                    render.pressed_link = None;
                    render.pressed_video = None;
                }
                WM_PAINT => {
                    paint(hwnd, ui);
                    return LRESULT(0);
                }
                WM_PRINTCLIENT => {
                    // Render to the caller's DC too: window capture must not
                    // depend on DWM retaining pixels for a cloaked client.
                    draw(hwnd, ui, HDC(wp.0 as *mut _));
                    return LRESULT(0);
                }
                WM_MOUSEWHEEL => {
                    let delta = ((wp.0 >> 16) as u16) as i16;
                    let changed = {
                        let mut render = ui.render.lock().unwrap();
                        render.pressed_link = None;
                        render.pressed_video = None;
                        let changed = render.scroll(delta);
                        if changed {
                            // Keep a visible bar steady; restart fade-in only from hidden.
                            let since = render.scrolled.map(|at| at.elapsed().as_secs_f64() * 1000.0);
                            let visible = since.is_some_and(|ms| super::style::scrollbar_visibility(ms).0 > 0.0);
                            let now = Instant::now();
                            // Past the 90 ms fade-in: fully shown, idle timer restarted.
                            render.scrolled = Some(if visible {
                                now.checked_sub(Duration::from_millis(90)).unwrap_or(now)
                            } else {
                                now
                            });
                        }
                        changed
                    };
                    if changed {
                        draw(hwnd, ui, HDC::default());
                        scrollbar_tick(hwnd, ui);
                    }
                    return LRESULT(0);
                }
                WM_TIMER => {
                    if wp.0 == 4 {
                        glide_tick(hwnd, ui);
                        return LRESULT(0);
                    }
                    if wp.0 == 3 {
                        draw(hwnd, ui, HDC::default());
                        scrollbar_tick(hwnd, ui);
                        return LRESULT(0);
                    }
                    if wp.0 == 2 {
                        #[cfg(test)]
                        ui.shared.animation_ticks.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        animate(hwnd, ui);
                        return LRESULT(0);
                    }
                    let mut render = ui.render.lock().unwrap();
                    if let Some(show) = &render.shown {
                        let mut point = POINT::default();
                        let _ = GetCursorPos(&mut point);
                        let mut rect = RECT::default();
                        let _ = GetWindowRect(hwnd, &mut rect);
                        let hit = WindowFromPoint(point);
                        let host = handle(show.owner);
                        let in_anchor = contains(show.anchor, point)
                            && (hit == host
                                || IsChild(host, hit).as_bool()
                                || GetAncestor(hit, GA_ROOT) == host);
                        let in_popup = contains(rect, point) && hit == hwnd;
                        if !IsWindowVisible(host).as_bool() || !shown_valid(&ui.shared, show.generation)
                        {
                            render.leaving = Some(Instant::now() - Duration::from_secs(1));
                        } else if in_anchor || in_popup {
                            render.leaving = None;
                        } else if render.leaving.is_none() {
                            render.leaving = Some(Instant::now());
                        }
                        if render
                            .leaving
                            .is_some_and(|time| time.elapsed() >= Duration::from_millis(180))
                        {
                            render.clear();
                            drop(render);
                            let _ = KillTimer(hwnd, 1);
                            let _ = KillTimer(hwnd, 2);
                            let _ = KillTimer(hwnd, 3);
                            let _ = ShowWindow(hwnd, SW_HIDE);
                        }
                    }
                    return LRESULT(0);
                }
                WM_DESTROY => {
                    retire_ui(&ui.shared, hwnd, None);
                    PostQuitMessage(0);
                    return LRESULT(0);
                }
                _ => {}
            }
        }
        DefWindowProcW(hwnd, message, wp, lp)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::io::Cursor;
        fn rect(hwnd: HWND) -> RECT {
            unsafe {
                let mut r = RECT::default();
                GetWindowRect(hwnd, &mut r).unwrap();
                r
            }
        }
        fn wait(mut accept: impl FnMut() -> bool) {
            let until = Instant::now() + Duration::from_secs(2);
            while !accept() {
                // The test owns a Win32 window too: pump its queue like Tauri does.
                // Cross-thread owner changes may synchronously message this thread.
                unsafe {
                    let mut message = MSG::default();
                    while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                        let _ = TranslateMessage(&message);
                        DispatchMessageW(&message);
                    }
                }
                assert!(Instant::now() < until, "native hover condition timed out");
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        fn request(action: &str, ticket: u64, text: &str) -> Request {
            Request {
                action: action.into(),
                ticket,
                anchor: [10.0, 10.0, 140.0, 30.0],
                image_src: None,
                text: Some(text.into()),
                meta_line: None,
                dark: true,
                force: true,
                keep: false,
                side_lock: false,
            }
        }
        struct Owner(HWND);
        impl Drop for Owner {
            fn drop(&mut self) {
                unsafe {
                    let _ = DestroyWindow(self.0);
                }
            }
        }

        fn lifecycle_owner() -> Owner {
            unsafe {
                let hwnd = CreateWindowExW(WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                    w!("STATIC"), w!("Hover lifecycle owned fixture"), WS_POPUP,
                    200, 100, 240, 180, None, None, GetModuleHandleW(None).unwrap(), None).unwrap();
                let cloak = windows::Win32::Foundation::BOOL(1);
                DwmSetWindowAttribute(hwnd, DWMWA_CLOAK, &cloak as *const _ as *const _, 4).unwrap();
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                // Pump this owner's first STATIC paint before process GDI
                // baselines; its lazy system font is not a popup allocation.
                let _ = UpdateWindow(hwnd);
                Owner(hwnd)
            }
        }
        fn lifecycle_host() -> (tempfile::TempDir, cliphost_core::HostState) {
            std::env::set_var("CLIPHOST_TAURI_HEADLESS", "1");
            let audit = std::env::var_os("CLIPHOST_HOVER_TEST_AUDIT_DIR")
                .map(PathBuf::from).unwrap_or_else(|| std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../.tmp/hover-native-thread-lifecycle"));
            std::fs::create_dir_all(&audit).unwrap();
            let data = tempfile::tempdir_in(audit).unwrap();
            let host = cliphost_core::HostState::open(
                cliphost_core::AppPaths::from_data_root(data.path().join("data")), 29065,
            ).unwrap();
            (data, host)
        }
        fn finish_lifecycle_thread(service: &Service) {
            let worker = service.worker.lock().unwrap().take().unwrap();
            wait(|| worker.is_finished());
            worker.join().unwrap();
        }
        struct ReleasePause(Arc<DecodePause>);
        impl Drop for ReleasePause {
            fn drop(&mut self) { self.0.release(); }
        }
        fn lifecycle_image_request(path: &std::path::Path, ticket: u64) -> Request {
            let mut source = url::Url::parse("http://127.0.0.1:29065/api/clip/local-image").unwrap();
            source.query_pairs_mut().append_pair("path", &path.to_string_lossy());
            let mut value = request("image", ticket, "");
            value.image_src = Some(source.to_string());
            value
        }

        #[test]
        fn native_thread_lifecycle_quit_destroys_window_links_and_gdi_before_release() {
            use super::windows::Win32::System::Threading::{GetCurrentProcess, GetGuiResources, GR_GDIOBJECTS};
            let (_data, host) = lifecycle_host();
            let owner = lifecycle_owner();
            let baseline = unsafe { GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS) };
            for _ in 0..3 {
                let service = Service::new().unwrap();
                service.bind_owner(owner.0.0 as isize).unwrap();
                test_runtime::block_on(service.request(owner.0.0 as isize, 1.0,
                    request("text", 1, "真实退出回归：圆角文字窗口\n中文 🦀"), host.clone())).unwrap();
                wait(|| service.shared.painted.load(std::sync::atomic::Ordering::SeqCst) != 0);
                let hwnd = handle(service.hwnd);
                let thread = service.shared.control.lock().unwrap().ui_thread;
                unsafe {
                    assert_ne!(GetWindowLongPtrW(hwnd, GWL_EXSTYLE) & WS_EX_NOACTIVATE.0 as isize, 0);
                    assert_ne!(GetForegroundWindow(), hwnd);
                    PostThreadMessageW(thread, WM_QUIT, WPARAM(0), LPARAM(0)).unwrap();
                }
                finish_lifecycle_thread(&service);
                assert!(!service.is_alive());
                unsafe {
                    assert!(!IsWindow(hwnd).as_bool());
                    assert!(GetPropW(owner.0, OWNER_POPUP_PROPERTY).0.is_null());
                    assert!(GetPropW(owner.0, OWNER_INSTANCE_PROPERTY).0.is_null());
                    assert_eq!(GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS), baseline);
                }
                let state = service.shared.control.lock().unwrap();
                assert!(state.closed && state.ui_exited);
                assert_eq!(state.live_hwnd, 0);
                assert_eq!(state.owner, 0);
                assert!(state.pending.is_none() && state.tickets.is_empty() && state.owners.is_empty());
                drop(state);
                assert!(service.bind_owner(owner.0.0 as isize).is_err());
                assert!(service.shutdown_checked().is_err());
                assert!(test_runtime::block_on(service.request(owner.0.0 as isize, 1.0,
                    request("text", 2, "不得迟到显示"), host.clone())).is_err());
            }
        }

        #[test]
        fn native_thread_lifecycle_getmessage_error_and_lost_ready_destroy_owned_window() {
            for abandoned in [false, true] {
                let shared = shared_state();
                let (ready, receive) = std::sync::mpsc::sync_channel(1);
                if abandoned { drop(receive); }
                else {
                    // Keep receiver alive through creation and discard the ready
                    // value only after the actual invalid-filter loop has exited.
                    let ui_shared = shared.clone();
                    let worker = std::thread::spawn(move || unsafe { run_ui(ui_shared, ready, -42); });
                    let hwnd = receive.recv_timeout(Duration::from_secs(2)).unwrap().unwrap();
                    wait(|| worker.is_finished());
                    worker.join().unwrap();
                    assert!(!unsafe { IsWindow(handle(hwnd)).as_bool() });
                    assert!(shared.control.lock().unwrap().terminal_error.as_deref().unwrap().contains("GetMessage"));
                    continue;
                }
                let ui_shared = shared.clone();
                let worker = std::thread::spawn(move || unsafe { run_ui(ui_shared, ready, 0); });
                wait(|| worker.is_finished());
                worker.join().unwrap();
                let hwnd = shared.created_hwnd.load(std::sync::atomic::Ordering::SeqCst);
                assert_ne!(hwnd, 0);
                assert!(!unsafe { IsWindow(handle(hwnd)).as_bool() });
                let state = shared.control.lock().unwrap();
                assert!(state.closed && state.ui_exited && state.pending.is_none());
                assert_eq!(state.live_hwnd, 0);
                assert!(state.terminal_error.as_deref().unwrap().contains("readiness receiver"));
            }
        }

        #[test]
        fn native_thread_lifecycle_old_shutdown_cannot_unlink_or_close_replacement() {
            let owner = lifecycle_owner();
            let (_data, host) = lifecycle_host();
            let old = Service::new().unwrap();
            old.bind_owner(owner.0.0 as isize).unwrap();
            let new = Service::new().unwrap();
            new.bind_owner(owner.0.0 as isize).unwrap();
            old.shutdown_checked().unwrap();
            finish_lifecycle_thread(&old);
            assert!(old.shutdown_checked().is_err());
            unsafe {
                assert_eq!(GetPropW(owner.0, OWNER_POPUP_PROPERTY).0, handle(new.hwnd).0);
                assert_eq!(GetPropW(owner.0, OWNER_INSTANCE_PROPERTY), instance(&new.shared));
            }
            assert!(new.is_alive());
            test_runtime::block_on(new.request(owner.0.0 as isize, 1.0,
                request("text", 1, "replacement remains alive"), host)).unwrap();
            wait(|| new.shared.painted.load(std::sync::atomic::Ordering::SeqCst) != 0);
            new.shutdown_checked().unwrap();
            finish_lifecycle_thread(&new);
        }

        #[test]
        fn native_thread_lifecycle_close_releases_waiters_and_rejects_late_real_decode() {
            let (data, host) = lifecycle_host();
            let owner = lifecycle_owner();
            let file = data.path().join("late-real-decode.png");
            image::RgbaImage::from_pixel(64, 48, image::Rgba([20, 90, 160, 255])).save(&file).unwrap();
            let service = Arc::new(Service::new().unwrap());
            service.bind_owner(owner.0.0 as isize).unwrap();
            let pause = Arc::new(DecodePause::default());
            let _release = ReleasePause(pause.clone());
            *service.shared.before_cache.lock().unwrap() = Some(pause.clone());
            let task = {
                let service = service.clone();
                let host = host.clone();
                let owner = owner.0.0 as isize;
                let value = lifecycle_image_request(&file, 1);
                test_runtime::spawn(async move { service.request(owner, 1.0, value, host).await })
            };
            wait(|| pause.reached.load(std::sync::atomic::Ordering::SeqCst) || task.is_finished());
            assert!(pause.reached.load(std::sync::atomic::Ordering::SeqCst), "real image decode did not reach publication gate");
            let thread = service.shared.control.lock().unwrap().ui_thread;
            unsafe { PostThreadMessageW(thread, WM_QUIT, WPARAM(0), LPARAM(0)).unwrap(); }
            finish_lifecycle_thread(&service);
            // The request must finish even while its real blocking decode job
            // is paused after decoding and before cache/Show publication.
            wait(|| task.is_finished());
            assert!(test_runtime::block_on(task).unwrap().is_err());
            pause.release();
            wait(|| service.decoder.available_permits() == 1);
            assert!(service.shared.cache.lock().unwrap().entries.is_empty());
            assert!(service.shared.control.lock().unwrap().pending.is_none());
            assert_eq!(service.shared.painted.load(std::sync::atomic::Ordering::SeqCst), 0);
        }

        #[test]
        fn native_thread_lifecycle_queue_full_post_returns_error_and_still_finalizes() {
            let service = Service::new().unwrap();
            let pause = Arc::new(DecodePause::default());
            let _release = ReleasePause(pause.clone());
            *service.shared.before_dispatch.lock().unwrap() = Some(pause.clone());
            let hwnd = handle(service.hwnd);
            unsafe { PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0)).unwrap(); }
            wait(|| pause.reached.load(std::sync::atomic::Ordering::SeqCst));
            let mut filled = false;
            for _ in 0..65_536 {
                if unsafe { PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0)) }.is_err() { filled = true; break; }
            }
            assert!(filled, "self-owned UI queue did not reach its bounded OS quota");
            let error = service.wake().unwrap_err();
            assert!(error.contains("message dispatch failed"));
            assert!(!service.is_alive());
            pause.release();
            finish_lifecycle_thread(&service);
            assert!(!unsafe { IsWindow(hwnd).as_bool() });
            assert!(service.shared.control.lock().unwrap().ui_exited);
        }

        // Keep the renderer on this test thread so the request queue can be
        // deliberately coalesced before a UI update. Use the real request/apply
        // code and cloaked Win32 windows, without cursor/desktop input.
        struct LeaveFixture {
            svc: Service,
            ui: Ui,
            host: cliphost_core::HostState,
            popup: Owner,
            first: Owner,
            second: Owner,
            _data: tempfile::TempDir,
        }
        impl LeaveFixture {
            fn new() -> Self {
                std::env::set_var("CLIPHOST_TAURI_HEADLESS", "1");
                let audit = std::env::var_os("CLIPHOST_HOVER_TEST_AUDIT_DIR")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join("../../.tmp/cliphost-hover-leave-race-20261001/native"));
                std::fs::create_dir_all(&audit).unwrap();
                let data = tempfile::tempdir_in(audit).unwrap();
                let host = cliphost_core::HostState::open(
                    cliphost_core::AppPaths::from_data_root(data.path().join("data")), 28965,
                ).unwrap();
                let make = || unsafe {
                    let hwnd = CreateWindowExW(WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                        w!("STATIC"), w!("Hover leave queue test"), WS_POPUP, 200, 100, 200, 160,
                        None, None, GetModuleHandleW(None).unwrap(), None).unwrap();
                    let cloak = windows::Win32::Foundation::BOOL(1);
                    DwmSetWindowAttribute(hwnd, DWMWA_CLOAK, &cloak as *const _ as *const _, 4).unwrap();
                    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                    Owner(hwnd)
                };
                let (popup, first, second) = (make(), make(), make());
                unsafe {
                    SetWindowLongPtrW(popup.0, GWL_EXSTYLE,
                        GetWindowLongPtrW(popup.0, GWL_EXSTYLE) | WS_EX_LAYERED.0 as isize);
                }
                let shared = shared_state();
                {
                    let mut state = shared.control.lock().unwrap();
                    state.live_hwnd = popup.0.0 as isize;
                    state.ui_thread = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
                }
                unsafe { SetPropW(popup.0, INSTANCE_PROPERTY, instance(&shared)).unwrap(); }
                let svc = Service {
                    shared: shared.clone(), hwnd: popup.0.0 as isize,
                    decoder: Arc::new(tokio::sync::Semaphore::new(1)),
                    resolver: Arc::new(tokio::sync::Semaphore::new(0)),
                    worker: Mutex::new(None),
                };
                let ui = Ui { shared, render: Mutex::new(Render::default()), surface: Mutex::new(Surface::default()), side_lock: Mutex::new(None), before_present: Mutex::new(None) };
                let fixture = Self { svc, ui, host, popup, first, second, _data: data };
                test_runtime::block_on(fixture.svc.request(
                    fixture.first.0.0 as isize, 1.0, request("text", 1, "original preview"), fixture.host.clone(),
                )).unwrap();
                unsafe { apply(fixture.popup.0, &fixture.ui); }
                assert!(unsafe { IsWindowVisible(fixture.popup.0).as_bool() });
                assert_eq!(fixture.ui.render.lock().unwrap().shown.as_ref().unwrap().content.text, text("original preview"));
                fixture
            }
            fn leave(&self, owner: isize, ticket: u64, force: bool) {
                let mut leave = request("hide", ticket, ""); leave.force = force;
                test_runtime::block_on(self.svc.request(owner, 1.0, leave, self.host.clone())).unwrap();
            }
            fn cancel_new_image(&self, owner: isize, ticket: u64) {
                test_runtime::block_on(async {
                    let mut image = request("image", ticket, ""); image.image_src = Some("never-read".into());
                    let pending = self.svc.request(owner, 1.0, image, self.host.clone());
                    tokio::pin!(pending);
                    tokio::select! {
                        biased;
                        result = &mut pending => panic!("resolver gate unexpectedly completed: {result:?}"),
                        _ = tokio::time::sleep(Duration::from_millis(1)) => {}
                    }
                    assert!(matches!(self.svc.shared.control.lock().unwrap().pending, Some(Pending::Hide)));
                    let mut leave = request("hide", ticket + 1, ""); leave.force = false;
                    self.svc.request(owner, 1.0, leave, self.host.clone()).await.unwrap();
                    unsafe { apply(self.popup.0, &self.ui); }
                    assert!(self.ui.render.lock().unwrap().shown.is_none(),
                        "leaving a pending image revived the previous popup content");
                    assert!(!unsafe { IsWindowVisible(self.popup.0).as_bool() },
                        "canceled image kept the previous native popup visible");
                });
            }
        }
        impl Drop for LeaveFixture {
            fn drop(&mut self) {
                unsafe {
                    let _ = KillTimer(self.popup.0, 1); let _ = KillTimer(self.popup.0, 2);
                    // PeekMessage cannot remove WM_PAINT while its update region
                    // is invalid; an unpainted visible fixture would loop forever.
                    let _ = windows::Win32::Graphics::Gdi::ValidateRect(self.popup.0, None);
                    // STATIC deliberately has no app wndproc: drain only this
                    // owned fixture's queued wake/close messages before destroy.
                    let mut message = MSG::default();
                    while PeekMessageW(&mut message, self.popup.0, 0, 0, PM_REMOVE).as_bool() {}
                }
            }
        }
        #[test]
        fn native_leave_queue_cancels_old_content_when_new_image_has_not_reached_ui() {
            let fixture = LeaveFixture::new();
            fixture.cancel_new_image(fixture.first.0.0 as isize, 2);
        }
        #[test]
        fn native_leave_queue_cannot_transfer_old_content_to_another_owner() {
            let fixture = LeaveFixture::new();
            fixture.cancel_new_image(fixture.second.0.0 as isize, 1);
        }
        #[test]
        fn native_leave_queue_preserves_grace_for_current_content_and_repeated_leave() {
            let fixture = LeaveFixture::new();
            let owner = fixture.first.0.0 as isize;
            fixture.leave(owner, 2, false);
            fixture.leave(owner, 3, false);
            unsafe { apply(fixture.popup.0, &fixture.ui); }
            {
                let render = fixture.ui.render.lock().unwrap();
                let shown = render.shown.as_ref().expect("current preview lost its gap grace");
                assert_eq!(shown.owner, owner);
                assert_eq!(shown.content.text, text("original preview"));
                assert_eq!(shown.generation, fixture.svc.shared.control.lock().unwrap().generation);
                assert!(render.leaving.is_some());
            }
            assert!(unsafe { IsWindowVisible(fixture.popup.0).as_bool() });
            fixture.leave(fixture.second.0.0 as isize, 1, true);
            unsafe { apply(fixture.popup.0, &fixture.ui); }
            assert!(fixture.ui.render.lock().unwrap().shown.is_some(), "other owner canceled this preview");
            fixture.leave(owner, 4, true);
            unsafe { apply(fixture.popup.0, &fixture.ui); }
            assert!(fixture.ui.render.lock().unwrap().shown.is_none());
            assert!(!unsafe { IsWindowVisible(fixture.popup.0).as_bool() });
        }

        // The actual UI thread measures first. A worker then completes a real
        // cancellation/replacement while that layout is paused. No HWND or GDI
        // resources are shared with the worker, and no desktop input is sent.
        fn present_during_change(change: &str) {
            let fixture = LeaveFixture::new();
            let owner = fixture.first.0.0 as isize;
            fixture.leave(owner, 2, true);
            unsafe { apply(fixture.popup.0, &fixture.ui); }
            assert!(!unsafe { IsWindowVisible(fixture.popup.0).as_bool() });
            test_runtime::block_on(fixture.svc.request(owner, 1.0,
                request("text", 3, "stale layout: Chinese 中文 and emoji 🦀"), fixture.host.clone())).unwrap();
            let gate = Arc::new(std::sync::Barrier::new(2));
            *fixture.ui.before_present.lock().unwrap() = Some(gate.clone());
            let svc = &fixture.svc;
            let host = fixture.host.clone();
            let second_owner = fixture.second.0.0 as isize;
            std::thread::scope(|scope| {
                let worker = scope.spawn(move || {
                    gate.wait();
                    match change {
                        "hide" => test_runtime::block_on(svc.request(owner, 1.0,
                            request("hide", 4, ""), host)).unwrap(),
                        "owner-hide" => svc.hide_owner(owner),
                        "shutdown" => svc.shutdown(),
                        "replace" => test_runtime::block_on(svc.request(second_owner, 1.0,
                            request("text", 1, "latest owner preview"), host)).unwrap(),
                        "unchanged" => {},
                        _ => panic!("Unknown fixture change"),
                    }
                    gate.wait();
                });
                unsafe { apply(fixture.popup.0, &fixture.ui); }
                worker.join().unwrap();
            });
            if change == "unchanged" {
                let render = fixture.ui.render.lock().unwrap();
                assert_eq!(render.shown.as_ref().unwrap().generation, fixture.svc.shared.control.lock().unwrap().generation);
                assert!(unsafe { IsWindowVisible(fixture.popup.0).as_bool() });
                return;
            }
            assert!(!unsafe { IsWindowVisible(fixture.popup.0).as_bool() },
                "a completed {change} during layout resurrected a stale native popup");
            assert!(fixture.ui.render.lock().unwrap().shown.is_none(),
                "a completed {change} published stale render state");
            if change == "replace" {
                unsafe { apply(fixture.popup.0, &fixture.ui); }
                let render = fixture.ui.render.lock().unwrap();
                let shown = render.shown.as_ref().expect("newer preview was lost");
                assert_eq!(shown.owner, second_owner);
                assert_eq!(shown.content.text, text("latest owner preview"));
                assert_eq!(shown.generation, fixture.svc.shared.control.lock().unwrap().generation);
                assert!(unsafe { IsWindowVisible(fixture.popup.0).as_bool() });
            } else {
                unsafe { apply(fixture.popup.0, &fixture.ui); }
                assert!(!unsafe { IsWindowVisible(fixture.popup.0).as_bool() });
            }
        }
        #[test]
        fn native_present_rechecks_force_hide_completed_during_layout() { present_during_change("hide"); }
        #[test]
        fn native_present_rechecks_owner_hide_completed_during_layout() { present_during_change("owner-hide"); }
        #[test]
        fn native_present_rechecks_shutdown_completed_during_layout() { present_during_change("shutdown"); }
        #[test]
        fn native_present_preserves_newer_owner_request_completed_during_layout() { present_during_change("replace"); }
        #[test]
        fn native_present_keeps_current_request_after_layout() { present_during_change("unchanged"); }

        #[test]
        fn native_wheel_scroll_preserves_partial_pixels_and_reversals() {
            let mut render = Render { max_offset: 300, ..Default::default() };
            for expected in [20, 40, 60] {
                assert!(render.scroll(-40));
                assert_eq!(render.offset, expected);
            }
            assert!(render.scroll(40));
            assert_eq!(render.offset, 40);
            assert!(!render.scroll(-1));
            assert!(!render.scroll(1));
            assert_eq!(render.offset, 40);
            assert!(!render.scroll(-1));
            assert!(render.scroll(-1));
            assert_eq!(render.offset, 41);
        }

        #[test]
        fn native_wheel_scroll_clamps_and_drops_outward_partial_input() {
            let mut render = Render { offset: 30, max_offset: 65, ..Default::default() };
            assert!(render.scroll(-240));
            assert_eq!(render.offset, 65);
            assert!(!render.scroll(-1));
            assert!(!render.scroll(1));
            assert!(render.scroll(1));
            assert_eq!(render.offset, 64);
            assert!(render.scroll(i16::MAX));
            assert_eq!(render.offset, 0);
            assert!(!render.scroll(1));
            assert!(!render.scroll(-1));
            assert!(render.scroll(-1));
            assert_eq!(render.offset, 1);
            assert!(render.scroll(i16::MIN));
            assert_eq!(render.offset, 65);
        }

        #[test]
        fn native_wheel_scroll_full_detents_preserve_existing_distance() {
            let mut render = Render { offset: 180, max_offset: 300, ..Default::default() };
            for (delta, expected) in [(120, 120), (-120, 180), (240, 60), (-240, 180)] {
                assert!(render.scroll(delta));
                assert_eq!(render.offset, expected);
            }
        }

        #[test]
        fn native_wheel_scroll_without_overflow_does_not_repaint() {
            let mut render = Render::default();
            for delta in [0, 1, -1, 40, -40, 120, -120, i16::MIN, i16::MAX] {
                assert!(!render.scroll(delta));
                assert_eq!(render.offset, 0);
            }
        }

        #[test]
        fn placement_uses_the_screen_including_negative_coordinates() {
            let work = RECT {
                left: -1920,
                top: -200,
                right: 0,
                bottom: 880,
            };
            let anchor = RECT {
                left: -240,
                top: 840,
                right: -40,
                bottom: 870,
            };
            assert_eq!(placement(anchor, work, 800, 600, 8), (-1048, 280));
            let anchor = RECT {
                left: -1900,
                top: -180,
                right: -1700,
                bottom: -150,
            };
            assert_eq!(placement(anchor, work, 800, 600, 8), (-1692, -180));
        }

        #[test]
        fn image_preview_stays_beside_the_entire_owner_on_either_screen_edge() {
            let work = RECT { left: -1920, top: -200, right: 0, bottom: 880 };
            for owner in [
                RECT { left: -1910, top: 80, right: -1490, bottom: 800 },
                RECT { left: -440, top: 80, right: -20, bottom: 800 },
                RECT { left: -1120, top: 80, right: -700, bottom: 800 },
            ] {
                let anchor = RECT { left: owner.left + 12, top: 840,
                    right: owner.right - 12, bottom: 870 };
                let side = image_side(owner, work, 960, 10).unwrap();
                let width = 960.min(side.width);
                let (x, y) = side.position(anchor, work, width, 600);
                assert!(x >= work.left && x + width <= work.right);
                assert!(y >= work.top && y + 600 <= work.bottom);
                assert!(x >= owner.right + 10 || x + width <= owner.left - 10,
                    "a large preview must shrink beside the owner instead of covering its rows");
            }
        }

        #[test]
        fn side_lock_starts_from_room_and_moves_only_for_an_unusable_image() {
            let work = RECT { left: 0, top: 0, right: 1120, bottom: 640 };
            // Right side narrower (212) than left (552): start left for every image.
            let owner = RECT { left: 560, top: 40, right: 900, bottom: 600 };
            let rooms = side_rooms(owner, work, 8);
            assert_eq!(rooms, (212, 552));
            assert!(!default_right(rooms, 1.0));
            for image in [(160, 160), (1920, 1080), (64, 64), (4000, 1100)] {
                assert!(!locked_image_right(owner, work, 8, 1.0, None, image, 18, 42, 960, 624));
            }
            // A comfortable right side wins even when the left is larger.
            let owner = RECT { left: 100, top: 40, right: 440, bottom: 600 };
            assert!(default_right(side_rooms(owner, work, 8), 1.0));
            // Locked right but cramped (212): a wide image moves once to the left.
            let owner = RECT { left: 560, top: 40, right: 900, bottom: 600 };
            assert!(!locked_image_right(owner, work, 8, 1.0, Some(true), (4000, 1100), 18, 42, 960, 624));
            // ...while a small image that is usable on the right stays there.
            assert!(locked_image_right(owner, work, 8, 1.0, Some(true), (160, 160), 18, 42, 960, 624));
            assert_eq!(side_at(owner, work, 8, false).unwrap().edge, 552);
            assert!(side_at(RECT { left: 0, top: 0, right: 1120, bottom: 600 }, work, 8, true).is_none());
        }

        #[test]
        fn image_preview_prefers_a_fitting_right_side_and_has_no_overlay_fallback() {
            let work = RECT { left: 0, top: 0, right: 1920, bottom: 1080 };
            let owner = RECT { left: 400, top: 100, right: 820, bottom: 860 };
            assert!(image_side(owner, work, 400, 8).unwrap().right);
            let owner = RECT { left: 0, top: 0, right: 1920, bottom: 1080 };
            assert!(image_side(owner, work, 400, 8).is_none());
        }

        #[test]
        fn menu_text_and_media_use_the_column_union_without_overlay_fallback() {
            let work = RECT { left: -1920, top: -200, right: 0, bottom: 880 };
            let columns = RECT { left: -1300, top: -120, right: -700, bottom: 700 };
            let anchor = RECT { left: -1000, top: 100, right: -720, bottom: 130 };
            for locked in [None, Some(true), Some(false)] {
                let side = menu_text_side(columns, work, 10, 1.25, locked).unwrap();
                let width = 520.min(side.width);
                let (x, _) = side.position(anchor, work, width, 200);
                assert!(x >= columns.right + 10 || x + width <= columns.left - 10);
                assert!(x >= work.left && x + width <= work.right);
            }
            assert!(menu_text_side(work, work, 10, 1.25, None).is_none());
            let from = RECT { left: -650, top: 100, right: -150, bottom: 300 };
            assert!(beside_glide_safe(Some(columns), from, (-600, 160), 500));
            assert!(!beside_glide_safe(Some(columns), from, (-1850, 160), 500));
            let from = RECT { left: -1850, top: 100, right: -1350, bottom: 300 };
            assert!(!beside_glide_safe(Some(columns), from, (-1850, 160), 600));
            assert!(beside_glide_safe(None, from, (-600, 160), 500));
        }

        #[test]
        fn native_menu_preview_uses_columns_instead_of_fullscreen_owner() {
            let fixture = LeaveFixture::new();
            let owner = fixture.first.0.0 as isize;
            let mut monitor = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            unsafe {
                let handle = MonitorFromWindow(fixture.first.0, MONITOR_DEFAULTTONEAREST);
                assert!(GetMonitorInfoW(handle, &mut monitor).as_bool());
                let work = monitor.rcWork;
                SetWindowPos(fixture.first.0, None, work.left, work.top,
                    work.right - work.left, work.bottom - work.top,
                    SWP_NOACTIVATE | SWP_NOZORDER).unwrap();
            }
            let work = monitor.rcWork;
            let width = work.right - work.left;
            let columns = [work.left + width / 3, work.top + 20,
                work.left + width * 2 / 3, work.bottom - 20];
            let anchor = [columns[0] + 8, columns[1] + 8, columns[2] - 8, columns[1] + 38];
            let file = fixture._data.path().join("menu-preview.png");
            image::RgbaImage::from_pixel(800, 600, image::Rgba([30, 210, 80, 255]))
                .save(&file).unwrap();
            let mut source = url::Url::parse("http://localhost/api/clip/local-image").unwrap();
            source.query_pairs_mut().append_pair("path", &file.to_string_lossy());
            fixture.svc.resolver.add_permits(1);
            for (ticket, action) in [(2, "text"), (3, "image")] {
                let mut next = request(action, ticket, "A file beside the menu");
                next.image_src = Some(source.to_string());
                test_runtime::block_on(fixture.svc.request_screen_beside(
                    owner, 1.0, next, fixture.host.clone(), anchor, Some(columns),
                )).unwrap();
                unsafe { apply(fixture.popup.0, &fixture.ui); }
                let shown = rect(fixture.popup.0);
                assert!(unsafe { IsWindowVisible(fixture.popup.0).as_bool() });
                assert!(shown.right <= columns[0] - 8 || shown.left >= columns[2] + 8);
                assert_eq!(unsafe { GetWindow(fixture.popup.0, GW_OWNER).unwrap() }, fixture.first.0);
                assert_eq!(fixture.ui.render.lock().unwrap().shown.as_ref().unwrap().owner, owner);
            }
            test_runtime::block_on(fixture.svc.request_screen_beside(
                owner, 1.0, request("text", 4, "No room beside the menu"), fixture.host.clone(),
                anchor, Some([work.left, work.top, work.right, work.bottom]),
            )).unwrap();
            unsafe { apply(fixture.popup.0, &fixture.ui); }
            assert!(!unsafe { IsWindowVisible(fixture.popup.0).as_bool() });
            assert!(fixture.ui.render.lock().unwrap().shown.is_none());
            let generation = fixture.ui.shared.control.lock().unwrap().generation;
            assert!(test_runtime::block_on(fixture.svc.request_screen_beside(
                owner, 1.0, request("text", 5, "Invalid bounds"), fixture.host.clone(),
                anchor, Some([0, 0, 0, 10]),
            )).is_err());
            assert_eq!(fixture.ui.shared.control.lock().unwrap().generation, generation);
        }

        #[test]
        fn decoder_preserves_aspect_ratio_and_composites_transparency() {
            let mut bytes = Cursor::new(Vec::new());
            image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
                20,
                40,
                image::Rgba([255, 0, 0, 0]),
            ))
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
            let bitmap = decode(bytes.get_ref(), true).unwrap();
            assert_eq!((bitmap.width, bitmap.height), (20, 40));
            assert_eq!(&bitmap.pixels[..4], &[27, 27, 27, 255]);
            assert!(decode(b"not an image", true).is_err());
        }

        #[test]
        fn cached_image_and_text_paint_while_cold_decoder_is_busy() {
            std::env::set_var("CLIPHOST_TAURI_HEADLESS", "1");
            let audit = std::env::var_os("CLIPHOST_HOVER_TEST_AUDIT_DIR")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../.tmp/cliphost-hover-cache-hit-20260930/native"));
            std::fs::create_dir_all(&audit).unwrap();
            let dir = tempfile::tempdir_in(&audit).unwrap();
            let host = cliphost_core::HostState::open(
                cliphost_core::AppPaths::from_data_root(dir.path().join("data")), 28965,
            ).unwrap();
            let owner = unsafe {
                SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
                let hwnd = CreateWindowExW(WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                    w!("STATIC"), w!("Cached hover test owner"), WS_POPUP,
                    200, 100, 260, 220, None, None, GetModuleHandleW(None).unwrap(), None).unwrap();
                let cloak = windows::Win32::Foundation::BOOL(1);
                DwmSetWindowAttribute(hwnd, DWMWA_CLOAK, &cloak as *const _ as *const _, 4).unwrap();
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                Owner(hwnd)
            };
            let svc = Arc::new(Service::new().unwrap());
            let hwnd = handle(svc.hwnd);
            let scale = unsafe { windows::Win32::UI::HiDpi::GetDpiForWindow(owner.0) as f64 / 96.0 };
            let file = dir.path().join("cached.png");
            image::RgbaImage::from_pixel(80, 60, image::Rgba([210, 30, 80, 255])).save(&file).unwrap();
            let mut url = url::Url::parse("http://127.0.0.1:28965/api/clip/local-image").unwrap();
            url.query_pairs_mut().append_pair("path", &file.to_string_lossy());
            let mut image = request("image", 1, "");
            image.image_src = Some(url.to_string());
            test_runtime::block_on(svc.request(owner.0.0 as isize, scale, image.clone(), host.clone())).unwrap();
            wait(|| svc.shared.painted.load(std::sync::atomic::Ordering::SeqCst) == 1);
            let cached = svc.shared.cache.lock().unwrap().entries[0].1.clone();
            // Hold the expensive cold-decode gate. A cached image must not wait
            // for it, and must still reach the actual native painting path.
            let permit = test_runtime::block_on(svc.decoder.clone().acquire_owned()).unwrap();
            image.ticket = 2;
            let worker = {
                let svc = svc.clone();
                let host = host.clone();
                let owner = owner.0.0 as isize;
                test_runtime::spawn(async move { svc.request(owner, scale, image, host).await })
            };
            wait(|| svc.shared.painted.load(std::sync::atomic::Ordering::SeqCst) == 2);
            test_runtime::block_on(worker).unwrap().unwrap();
            assert!(Arc::ptr_eq(&cached, &svc.shared.cache.lock().unwrap().entries[0].1));
            test_runtime::block_on(svc.request(owner.0.0 as isize, scale,
                request("text", 3, "缓存预览和文本不等待大图解码"), host.clone())).unwrap();
            wait(|| svc.shared.painted.load(std::sync::atomic::Ordering::SeqCst) == 3);
            unsafe {
                assert!(IsWindowVisible(hwnd).as_bool());
                assert_ne!(GetForegroundWindow(), hwnd);
                assert_ne!(GetForegroundWindow(), owner.0);
            }
            // A cold request's file can change while waiting. Its decoded
            // result must be cached under the revision read after dequeue.
            let cold_file = dir.path().join("cold.png");
            image::RgbaImage::from_pixel(80, 60, image::Rgba([20, 30, 40, 255])).save(&cold_file).unwrap();
            let original_key = image_key(&cold_file, true).unwrap();
            url.query_pairs_mut().clear().append_pair("path", &cold_file.to_string_lossy());
            let mut cold = request("image", 4, "");
            cold.image_src = Some(url.to_string());
            let worker = {
                let svc = svc.clone();
                let host = host.clone();
                let owner = owner.0.0 as isize;
                let cold = cold.clone();
                test_runtime::spawn(async move { svc.request(owner, scale, cold, host).await })
            };
            // The owned permit and Service hold two semaphore Arcs; a third
            // establishes that this request finished lookup and queued decode.
            wait(|| Arc::strong_count(&svc.decoder) >= 3);
            std::thread::sleep(Duration::from_millis(5));
            image::RgbaImage::from_pixel(120, 80, image::Rgba([40, 210, 60, 255])).save(&cold_file).unwrap();
            assert_ne!(image_key(&cold_file, true).unwrap(), original_key);
            drop(permit);
            wait(|| svc.shared.painted.load(std::sync::atomic::Ordering::SeqCst) == 4);
            test_runtime::block_on(worker).unwrap().unwrap();
            let decoded = svc.shared.cache.lock().unwrap().entries[0].1.clone();
            assert_eq!((decoded.width, decoded.height), (120, 80));
            let permit = test_runtime::block_on(svc.decoder.clone().acquire_owned()).unwrap();
            cold.ticket = 5;
            let worker = {
                let svc = svc.clone();
                let owner = owner.0.0 as isize;
                test_runtime::spawn(async move { svc.request(owner, scale, cold, host).await })
            };
            wait(|| svc.shared.painted.load(std::sync::atomic::Ordering::SeqCst) == 5);
            test_runtime::block_on(worker).unwrap().unwrap();
            assert!(Arc::ptr_eq(&decoded, &svc.shared.cache.lock().unwrap().entries[0].1));
            let cache = svc.shared.cache.lock().unwrap();
            assert_eq!(cache.bytes, cache.entries.iter().map(|(_, image)| image.byte_len()).sum::<usize>());
            assert_eq!(cache.entries.len(), 2);
            drop(cache);
            drop(permit);
            svc.shutdown();
            wait(|| unsafe { !IsWindow(hwnd).as_bool() });
        }

        #[test]
        fn native_video_poster_click_pause_replay_and_retirement() {
            let fixture = LeaveFixture::new();
            let file = fixture._data.path().join("preview 中文.mp4");
            std::fs::write(&file, include_bytes!("../tests/fixtures/hover-video.mp4")).unwrap();
            let mut request = request("video", 2, "");
            request.image_src = Some(file.to_string_lossy().into_owned());
            test_runtime::block_on(fixture.svc.request(fixture.first.0.0 as isize, 1.0,
                request, fixture.host.clone())).unwrap();
            let hwnd = fixture.popup.0;
            unsafe { apply(hwnd, &fixture.ui); }
            let player = fixture.ui.render.lock().unwrap().shown.as_ref().unwrap()
                .content.video.as_ref().expect("H.264 fixture should decode").player.clone();
            assert!(!player.snapshot().playing, "hover should show a paused poster");
            let bounds = rect(hwnd);
            let point = POINT { x: (bounds.right - bounds.left) / 2,
                y: fixture.ui.render.lock().unwrap().pad + 12 };
            let save = |name: &str| {
                if let Some(folder) = std::env::var_os("CLIPHOST_HOVER_TEST_AUDIT_DIR") {
                    let surface = fixture.ui.surface.lock().unwrap();
                    let image = image::RgbaImage::from_fn((bounds.right - bounds.left) as u32,
                        (bounds.bottom - bounds.top) as u32, |x, y| {
                            let [b, g, r, a] = unsafe { surface.pixel(x as i32, y as i32) };
                            let straight = |c: u8| if a == 0 { 0 } else { (c as u32 * 255 / a as u32).min(255) as u8 };
                            image::Rgba([straight(r), straight(g), straight(b), a])
                        });
                    image.save(PathBuf::from(folder).join(name)).unwrap();
                }
            };
            save("video-poster.png");
            // A leave updates the request generation; an old press must not
            // start playback when its mouse-up reaches the retained popup.
            unsafe { press_video(hwnd, &fixture.ui, point); }
            fixture.leave(fixture.first.0.0 as isize, 3, false);
            unsafe {
                apply(hwnd, &fixture.ui);
                release_video(hwnd, &fixture.ui, point);
            }
            assert!(!player.snapshot().playing);
            let foreground = unsafe { GetForegroundWindow() };
            unsafe {
                press_video(hwnd, &fixture.ui, point);
                release_video(hwnd, &fixture.ui, point);
            }
            assert!(player.snapshot().playing);
            wait(|| player.snapshot().frame.timestamp >= Duration::from_millis(100));
            unsafe { animate(hwnd, &fixture.ui); }
            save("video-playing.png");
            unsafe {
                press_video(hwnd, &fixture.ui, point);
                release_video(hwnd, &fixture.ui, point);
            }
            let paused = player.snapshot();
            assert!(!paused.playing);
            std::thread::sleep(Duration::from_millis(120));
            assert_eq!(player.snapshot().frame.timestamp, paused.frame.timestamp);
            player.toggle();
            wait(|| player.snapshot().ended);
            let end = player.snapshot().frame.timestamp;
            player.toggle();
            wait(|| player.snapshot().frame.timestamp < end);
            fixture.leave(fixture.first.0.0 as isize, 4, true);
            unsafe { apply(hwnd, &fixture.ui); }
            assert!(!player.snapshot().playing, "hidden video must stop immediately");
            assert!(!unsafe { IsWindowVisible(hwnd).as_bool() });
            assert_eq!(unsafe { GetForegroundWindow() }, foreground);
        }

        #[test]
        fn queued_video_cannot_replace_newer_text_or_start_decode_after_retirement() {
            let fixture = LeaveFixture::new();
            test_runtime::block_on(async {
                let permit = fixture.svc.decoder.clone().acquire_owned().await.unwrap();
                let mut video = request("video", 2, "");
                video.image_src = Some("never-open-this-missing-file.mp4".into());
                let pending = fixture.svc.request(fixture.first.0.0 as isize, 1.0,
                    video, fixture.host.clone());
                tokio::pin!(pending);
                tokio::select! {
                    biased;
                    result = &mut pending => panic!("video decoder gate completed early: {result:?}"),
                    _ = tokio::time::sleep(Duration::from_millis(1)) => {}
                }
                fixture.svc.request(fixture.first.0.0 as isize, 1.0,
                    request("text", 3, "new text without decoder wait"), fixture.host.clone()).await.unwrap();
                unsafe { apply(fixture.popup.0, &fixture.ui); }
                drop(permit);
                pending.await.unwrap();
                unsafe { apply(fixture.popup.0, &fixture.ui); }
                assert_eq!(fixture.ui.render.lock().unwrap().shown.as_ref().unwrap().content.text,
                    text("new text without decoder wait"));
            });
        }

        #[test]
        fn warm_keep_request_retains_the_visible_preview_until_its_replacement() {
            let fixture = LeaveFixture::new();
            let owner = fixture.first.0.0 as isize;
            let shown = fixture.ui.render.lock().unwrap().shown.as_ref().unwrap().generation;
            test_runtime::block_on(async {
                // The fixture's resolver gate never opens: this image stays loading.
                let mut image = request("image", 2, "");
                image.keep = true;
                image.image_src = Some("/api/clip/local-image?path=C%3A%5Cmissing.png".into());
                let pending = fixture.svc.request(owner, 1.0, image, fixture.host.clone());
                tokio::pin!(pending);
                tokio::select! {
                    biased;
                    result = &mut pending => panic!("resolver gate completed early: {result:?}"),
                    _ = tokio::time::sleep(Duration::from_millis(1)) => {}
                }
                assert!(fixture.ui.shared.control.lock().unwrap().pending.is_none(), "a warm request queues no hide");
                assert!(!current(&fixture.ui.shared, shown));
                assert!(shown_valid(&fixture.ui.shared, shown), "the visible row stays valid while loading");
                let mut next = request("text", 3, "second row");
                next.keep = true;
                next.anchor = [10.0, 70.0, 140.0, 30.0];
                fixture.svc.request(owner, 1.0, next, fixture.host.clone()).await.unwrap();
                unsafe { apply(fixture.popup.0, &fixture.ui); }
                {
                    let render = fixture.ui.render.lock().unwrap();
                    let show = render.shown.as_ref().unwrap();
                    assert_eq!(show.content.text, text("second row"));
                    assert!(show.glide);
                    assert_eq!(render.glide.is_some(), motion_enabled());
                }
                fixture.svc.request(owner, 1.0, request("text", 4, "ordinary"), fixture.host.clone()).await.unwrap();
                assert!(fixture.ui.shared.control.lock().unwrap().keep_floor.is_none());
                assert!(!shown_valid(&fixture.ui.shared, shown));
            });
        }

        #[test]
        fn native_video_unsupported_codec_falls_back_and_later_text_replaces_it() {
            let fixture = LeaveFixture::new();
            let file = fixture._data.path().join("invalid.mp4");
            std::fs::write(&file, b"invalid video fixture").unwrap();
            let mut video = request("video", 2, "");
            video.image_src = Some(file.to_string_lossy().into_owned());
            test_runtime::block_on(fixture.svc.request(fixture.first.0.0 as isize, 1.0,
                video, fixture.host.clone())).unwrap();
            unsafe { apply(fixture.popup.0, &fixture.ui); }
            let render = fixture.ui.render.lock().unwrap();
            let content = &render.shown.as_ref().unwrap().content;
            assert!(content.video.is_none() && content.image.is_none());
            assert!(String::from_utf16_lossy(&content.text).contains("invalid.mp4"));
            drop(render);
            test_runtime::block_on(fixture.svc.request(fixture.first.0.0 as isize, 1.0,
                request("text", 3, "new content"), fixture.host.clone())).unwrap();
            unsafe { apply(fixture.popup.0, &fixture.ui); }
            assert_eq!(fixture.ui.render.lock().unwrap().shown.as_ref().unwrap().content.text,
                text("new content"));
        }

        #[test]
        fn native_animation_advances_actual_pixels_without_activation_and_stops_on_hide() {
            std::env::set_var("CLIPHOST_TAURI_HEADLESS", "1");
            let audit = std::env::var_os("CLIPHOST_HOVER_TEST_AUDIT_DIR")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../.tmp/cliphost-hover-animation-20260930/native"));
            std::fs::create_dir_all(&audit).unwrap();
            let dir = tempfile::tempdir_in(&audit).unwrap();
            let host = cliphost_core::HostState::open(
                cliphost_core::AppPaths::from_data_root(dir.path().join("data")), 28965,
            ).unwrap();
            let owner = unsafe {
                SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
                let hwnd = CreateWindowExW(WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW, w!("STATIC"),
                    w!("Animated hover test owner"), WS_POPUP, 200, 100, 260, 220,
                    None, None, GetModuleHandleW(None).unwrap(), None).unwrap();
                let cloak = windows::Win32::Foundation::BOOL(1);
                DwmSetWindowAttribute(hwnd, DWMWA_CLOAK, &cloak as *const _ as *const _, 4).unwrap();
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                Owner(hwnd)
            };
            let svc = Arc::new(Service::new().unwrap());
            let hwnd = handle(svc.hwnd);
            let scale = unsafe { windows::Win32::UI::HiDpi::GetDpiForWindow(owner.0) as f64 / 96.0 };
            let assets: [(&str, &[u8]); 4] = [
                ("animated.gif", include_bytes!("../tests/fixtures/hover-animated.gif")),
                ("animated.png", include_bytes!("../tests/fixtures/hover-animated.png")),
                ("animated.webp", include_bytes!("../tests/fixtures/hover-animated.webp")),
                ("poster.png", include_bytes!("../tests/fixtures/hover-poster.png")),
            ];
            let mut output = Surface::default();
            for (index, (name, bytes)) in assets.into_iter().enumerate() {
                let file = dir.path().join(name);
                std::fs::write(&file, bytes).unwrap();
                let mut url = url::Url::parse("http://127.0.0.1:28965/api/clip/local-image").unwrap();
                url.query_pairs_mut().append_pair("path", &file.to_string_lossy());
                let mut image = request("image", index as u64 + 1, "");
                image.image_src = Some(url.to_string());
                test_runtime::block_on(svc.request(owner.0.0 as isize, scale, image, host.clone())).unwrap();
                let generation = svc.shared.control.lock().unwrap().generation;
                wait(|| svc.shared.painted.load(std::sync::atomic::Ordering::SeqCst) == generation
                    && unsafe { IsWindowVisible(hwnd).as_bool() });
                // Hold the synthetic hover position without desktop input. The
                // playback timer remains live; only cursor-leave polling stops.
                unsafe { let _ = KillTimer(hwnd, 1); }
                let bounds = rect(hwnd);
                let target = unsafe { output.prepare(bounds.right - bounds.left, bounds.bottom - bounds.top, 14).unwrap() };
                let pad = (12.0 * scale).round() as i32 + super::super::style::shadow_padding(scale);
                let pixel = || unsafe {
                    SendMessageW(hwnd, WM_PRINTCLIENT, WPARAM(target.0 as usize), LPARAM(0));
                    GetPixel(target, pad + 16, pad + 10)
                };
                assert_eq!(pixel(), COLORREF(0x00501ed2), "first animated frame differs for {name}");
                // Keep native rendering exact against the pinned codec's
                // known opaque WebP fixed-point output (see frame regression).
                let green = if name == "animated.webp" { COLORREF(0x004fd11d) } else { COLORREF(0x0050d21e) };
                println!("Native {name}: first={:?}, expected second={green:?}", pixel());
                wait(|| pixel() == green);
                println!("Native {name}: second={:?}", pixel());
                unsafe {
                    assert!(IsWindowVisible(hwnd).as_bool());
                    assert_ne!(GetForegroundWindow(), hwnd);
                    assert_ne!(GetForegroundWindow(), owner.0);
                }
                svc.hide_owner(owner.0.0 as isize);
                wait(|| unsafe { !IsWindowVisible(hwnd).as_bool() });
            }
            std::thread::sleep(Duration::from_millis(50));
            let ticks = svc.shared.animation_ticks.load(std::sync::atomic::Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(600));
            assert_eq!(svc.shared.animation_ticks.load(std::sync::atomic::Ordering::SeqCst), ticks,
                "hidden popup must stop playback timer work");
            svc.shutdown();
            wait(|| unsafe { !IsWindow(hwnd).as_bool() });
        }

        #[test]
        fn native_image_caption_fits_measured_text_at_multiple_dpi_scales() {
            unsafe {
                let previous_dpi = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
                let fixture = LeaveFixture::new();
                SetWindowLongPtrW(fixture.popup.0, GWL_EXSTYLE,
                    GetWindowLongPtrW(fixture.popup.0, GWL_EXSTYLE) | WS_EX_LAYERED.0 as isize);
                let monitor = MonitorFromWindow(fixture.first.0, MONITOR_DEFAULTTONEAREST);
                let mut info = MONITORINFO {
                    cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                    ..Default::default()
                };
                assert!(GetMonitorInfoW(monitor, &mut info).as_bool());
                let work = info.rcWork;
                for scale in [1.0, 1.25, 1.5, 2.0] {
                    for (narrow, image_width, image_height) in [(false, 610, 191), (true, 610, 191), (true, 40, 20)] {
                        let bitmap = Arc::new(Bitmap::still(
                            [80, 30, 210, 255].repeat(image_width as usize * image_height as usize), image_width, image_height));
                        let gap = (8.0_f64 * scale).round() as i32;
                        let side_width = (if narrow { (120.0 * scale) as i32 } else { 720 })
                            .min(work.right - work.left - gap - 80);
                        let owner_left = work.left + side_width + gap;
                        SetWindowPos(fixture.first.0, None, owner_left, work.top + 20,
                            work.right - owner_left, 160, SWP_NOACTIVATE | SWP_NOZORDER).unwrap();
                        let generation = {
                            let mut control = fixture.ui.shared.control.lock().unwrap();
                            control.generation += 1;
                            control.owner = fixture.first.0.0 as isize;
                            control.generation
                        };
                        let content = Arc::new(Content {
                            image: Some(bitmap.clone()), video: None, text: text("610×191 · 27.09KB"), dark: true, opener: None, truncated: false,
                        });
                        fixture.ui.shared.control.lock().unwrap().pending = Some(Pending::Show(Presentation {
                            content: content.clone(), owner: fixture.first.0.0 as isize,
                            anchor: RECT { left: owner_left, top: work.top + 20, right: owner_left + 40, bottom: work.top + 60 },
                            beside: None,
                            scale, generation, glide: false, side_lock: false,
                        }));
                        apply(fixture.popup.0, &fixture.ui);
                        let bounds = rect(fixture.popup.0);
                        let width = bounds.right - bounds.left;
                        let height = bounds.bottom - bounds.top;
                        let render = fixture.ui.render.lock().unwrap();
                        assert!(render.shown.is_some());
                        let content_pad = (12.0 * scale).round() as i32;
                        let caption_top = render.pad + render.image_size.1 + content_pad;
                        let caption_height = height - render.pad - caption_top;
                        let px = (14.0 * scale).round() as i32;
                        let (_, measured_height) = measure(&content, width - 2 * render.pad, px,
                            &mut fixture.ui.surface.lock().unwrap());
                        assert!(caption_height >= measured_height,
                            "caption clipped: scale={scale}, narrow={narrow}, allocated={caption_height}, measured={measured_height}");
                        if narrow {
                            let (_, single_line_height) = measure(&content, 1024, px,
                                &mut fixture.ui.surface.lock().unwrap());
                            assert!(measured_height > single_line_height, "narrow sides must exercise caption wrapping");
                        }
                        assert!(width <= side_width && bounds.right <= owner_left - gap);
                        assert!(height <= work.bottom - work.top);
                        assert!((render.image_size.0 as f64 / image_width as f64
                            - render.image_size.1 as f64 / image_height as f64).abs() < 0.01);
                        let image_left = (width - render.image_size.0) / 2;
                        let image_bottom = render.pad + render.image_size.1 - 1;
                        drop(render);
                        let mut output = Surface::default();
                        let target = output.prepare(width, height, px).unwrap();
                        draw(fixture.popup.0, &fixture.ui, target);
                        assert_eq!(GetPixel(target, image_left, image_bottom), COLORREF(0x00501ed2),
                            "the full image must fit above its caption");
                        if let Some(folder) = std::env::var_os("CLIPHOST_HOVER_TEST_AUDIT_DIR") {
                            let surface = fixture.ui.surface.lock().unwrap();
                            let sample = image::RgbaImage::from_fn(width as u32, height as u32, |x, y| {
                                let [b, g, r, a] = surface.pixel(x as i32, y as i32);
                                let straight = |c: u8| if a == 0 { 0 } else {
                                    (c as u32 * 255 / a as u32).min(255) as u8
                                };
                                image::Rgba([straight(r), straight(g), straight(b), a])
                            });
                            sample.save(std::path::PathBuf::from(folder)
                                .join(format!("image-caption-{scale}-{narrow}-{image_width}.png"))).unwrap();
                        }
                    }
                }
                drop(fixture);
                SetThreadDpiAwarenessContext(previous_dpi);
            }
        }

        #[test]
        fn native_url_hits_require_successful_layered_publication_and_print_cannot_revive_them() {
            unsafe {
                let fixture = LeaveFixture::new();
                let hwnd = fixture.popup.0;
                test_runtime::block_on(fixture.svc.request(fixture.first.0.0 as isize, 1.0,
                    request("text", 2, "https://example.invalid/published"), fixture.host.clone())).unwrap();
                apply(hwnd, &fixture.ui);
                let show = fixture.ui.render.lock().unwrap().shown.as_ref().unwrap().clone();
                let rects = fixture.ui.surface.lock().unwrap().link_rects();
                let (_, rect) = &rects[0];
                let point = POINT { x: (rect.left + rect.right) / 2, y: (rect.top + rect.bottom) / 2 };
                assert!(link_at(hwnd, &fixture.ui, point).is_some());
                let mut bounds = RECT::default();
                GetClientRect(hwnd, &mut bounds).unwrap();
                let mut output = Surface::default();
                let target = output.prepare(bounds.right, bounds.bottom, 14).unwrap();
                draw(hwnd, &fixture.ui, target);
                assert!(link_at(hwnd, &fixture.ui, point).is_some(),
                    "capturing already published content must preserve its committed URL hits");
                let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
                SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style & !(WS_EX_LAYERED.0 as isize));
                draw(hwnd, &fixture.ui, HDC::default());
                assert!(fixture.ui.render.lock().unwrap().shown.is_none());
                assert!(!IsWindowVisible(hwnd).as_bool());
                assert!(fixture.ui.surface.lock().unwrap().hit_link(show.generation, point.x, point.y).is_none());
                // A capture may paint a retained candidate but is never a
                // successful UpdateLayeredWindow publication.
                fixture.ui.render.lock().unwrap().shown = Some(show.clone());
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                draw(hwnd, &fixture.ui, target);
                assert!(fixture.ui.surface.lock().unwrap().hit_link(show.generation, point.x, point.y).is_none(),
                    "WM_PRINTCLIENT must not publish URL hit geometry");
                assert!(link_at(hwnd, &fixture.ui, point).is_none());
                SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style);
                draw(hwnd, &fixture.ui, HDC::default());
                assert!(link_at(hwnd, &fixture.ui, point).is_some());
            }
        }

        #[test]
        fn native_url_tooltip_paint_and_single_click_follow_layout_scroll_dpi_and_generation() {
            unsafe {
                let previous_dpi = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
                let fixture = LeaveFixture::new();
                let hwnd = fixture.popup.0;
                // Install the real popup dispatcher only on this cloaked,
                // test-owned window. No desktop mouse or browser is touched.
                struct Dispatcher { hwnd: HWND, proc: isize, data: isize }
                impl Drop for Dispatcher {
                    fn drop(&mut self) { unsafe {
                        SetWindowLongPtrW(self.hwnd, GWLP_WNDPROC, self.proc);
                        SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, self.data);
                    } }
                }
                let dispatcher = Dispatcher {
                    hwnd,
                    data: SetWindowLongPtrW(hwnd, GWLP_USERDATA, &fixture.ui as *const Ui as isize),
                    proc: SetWindowLongPtrW(hwnd, GWLP_WNDPROC, wndproc as *const () as isize),
                };
                SetWindowLongPtrW(hwnd, GWL_EXSTYLE,
                    GetWindowLongPtrW(hwnd, GWL_EXSTYLE) | WS_EX_LAYERED.0 as isize);
                let foreground = GetForegroundWindow();
                let first_uri = format!("https://example.invalid/{}", "wrapped-url-".repeat(24));
                let last_uri = "https://last.example.invalid/end";
                let content = format!("说明 🦀 {first_uri}\n{}\n{last_uri}",
                    (0..85).map(|index| format!("普通正文第 {index} 行")).collect::<Vec<_>>().join("\n"));
                let packed = |point: POINT| LPARAM(((point.y as u16 as u32) << 16 | point.x as u16 as u32) as isize);
                let down = |point: POINT| { SendMessageW(hwnd, WM_LBUTTONDOWN, WPARAM(0), packed(point)); };
                let up = |point: POINT| { SendMessageW(hwnd, WM_LBUTTONUP, WPARAM(0), packed(point)); };
                let point_for = |uri: &str| {
                    let rects = fixture.ui.surface.lock().unwrap().link_rects();
                    rects.into_iter()
                        .filter(|(target, _)| target == uri)
                        .map(|(_, rect)| POINT { x: (rect.left + rect.right) / 2, y: (rect.top + rect.bottom) / 2 })
                        .find(|point| link_at(hwnd, &fixture.ui, *point).is_some())
                        .expect("painted URL must have an exact visible hit rectangle")
                };
                for (index, scale) in [1.0, 1.25, 1.5, 2.0].into_iter().enumerate() {
                    test_runtime::block_on(fixture.svc.request(fixture.first.0.0 as isize, scale,
                        request("text", index as u64 * 3 + 2, &content), fixture.host.clone())).unwrap();
                    apply(hwnd, &fixture.ui);
                    let (send, receive) = std::sync::mpsc::channel();
                    let shared = fixture.ui.shared.clone();
                    let opener: LinkOpener = Arc::new(move |uri| {
                        send.send((uri.to_string(), shared.control.try_lock().is_ok(),
                            windows::Win32::System::Threading::GetCurrentThreadId())).unwrap();
                        Ok(())
                    });
                    {
                        let mut render = fixture.ui.render.lock().unwrap();
                        let show = render.shown.as_mut().unwrap();
                        show.content = Arc::new(Content { image: None, video: None, text: show.content.text.clone(),
                            dark: true, opener: Some(opener), truncated: show.content.truncated });
                    }
                    draw(hwnd, &fixture.ui, HDC::default());
                    assert!(fixture.ui.render.lock().unwrap().max_offset > 0);
                    let point = point_for(&first_uri);
                    let normal = POINT { x: fixture.ui.render.lock().unwrap().pad + 1, y: point.y };
                    assert!(link_at(hwnd, &fixture.ui, normal).is_none(),
                        "ordinary text before the URL must not have a link hit");
                    let surface = fixture.ui.surface.lock().unwrap();
                    let (_, url_rect) = surface.link_rects().into_iter().find(|(uri, rect)|
                        uri == &first_uri && contains(*rect, point)).unwrap();
                    assert!((url_rect.top..url_rect.bottom).any(|y|
                        (url_rect.left..url_rect.right).any(|x| {
                            let [blue, _, red, alpha] = surface.pixel(x, y);
                            alpha == 255 && blue as i32 > red as i32 + 30
                        })), "the clicked glyph rectangle must contain actual blue URL pixels at {scale} DPI scale");
                    assert_eq!(surface.pixel(0, 0), [0; 4], "text rendering must preserve the cached transparent shadow");
                    let handles = surface.handles();
                    let link_layout = surface.link_layout_identity();
                    drop(surface);
                    down(point);
                    up(POINT { x: 1, y: 1 });
                    assert!(receive.try_recv().is_err(), "releasing outside a URL must not open it");
                    down(point);
                    up(point);
                    let opened = receive.recv_timeout(Duration::from_secs(2)).unwrap();
                    assert_eq!(opened.0, first_uri);
                    assert!(opened.1, "the opener must run without the request mutex");
                    assert_ne!(opened.2, windows::Win32::System::Threading::GetCurrentThreadId(),
                        "the opener must run outside the native popup UI thread");
                    down(point);
                    SendMessageW(hwnd, WM_MOUSEWHEEL, WPARAM((-32760i16 as u16 as usize) << 16), LPARAM(0));
                    up(point);
                    assert!(receive.try_recv().is_err(), "scrolling cancels a pending click");
                    assert_eq!(fixture.ui.surface.lock().unwrap().handles(), handles,
                        "scroll redraws must reuse the existing DC, DIB, font and text storage");
                    assert_eq!(fixture.ui.surface.lock().unwrap().link_layout_identity(), link_layout,
                        "scroll redraws must reuse the DirectWrite layout and DC render target");
                    let last = point_for(last_uri);
                    down(last);
                    up(last);
                    assert_eq!(receive.recv_timeout(Duration::from_secs(2)).unwrap().0, last_uri);
                    down(last);
                    // Cancel the generation before dispatching the queued hide.
                    fixture.svc.hide_owner(fixture.first.0.0 as isize);
                    up(last);
                    assert!(receive.try_recv().is_err(), "a stale painted request must never open its URL");
                    apply(hwnd, &fixture.ui);
                    test_runtime::block_on(fixture.svc.request(fixture.first.0.0 as isize, scale,
                        request("text", index as u64 * 3 + 3, "替换后的普通文本"), fixture.host.clone())).unwrap();
                    apply(hwnd, &fixture.ui);
                    assert!(link_at(hwnd, &fixture.ui, last).is_none(),
                        "a new ordinary-text presentation must discard the old URL hit geometry");
                    fixture.svc.hide_owner(fixture.first.0.0 as isize);
                    apply(hwnd, &fixture.ui);
                    assert_eq!(GetForegroundWindow(), foreground, "URL hover/click must preserve no-activate behavior");
                }
                drop(dispatcher);
                drop(fixture);
                SetThreadDpiAwarenessContext(previous_dpi);
            }
        }

        #[test]
        fn native_paint_reuses_surface_across_content_theme_and_size_changes() {
            unsafe {
                let hwnd = CreateWindowExW(
                    WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW, w!("STATIC"), w!("Hover paint buffer test"),
                    WS_POPUP, 0, 0, 180, 120, None, None, GetModuleHandleW(None).unwrap(), None,
                ).unwrap();
                let owner = Owner(hwnd);
                let ui = Ui {
                    shared: shared_state(),
                    render: Mutex::new(Render {
                        shown: Some(Presentation {
                            content: Arc::new(Content { image: None, video: None, text: text("第一行\n第二行\n第三行"), dark: true, opener: None, truncated: false }),
                            owner: hwnd.0 as isize, anchor: RECT::default(), beside: None, scale: 1.0, generation: 1, glide: false, side_lock: false,
                        }),
                        offset: 0, wheel_remainder: 0, max_offset: 60, pad: 18, image_size: (0, 0), leaving: None, frame: 0, animation_started: None, scrolled: None, pressed_link: None, pressed_video: None, video_revision: 0, glide: None, fade: None,
                    }),
                    side_lock: Mutex::new(None),
                    surface: Mutex::new(Surface::default()),
                    before_present: Mutex::new(None),
                };
                let mut output = Surface::default();
                let target = output.prepare(400, 260, 14).unwrap();
                let _ = GetStockObject(DC_BRUSH);
                // Chinese fallback fonts may initialize a process-wide GDI
                // object on first draw. Warm that cache before counting only
                // the popup's owned resources.
                {
                    let mut warm = Surface::default();
                    warm.prepare(40, 30, 14).unwrap();
                    warm.draw_text(&text("第一行"), &mut RECT {
                        right: 40, bottom: 30, ..Default::default()
                    }, DT_NOPREFIX);
                    let _ = GdiFlush();
                }
                let process = windows::Win32::System::Threading::GetCurrentProcess();
                let baseline = windows::Win32::System::Threading::GetGuiResources(process,
                    windows::Win32::System::Threading::GR_GDIOBJECTS);
                draw(hwnd, &ui, target);
                let save_sample = |name: &str| {
                    if let Some(folder) = std::env::var_os("CLIPHOST_HOVER_TEST_AUDIT_DIR") {
                        let surface = ui.surface.lock().unwrap();
                        let image = image::RgbaImage::from_fn(180, 120, |x, y| {
                            let [b, g, r, a] = surface.pixel(x as i32, y as i32);
                            let straight = |c: u8| if a == 0 { 0 } else {
                                (c as u32 * 255 / a as u32).min(255) as u8
                            };
                            image::Rgba([straight(r), straight(g), straight(b), a])
                        });
                        image.save(std::path::PathBuf::from(folder).join(name)).unwrap();
                    }
                };
                save_sample("tooltip-dark.png");
                let original = ui.surface.lock().unwrap().handles();
                for offset in [0, 30, 60].into_iter().cycle().take(200) {
                    ui.render.lock().unwrap().offset = offset;
                    draw(hwnd, &ui, target);
                    assert_eq!(ui.surface.lock().unwrap().pixel(0, 0), [0; 4]);
                    assert_eq!(GetPixel(target, 90, 10), rgb(true));
                    assert_eq!(ui.surface.lock().unwrap().handles(), original);
                }
                {
                    let mut render = ui.render.lock().unwrap();
                    render.offset = 0;
                    render.shown.as_mut().unwrap().content = Arc::new(Content {
                        video: None,
                        image: None, text: text("第一行\n第二行\n第三行"), dark: false, opener: None, truncated: false,
                    });
                }
                draw(hwnd, &ui, target);
                save_sample("tooltip-light.png");
                // The preceding text draw left a clip. A different theme must
                // repaint the border too, not leave dark pixels outside it.
                let image = Arc::new(Bitmap::still([80, 30, 210, 255].repeat(60 * 40), 60, 40));
                {
                    let mut render = ui.render.lock().unwrap();
                    render.shown.as_mut().unwrap().content = Arc::new(Content { image: Some(image.clone()), video: None, text: vec![], dark: false, opener: None, truncated: false });
                    render.image_size = (60, 40);
                }
                draw(hwnd, &ui, target);
                assert_eq!(ui.surface.lock().unwrap().pixel(0, 0), [0; 4]);
                assert_eq!(GetPixel(target, 90, 10), rgb(false));
                assert_eq!(GetPixel(target, 90, 30), COLORREF(0x00501ed2));
                assert_eq!(ui.surface.lock().unwrap().handles(), original);
                let _ = SetWindowPos(hwnd, None, 0, 0, 400, 260, SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOZORDER);
                {
                    let mut render = ui.render.lock().unwrap();
                    render.shown.as_mut().unwrap().scale = 2.0;
                    render.pad = 36;
                }
                draw(hwnd, &ui, target);
                assert_eq!(ui.surface.lock().unwrap().pixel(399, 259), [0; 4]);
                assert_eq!(GetPixel(target, 200, 20), rgb(false));
                let enlarged = ui.surface.lock().unwrap().handles();
                assert_ne!(enlarged.1, original.1);
                assert_ne!(enlarged.2, original.2);
                let _ = SetWindowPos(hwnd, None, 0, 0, 80, 70, SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOZORDER);
                draw(hwnd, &ui, target);
                assert_eq!(ui.surface.lock().unwrap().pixel(79, 69), [0; 4],
                    "a smaller tooltip must use the allocated DIB stride");
                assert_eq!(ui.surface.lock().unwrap().handles(), enlarged);
                assert_eq!(ui.surface.lock().unwrap().allocations, [1, 2, 2]);
                drop(ui);
                let _ = GdiFlush();
                assert_eq!(windows::Win32::System::Threading::GetGuiResources(process,
                    windows::Win32::System::Threading::GR_GDIOBJECTS), baseline);
                assert!(!IsWindowVisible(owner.0).as_bool());
            }
        }

        #[test]
        fn real_native_popup_reuses_hwnd_caches_images_and_cancels_pending_work() {
            std::env::set_var("CLIPHOST_TAURI_HEADLESS", "1");
            let audit = std::env::var_os("CLIPHOST_HOVER_TEST_AUDIT_DIR")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../.tmp/cliphost-native-hover-20260930"));
            std::fs::create_dir_all(&audit).unwrap();
            let dir = tempfile::tempdir_in(&audit).unwrap();
            let host = cliphost_core::HostState::open(
                cliphost_core::AppPaths::from_data_root(dir.path().join("data")),
                28965,
            )
            .unwrap();
            let owner = unsafe {
                SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
                let hwnd = CreateWindowExW(
                    WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                    w!("STATIC"),
                    w!("Native hover test owner"),
                    WS_POPUP,
                    200,
                    100,
                    260,
                    220,
                    None,
                    None,
                    GetModuleHandleW(None).unwrap(),
                    None,
                )
                .unwrap();
                let cloak = windows::Win32::Foundation::BOOL(1);
                DwmSetWindowAttribute(hwnd, DWMWA_CLOAK, &cloak as *const _ as *const _, 4)
                    .unwrap();
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                Owner(hwnd)
            };
            let svc = Arc::new(Service::new().unwrap());
            svc.bind_owner(owner.0.0 as isize).unwrap();
            assert!(!cliphost_core::host_hit_test::has_visible_native_hover_popup(owner.0.0 as isize));
            let hwnd = handle(svc.hwnd);
            let original_owner = rect(owner.0);
            let scale =
                unsafe { windows::Win32::UI::HiDpi::GetDpiForWindow(owner.0) as f64 / 96.0 };
            let before =
                unsafe { windows::Win32::System::DataExchange::GetClipboardSequenceNumber() };
            let mut times = Vec::new();
            let show = |r: Request| {
                let started = Instant::now();
                test_runtime::block_on(svc.request(
                    owner.0 .0 as isize,
                    scale,
                    r,
                    host.clone(),
                ))
                .unwrap();
                let generation = svc.shared.control.lock().unwrap().generation;
                wait(|| svc.shared.painted.load(std::sync::atomic::Ordering::SeqCst) == generation
                    && unsafe { IsWindowVisible(hwnd).as_bool() });
                unsafe {
                    assert!(IsWindowVisible(hwnd).as_bool());
                    assert!(cliphost_core::host_hit_test::has_visible_native_hover_popup(owner.0.0 as isize));
                    assert_ne!(GetForegroundWindow(), hwnd);
                    assert_ne!(GetForegroundWindow(), owner.0);
                }
                started.elapsed().as_secs_f64() * 1000.0
            };
            times.push(show(request("text", 1, "固定")));
            let small = rect(hwnd);
            assert!(small.right - small.left < 200);
            let file = dir.path().join("hover.png");
            image::RgbaImage::from_pixel(800, 600, image::Rgba([210, 30, 80, 255]))
                .save(&file)
                .unwrap();
            let mut url = url::Url::parse("http://127.0.0.1:28965/api/clip/local-image").unwrap();
            url.query_pairs_mut()
                .append_pair("path", &file.to_string_lossy());
            let mut image = request("image", 2, "");
            image.image_src = Some(url.to_string());
            times.push(show(image.clone()));
            let large = rect(hwnd);
            assert!(
                large.right - large.left > original_owner.right - original_owner.left,
                "preview must extend outside its small owner"
            );
            unsafe {
                assert_eq!(GetWindow(hwnd, GW_OWNER).unwrap(), owner.0);
                assert_eq!(
                    GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_NOACTIVATE.0,
                    WS_EX_NOACTIVATE.0
                );
                assert_ne!(GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_LAYERED.0, 0);
            }
            let cached = svc.shared.cache.lock().unwrap().entries[0].1.clone();
            image.ticket = 3;
            image.meta_line = Some("800 × 600 · PNG".into());
            times.push(show(image.clone()));
            assert!(
                Arc::ptr_eq(&cached, &svc.shared.cache.lock().unwrap().entries[0].1),
                "cache hit must reuse decoded pixels"
            );
            std::thread::sleep(Duration::from_millis(5));
            image::RgbaImage::from_pixel(800, 600, image::Rgba([30, 210, 80, 255]))
                .save(&file)
                .unwrap();
            image.ticket = 4;
            times.push(show(image));
            assert!(
                !Arc::ptr_eq(&cached, &svc.shared.cache.lock().unwrap().entries[0].1),
                "changed file at same URL must invalidate cache"
            );
            // Stall first file resolution. A newer text request must still
            // paint, and the obsolete lookup must never resolve its invalid source.
            let permit =
                test_runtime::block_on(svc.resolver.clone().acquire_owned()).unwrap();
            let mut pending = request("image", 5, "");
            pending.image_src = Some("invalid-source".into());
            let worker = {
                let svc = svc.clone();
                let host = host.clone();
                let owner = owner.0 .0 as isize;
                test_runtime::spawn(async move {
                    svc.request(owner, scale, pending, host).await
                })
            };
            wait(|| {
                svc.shared
                    .control
                    .lock()
                    .unwrap()
                    .tickets
                    .get(&(owner.0 .0 as isize))
                    == Some(&5)
            });
            times.push(show(request(
                "text",
                6,
                "后台图片排队时，文本仍能显示。\n换行保持。",
            )));
            drop(permit);
            test_runtime::block_on(worker).unwrap().unwrap();
            let generation = svc.shared.control.lock().unwrap().generation;
            test_runtime::block_on(svc.request(
                owner.0 .0 as isize,
                scale,
                request("hide", 4, ""),
                host.clone(),
            ))
            .unwrap();
            assert_eq!(
                svc.shared.control.lock().unwrap().generation,
                generation,
                "out-of-order cleanup must be ignored"
            );
            svc.hide_owner(owner.0 .0 as isize);
            wait(|| unsafe { !IsWindowVisible(hwnd).as_bool() });
            assert!(!cliphost_core::host_hit_test::has_visible_native_hover_popup(owner.0.0 as isize));
            assert_eq!(svc.hwnd, hwnd.0 as isize);
            assert_eq!(rect(owner.0).right, original_owner.right);
            assert_eq!(
                unsafe { windows::Win32::System::DataExchange::GetClipboardSequenceNumber() },
                before
            );
            let report = serde_json::json!({"ok":true,"nativeHwnd":svc.hwnd,"smallTipWidth":small.right-small.left,
                "imagePopupSize":[large.right-large.left,large.bottom-large.top],"ownerSize":[260,220],"scale":scale,
                "requestToPaintMs":times,"cacheBudgetBytes":CACHE_BYTES,"noDesktopInput":true,"noClipboardWrites":true,
                "scope":"Actual DWM-cloaked Win32 popup paint. Excludes WebView IPC, hover delay and real desktop hover."});
            std::fs::write(
                audit.join("native-tests.json"),
                serde_json::to_vec_pretty(&report).unwrap(),
            )
            .unwrap();
            eprintln!("{report}");
            svc.shutdown();
            wait(|| unsafe { !IsWindow(hwnd).as_bool() });
        }
    }
}
