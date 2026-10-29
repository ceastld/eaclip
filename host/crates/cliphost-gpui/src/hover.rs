//! Typed GPUI adapter for the product's existing native hover popup.
//!
//! One service per host, one retained owner lease per window display session.
//! Bounds are client logical pixels. Bind/schedule on that owner's UI thread;
//! the native source owns the separate no-activate popup and its GDI resources.
//! Call `cancel` on list replacement/scroll and `set_menu_open` for menu leases.
//! Text is a plain tooltip, never a lossy replacement for HTML/QQ payloads.

#[path = "hover_state.rs"]
mod hover_state;
use hover_state::{Binding, Hide, Ledger};
pub use hover_state::{Owner, Ticket, HOVER_DELAY, WARM_GRACE};
#[path = "hover_content.rs"]
pub mod content;
pub use content::{
    row_hover_content, FileCard, HoverThumbnail, HoverThumbnailTarget, RichCard, RichPart,
    RowHoverContent, RowHoverInput, RowHoverSource,
};

// Reuses the same presenter, surface/style, budgets, GIF/APNG/WebP playback,
// revision-aware LRU and leave grace as the Tauri host. The shared source's
// runtime and Windows imports are supplied by the host crate, not by Tauri.
#[cfg(windows)]
#[path = "../../../shared/windows/src/hover_popup.rs"]
mod native_popup;

use cliphost_core::HostState;
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Instant,
};
use tokio::runtime::Handle;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ImageSource {
    History(String),
    Favorite(String),
    /// Local path or file URI, validated by the existing core resolver.
    Local(String),
}

impl ImageSource {
    fn reference(&self) -> String {
        // This is only the existing resolver's source identifier. No HTTP call
        // is made: hover_popup resolves the ID/path directly through HostState.
        match self {
            Self::History(id) => format!("/api/clip/items/{}/image", encode_component(id)),
            Self::Favorite(id) => format!("/api/clip/favorites/{}/image", encode_component(id)),
            Self::Local(path) => format!("/api/clip/local-image?path={}", encode_component(path)),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Content {
    Text(String),
    /// Resolve the original detail only after the complete hover delay.
    HistoryText(String),
    FavoriteText(String),
    Image {
        source: ImageSource,
        meta_line: String,
    },
    Video {
        path: String,
    },
    /// QQ text and local images in their original order, shown as one card
    /// by the GPUI hover card instead of a per-image native preview.
    Rich(RichCard),
    /// Several files (list + cascade preview) or one previewable text file.
    Files(FileCard),
}

#[derive(Clone, Debug)]
pub struct Request {
    /// x/y/width/height within the owner's client area, in logical pixels.
    pub anchor: [f64; 4],
    pub scale: f64,
    pub dark: bool,
    pub content: Content,
}

/// Rich and file cards are drawn by the GPUI hover card, not the native popup.
/// The sink runs on a runtime thread; it must only queue to the UI thread.
#[derive(Clone, Debug)]
pub enum CardCommand {
    Show {
        owner: isize,
        ticket: u64,
        /// Physical screen rectangle of the hovered row or chip.
        anchor: [i32; 4],
        /// Physical bounds to avoid for transparent owners such as menus.
        beside: Option<[i32; 4]>,
        scale: f64,
        dark: bool,
        /// A warm switch replaces visible content in place.
        warm: bool,
        content: Content,
    },
    /// `force: false` is the 180ms leave grace (pointer may enter the card).
    Hide { owner: isize, ticket: u64, force: bool },
}
pub type CardSink = Arc<dyn Fn(CardCommand) + Send + Sync>;
pub(crate) type CardAdmission = Arc<dyn Fn(isize, u64) -> bool + Send + Sync>;

#[derive(Clone, Debug)]
pub struct HoverError {
    pub ticket: Ticket,
    pub message: String,
}

struct ErrorEntry {
    binding: Binding,
    error: HoverError,
}
#[derive(Default)]
struct State {
    ledger: Ledger,
    errors: VecDeque<ErrorEntry>,
}
struct Inner {
    state: Mutex<State>,
    card: Mutex<Option<CardSink>>,
    host: HostState,
    runtime: Handle,
    #[cfg(windows)]
    native: Arc<native_popup::Service>,
}

/// Dropping the service closes the retained surface and cancels every owner.
pub struct HoverService {
    inner: Arc<Inner>,
}

/// Hold in an `Arc` when GPUI callbacks need shared access; dropping its last
/// reference retires this exact session. A new bind never reuses an old epoch.
pub struct HoverOwner {
    inner: Arc<Inner>,
    binding: Binding,
}

impl HoverService {
    pub fn new(host: HostState, runtime: Handle) -> Result<Self, String> {
        #[cfg(windows)]
        {
            let native = Arc::new(native_popup::Service::new()?);
            Ok(Self {
                inner: Arc::new(Inner {
                    state: Mutex::new(State::default()),
                    card: Mutex::new(None),
                    host,
                    runtime,
                    native,
                }),
            })
        }
        #[cfg(not(windows))]
        {
            let _ = (host, runtime);
            Err("Native hover is unavailable".into())
        }
    }

    /// Must run on the root owner's UI thread; no descendant is subclassed.
    pub fn bind_owner(&self, owner: Owner) -> Result<HoverOwner, String> {
        #[cfg(windows)]
        {
            check_owner(owner.hwnd)?;
            self.inner.native.bind_owner(owner.hwnd)?;
            attach_owner_hook(owner.hwnd, &self.inner)?;
        }
        #[cfg(not(windows))]
        return Err("Native hover is unavailable".into());
        #[cfg(windows)]
        {
            let (binding, hide) = {
                let mut state = self.inner.state.lock().unwrap();
                state
                    .errors
                    .retain(|entry| entry.binding.owner.hwnd != owner.hwnd);
                state.ledger.bind(owner)?
            };
            if let Some(hide) = hide {
                self.inner.send_hide(hide);
            }
            Ok(HoverOwner {
                inner: self.inner.clone(),
                binding,
            })
        }
    }

    pub fn shutdown(&self) {
        self.inner.shutdown();
    }

    /// Install the GPUI hover card. Without one, rich and file cards fall
    /// back to the plain row tooltip.
    pub fn set_card_sink(&self, sink: Option<CardSink>) {
        *self.inner.card.lock().unwrap() = sink;
    }

    /// Recheck at actual UI presentation, after the worker's queued Show may
    /// have been overtaken by cancellation. A weak lease avoids retaining the
    /// service through its own sink/receiver loop.
    pub(crate) fn card_admission(&self) -> CardAdmission {
        let inner = Arc::downgrade(&self.inner);
        Arc::new(move |owner, ticket| {
            inner.upgrade().is_some_and(|inner| {
                inner.state.lock().unwrap().ledger.card_request_current(owner, ticket, Instant::now())
            })
        })
    }
}

impl Drop for HoverService {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl HoverOwner {
    pub fn owner(&self) -> Owner {
        self.binding.owner
    }

    /// Session identity, for retained callbacks and NativeInteractions rebinding.
    /// An HWND hide/destroy hook or explicit `retire` invalidates this lease
    /// even while another row callback still holds an Arc to it.
    pub fn is_current(&self) -> bool {
        self.inner
            .state
            .lock()
            .unwrap()
            .ledger
            .is_bound(self.binding)
    }

    /// Retire immediately before a NativeInteractions root stops accepting or
    /// is dropped. Dropping only one Arc or canceling a request cannot retire
    /// other callbacks that still hold this owner lease.
    pub fn retire(&self) {
        let hide = {
            let mut state = self.inner.state.lock().unwrap();
            state.errors.retain(|entry| entry.binding != self.binding);
            state.ledger.retire(self.binding)
        };
        if let Some(hide) = hide {
            self.inner.send_hide(hide);
        }
    }

    /// Returns `None` for an image suppressed by an open menu. Resolution and
    /// decode begin only after the full 500ms delay; changed/retired requests
    /// are rejected again by the native service's monotonic ticket ledger.
    pub fn schedule(&self, request: Request) -> Result<Option<Ticket>, String> {
        self.schedule_beside(request, None)
    }

    /// The HWND still owns lifetime and input; only placement uses these
    /// client logical bounds instead of the transparent owner's full window.
    pub(crate) fn schedule_beside(
        &self,
        request: Request,
        beside: Option<[f64; 4]>,
    ) -> Result<Option<Ticket>, String> {
        #[cfg(not(windows))]
        {
            let _ = (request, beside);
            Err("Native hover is unavailable".into())
        }
        #[cfg(windows)]
        {
            let anchor = capture_anchor(self.binding.owner.hwnd, &request)?;
            let beside = beside.map(|bounds| {
                capture_anchor(self.binding.owner.hwnd, &Request {
                    anchor: bounds, ..request.clone()
                })
            }).transpose()?;
            let started = Instant::now();
            let card = matches!(&request.content, Content::Rich(_) | Content::Files(_));
            if card && self.inner.card.lock().unwrap().is_none() {
                return Ok(None);
            }
            let image = card
                || matches!(&request.content, Content::Image { .. } | Content::Video { .. });
            let scheduled = {
                let mut state = self.inner.state.lock().unwrap();
                let scheduled = state.ledger.begin(self.binding, image, started)?;
                if scheduled.is_some() {
                    state.errors.retain(|entry| entry.binding != self.binding);
                }
                scheduled.map(|(ticket, hide)| (ticket, hide, state.ledger.warm(self.binding, ticket)))
            };
            let Some((ticket, hide, warm)) = scheduled else {
                return Ok(None);
            };
            if let Some(hide) = hide {
                self.inner.send_hide(hide);
            }
            let inner = self.inner.clone();
            let binding = self.binding;
            // Cold image hovers decode during the delay so the preview paints
            // as soon as it is due instead of starting the decode then.
            if let (false, Content::Image { source, .. }) = (warm, &request.content) {
                let prefetch = self.inner.clone();
                let source = source.reference();
                let dark = request.dark;
                self.inner.runtime.spawn(async move {
                    prefetch
                        .native
                        .prefetch_image(source, dark, prefetch.host.clone())
                        .await;
                });
            }
            self.inner.runtime.spawn(async move {
                if !warm {
                    tokio::time::sleep_until(tokio::time::Instant::from_std(started + HOVER_DELAY))
                        .await;
                }
                if !inner
                    .state
                    .lock()
                    .unwrap()
                    .ledger
                    .due(binding, ticket, Instant::now())
                {
                    return;
                }
                let content = match inner
                    .resolve_content(binding, ticket, request.content)
                    .await
                {
                    Ok(Some(content)) => content,
                    Ok(None) => return,
                    Err(message) => {
                        inner.report(binding, ticket, message);
                        return;
                    }
                };
                if !inner
                    .state
                    .lock()
                    .unwrap()
                    .ledger
                    .due(binding, ticket, Instant::now())
                {
                    return;
                }
                if let Content::Rich(_) | Content::Files(_) = &content {
                    // Exactly one hover surface per owner: retire the native
                    // popup, then hand the card to the GPUI thread.
                    inner.send_native_hide(binding.owner.hwnd, ticket.id(), true);
                    if let Some(sink) = inner.card.lock().unwrap().clone() {
                        sink(CardCommand::Show {
                            owner: binding.owner.hwnd,
                            ticket: ticket.id(),
                            anchor,
                            beside,
                            scale: request.scale,
                            dark: request.dark,
                            warm,
                            content,
                        });
                    }
                    return;
                }
                if let Some(sink) = inner.card.lock().unwrap().clone() {
                    sink(CardCommand::Hide { owner: binding.owner.hwnd, ticket: ticket.id(), force: true });
                }
                let (action, image_src, text, meta_line) = match content {
                    Content::Text(text) => ("text", None, Some(text), None),
                    Content::Image { source, meta_line } => {
                        ("image", Some(source.reference()), None, Some(meta_line))
                    }
                    Content::Video { path } => ("video", Some(path), None, None),
                    Content::HistoryText(_)
                    | Content::FavoriteText(_)
                    | Content::Rich(_)
                    | Content::Files(_) => {
                        unreachable!("Resolved hover detail")
                    }
                };
                let native = native_popup::Request {
                    action: action.into(),
                    ticket: ticket.id(),
                    anchor: request.anchor,
                    image_src,
                    text,
                    meta_line,
                    dark: request.dark,
                    force: true,
                    keep: warm,
                    side_lock: true,
                };
                let result = inner
                    .native
                    .request_screen_beside(
                        binding.owner.hwnd,
                        request.scale,
                        native,
                        inner.host.clone(),
                        anchor,
                        beside,
                    )
                    .await;
                if let Err(message) = result {
                    inner.report(binding, ticket, message);
                }
            });
            Ok(Some(ticket))
        }
    }

    /// Use the ticket returned by this row's schedule so stale mouse-leave
    /// events cannot dismiss a newer row. Existing native 180ms gap grace stays.
    pub fn leave(&self, ticket: Ticket) {
        let hide = {
            let mut state = self.inner.state.lock().unwrap();
            // A decode may have failed and queued its error just before leave.
            // The exact old row's leave must also retire that queued feedback.
            state
                .errors
                .retain(|entry| entry.binding != self.binding || entry.error.ticket != ticket);
            state.ledger.leave(self.binding, ticket)
        };
        if let Some(hide) = hide {
            self.inner.send_hide(hide);
        }
    }
    /// For explicit input, scroll, resize, content change, hide or navigation.
    pub fn cancel(&self) {
        let hide = {
            let mut state = self.inner.state.lock().unwrap();
            state.errors.retain(|entry| entry.binding != self.binding);
            state.ledger.cancel(self.binding, true)
        };
        if let Some(hide) = hide {
            self.inner.send_hide(hide);
        }
    }
    /// Set before opening a menu and clear after its lease closes. Closing a
    /// menu never revives the old hover; a new pointer hover must schedule it.
    pub fn set_menu_open(&self, open: bool) -> Result<(), String> {
        let hide = {
            let mut state = self.inner.state.lock().unwrap();
            if open {
                state.errors.retain(|entry| entry.binding != self.binding);
            }
            state.ledger.menu(self.binding, open)?
        };
        if let Some(hide) = hide {
            self.inner.send_hide(hide);
        }
        Ok(())
    }
    /// Drain preview diagnostics for logging, without persistent action errors.
    /// Canceled/old sessions never deliver errors to a newer owner lease.
    pub fn take_error(&self) -> Option<HoverError> {
        let mut state = self.inner.state.lock().unwrap();
        if !state.ledger.is_bound(self.binding) {
            state.errors.retain(|entry| entry.binding != self.binding);
            return None;
        }
        let index = state
            .errors
            .iter()
            .position(|entry| entry.binding == self.binding)?;
        Some(state.errors.remove(index)?.error)
    }
}

impl Drop for HoverOwner {
    fn drop(&mut self) {
        self.retire();
    }
}

impl Inner {
    /// Worker admission is checked when the blocking job actually dequeues,
    /// then after the detail lookup. Old bodies and failures cannot reach a
    /// newer destination/session or install a native request ticket.
    async fn resolve_content(
        self: &Arc<Self>,
        binding: Binding,
        ticket: Ticket,
        content: Content,
    ) -> Result<Option<Content>, String> {
        let (id, favorite) = match content {
            Content::HistoryText(id) => (id, false),
            Content::FavoriteText(id) => (id, true),
            content => return Ok(Some(content)),
        };
        let inner = self.clone();
        tokio::task::spawn_blocking(move || {
            if !inner
                .state
                .lock()
                .unwrap()
                .ledger
                .due(binding, ticket, Instant::now())
            {
                return Ok(None);
            }
            let detail = if favorite {
                inner.host.favor_get(&json!({"id": id}))
            } else {
                inner.host.detail(&json!({"id": id}))
            };
            if !inner
                .state
                .lock()
                .unwrap()
                .ledger
                .due(binding, ticket, Instant::now())
            {
                return Ok(None);
            }
            detail.map(|detail| Some(Content::Text(detail_hover_text(&detail))))
        })
        .await
        .map_err(|error| error.to_string())?
    }

    fn send_hide(&self, hide: Hide) {
        if let Some(sink) = self.card.lock().unwrap().clone() {
            sink(CardCommand::Hide {
                owner: hide.owner.hwnd,
                ticket: hide.ticket.id(),
                force: hide.force,
            });
        }
        self.send_native_hide(hide.owner.hwnd, hide.ticket.id(), hide.force);
    }
    fn send_native_hide(&self, owner: isize, ticket: u64, force: bool) {
        #[cfg(windows)]
        {
            use std::{
                future::Future,
                task::{Context, Poll, Waker},
            };
            let request = native_popup::Request {
                action: "hide".into(),
                ticket,
                anchor: [0.0; 4],
                image_src: None,
                text: None,
                meta_line: None,
                dark: false,
                force,
                keep: false,
                side_lock: false,
            };
            // The existing native hide path has no await. Poll it immediately
            // to install its higher-ticket tombstone before returning to the
            // UI, even if an older show is between its delay check/first poll.
            let mut hide_future = std::pin::pin!(self.native.request(
                owner,
                1.0,
                request,
                self.host.clone()
            ));
            match hide_future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
            {
                Poll::Ready(Ok(())) => {}
                Poll::Ready(Err(error)) => tracing::warn!(%error, "Native hover hide failed"),
                Poll::Pending => {
                    self.native.hide_owner(owner);
                    tracing::error!("Native hover hide unexpectedly suspended");
                }
            }
        }
        #[cfg(not(windows))]
        let _ = (owner, ticket, force);
    }
    fn report(&self, binding: Binding, ticket: Ticket, message: String) {
        let hide = {
            let mut state = self.state.lock().unwrap();
            if !state.ledger.due(binding, ticket, Instant::now()) {
                return;
            }
            let hide = state.ledger.cancel(binding, true);
            if state.errors.len() >= 16 {
                state.errors.pop_front();
            }
            state.errors.push_back(ErrorEntry {
                binding,
                error: HoverError { ticket, message },
            });
            hide
        };
        if let Some(hide) = hide {
            self.send_hide(hide);
        }
    }
    fn owner_event(&self, hwnd: isize, retired: bool) {
        let hide = {
            let mut state = self.state.lock().unwrap();
            state
                .errors
                .retain(|entry| entry.binding.owner.hwnd != hwnd);
            if retired {
                state.ledger.destroy(hwnd)
            } else {
                state.ledger.cancel_window(hwnd)
            }
        };
        if let Some(hide) = hide {
            self.send_hide(hide);
        }
    }
    fn shutdown(&self) {
        let hide = {
            let mut state = self.state.lock().unwrap();
            state.errors.clear();
            state.ledger.close()
        };
        if let Some(hide) = hide {
            self.send_hide(hide);
        }
        #[cfg(windows)]
        self.native.shutdown();
    }
}

fn detail_hover_text(detail: &Value) -> String {
    content::detail_hover_text(detail)
}

fn encode_component(value: &str) -> String {
    use std::fmt::Write;
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

#[cfg(windows)]
fn check_owner(owner: isize) -> Result<(), String> {
    use crate::windows_api as windows;
    use windows::Win32::{
        Foundation::HWND,
        System::Threading::{GetCurrentProcessId, GetCurrentThreadId},
        UI::WindowsAndMessaging::{GetWindowThreadProcessId, IsWindow},
    };
    let hwnd = HWND(owner as *mut _);
    let mut pid = 0;
    let thread = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if owner == 0
        || !unsafe { IsWindow(hwnd).as_bool() }
        || pid != unsafe { GetCurrentProcessId() }
        || thread != unsafe { GetCurrentThreadId() }
    {
        return Err("Hover owner must be the current UI thread's own window".into());
    }
    Ok(())
}

#[cfg(windows)]
fn capture_anchor(owner: isize, request: &Request) -> Result<[i32; 4], String> {
    use crate::windows_api as windows;
    use windows::Win32::{
        Foundation::{HWND, POINT},
        Graphics::Gdi::ClientToScreen,
    };
    check_owner(owner)?;
    if !request.scale.is_finite()
        || request.scale <= 0.0
        || !request.anchor.iter().all(|value| value.is_finite())
        || request.anchor[2] <= 0.0
        || request.anchor[3] <= 0.0
    {
        return Err("Invalid hover anchor or scale".into());
    }
    let mut origin = POINT::default();
    if !unsafe { ClientToScreen(HWND(owner as *mut _), &mut origin).as_bool() } {
        return Err("Hover owner is unavailable".into());
    }
    let [x, y, width, height] = request.anchor;
    let coordinates = [
        origin.x as f64 + (x * request.scale).round(),
        origin.y as f64 + (y * request.scale).round(),
        origin.x as f64 + ((x + width) * request.scale).round(),
        origin.y as f64 + ((y + height) * request.scale).round(),
    ];
    if coordinates
        .iter()
        .any(|value| !value.is_finite() || *value < i32::MIN as f64 || *value > i32::MAX as f64)
    {
        return Err("Hover anchor exceeds screen coordinates".into());
    }
    let anchor = coordinates.map(|value| value as i32);
    if anchor[2] <= anchor[0] || anchor[3] <= anchor[1] {
        return Err("Hover anchor is smaller than a physical pixel".into());
    }
    Ok(anchor)
}

#[cfg(windows)]
fn attach_owner_hook(hwnd: isize, inner: &Arc<Inner>) -> Result<(), String> {
    use crate::windows_api as windows;
    use std::sync::Weak;
    use windows::Win32::{
        Foundation::HWND,
        UI::Shell::{GetWindowSubclass, RemoveWindowSubclass, SetWindowSubclass},
    };
    const HOOK: usize = 0xBC72;
    unsafe {
        let hwnd = HWND(hwnd as *mut _);
        let mut previous = 0;
        if GetWindowSubclass(hwnd, Some(owner_proc), HOOK, Some(&mut previous)).as_bool() {
            let weak = &*(previous as *const Weak<Inner>);
            if weak.upgrade().is_some_and(|old| Arc::ptr_eq(&old, inner)) {
                return Ok(());
            }
            if !RemoveWindowSubclass(hwnd, Some(owner_proc), HOOK).as_bool() {
                return Err("Cannot replace hover owner cleanup".into());
            }
            drop(Box::from_raw(previous as *mut Weak<Inner>));
        }
        let hook = Box::new(Arc::downgrade(inner));
        if !SetWindowSubclass(hwnd, Some(owner_proc), HOOK, &*hook as *const _ as usize).as_bool() {
            return Err("Cannot register hover owner cleanup".into());
        }
        let _ = Box::into_raw(hook);
    }
    Ok(())
}

#[cfg(windows)]
unsafe extern "system" fn owner_proc(
    hwnd: crate::windows_api::Win32::Foundation::HWND,
    message: u32,
    wp: crate::windows_api::Win32::Foundation::WPARAM,
    lp: crate::windows_api::Win32::Foundation::LPARAM,
    id: usize,
    data: usize,
) -> crate::windows_api::Win32::Foundation::LRESULT {
    use crate::windows_api as windows;
    use std::sync::Weak;
    use windows::Win32::UI::{
        Shell::{DefSubclassProc, RemoveWindowSubclass},
        WindowsAndMessaging::*,
    };
    let weak = &*(data as *const Weak<Inner>);
    if let Some(inner) = weak.upgrade() {
        match message {
            WM_NCDESTROY => inner.owner_event(hwnd.0 as isize, true),
            WM_SHOWWINDOW if wp.0 == 0 => inner.owner_event(hwnd.0 as isize, true),
            WM_MOVE | WM_SIZE | WM_DPICHANGED | WM_MOUSEWHEEL | WM_MOUSEHWHEEL | WM_KEYDOWN
            | WM_SYSKEYDOWN | WM_CONTEXTMENU | WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN
            | WM_XBUTTONDOWN => inner.owner_event(hwnd.0 as isize, false),
            _ => {}
        }
    }
    if message == WM_NCDESTROY {
        let _ = RemoveWindowSubclass(hwnd, Some(owner_proc), id);
        drop(Box::from_raw(data as *mut Weak<Inner>));
    }
    DefSubclassProc(hwnd, message, wp, lp)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn image_references_preserve_paths_and_ids_as_single_url_components() {
        assert_eq!(
            ImageSource::History("a/b+c&d".into()).reference(),
            "/api/clip/items/a%2Fb%2Bc%26d/image"
        );
        assert_eq!(
            ImageSource::Favorite("收藏".into()).reference(),
            "/api/clip/favorites/%E6%94%B6%E8%97%8F/image"
        );
        assert_eq!(
            ImageSource::Local(r"C:\夹具\a b+#.png".into()).reference(),
            "/api/clip/local-image?path=C%3A%5C%E5%A4%B9%E5%85%B7%5Ca%20b%2B%23.png"
        );
    }
    #[test]
    fn detail_text_preserves_original_empty_body_and_whitespace() {
        assert_eq!(
            detail_hover_text(&json!({"bodyText":"  原文\r\n\t", "summary":"short"})),
            "  原文\r\n\t"
        );
        assert_eq!(
            detail_hover_text(&json!({"bodyText":"", "displayText":"fallback"})),
            ""
        );
        assert_eq!(
            detail_hover_text(&json!({"bodyText":null, "displayText":"full"})),
            "full"
        );
        assert_eq!(
            detail_hover_text(&json!({"summary":"other-kind", "title":"title"})),
            "other-kind"
        );
    }
}
