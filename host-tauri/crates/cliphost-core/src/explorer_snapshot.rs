//! Keep Shell COM waits off callers and coalesce concurrent Explorer queries.
use super::OpenedFolder;
use parking_lot::{Condvar, Mutex};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

const FRESH_FOR: Duration = Duration::from_secs(1);
pub(super) const QUERY_BUDGET: Duration = Duration::from_millis(250);

#[derive(Default)]
struct Published {
    folders: Vec<OpenedFolder>,
    checked_at: Option<Instant>,
    epoch: u64,
    refreshing: bool,
}

pub(super) struct ExplorerSnapshots {
    request: mpsc::SyncSender<()>,
    state: Arc<(Mutex<Published>, Condvar)>,
}

impl ExplorerSnapshots {
    pub(super) fn new(
        mut read: impl FnMut() -> Result<Vec<OpenedFolder>, String> + Send + 'static,
    ) -> Self {
        let (request, requests) = mpsc::sync_channel(1);
        let state = Arc::new((Mutex::new(Published::default()), Condvar::new()));
        let worker_state = state.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("cliphost-shell-query".into())
            .spawn(move || {
                // The production reader initializes its STA on this thread. No
                // COM interfaces leave it, and a stalled call cannot spawn more readers.
                while requests.recv().is_ok() {
                    let result = read();
                    let (state, changed) = &*worker_state;
                    let mut published = state.lock();
                    match result {
                        Ok(folders) => published.folders = folders,
                        Err(error) => {
                            tracing::warn!(%error, "Explorer refresh failed; retaining snapshot")
                        }
                    }
                    // Failed refreshes also get a retry interval. They must not
                    // look like every Explorer tab was closed or cause a retry storm.
                    published.checked_at = Some(Instant::now());
                    published.epoch = published.epoch.wrapping_add(1);
                    published.refreshing = false;
                    changed.notify_all();
                }
            })
        {
            tracing::warn!(%error, "Explorer query worker could not start");
        }
        Self { request, state }
    }

    pub(super) fn snapshot(&self, budget: Duration) -> Vec<OpenedFolder> {
        let deadline = Instant::now() + budget;
        let (state, changed) = &*self.state;
        let mut published = state.lock();
        if published
            .checked_at
            .is_some_and(|at| at.elapsed() < FRESH_FOR)
        {
            return published.folders.clone();
        }
        let epoch = published.epoch;
        if !published.refreshing {
            if self.request.try_send(()).is_err() {
                return published.folders.clone();
            }
            published.refreshing = true;
        }
        while published.epoch == epoch {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() || changed.wait_for(&mut published, remaining).timed_out() {
                break;
            }
        }
        published.folders.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn folders(path: &str) -> Vec<OpenedFolder> {
        vec![OpenedFolder {
            hwnd: 1,
            path: path.into(),
            title: "fixture".into(),
        }]
    }

    fn expire(service: &ExplorerSnapshots) {
        service.state.0.lock().checked_at = None;
    }

    #[test]
    fn explorer_snapshot_slow_shell_returns_cached_tabs_and_coalesces_callers() {
        let calls = Arc::new(AtomicUsize::new(0));
        let worker_calls = calls.clone();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let mut owner = None;
        let service = Arc::new(ExplorerSnapshots::new(move || {
            let thread = std::thread::current().id();
            assert_eq!(*owner.get_or_insert(thread), thread);
            if worker_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                return Ok(folders(r"C:\before"));
            }
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            Ok(folders(r"C:\after"))
        }));
        assert_eq!(
            service.snapshot(Duration::from_secs(3))[0].path,
            r"C:\before"
        );
        expire(&service);
        // Start one refresh, then leave Shell blocked while all callers return.
        assert_eq!(service.snapshot(Duration::ZERO)[0].path, r"C:\before");
        entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let (done_tx, done_rx) = mpsc::channel();
        for _ in 0..8 {
            let service = service.clone();
            let done_tx = done_tx.clone();
            std::thread::spawn(move || {
                done_tx
                    .send(service.snapshot(Duration::from_millis(30))[0].path.clone())
                    .unwrap();
            });
        }
        for _ in 0..8 {
            assert_eq!(
                done_rx.recv_timeout(Duration::from_secs(3)).unwrap(),
                r"C:\before"
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        release_tx.send(()).unwrap();
        assert_eq!(
            service.snapshot(Duration::from_secs(3))[0].path,
            r"C:\after"
        );
        assert_eq!(service.snapshot(Duration::ZERO)[0].path, r"C:\after");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn explorer_snapshot_error_retains_tabs_but_successful_empty_refresh_clears_them() {
        let mut call = 0;
        let service = ExplorerSnapshots::new(move || {
            call += 1;
            match call {
                1 => Ok(folders(r"C:\open")),
                2 => Err("Shell temporarily unavailable".into()),
                _ => Ok(Vec::new()),
            }
        });
        let budget = Duration::from_secs(3);
        assert_eq!(service.snapshot(budget)[0].path, r"C:\open");
        expire(&service);
        let after_error = service.snapshot(budget);
        assert_eq!(after_error[0].path, r"C:\open");
        let paths = after_error.into_iter().map(|f| f.path).collect::<Vec<_>>();
        assert!(super::super::newly_closed(&[r"C:\open".into()], &paths).is_empty());
        expire(&service);
        assert!(service.snapshot(budget).is_empty());
    }
}
