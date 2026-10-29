//! Lifetime of one favorite feedback message, paused while the user engages it.
use std::time::{Duration, Instant};

use crate::motion;

pub(super) struct FeedbackClock {
    remaining: Duration,
    started: Instant,
    paused: bool,
    expired: bool,
}

impl FeedbackClock {
    pub(super) fn new(duration: Duration, now: Instant) -> Self {
        Self {
            remaining: duration,
            started: now,
            paused: false,
            expired: false,
        }
    }

    /// Returns true once this message has expired. Engagement is a state, not a
    /// recurring event: repeated observations cannot spend paused time or reset
    /// the running deadline. A retired clock cannot revive a replacement notice.
    pub(super) fn observe(&mut self, now: Instant, engaged: bool) -> bool {
        if self.expired {
            return true;
        }
        if engaged {
            if !self.paused {
                self.remaining = self
                    .remaining
                    .saturating_sub(now.saturating_duration_since(self.started));
                self.paused = true;
            }
            return false;
        }
        if self.paused {
            self.remaining = self.remaining.max(motion::FAVORITE_FEEDBACK_RESUME);
            self.started = now;
            self.paused = false;
        }
        self.expired = now.saturating_duration_since(self.started) >= self.remaining;
        self.expired
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn after(start: Instant, milliseconds: u64) -> Instant {
        start + Duration::from_millis(milliseconds)
    }

    #[test]
    fn feedback_clock_save_and_undo_expire_at_their_exact_boundaries() {
        let start = Instant::now();
        for (duration, boundary) in [
            (motion::FAVORITE_FEEDBACK_SAVE, 2500),
            (motion::FAVORITE_FEEDBACK_UNDO, 1500),
        ] {
            let mut clock = FeedbackClock::new(duration, start);
            assert!(!clock.observe(start, false));
            assert!(!clock.observe(after(start, boundary - 1), false));
            assert!(clock.observe(after(start, boundary), false));
            assert!(clock.observe(after(start, boundary + 1), true));
        }
    }

    #[test]
    fn feedback_clock_pause_preserves_unspent_time_and_resumes_from_release() {
        let start = Instant::now();
        let mut clock = FeedbackClock::new(motion::FAVORITE_FEEDBACK_SAVE, start);
        assert!(!clock.observe(after(start, 600), true));
        assert!(!clock.observe(after(start, 30_000), true));
        assert!(!clock.observe(after(start, 40_000), false));
        assert!(!clock.observe(after(start, 41_899), false));
        assert!(clock.observe(after(start, 41_900), false));
    }

    #[test]
    fn feedback_clock_release_grants_one_second_when_little_time_remains() {
        let start = Instant::now();
        let mut clock = FeedbackClock::new(motion::FAVORITE_FEEDBACK_UNDO, start);
        assert!(!clock.observe(after(start, 1499), true));
        assert!(!clock.observe(after(start, 10_000), false));
        assert!(!clock.observe(after(start, 10_999), false));
        assert!(clock.observe(after(start, 11_000), false));
    }

    #[test]
    fn feedback_clock_repeated_observations_do_not_recharge_or_spend_paused_time() {
        let start = Instant::now();
        let mut clock = FeedbackClock::new(motion::FAVORITE_FEEDBACK_SAVE, start);
        for now in [100, 400, 900] {
            assert!(!clock.observe(after(start, now), false));
        }
        assert!(!clock.observe(after(start, 1000), true));
        for now in [1000, 1500, 20_000] {
            assert!(!clock.observe(after(start, now), true));
        }
        assert!(!clock.observe(after(start, 25_000), false));
        for now in [25_000, 25_500, 26_499] {
            assert!(!clock.observe(after(start, now), false));
        }
        assert!(clock.observe(after(start, 26_500), false));
    }

    #[test]
    fn feedback_clock_replacement_has_its_own_budget_and_engagement() {
        let start = Instant::now();
        let mut old = FeedbackClock::new(motion::FAVORITE_FEEDBACK_SAVE, start);
        assert!(!old.observe(after(start, 2400), true));
        let mut replacement =
            FeedbackClock::new(motion::FAVORITE_FEEDBACK_UNDO, after(start, 3000));
        assert!(!old.observe(after(start, 4000), true));
        assert!(!replacement.observe(after(start, 4499), false));
        assert!(replacement.observe(after(start, 4500), false));
        assert!(!old.observe(after(start, 10_000), true));
        assert!(!old.observe(after(start, 11_000), false));
        assert!(!old.observe(after(start, 11_999), false));
        assert!(old.observe(after(start, 12_000), false));
    }
}
