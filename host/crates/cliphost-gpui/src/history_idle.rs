//! One automatic return per interval without history-list interaction.

use std::time::{Duration, Instant};

/// Visibility gates the action without stopping idle time. Busy interactions
/// instead grant a fresh interval when they finish, so closing a dialog or
/// committing composed text cannot immediately move the reader's list.
pub(crate) struct HistoryIdle {
    enabled: bool,
    timeout: Duration,
    last_activity: Option<Instant>,
    fired: bool,
    was_busy: bool,
}

impl Default for HistoryIdle {
    fn default() -> Self {
        Self {
            enabled: false,
            timeout: Duration::from_secs(60),
            last_activity: None,
            fired: false,
            was_busy: false,
        }
    }
}

impl HistoryIdle {
    /// Safe to call on every refresh: only a changed setting restarts timing.
    /// Enabling the feature never inherits idle time from before it was enabled.
    pub(crate) fn configure(&mut self, enabled: bool, seconds: u64, now: Instant) {
        let timeout = Duration::from_secs(seconds);
        if self.enabled == enabled && self.timeout == timeout {
            return;
        }
        self.enabled = enabled;
        self.timeout = timeout;
        self.activity(now);
    }

    /// Called for actual list interaction, independently of clipboard refreshes.
    pub(crate) fn activity(&mut self, now: Instant) {
        self.last_activity = Some(now);
        self.fired = false;
    }

    /// `presented` means the expanded history list is currently shown; hidden,
    /// folded, edge-hidden, and favorites views all pass false. `busy` means a
    /// dialog, edit, composition, or another protected interaction is active.
    ///
    /// True consumes the interval even if the caller is already at the top.
    /// Only new activity or changed settings can rearm a consumed interval.
    pub(crate) fn poll(&mut self, now: Instant, presented: bool, busy: bool) -> bool {
        if busy {
            self.was_busy = true;
            return false;
        }
        if self.was_busy {
            self.was_busy = false;
            self.activity(now);
            return false;
        }
        if !self.enabled || self.fired || !presented {
            return false;
        }
        let last_activity = self.last_activity.get_or_insert(now);
        if now.saturating_duration_since(*last_activity) < self.timeout {
            return false;
        }
        self.fired = true;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled(now: Instant) -> HistoryIdle {
        let mut idle = HistoryIdle::default();
        idle.configure(true, 60, now);
        idle
    }

    #[test]
    fn disabled_by_default_and_after_being_disabled() {
        let now = Instant::now();
        let mut idle = HistoryIdle::default();
        assert!(!idle.poll(now + Duration::from_secs(3_600), true, false));
        idle.configure(true, 60, now);
        idle.configure(false, 60, now + Duration::from_secs(10));
        assert!(!idle.poll(now + Duration::from_secs(3_600), true, false));
    }

    #[test]
    fn exact_deadline_fires_only_once() {
        let now = Instant::now();
        let mut idle = enabled(now);
        assert!(!idle.poll(now + Duration::from_millis(59_999), true, false));
        assert!(idle.poll(now + Duration::from_secs(60), true, false));
        assert!(!idle.poll(now + Duration::from_secs(60), true, false));
        assert!(!idle.poll(now + Duration::from_secs(3_600), true, false));
    }

    #[test]
    fn hidden_or_folded_time_counts_until_history_is_presented() {
        let now = Instant::now();
        let mut idle = enabled(now);
        assert!(!idle.poll(now + Duration::from_secs(30), false, false));
        assert!(!idle.poll(now + Duration::from_secs(90), false, false));
        assert!(idle.poll(now + Duration::from_secs(91), true, false));
        assert!(!idle.poll(now + Duration::from_secs(92), true, false));
    }

    #[test]
    fn busy_interaction_grants_a_full_interval_after_finishing() {
        let now = Instant::now();
        let mut idle = enabled(now);
        assert!(!idle.poll(now + Duration::from_secs(55), true, true));
        assert!(!idle.poll(now + Duration::from_secs(100), true, true));
        assert!(!idle.poll(now + Duration::from_secs(110), true, false));
        assert!(!idle.poll(now + Duration::from_secs(169), true, false));
        assert!(idle.poll(now + Duration::from_secs(170), true, false));
    }

    #[test]
    fn busy_finishes_while_hidden_without_restarting_every_hidden_poll() {
        let now = Instant::now();
        let mut idle = enabled(now);
        assert!(!idle.poll(now + Duration::from_secs(70), false, true));
        assert!(!idle.poll(now + Duration::from_secs(80), false, false));
        assert!(!idle.poll(now + Duration::from_secs(100), false, false));
        assert!(!idle.poll(now + Duration::from_secs(139), true, false));
        assert!(idle.poll(now + Duration::from_secs(140), true, false));
    }

    #[test]
    fn new_activity_rearms_a_consumed_interval() {
        let now = Instant::now();
        let mut idle = enabled(now);
        assert!(idle.poll(now + Duration::from_secs(60), true, false));
        idle.activity(now + Duration::from_secs(100));
        assert!(!idle.poll(now + Duration::from_secs(159), true, false));
        assert!(idle.poll(now + Duration::from_secs(160), true, false));
    }

    #[test]
    fn changed_settings_restart_timing_but_same_settings_preserve_it() {
        let now = Instant::now();
        let mut idle = enabled(now);
        idle.configure(true, 60, now + Duration::from_secs(40));
        assert!(idle.poll(now + Duration::from_secs(60), true, false));
        idle.configure(true, 90, now + Duration::from_secs(70));
        assert!(!idle.poll(now + Duration::from_secs(159), true, false));
        assert!(idle.poll(now + Duration::from_secs(160), true, false));
    }

    #[test]
    fn enabling_starts_now_instead_of_inheriting_previous_idle_time() {
        let now = Instant::now();
        let mut idle = HistoryIdle::default();
        idle.activity(now);
        idle.configure(true, 60, now + Duration::from_secs(600));
        assert!(!idle.poll(now + Duration::from_secs(659), true, false));
        assert!(idle.poll(now + Duration::from_secs(660), true, false));
    }
}
