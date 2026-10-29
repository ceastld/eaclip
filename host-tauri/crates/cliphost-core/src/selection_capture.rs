//! Read the result of an explicit copy, never the unchanged previous clipboard.
use std::time::{Duration, Instant};

use crate::clipboard::sequence_number;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Captured<T> {
    pub value: T,
    pub sequence: Option<u32>,
}

pub(crate) fn verify_unchanged(expected: Option<u32>) -> Result<(), String> {
    if expected.is_some_and(|value| value == 0 || current_sequence() != Some(value)) {
        Err("选区复制后剪贴板已变化，已取消本次文本回贴。".into())
    } else { Ok(()) }
}

/// None is only used by headless calls without an explicit clipboard fixture.
pub(crate) fn current_sequence() -> Option<u32> {
    #[cfg(test)]
    if let Some(sequence) = TEST_SEQUENCE.with(|slot| slot.borrow().clone()) {
        return Some(sequence.load(std::sync::atomic::Ordering::SeqCst));
    }
    (!crate::clipboard::is_headless()).then(sequence_number)
}

#[cfg(test)]
thread_local! {
    static TEST_SEQUENCE: std::cell::RefCell<Option<std::sync::Arc<std::sync::atomic::AtomicU32>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) struct TestSequenceGuard;
#[cfg(test)]
impl Drop for TestSequenceGuard {
    fn drop(&mut self) { TEST_SEQUENCE.with(|slot| *slot.borrow_mut() = None); }
}
#[cfg(test)]
pub(crate) fn install_test_sequence(sequence: std::sync::Arc<std::sync::atomic::AtomicU32>) -> TestSequenceGuard {
    TEST_SEQUENCE.with(|slot| *slot.borrow_mut() = Some(sequence));
    TestSequenceGuard
}

pub(crate) fn wait_for_copy<T>(
    previous: u32,
    verify_target: impl FnMut() -> Result<(), String>,
    read: impl FnMut() -> Option<T>,
) -> Result<Option<Captured<T>>, String> {
    wait_for_fresh(
        previous,
        Duration::from_millis(500),
        verify_target,
        sequence_number,
        read,
        std::thread::sleep,
    )
}

fn wait_for_fresh<T>(
    previous: u32,
    timeout: Duration,
    mut verify_target: impl FnMut() -> Result<(), String>,
    mut sequence: impl FnMut() -> u32,
    mut read: impl FnMut() -> Option<T>,
    mut wait: impl FnMut(Duration),
) -> Result<Option<Captured<T>>, String> {
    let started = Instant::now();
    loop {
        verify_target()?;
        let before = sequence();
        // Sequence numbers are DWORDs and can wrap. Zero is an unavailable query.
        if before != 0 && before != previous {
            let snapshot = read();
            verify_target()?;
            // Delayed rendering or another writer may change the clipboard while
            // reading. Retry until one snapshot belongs to a stable sequence.
            if sequence() == before && snapshot.is_some() {
                return Ok(snapshot.map(|value| Captured { value, sequence: Some(before) }));
            }
        }
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Ok(None);
        }
        wait(Duration::from_millis(10).min(remaining));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn selection_version_rejects_new_copy_wrap_and_unavailable_sequence() {
        use std::sync::{Arc, atomic::{AtomicU32, Ordering}};
        let sequence = Arc::new(AtomicU32::new(u32::MAX));
        let _guard = install_test_sequence(sequence.clone());
        assert!(verify_unchanged(Some(u32::MAX)).is_ok());
        for current in [1, 0, 42] {
            sequence.store(current, Ordering::SeqCst);
            assert!(verify_unchanged(Some(u32::MAX)).is_err());
        }
        assert!(verify_unchanged(Some(0)).is_err());
        assert!(verify_unchanged(None).is_ok(), "actions without selected input do not depend on a clipboard version");
    }

    #[test]
    fn selection_capture_never_reads_unchanged_or_unavailable_clipboard() {
        for current in [0, 7] {
            let result = wait_for_fresh::<()>(
                7,
                Duration::ZERO,
                || Ok(()),
                || current,
                || panic!("old clipboard must not be read"),
                |_| panic!("zero timeout"),
            );
            assert!(result.unwrap().is_none());
        }
    }

    #[test]
    fn selection_capture_waits_for_delayed_unlocked_stable_snapshot_and_sequence_wrap() {
        let tick = Cell::new(0);
        let reads = Cell::new(0);
        let result = wait_for_fresh(
            u32::MAX,
            Duration::from_secs(1),
            || Ok(()),
            || {
                if tick.get() == 0 {
                    u32::MAX
                } else {
                    tick.get()
                }
            },
            || {
                reads.set(reads.get() + 1);
                match tick.get() {
                    1 => None, // Clipboard still locked by the copying app.
                    2 => {
                        tick.set(3);
                        Some("torn snapshot")
                    }
                    _ => Some("selected content"),
                }
            },
            |_| tick.set(tick.get() + 1),
        );
        assert_eq!(result.unwrap(), Some(Captured { value: "selected content", sequence: Some(4) }));
        assert_eq!(reads.get(), 3);
    }

    #[test]
    fn selection_capture_stops_if_focus_changes_before_or_during_read() {
        for before_read in [true, false] {
            let changed = Cell::new(before_read);
            let result = wait_for_fresh(
                7,
                Duration::from_secs(1),
                || {
                    if changed.get() {
                        Err("target changed".into())
                    } else {
                        Ok(())
                    }
                },
                || 8,
                || {
                    changed.set(true);
                    Some("untrusted selection")
                },
                |_| panic!("focus loss must not be retried"),
            );
            assert_eq!(result.unwrap_err(), "target changed");
        }
    }
}
