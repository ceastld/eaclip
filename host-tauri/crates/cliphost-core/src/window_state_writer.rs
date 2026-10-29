//! Coalesce window events without performing file I/O on the window thread.
use crate::window_state::SavedWindowState;
use parking_lot::Mutex;
use std::{
    collections::HashMap,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

const SAVE_DELAY: Duration = Duration::from_secs(1);

#[derive(Default)]
pub(crate) struct WindowStateWriter {
    pending: Mutex<Pending>,
    writing: Mutex<()>,
    wakeup: Arc<tokio::sync::Notify>,
}

#[derive(Default)]
struct Pending {
    revision: u64,
    entries: HashMap<String, Entry>,
    latest: HashMap<String, SavedWindowState>,
}

#[derive(Clone)]
struct Entry {
    state: SavedWindowState,
    revision: u64,
    due: Instant,
}

impl WindowStateWriter {
    pub fn with_notifier(wakeup: Arc<tokio::sync::Notify>) -> Self {
        Self { wakeup, ..Self::default() }
    }

    pub fn remember(&self, state: SavedWindowState, now: Instant) {
        let mut pending = self.pending.lock();
        Self::queue(&mut pending, state, now);
        drop(pending);
        self.wakeup.notify_one();
    }

    fn queue(pending: &mut Pending, state: SavedWindowState, now: Instant) {
        pending.revision = pending.revision.wrapping_add(1);
        let revision = pending.revision;
        pending.latest.insert(state.name.clone(), state.clone());
        pending.entries.insert(
            state.name.clone(),
            Entry {
                state,
                revision,
                due: now + SAVE_DELAY,
            },
        );
    }

    /// Cache an initial disk restore without replacing a newer native snapshot.
    pub fn cache_loaded(&self, state: SavedWindowState) -> SavedWindowState {
        self.pending.lock().latest.entry(state.name.clone()).or_insert(state).clone()
    }

    /// Property changes use the last geometry even after its dirty entry was flushed.
    /// The caller holds the host state lock, as it does for geometry snapshots.
    pub fn remember_property(&self, name: &str, key: &str, value: String, now: Instant) {
        let mut pending = self.pending.lock();
        let Some(mut state) = pending.latest.get(name).cloned() else { return; };
        if state.properties.get(key) == Some(&value) { return; }
        state.properties.insert(key.into(), value);
        state.last_update_time = chrono::Local::now();
        Self::queue(&mut pending, state, now);
        drop(pending);
        self.wakeup.notify_one();
    }

    pub fn latest(&self, name: &str) -> Option<SavedWindowState> {
        self.pending
            .lock()
            .latest
            .get(name)
            .cloned()
    }

    pub fn has_due(&self, now: Instant) -> bool {
        self.pending
            .lock()
            .entries
            .values()
            .any(|entry| entry.due <= now)
    }

    pub fn next_delay(&self, now: Instant) -> Option<Duration> {
        let Some(pending) = self.pending.try_lock() else { return Some(Duration::from_millis(50)); };
        pending.entries.values().map(|entry| entry.due.saturating_duration_since(now)).min()
    }

    pub fn flush(&self, root: &Path, force: bool) -> Result<(), String> {
        self.flush_with(Instant::now(), force, |state| {
            crate::window_state::save(root, state)
        })
    }

    pub(crate) fn flush_with(
        &self,
        now: Instant,
        force: bool,
        mut save: impl FnMut(&SavedWindowState) -> Result<(), String>,
    ) -> Result<(), String> {
        let _writing = self.writing.lock();
        let entries: Vec<_> = self
            .pending
            .lock()
            .entries
            .values()
            .filter(|entry| force || entry.due <= now)
            .cloned()
            .collect();
        let mut errors = Vec::new();
        for entry in entries {
            let saved = save(&entry.state);
            let mut pending = self.pending.lock();
            // A newer event may have arrived during a slow disk write. Never remove it.
            if let Some(current) = pending
                .entries
                .get_mut(&entry.state.name)
                .filter(|current| current.revision == entry.revision)
            {
                if saved.is_ok() {
                    pending.entries.remove(&entry.state.name);
                } else {
                    current.due = now + SAVE_DELAY;
                }
            }
            if let Err(error) = saved {
                errors.push(error);
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            self.wakeup.notify_one();
            Err(errors.join("; "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::task::{Context, Waker};

    #[test]
    fn pending_save_notifies_and_retries_only_at_its_deadline() {
        let wakeup = Arc::new(tokio::sync::Notify::new());
        let writer = WindowStateWriter::with_notifier(wakeup.clone());
        let now = Instant::now();
        let mut context = Context::from_waker(Waker::noop());
        assert_eq!(writer.next_delay(now), None);
        writer.remember(SavedWindowState::new("fixture", 0.0, 0.0, 100.0, 100.0), now);
        assert!(std::pin::pin!(wakeup.notified()).as_mut().poll(&mut context).is_ready());
        assert_eq!(writer.next_delay(now), Some(SAVE_DELAY));
        let due = now + SAVE_DELAY;
        assert!(writer.flush_with(due, false, |_| Err("fixture failure".into())).is_err());
        assert_eq!(writer.next_delay(due), Some(SAVE_DELAY));
        writer.flush_with(due + SAVE_DELAY, false, |_| Ok(())).unwrap();
        assert_eq!(writer.next_delay(due + SAVE_DELAY), None);
        writer.remember_property("fixture", "IsPinned", "true".into(), now);
        assert_eq!(writer.next_delay(now), Some(SAVE_DELAY));
    }
}
