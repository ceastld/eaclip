use super::*;
use std::sync::atomic::AtomicUsize;

#[test]
fn revocation_is_fifo_and_not_available_until_last_lease_drops() {
    let budget = ContentBudget::new(100);
    let first = budget.try_acquire(60).unwrap();
    let first_alias = first.clone();
    let second = budget.try_acquire(40).unwrap();
    assert!(budget.try_acquire(30).is_none());
    assert!(first.revoked());
    assert!(!second.revoked());
    assert_eq!(budget.snapshot().retained_bytes, 100);
    assert_eq!(budget.snapshot().pending_eviction_bytes, 60);
    drop(first);
    assert_eq!(budget.snapshot().retained_bytes, 100);
    assert!(budget.try_acquire(30).is_none());
    drop(first_alias);
    assert_eq!(budget.snapshot().retained_bytes, 40);
    let replacement = budget.try_acquire(30).unwrap();
    assert!(!replacement.revoked());
    assert!(!budget.pressure_pending());
    drop((second, replacement));
    assert_eq!(budget.snapshot().retained_bytes, 0);
    assert_eq!(budget.snapshot().entries, 0);
}

#[test]
fn oversized_result_does_not_revoke_unrelated_owners() {
    let budget = ContentBudget::new(10);
    let entry = budget.try_acquire(8).unwrap();
    assert!(budget.try_acquire(11).is_none());
    assert!(!entry.revoked());
    assert_eq!(budget.snapshot().pending_evictions, 0);
}

#[test]
fn pressure_callback_is_outside_lock_coalesced_and_rearmed_on_acknowledgement() {
    let budget = ContentBudget::new(100);
    let calls = Arc::new(AtomicUsize::new(0));
    let callback_budget = budget.clone();
    let callback_calls = calls.clone();
    budget.set_pressure_notifier(Arc::new(move || {
        // This reentrant read would deadlock if notification held the mutex.
        assert!(callback_budget.snapshot().pending_evictions > 0);
        callback_calls.fetch_add(1, Ordering::SeqCst);
    }));
    let entries: Vec<_> = (0..4).map(|_| budget.try_acquire(25).unwrap()).collect();
    assert!(budget.try_acquire(10).is_none());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(budget.snapshot().pending_evictions, 1);
    for _ in 0..20 {
        assert!(budget.try_acquire(10).is_none());
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(budget.try_acquire(40).is_none());
    assert_eq!(budget.snapshot().pending_evictions, 2);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    budget.acknowledge_pressure();
    assert!(budget.try_acquire(60).is_none());
    assert_eq!(budget.snapshot().pending_evictions, 3);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    drop(entries);
    assert_eq!(budget.snapshot().retained_bytes, 0);
    // This reentrancy fixture deliberately captured its own budget. Remove that
    // test callback so it does not create an unrelated Arc cycle.
    budget.set_pressure_notifier(Arc::new(|| {}));
}

#[test]
fn registering_foreground_handoff_replays_early_pressure_once() {
    let budget = ContentBudget::new(10);
    let entry = budget.try_acquire(10).unwrap();
    assert!(budget.try_acquire(1).is_none());
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    budget.set_pressure_notifier(Arc::new(move || {
        counter.fetch_add(1, Ordering::SeqCst);
    }));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    budget.acknowledge_pressure();
    assert!(budget.try_acquire(1).is_none());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    drop(entry);
    assert!(budget.try_acquire(10).is_some());
}

#[test]
fn racing_admissions_never_exceed_actual_held_capacity() {
    let budget = ContentBudget::new(90);
    let barrier = Arc::new(std::sync::Barrier::new(17));
    std::thread::scope(|scope| {
        for _ in 0..16 {
            let budget = budget.clone();
            let barrier = barrier.clone();
            scope.spawn(move || {
                let lease = budget.try_acquire(30);
                barrier.wait();
                assert!(budget.snapshot().retained_bytes <= 90);
                barrier.wait();
                drop(lease);
            });
        }
        barrier.wait();
        assert_eq!(budget.snapshot().retained_bytes, 90);
        assert_eq!(budget.snapshot().entries, 3);
        barrier.wait();
    });
    assert_eq!(budget.snapshot().retained_bytes, 0);
    assert!(budget.try_acquire(90).is_some());
}

#[test]
fn default_handles_share_one_process_ledger_without_mutating_it() {
    let a = ContentBudget::default();
    let b = ContentBudget::default();
    assert!(Arc::ptr_eq(&a.ledger, &b.ledger));
    assert_eq!(a.snapshot().limit_bytes, SHARED_CONTENT_RETAINED_BYTES);
}

#[test]
fn dropped_cache_handoff_keeps_budget_charged_until_foreground_batch_is_finished() {
    let budget = ContentBudget::new(100);
    let lease = budget.try_acquire(100).unwrap();
    let received = Arc::new(Mutex::new(Vec::new()));
    let destination = received.clone();
    let schedule: RetirementScheduler = Arc::new(move |batch| {
        destination.lock().unwrap().push(batch);
    });
    dispatch_retirement(RetirementBatch { images: Vec::new(), leases: vec![lease] }, Some(schedule));
    assert_eq!(budget.snapshot().retained_bytes, 100);
    assert!(budget.try_acquire(1).is_none());
    let batch = received.lock().unwrap().pop().unwrap();
    assert_eq!(budget.snapshot().retained_bytes, 100);
    drop(batch); // The real App performs this after its CPU/atlas defer guards.
    assert_eq!(budget.snapshot().retained_bytes, 0);
    assert!(budget.try_acquire(100).is_some());
}

#[test]
fn unavailable_app_or_closed_handoff_cannot_leak_a_reservation() {
    let budget = ContentBudget::new(100);
    let lease = budget.try_acquire(100).unwrap();
    dispatch_retirement(RetirementBatch { images: Vec::new(), leases: vec![lease] }, None);
    assert_eq!(budget.snapshot().retained_bytes, 0);
    let (sender, receiver) = std::sync::mpsc::channel::<RetirementBatch>();
    drop(receiver);
    let schedule: RetirementScheduler = Arc::new(move |batch| { let _ = sender.send(batch); });
    let lease = budget.try_acquire(100).unwrap();
    dispatch_retirement(RetirementBatch { images: Vec::new(), leases: vec![lease] }, Some(schedule));
    assert_eq!(budget.snapshot().retained_bytes, 0);
    assert_eq!(budget.snapshot().entries, 0);
}
