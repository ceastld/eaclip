//! Accepted OLE writes are registered on the UI before worker dispatch. This
//! registry outlives the source window, so Exit cannot abandon a queued drop.

use std::{
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

#[derive(Default)]
struct Registry {
    exit_pending: bool,
    writes: Vec<Arc<Mutex<Option<Result<(), String>>>>>,
}

static WRITES: OnceLock<Mutex<Registry>> = OnceLock::new();

pub fn set_exit_pending(pending: bool) {
    WRITES
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .exit_pending = pending;
}

pub(crate) struct AcceptedWrite {
    state: Arc<Mutex<Option<Result<(), String>>>>,
}

pub(crate) fn accept() -> Result<AcceptedWrite, String> {
    let mut registry = WRITES.get_or_init(Default::default).lock().unwrap();
    if registry.exit_pending {
        return Err("The application is waiting for accepted writes before exiting.".into());
    }
    let state = Arc::new(Mutex::new(None));
    registry.writes.push(state.clone());
    Ok(AcceptedWrite { state })
}

impl AcceptedWrite {
    pub fn finish(self, result: &Result<serde_json::Value, String>) {
        *self.state.lock().unwrap() = Some(result.as_ref().map(|_| ()).map_err(Clone::clone));
        if result.is_ok() {
            WRITES
                .get_or_init(Default::default)
                .lock()
                .unwrap()
                .writes
                .retain(|write| !Arc::ptr_eq(write, &self.state));
        }
    }
}

impl Drop for AcceptedWrite {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap();
        if state.is_none() {
            *state = Some(Err("The native drop write worker was interrupted.".into()));
        }
    }
}

/// Call after closing admission with set_exit_pending(true). A failure keeps
/// the app alive and is reported once; the UI also receives the original reply.
pub async fn flush() -> Result<(), String> {
    let writes = WRITES
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .writes
        .clone();
    let mut errors = Vec::new();
    for write in &writes {
        loop {
            let result = write.lock().unwrap().clone();
            if let Some(result) = result {
                if let Err(error) = result {
                    errors.push(error);
                }
                break;
            }
            gpui::Timer::after(Duration::from_millis(10)).await;
        }
    }
    WRITES
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .writes
        .retain(|write| !writes.iter().any(|finished| Arc::ptr_eq(write, finished)));
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;

    #[tokio::test]
    async fn exit_waits_for_an_accepted_worker_that_has_not_started_and_reports_its_failure() {
        set_exit_pending(false);
        let queued = accept().unwrap();
        set_exit_pending(true);
        assert!(
            accept().is_err(),
            "all windows stop admitting new drops during Exit"
        );
        let (release, wait) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            wait.recv().unwrap();
            queued.finish(&Err("Captured folder was removed.".into()));
        });
        let mut barrier = std::pin::pin!(flush());
        use std::task::{Context, Poll, Waker};
        assert!(matches!(
            barrier
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop())),
            Poll::Pending
        ));
        release.send(()).unwrap();
        assert_eq!(barrier.await, Err("Captured folder was removed.".into()));
        worker.join().unwrap();
        assert!(
            flush().await.is_ok(),
            "reported failure does not permanently prevent Exit"
        );
        set_exit_pending(false);
    }
}
