//! One outstanding frame per window, including while its HWND is hidden.
//! The VSync thread blocks on the receiver when there is no pending work.
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc::Sender,
};

pub(crate) type FrameSender = Sender<Option<Arc<FrameRequest>>>;

pub(crate) struct FrameRequest {
    pub(crate) hwnd: usize,
    pub(crate) id: usize,
    sender: FrameSender,
    pending: AtomicBool,
    alive: AtomicBool,
}

impl FrameRequest {
    pub(crate) fn new(hwnd: usize, sender: FrameSender) -> Arc<Self> {
        static NEXT_ID: AtomicUsize = AtomicUsize::new(1);
        Arc::new(Self {
            hwnd,
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            sender,
            pending: AtomicBool::new(false),
            alive: AtomicBool::new(true),
        })
    }

    pub(crate) fn request(self: &Arc<Self>) {
        if self.is_alive() && !self.pending.swap(true, Ordering::AcqRel) {
            if self.sender.send(Some(self.clone())).is_err() {
                self.complete();
            }
        }
    }

    pub(crate) fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    // Acknowledge before running callbacks: callbacks may request another frame.
    pub(crate) fn complete(&self) {
        self.pending.store(false, Ordering::Release);
    }

    pub(crate) fn cancel(&self) {
        self.alive.store(false, Ordering::Release);
        self.complete();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{RecvTimeoutError, channel};
    use std::time::Duration;

    #[test]
    fn idle_has_no_work_and_requests_coalesce_until_ui_acknowledges() {
        let (tx, rx) = channel();
        let window = FrameRequest::new(42, tx);
        assert!(matches!(
            rx.recv_timeout(Duration::from_millis(20)),
            Err(RecvTimeoutError::Timeout)
        ));
        for _ in 0..100 {
            window.request();
        }
        let frame = rx.try_recv().unwrap().unwrap();
        assert_eq!(frame.hwnd, 42);
        window.request(); // Delivery to the UI is still pending.
        assert!(rx.try_recv().is_err());
        frame.complete();
        window.request(); // Animation/resource retirement scheduled inside a frame.
        assert!(rx.try_recv().unwrap().is_some());
    }

    #[test]
    fn cancelled_windows_do_not_wake_and_reused_handles_have_new_ids() {
        let (tx, rx) = channel();
        let old = FrameRequest::new(42, tx.clone());
        old.request();
        old.cancel();
        assert!(!rx.try_recv().unwrap().unwrap().is_alive());
        old.request();
        assert!(rx.try_recv().is_err());
        let new = FrameRequest::new(42, tx);
        assert_ne!(old.id, new.id);
        new.request();
        assert_eq!(rx.try_recv().unwrap().unwrap().id, new.id);
    }
}
