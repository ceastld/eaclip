use super::*;

fn duration_from_nanos(nanos: u128) -> Duration {
    Duration::new((nanos / 1_000_000_000) as u64, (nanos % 1_000_000_000) as u32)
}

fn earliest_representable_instant() -> Instant {
    let now = Instant::now();
    let mut low = 0;
    let mut high = Duration::MAX.as_nanos();
    while low < high {
        let middle = low + (high - low).div_ceil(2);
        if now.checked_sub(duration_from_nanos(middle)).is_some() {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    now.checked_sub(duration_from_nanos(low)).unwrap()
}

#[test]
fn clipboard_suppression_initialization_is_safe_near_clock_origin() {
    let origin = earliest_representable_instant();
    for seconds in [5, 31] {
        let now = origin.checked_add(Duration::from_secs(seconds)).unwrap();
        assert_eq!(now.checked_sub(Duration::from_secs(30)).is_none(), seconds < 30,
            "the synthetic startup instant must cover the former underflow boundary");
        let slot = unsuppressed_slot(now);
        let deadline = *slot.lock();
        assert_eq!(deadline, now);
        assert!(!(now < deadline), "the initial slot must allow capture immediately");
        assert!(!(now.checked_add(Duration::from_nanos(1)).unwrap() < deadline));
    }
}

#[test]
fn clipboard_suppression_explicit_deadline_keeps_strict_expiry() {
    let now = earliest_representable_instant().checked_add(Duration::from_secs(5)).unwrap();
    let slot = unsuppressed_slot(now);
    let deadline = now.checked_add(HANDLE_SUPPRESS_DURATION).unwrap();
    *slot.lock() = deadline;
    assert!(now < *slot.lock(), "explicit suppression must still take effect");
    assert!(deadline.checked_sub(Duration::from_nanos(1)).unwrap() < *slot.lock());
    assert!(!(deadline < *slot.lock()), "equality must allow capture again");
}
