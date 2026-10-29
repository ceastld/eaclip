//! Pure per-card lifetime rules for the native copy notification.
//! Each timer keeps its own remaining time; the surface supplies region/card
//! hover, menu and action pause state without replacing one timer with another.

use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct CardTimer {
    deadline: Option<Instant>,
    remaining: Option<Duration>,
    paused: bool,
    exit_at: Option<Instant>,
}

impl CardTimer {
    /// A zero/non-positive frontend duration represents a permanent card.
    pub fn new(duration: Option<Duration>, now: Instant) -> Self {
        Self {
            deadline: duration.map(|duration| now + duration),
            remaining: duration,
            paused: false,
            exit_at: None,
        }
    }

    pub fn pause(&mut self, paused: bool, now: Instant) {
        if self.paused == paused || self.exit_at.is_some() {
            return;
        }
        if paused {
            self.remaining = self
                .deadline
                .map(|deadline| deadline.saturating_duration_since(now));
            self.deadline = None;
        } else {
            self.deadline = self.remaining.map(|remaining| now + remaining);
        }
        self.paused = paused;
    }

    pub fn expired(&self, now: Instant) -> bool {
        !self.paused
            && self.exit_at.is_none()
            && self.deadline.is_some_and(|deadline| deadline <= now)
    }

    pub fn remaining(&self, now: Instant) -> Option<Duration> {
        if let Some(exit_at) = self.exit_at {
            Some(exit_at.saturating_duration_since(now))
        } else if self.paused {
            self.remaining
        } else {
            self.deadline
                .map(|deadline| deadline.saturating_duration_since(now))
        }
    }

    pub fn paused(&self) -> bool {
        self.paused
    }

    pub fn exiting(&self) -> bool {
        self.exit_at.is_some()
    }

    /// A failed action needs enough time to read the inline error and retry.
    /// Permanent cards stay permanent; configured successful timers are intact.
    pub fn keep_for(&mut self, minimum: Duration, now: Instant) {
        if self.exit_at.is_some() {
            return;
        }
        if self
            .remaining(now)
            .is_some_and(|remaining| remaining < minimum)
        {
            self.remaining = Some(minimum);
            if !self.paused {
                self.deadline = Some(now + minimum);
            }
        }
    }

    pub fn begin_exit(&mut self, now: Instant, duration: Duration) {
        if self.exit_at.is_none() {
            self.exit_at = Some(now + duration);
            self.deadline = None;
            self.remaining = None;
            self.paused = false;
        }
    }

    pub fn finished(&self, now: Instant) -> bool {
        self.exit_at.is_some_and(|deadline| deadline <= now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_pause_of_one_card_does_not_pause_the_others() {
        let now = Instant::now();
        let mut hovered = CardTimer::new(Some(Duration::from_secs(2)), now);
        let other = CardTimer::new(Some(Duration::from_secs(2)), now);
        hovered.pause(true, now + Duration::from_secs(1));
        assert!(!hovered.expired(now + Duration::from_secs(4)));
        assert!(other.expired(now + Duration::from_secs(4)));
        hovered.pause(false, now + Duration::from_secs(4));
        assert!(!hovered.expired(now + Duration::from_millis(4_999)));
        assert!(hovered.expired(now + Duration::from_secs(5)));
    }

    #[test]
    fn new_card_during_hover_keeps_its_full_duration() {
        let now = Instant::now();
        let mut old = CardTimer::new(Some(Duration::from_secs(2)), now);
        old.pause(true, now + Duration::from_secs(1));
        let mut fresh = CardTimer::new(Some(Duration::from_secs(2)), now + Duration::from_secs(3));
        fresh.pause(true, now + Duration::from_secs(3));
        assert!(!fresh.expired(now + Duration::from_secs(5)));
        assert!(!old.expired(now + Duration::from_secs(5)));
        fresh.pause(false, now + Duration::from_secs(5));
        old.pause(false, now + Duration::from_secs(5));
        assert!(old.expired(now + Duration::from_secs(6)));
        assert!(!fresh.expired(now + Duration::from_secs(6)));
        assert!(fresh.expired(now + Duration::from_secs(7)));
    }

    #[test]
    fn permanent_card_only_leaves_when_dismissed() {
        let now = Instant::now();
        let mut permanent = CardTimer::new(None, now);
        assert!(!permanent.expired(now + Duration::from_secs(20)));
        permanent.pause(true, now);
        permanent.pause(false, now + Duration::from_secs(1));
        assert_eq!(permanent.remaining(now), None);
        permanent.begin_exit(now, Duration::from_millis(380));
        assert!(!permanent.finished(now + Duration::from_millis(379)));
        assert!(permanent.finished(now + Duration::from_millis(380)));
    }

    #[test]
    fn menu_and_busy_share_one_pause_without_adding_time_twice() {
        let now = Instant::now();
        let mut timer = CardTimer::new(Some(Duration::from_secs(3)), now);
        timer.pause(true, now + Duration::from_secs(1));
        timer.pause(true, now + Duration::from_secs(2));
        timer.pause(false, now + Duration::from_secs(4));
        assert_eq!(
            timer.remaining(now + Duration::from_secs(4)),
            Some(Duration::from_secs(2))
        );
        assert!(timer.expired(now + Duration::from_secs(6)));
    }

    #[test]
    fn failed_action_keeps_a_short_lived_error_readable_after_resume() {
        let now = Instant::now();
        let mut timer = CardTimer::new(Some(Duration::from_secs(1)), now);
        timer.pause(true, now + Duration::from_millis(950));
        timer.keep_for(Duration::from_secs(3), now + Duration::from_secs(2));
        timer.pause(false, now + Duration::from_secs(2));
        assert!(!timer.expired(now + Duration::from_secs(4)));
        assert!(timer.expired(now + Duration::from_secs(5)));
        let mut permanent = CardTimer::new(None, now);
        permanent.keep_for(Duration::from_secs(3), now);
        assert_eq!(permanent.remaining(now), None);
    }
}
