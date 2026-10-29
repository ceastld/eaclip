//! Monotonic update-check deadlines. Missed periods coalesce to one check.

use std::time::{Duration, Instant};

pub(super) const CHECK_INTERVAL: Duration = Duration::from_secs(2 * 60 * 60);
const READINESS_POLL: Duration = Duration::from_millis(250);

struct CheckSchedule {
    interval: Duration,
    next: Option<Instant>,
}

impl CheckSchedule {
    fn new(interval: Duration) -> Self {
        assert!(!interval.is_zero());
        Self {
            interval,
            next: None,
        }
    }

    fn claim_due(&mut self, now: Instant, ready: bool) -> bool {
        if !ready || self.next.is_some_and(|next| now < next) {
            return false;
        }
        self.next = Some(now + self.interval);
        true
    }

    fn delay(&self, now: Instant) -> Duration {
        self.next
            .map(|next| next.saturating_duration_since(now))
            .filter(|delay| !delay.is_zero())
            .unwrap_or(READINESS_POLL)
    }
}

/// Both callbacks run briefly on the host executor. The check callback dispatches
/// bounded network work to a worker; aborting this task stops future dispatches.
pub(super) async fn run(
    mut ready: impl FnMut() -> bool,
    mut check: impl FnMut(),
    interval: Duration,
) {
    let mut schedule = CheckSchedule::new(interval);
    loop {
        if schedule.claim_due(Instant::now(), ready()) {
            check();
        }
        tokio::time::sleep(schedule.delay(Instant::now())).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    };

    #[test]
    fn startup_waits_for_readiness_then_checks_every_two_hours() {
        let start = Instant::now();
        let mut schedule = CheckSchedule::new(CHECK_INTERVAL);
        assert!(!schedule.claim_due(start, false));
        let ready = start + Duration::from_secs(30);
        assert!(schedule.claim_due(ready, true));
        assert!(!schedule.claim_due(ready, true));
        assert!(!schedule.claim_due(ready + Duration::from_secs(7199), true));
        assert!(schedule.claim_due(ready + Duration::from_secs(7200), true));
    }

    #[test]
    fn initialization_does_not_consume_startup_and_resume_does_not_burst() {
        let start = Instant::now();
        let mut schedule = CheckSchedule::new(CHECK_INTERVAL);
        assert!(!schedule.claim_due(start, false));
        assert!(!schedule.claim_due(start + CHECK_INTERVAL, false));
        let initialized = start + CHECK_INTERVAL + Duration::from_secs(1);
        assert!(schedule.claim_due(initialized, true));
        let resumed = initialized + CHECK_INTERVAL * 4;
        assert!(schedule.claim_due(resumed, true));
        for _ in 0..5 {
            assert!(!schedule.claim_due(resumed, true));
        }
        assert_eq!(schedule.delay(resumed), CHECK_INTERVAL);
    }

    #[test]
    fn executor_waits_until_ready_and_stops_dispatching_when_aborted() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            let ready = Arc::new(AtomicBool::new(false));
            let count = Arc::new(AtomicUsize::new(0));
            let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
            let ready_for_task = ready.clone();
            let count_for_task = count.clone();
            let task = tokio::spawn(run(
                move || ready_for_task.load(Ordering::Acquire),
                move || {
                    let _ = sent.send(count_for_task.fetch_add(1, Ordering::AcqRel));
                },
                Duration::from_millis(20),
            ));
            tokio::task::yield_now().await;
            assert_eq!(count.load(Ordering::Acquire), 0);
            ready.store(true, Ordering::Release);
            for expected in [0, 1] {
                let event = tokio::time::timeout(Duration::from_secs(5), received.recv())
                    .await
                    .unwrap();
                assert_eq!(event, Some(expected));
            }
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            let stopped_count = count.load(Ordering::Acquire);
            tokio::time::sleep(Duration::from_millis(60)).await;
            assert_eq!(count.load(Ordering::Acquire), stopped_count);
        });
    }
}
