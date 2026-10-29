//! One-way editor picker leases. Retiring invalidates a worker's guard without
//! freeing the pending modal slot. Only that modal's exact completion can free
//! the slot; generation, target and HWND validation belongs to the editor after
//! `complete`, so a stale result still releases its own pending operation.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

/// Clone into a background guard. The private flag cannot be reactivated by a
/// caller; a new operation always receives a new allocation and identity.
#[derive(Clone, Debug)]
pub(super) struct EditorPickerLease {
    active: Arc<AtomicBool>,
}

impl EditorPickerLease {
    fn new() -> Self {
        Self {
            active: Arc::new(AtomicBool::new(true)),
        }
    }

    pub(super) fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    pub(super) fn retire(&self) {
        self.active.store(false, Ordering::Release);
    }

    fn same_operation(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.active, &other.active)
    }
}

#[derive(Debug, Default)]
pub(super) struct EditorPickerState {
    pending: Option<EditorPickerLease>,
}

impl EditorPickerState {
    /// Begin synchronously at the accepted click, after capturing its real HWND.
    /// Retired-but-unfinished operations remain busy until their modal returns.
    pub(super) fn begin(&mut self) -> Option<EditorPickerLease> {
        if self.pending.is_some() {
            return None;
        }
        let lease = EditorPickerLease::new();
        self.pending = Some(lease.clone());
        Some(lease)
    }

    pub(super) fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Loading, closing and destruction invalidate the guard only. In
    /// particular, never emulate retirement by resetting a separate busy flag.
    pub(super) fn retire(&self) {
        if let Some(lease) = &self.pending {
            lease.retire();
        }
    }

    /// Call only when the modal worker actually ended. `None` is an unrelated
    /// or duplicate callback and changes nothing. `Some(false)` frees a retired
    /// operation; `Some(true)` frees an operation whose result may still apply.
    ///
    /// Pending is released before the caller checks generation/target/owner.
    /// The completed token is also retired: use this returned prior-active bit,
    /// not `lease.is_active()`, for the subsequent result validation.
    pub(super) fn complete(&mut self, lease: &EditorPickerLease) -> Option<bool> {
        if !self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.same_operation(lease))
        {
            return None;
        }
        let completed = self.pending.take().expect("matching pending picker");
        Some(completed.active.swap(false, Ordering::AcqRel))
    }
}

impl Drop for EditorPickerState {
    fn drop(&mut self) {
        self.retire();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_active_picker_keeps_the_slot_and_rejects_another_begin() {
        let mut state = EditorPickerState::default();
        assert!(!state.is_pending());
        let lease = state.begin().unwrap();
        assert!(lease.is_active());
        assert!(state.is_pending());
        assert!(state.begin().is_none());
    }

    #[test]
    fn cloned_guards_share_one_way_retirement_without_freeing_pending() {
        let mut state = EditorPickerState::default();
        let lease = state.begin().unwrap();
        let worker = lease.clone();
        worker.retire();
        assert!(!lease.is_active());
        assert!(!worker.is_active());
        state.retire();
        state.retire();
        assert!(state.is_pending());
        assert!(state.begin().is_none());
        assert_eq!(state.complete(&lease), Some(false));
        assert!(!state.is_pending());
    }

    #[test]
    fn matching_clone_completion_releases_then_retires_the_active_token() {
        let mut state = EditorPickerState::default();
        let lease = state.begin().unwrap();
        assert_eq!(state.complete(&lease.clone()), Some(true));
        assert!(!state.is_pending());
        assert!(!lease.is_active());
        assert_eq!(state.complete(&lease), None);
    }

    #[test]
    fn old_completion_cannot_release_or_retire_a_new_operation() {
        let mut state = EditorPickerState::default();
        let old = state.begin().unwrap();
        state.retire();
        assert_eq!(state.complete(&old), Some(false));
        let current = state.begin().unwrap();
        assert_eq!(state.complete(&old), None);
        assert!(state.is_pending());
        assert!(current.is_active());
        assert!(!old.is_active());
        assert_eq!(state.complete(&current), Some(true));
    }

    #[test]
    fn equal_active_values_from_other_states_do_not_match_identity() {
        let mut first = EditorPickerState::default();
        let own = first.begin().unwrap();
        let mut second = EditorPickerState::default();
        let unrelated = second.begin().unwrap();
        assert!(own.is_active() && unrelated.is_active());
        assert_eq!(first.complete(&unrelated), None);
        assert!(first.is_pending() && second.is_pending());
        assert!(own.is_active() && unrelated.is_active());
        assert_eq!(first.complete(&own), Some(true));
        assert_eq!(second.complete(&unrelated), Some(true));
    }

    #[test]
    fn stale_generation_callback_releases_its_pending_slot_before_rejection() {
        let mut state = EditorPickerState::default();
        let captured_generation = 1;
        let lease = state.begin().unwrap();
        state.retire();
        let current_generation = 2;
        let was_active = state.complete(&lease).unwrap();
        let may_apply = was_active && captured_generation == current_generation;
        assert!(!may_apply);
        assert!(!state.is_pending());
        assert!(state.begin().unwrap().is_active());
        assert!(!lease.is_active());
    }

    #[test]
    fn background_guard_observes_retirement_while_modal_slot_stays_busy() {
        let mut state = EditorPickerState::default();
        let lease = state.begin().unwrap();
        let worker_lease = lease.clone();
        let (ready_sender, ready_receiver) = std::sync::mpsc::channel();
        let (finish_sender, finish_receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            assert!(worker_lease.is_active());
            ready_sender.send(()).unwrap();
            finish_receiver.recv().unwrap();
            worker_lease.is_active()
        });
        ready_receiver.recv().unwrap();
        state.retire();
        assert!(state.is_pending());
        assert!(state.begin().is_none());
        finish_sender.send(()).unwrap();
        assert!(!worker.join().unwrap());
        assert!(state.is_pending());
        assert_eq!(state.complete(&lease), Some(false));
        assert!(!state.is_pending());
    }

    #[test]
    fn dropping_the_state_retires_every_worker_clone() {
        let worker = {
            let mut state = EditorPickerState::default();
            let lease = state.begin().unwrap();
            assert!(lease.is_active());
            lease
        };
        assert!(!worker.is_active());
        worker.retire();
        assert!(!worker.is_active());
    }
}
