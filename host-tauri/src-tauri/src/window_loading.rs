//! Secondary WebViews are created once, on demand. Native construction can pump
//! messages: document readiness alone must not release work before HWND setup.
use cliphost_core::host::ChromeTask;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

// A completed native controller can still lose its document under memory
// pressure. Only a requested Show gives permission to retire such a load.
const SHOW_LOAD_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Default)]
struct WindowLoad {
    built: bool,
    ready: bool,
    loading_since: Option<Instant>,
    pending: Option<ChromeTask>,
}

pub struct WindowLoading {
    pub api_port: u16,
    pub vite_base: Option<String>,
    windows: Mutex<HashMap<String, WindowLoad>>,
}

impl WindowLoading {
    pub fn new(api_port: u16, vite_base: Option<String>) -> Self {
        Self { api_port, vite_base, windows: Mutex::new(HashMap::new()) }
    }

    pub fn begin_create(&self, label: &str) -> bool {
        use std::collections::hash_map::Entry;
        match self.windows.lock().unwrap().entry(label.to_owned()) {
            Entry::Vacant(entry) => {
                entry.insert(WindowLoad { loading_since: Some(Instant::now()), ..Default::default() });
                true
            }
            Entry::Occupied(_) => false,
        }
    }

    pub fn built(&self, label: &str) {
        if let Some(window) = self.windows.lock().unwrap().get_mut(label) {
            window.built = true;
        }
    }

    /// Creation and saved-state restoration pump native geometry events.
    /// They must not overwrite the saved startup snapshot before it is applied.
    pub fn is_built(&self, label: &str) -> bool {
        self.windows.lock().unwrap().get(label).is_some_and(|window| window.built)
    }

    pub fn page_ready(&self, label: &str, ready: bool) {
        self.page_ready_at(label, ready, Instant::now());
    }

    fn page_ready_at(&self, label: &str, ready: bool, now: Instant) {
        if let Some(window) = self.windows.lock().unwrap().get_mut(label) {
            if ready { window.loading_since = None; }
            else if window.ready || window.loading_since.is_none() {
                window.loading_since = Some(now);
            }
            window.ready = ready;
        }
    }

    pub fn stalled_for_show(&self, label: &str) -> bool {
        let now = Instant::now();
        self.windows.lock().unwrap().get(label).is_some_and(|window| stalled(window, now))
    }

    /// The user need not click a second time when a requested load stops making
    /// progress. The UI loop revalidates each task against current visibility.
    pub fn take_stalled_shows(&self) -> Vec<(String, ChromeTask)> {
        self.take_stalled_shows_at(Instant::now())
    }

    fn take_stalled_shows_at(&self, now: Instant) -> Vec<(String, ChromeTask)> {
        self.windows.lock().unwrap().iter_mut().filter_map(|(label, window)| {
            stalled(window, now).then(|| window.pending.take())
                .flatten().map(|task| (label.clone(), task))
        }).collect()
    }

    pub fn remove(&self, label: &str) {
        self.windows.lock().unwrap().remove(label);
    }

    /// Coalesce shows while loading; HostState retains the latest route/content.
    /// Callers still revalidate visibility immediately before executing the task.
    pub fn defer_show(&self, label: &str, task: ChromeTask) -> bool {
        let mut windows = self.windows.lock().unwrap();
        let window = windows.get_mut(label).expect("window creation registered");
        if window.built && window.ready {
            window.pending = None;
            false
        } else {
            window.pending = Some(task);
            true
        }
    }

    pub fn take_ready(&self) -> Vec<ChromeTask> {
        self.windows.lock().unwrap().values_mut()
            .filter(|window| window.built && window.ready)
            .filter_map(|window| window.pending.take()).collect()
    }
}

fn stalled(window: &WindowLoad, now: Instant) -> bool {
    window.built && !window.ready && window.loading_since
        .is_some_and(|started| now.saturating_duration_since(started) >= SHOW_LOAD_TIMEOUT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cliphost_core::{clipboard, paths::AppPaths, ChromeCommand, HostState};

    #[test]
    fn cold_show_waits_for_both_native_setup_and_document_then_honors_cancel() {
        clipboard::force_headless(true);
        let root = tempfile::tempdir().unwrap();
        let host = HostState::open(AppPaths::from_data_root(root.path().into()), 28965).unwrap();
        host.show_web_route("/settings/clip");
        let show = host.take_chrome_task().unwrap();
        assert_eq!(show.command(), ChromeCommand::ShowMain);
        assert!(host.prepare_chrome_task(show));
        let windows = WindowLoading::new(28965, None);
        assert!(windows.begin_create("main"));
        assert!(!windows.is_built("main"), "builder geometry must not replace saved startup properties");
        assert!(windows.defer_show("main", show));
        // WebView2 can complete navigation inside its construction message pump.
        windows.page_ready("main", true);
        assert!(!windows.is_built("main"), "page readiness is earlier than native restore completion");
        assert!(!windows.begin_create("main"));
        assert!(windows.take_ready().is_empty());
        host.mark_main_hidden();
        windows.built("main");
        assert!(windows.is_built("main"), "normal geometry events can persist after native restore");
        let pending = windows.take_ready();
        assert_eq!(pending.len(), 1);
        assert!(!host.prepare_chrome_task(pending[0]), "cancelled cold show reopened a window");
        assert!(windows.take_ready().is_empty());
        host.show_web_route("/settings/appearance");
        while let Some(task) = host.take_chrome_task() {
            if task.command() == ChromeCommand::ShowMain {
                assert!(!windows.defer_show("main", task));
                assert!(host.prepare_chrome_task(task));
            }
        }
        assert_eq!(host.take_pending_nav("main").as_deref(), Some("/settings/appearance"));
    }

    #[test]
    fn loading_coalesces_shows_without_consuming_routes_and_failed_creation_can_retry() {
        clipboard::force_headless(true);
        let root = tempfile::tempdir().unwrap();
        let host = HostState::open(AppPaths::from_data_root(root.path().into()), 28965).unwrap();
        let windows = WindowLoading::new(28965, None);
        assert!(windows.begin_create("main"));
        windows.built("main");
        for route in ["/settings/clip", "/docs", "/main/home"] {
            host.show_web_route(route);
            assert!(windows.defer_show("main", host.take_chrome_task().unwrap()));
        }
        assert!(windows.take_ready().is_empty());
        windows.page_ready("main", true);
        assert_eq!(windows.take_ready().len(), 1);
        assert_eq!(host.take_pending_nav("main").as_deref(), Some("/main/home"));
        assert!(windows.begin_create("panel"));
        windows.remove("panel");
        assert!(windows.begin_create("panel"));
        assert!(!windows.begin_create("main"), "another window's failure evicted main");
    }

    #[test]
    fn stalled_requested_show_recovers_once_and_hidden_intent_cancels_it() {
        clipboard::force_headless(true);
        let root = tempfile::tempdir().unwrap();
        let host = HostState::open(AppPaths::from_data_root(root.path().into()), 28965).unwrap();
        host.show_web_route("/settings/clip");
        let show = host.take_chrome_task().unwrap();
        let windows = WindowLoading::new(28965, None);
        assert!(windows.begin_create("main"));
        let started = Instant::now();
        windows.windows.lock().unwrap().get_mut("main").unwrap().loading_since = Some(started);
        assert!(windows.defer_show("main", show));
        assert!(windows.take_stalled_shows_at(started + SHOW_LOAD_TIMEOUT).is_empty(),
            "a reentrant builder must not be retired before native setup completes");
        windows.built("main");
        assert!(windows.take_stalled_shows_at(started + SHOW_LOAD_TIMEOUT / 2).is_empty());
        host.mark_main_hidden();
        let stalled = windows.take_stalled_shows_at(started + SHOW_LOAD_TIMEOUT);
        assert_eq!(stalled.len(), 1);
        assert!(!host.prepare_chrome_task(stalled[0].1), "timeout reopened a cancelled show");
        assert!(windows.take_stalled_shows_at(started + SHOW_LOAD_TIMEOUT).is_empty());
        windows.remove("main");
        assert!(windows.begin_create("main"));
        assert!(!windows.stalled_for_show("main"), "replacement inherited the old deadline");
    }

    #[test]
    fn finished_document_clears_timeout_and_repeated_started_does_not_postpone_it() {
        let windows = WindowLoading::new(28965, None);
        assert!(windows.begin_create("clipboard"));
        windows.built("clipboard");
        let started = Instant::now();
        windows.page_ready_at("clipboard", true, started);
        windows.page_ready_at("clipboard", false, started);
        windows.page_ready_at("clipboard", false, started + SHOW_LOAD_TIMEOUT / 2);
        let states = windows.windows.lock().unwrap();
        assert!(stalled(states.get("clipboard").unwrap(), started + SHOW_LOAD_TIMEOUT));
        drop(states);
        windows.page_ready_at("clipboard", true, started + SHOW_LOAD_TIMEOUT);
        assert!(!windows.stalled_for_show("clipboard"));
        assert_eq!(windows.windows.lock().unwrap().get("clipboard").unwrap().loading_since, None);
    }
}
