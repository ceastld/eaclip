//! WebView2 failure callbacks only enqueue work; recovery runs on the window thread.
//! No COM objects or HostInner locks are retained in this cross-platform mailbox.
use std::{collections::HashMap, sync::{Arc, Mutex}, time::{Duration, Instant}};
use tokio::sync::Notify;

const RETRY_DELAY: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Action { Reload, Recreate }

#[derive(Clone, Debug)]
pub struct Recovery {
    pub label: String,
    pub generation: u64,
    pub action: Action,
    request_id: u64,
}

struct Pending { action: Action, retry_at: Option<Instant> }
struct Flight { request_id: u64, action: Action }
struct Window {
    generation: u64,
    suspect: bool,
    pending: Option<Pending>,
    flight: Option<Flight>,
    wakeup: Arc<Notify>,
}
#[derive(Default)]
struct State {
    generation: u64,
    request_id: u64,
    windows: HashMap<String, Window>,
}

#[derive(Clone, Default)]
pub struct Service(Arc<Mutex<State>>);

impl Service {
    /// Call once for each newly created WebView, on its owning UI thread.
    /// A new generation retires callbacks and recovery replies from its predecessor.
    pub fn watch(&self, window: &tauri::WebviewWindow, wakeup: Arc<Notify>) -> Result<u64, String> {
        let label = window.label().to_owned();
        let generation = self.begin_watch(&label, wakeup);
        #[cfg(windows)]
        if let Err(error) = native_watch(window, self.clone(), label.clone(), generation) {
            self.registration_failed(&label, generation);
            return Err(error);
        }
        Ok(generation)
    }

    fn begin_watch(&self, label: &str, wakeup: Arc<Notify>) -> u64 {
        let mut state = self.0.lock().unwrap();
        state.generation = state.generation.wrapping_add(1);
        let generation = state.generation;
        state.windows.insert(label.to_owned(), Window {
            generation, suspect: false, pending: None, flight: None, wakeup,
        });
        generation
    }

    pub fn current_generation(&self, label: &str) -> Option<u64> {
        self.0.lock().unwrap().windows.get(label).map(|window| window.generation)
    }

    pub fn pending_action(&self, label: &str) -> Option<Action> {
        let state = self.0.lock().unwrap();
        let window = state.windows.get(label)?;
        window.pending.as_ref().map(|pending| pending.action)
            .into_iter().chain(window.flight.as_ref().map(|flight| flight.action)).max()
    }

    fn mark_unresponsive(&self, label: &str, generation: u64) -> bool {
        let mut state = self.0.lock().unwrap();
        let Some(window) = state.windows.get_mut(label)
            .filter(|window| window.generation == generation) else { return false; };
        window.suspect = true;
        true
    }

    /// Temporary load must not interrupt an editor. An explicit subsequent Show
    /// opts into rebuilding only the still-current view reported unresponsive.
    pub fn recover_on_show(&self, label: &str) {
        let wakeup = {
            let mut state = self.0.lock().unwrap();
            let Some(window) = state.windows.get_mut(label).filter(|window| window.suspect)
                else { return; };
            window.suspect = false;
            window.pending = Some(Pending { action: Action::Recreate, retry_at: None });
            window.wakeup.clone()
        };
        wakeup.notify_one();
    }

    /// A user-requested document that never finished loading must not keep
    /// every Show deferred forever after Reload merely accepted the request.
    pub fn recover_stalled_show(&self, label: &str) -> bool {
        let wakeup = {
            let mut state = self.0.lock().unwrap();
            let Some(window) = state.windows.get_mut(label) else { return false; };
            if window.flight.as_ref().is_some_and(|flight| flight.action == Action::Recreate)
                || window.pending.as_ref().is_some_and(|pending| pending.action == Action::Recreate)
            { return false; }
            match &mut window.pending {
                Some(pending) => pending.action = Action::Recreate,
                None => window.pending = Some(Pending { action: Action::Recreate, retry_at: None }),
            }
            window.wakeup.clone()
        };
        wakeup.notify_one();
        true
    }

    fn failure(&self, label: &str, generation: u64, action: Action) -> bool {
        let wakeup = {
            let mut state = self.0.lock().unwrap();
            let Some(window) = state.windows.get_mut(label)
                .filter(|window| window.generation == generation) else { return false; };
            match &mut window.pending {
                Some(pending) => pending.action = pending.action.max(action),
                None => window.pending = Some(Pending { action, retry_at: None }),
            }
            window.wakeup.clone()
        };
        wakeup.notify_one();
        true
    }

    #[cfg(windows)]
    fn registration_failed(&self, label: &str, generation: u64) {
        if self.failure(label, generation, Action::Recreate) {
            let mut state = self.0.lock().unwrap();
            if let Some(pending) = state.windows.get_mut(label)
                .filter(|window| window.generation == generation)
                .and_then(|window| window.pending.as_mut()) {
                pending.retry_at = Some(Instant::now() + RETRY_DELAY);
            }
        }
    }

    /// Each request is leased until succeeded/failed, so the 50ms maintenance
    /// poll cannot issue overlapping reloads or consume a failed rebuild forever.
    pub fn take_pending(&self) -> Vec<Recovery> { self.take_pending_at(Instant::now()) }

    fn take_pending_at(&self, now: Instant) -> Vec<Recovery> {
        let mut state = self.0.lock().unwrap();
        let mut labels: Vec<_> = state.windows.iter().filter_map(|(label, window)| {
            (window.flight.is_none() && window.pending.as_ref().is_some_and(|pending| {
                pending.retry_at.is_none_or(|retry_at| retry_at <= now)
            })).then(|| label.clone())
        }).collect();
        // The panel's environment adapter is prepared from the clipboard view.
        labels.sort_by_key(|label| (label != "clipboard", label.clone()));
        let mut requests = Vec::with_capacity(labels.len());
        for label in labels {
            state.request_id = state.request_id.wrapping_add(1);
            let request_id = state.request_id;
            let window = state.windows.get_mut(&label).unwrap();
            let pending = window.pending.take().unwrap();
            window.flight = Some(Flight { request_id, action: pending.action });
            requests.push(Recovery { label, generation: window.generation,
                action: pending.action, request_id });
        }
        requests
    }

    pub fn is_current(&self, request: &Recovery) -> bool {
        self.0.lock().unwrap().windows.get(&request.label).is_some_and(|window| {
            matches_request(window, request)
        })
    }

    /// New faults queued during a reload remain pending; finishing the old work
    /// cannot erase a subsequently reported browser failure.
    pub fn succeeded(&self, request: &Recovery) {
        let mut state = self.0.lock().unwrap();
        if let Some(window) = state.windows.get_mut(&request.label)
            .filter(|window| matches_request(window, request)) {
            window.flight = None;
        }
    }

    pub fn failed(&self, request: &Recovery) { self.failed_at(request, Instant::now()); }

    fn failed_at(&self, request: &Recovery, now: Instant) {
        let wakeup = {
            let mut state = self.0.lock().unwrap();
            let Some(window) = state.windows.get_mut(&request.label)
                .filter(|window| matches_request(window, request)) else { return; };
            window.flight = None;
            // A rejected Reload cannot revive its controller. Escalate once,
            // rather than issuing the same failed HRESULT every 500ms forever.
            window.pending = Some(Pending { action: Action::Recreate, retry_at: Some(now + RETRY_DELAY) });
            window.wakeup.clone()
        };
        // The existing maintenance poll observes the deadline even if this wake
        // happens before it. No timer or recovery COM call runs in the callback.
        wakeup.notify_one();
    }
}

fn matches_request(window: &Window, request: &Recovery) -> bool {
    window.generation == request.generation
        && window.flight.as_ref().is_some_and(|flight| flight.request_id == request.request_id)
}

#[cfg(windows)]
fn native_watch(window: &tauri::WebviewWindow, service: Service, label: String,
    generation: u64) -> Result<(), String> {
    use webview2_com::{ProcessFailedEventHandler, Microsoft::Web::WebView2::Win32::*};
    use windows61::core::Interface;
    let (sender, receiver) = std::sync::mpsc::channel();
    window.with_webview(move |view| {
        let result = (|| -> Result<(), String> {
            let core = unsafe { view.controller().CoreWebView2() }.map_err(|error| error.to_string())?;
            let handler = ProcessFailedEventHandler::create(Box::new(move |_, args| {
                let Some(args) = args else { return Ok(()); };
                let mut kind = COREWEBVIEW2_PROCESS_FAILED_KIND::default();
                unsafe { args.ProcessFailedKind(&mut kind)?; }
                let (reason, exit_code) = args.cast::<ICoreWebView2ProcessFailedEventArgs2>()
                    .ok().map(|detail| unsafe {
                        let mut reason = COREWEBVIEW2_PROCESS_FAILED_REASON::default();
                        let mut code = 0;
                        (detail.Reason(&mut reason).ok().map(|_| reason.0),
                            detail.ExitCode(&mut code).ok().map(|_| code))
                    }).unwrap_or((None, None));
                // Reading event args is the only COM operation here. Recovery
                // must wait until WebView2 returns from this event callback.
                let action = match kind {
                    COREWEBVIEW2_PROCESS_FAILED_KIND_BROWSER_PROCESS_EXITED => Some(Action::Recreate),
                    COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_EXITED => Some(Action::Reload),
                    _ => None,
                };
                if let Some(action) = action {
                    if service.failure(&label, generation, action) {
                        tracing::warn!(%label, generation, kind = kind.0, ?reason, ?exit_code, ?action, "WebView process failed; recovery queued");
                    }
                } else if kind == COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_UNRESPONSIVE {
                    if service.mark_unresponsive(&label, generation) {
                        tracing::warn!(%label, generation, ?reason, ?exit_code, "WebView renderer unresponsive; recovery waits for explicit Show");
                    }
                } else if service.current_generation(&label) == Some(generation) {
                    // GPU/utility exits auto-recover and do not require reload.
                    tracing::warn!(%label, generation, kind = kind.0, ?reason, ?exit_code, "WebView process failure observed");
                }
                Ok(())
            }));
            let mut token = 0;
            unsafe { core.add_ProcessFailed(&handler, &mut token) }.map_err(|error| error.to_string())?;
            // The control owns this event registration until its destruction.
            // The callback holds only the mailbox, never the control itself.
            Ok(())
        })();
        let _ = sender.send(result);
    }).map_err(|error| error.to_string())?;
    receiver.try_recv().map_err(|_| "WebView recovery watcher must be installed on its UI thread".to_owned())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webview_recovery_browser_failure_overrides_reload_including_in_flight_work() {
        let service = Service::default();
        let generation = service.begin_watch("clipboard", Arc::new(Notify::new()));
        service.failure("clipboard", generation, Action::Reload);
        let reload = service.take_pending().pop().unwrap();
        assert!(service.take_pending().is_empty(), "polls must not overlap recovery");
        service.failure("clipboard", generation, Action::Recreate);
        service.failure("clipboard", generation, Action::Reload);
        assert_eq!(service.pending_action("clipboard"), Some(Action::Recreate));
        service.succeeded(&reload);
        let recreate = service.take_pending().pop().unwrap();
        assert_eq!(recreate.action, Action::Recreate, "old success erased the newer browser failure");
        service.succeeded(&recreate);
        assert_eq!(service.pending_action("clipboard"), None);
    }

    #[test]
    fn webview_recovery_old_callbacks_and_completions_cannot_change_replacement() {
        let service = Service::default();
        let wakeup = Arc::new(Notify::new());
        let old_generation = service.begin_watch("clipboard", wakeup.clone());
        service.failure("clipboard", old_generation, Action::Recreate);
        let old_request = service.take_pending().pop().unwrap();
        let new_generation = service.begin_watch("clipboard", wakeup);
        assert_ne!(old_generation, new_generation);
        assert!(!service.failure("clipboard", old_generation, Action::Recreate));
        assert!(!service.is_current(&old_request));
        service.failure("clipboard", new_generation, Action::Reload);
        let new_request = service.take_pending().pop().unwrap();
        service.succeeded(&old_request);
        service.failed(&old_request);
        assert!(service.is_current(&new_request), "old completion retired the new view");
        service.succeeded(&new_request);
        assert!(service.take_pending().is_empty());
    }

    #[test]
    fn webview_recovery_failed_rebuild_retries_after_backoff_without_downgrading() {
        let service = Service::default();
        let generation = service.begin_watch("clipboard", Arc::new(Notify::new()));
        service.failure("clipboard", generation, Action::Recreate);
        let now = Instant::now();
        let request = service.take_pending_at(now).pop().unwrap();
        service.failed_at(&request, now);
        service.failure("clipboard", generation, Action::Reload);
        assert!(service.take_pending_at(now + RETRY_DELAY / 2).is_empty());
        let retry = service.take_pending_at(now + RETRY_DELAY).pop().unwrap();
        assert_eq!(retry.action, Action::Recreate);
        assert!(!service.is_current(&request), "consumed request was reused by retry");
        service.succeeded(&request);
        assert!(service.is_current(&retry));
        service.succeeded(&retry);
        assert!(service.take_pending().is_empty());
    }

    #[test]
    fn webview_recovery_unresponsive_waits_for_explicit_show_and_retires_with_generation() {
        let service = Service::default();
        let wakeup = Arc::new(Notify::new());
        let old_generation = service.begin_watch("clipboard", wakeup.clone());
        assert!(service.mark_unresponsive("clipboard", old_generation));
        assert_eq!(service.pending_action("clipboard"), None);
        assert!(service.take_pending().is_empty(), "temporary load interrupted an editor");
        service.recover_on_show("clipboard");
        let request = service.take_pending().pop().unwrap();
        assert_eq!(request.action, Action::Recreate);
        assert_eq!(request.generation, old_generation);

        let new_generation = service.begin_watch("clipboard", wakeup);
        assert_ne!(old_generation, new_generation);
        assert!(!service.mark_unresponsive("clipboard", old_generation));
        service.recover_on_show("clipboard");
        assert!(service.take_pending().is_empty(), "old suspect leaked into replacement");
    }

    #[test]
    fn rejected_reload_escalates_and_stalled_show_coalesces_with_recovery() {
        let service = Service::default();
        let generation = service.begin_watch("clipboard", Arc::new(Notify::new()));
        service.failure("clipboard", generation, Action::Reload);
        let now = Instant::now();
        let reload = service.take_pending_at(now).pop().unwrap();
        service.failed_at(&reload, now);
        assert_eq!(service.pending_action("clipboard"), Some(Action::Recreate));
        assert!(!service.recover_stalled_show("clipboard"), "a pending rebuild should be reused");
        assert!(service.take_pending_at(now + RETRY_DELAY / 2).is_empty(), "show bypassed failure backoff");
        let rebuild = service.take_pending_at(now + RETRY_DELAY).pop().unwrap();
        assert_eq!(rebuild.action, Action::Recreate);
        assert!(!service.recover_stalled_show("clipboard"), "in-flight rebuild should be reused");
        assert!(service.take_pending().is_empty(), "overlapping rebuilds were leased");
        service.succeeded(&rebuild);
        assert!(service.recover_stalled_show("clipboard"));
        let stalled = service.take_pending().pop().unwrap();
        assert_eq!(stalled.action, Action::Recreate);
        assert_eq!(stalled.generation, generation);
        service.succeeded(&stalled);
        assert!(!service.recover_stalled_show("missing"));
    }
}
