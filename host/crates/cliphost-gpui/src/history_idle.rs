//! Automatic return of the history list to its first row.
//!
//! The reset only happens where the reader cannot lose their place: after the
//! window was really hidden (reopening starts fresh), or after an idle interval
//! while the list is out of sight or nobody is using the visible window.

use std::time::{Duration, Instant};

/// A hide shorter than this keeps the position, so a mistyped hotkey toggle or
/// a transient re-show does not move the list.
const HIDE_GRACE: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HistoryPresence {
    /// The expanded history list is on screen.
    Presented,
    /// The window stays shown but the list cannot be read: folded to the title
    /// bar, tucked at the screen edge, or the favorites view is active.
    Concealed,
    /// The window is hidden; the next show is a fresh open.
    Hidden,
}

/// Busy interactions (dialogs, edits, composition, drags) and use of the
/// presented window (pointer inside, active or holding keyboard input) defer
/// the reset and grant a fresh interval when they finish, so closing a dialog
/// or moving the pointer away cannot immediately move the reader's list.
pub(crate) struct HistoryIdle {
    enabled: bool,
    timeout: Duration,
    last_activity: Option<Instant>,
    fired: bool,
    was_busy: bool,
    hidden_since: Option<Instant>,
    hide_reset_pending: bool,
}

impl Default for HistoryIdle {
    fn default() -> Self {
        Self {
            enabled: false,
            timeout: Duration::from_secs(60),
            last_activity: None,
            fired: false,
            was_busy: false,
            hidden_since: None,
            hide_reset_pending: false,
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

    /// A real hide still resets after its grace while the rest of the hidden
    /// UI sleeps. Protected interactions re-arm this after they finish.
    pub(crate) fn next_hidden_deadline(&self) -> Option<Instant> {
        (self.enabled && self.hide_reset_pending && !self.was_busy)
            .then(|| self.hidden_since.map(|since| since + HIDE_GRACE))
            .flatten()
    }

    /// `busy` means a dialog, edit, composition, or another protected
    /// interaction is active. `in_use` means the pointer is over the window or
    /// it is active / holds keyboard input; it only matters while presented.
    ///
    /// True asks the caller to return to the top and clear the selection.
    /// A hide resets once per hide; the idle interval fires once until new
    /// activity or changed settings rearm it.
    pub(crate) fn poll(&mut self, now: Instant, presence: HistoryPresence, busy: bool, in_use: bool) -> bool {
        let hidden = presence == HistoryPresence::Hidden;
        if !hidden {
            // Reopened before the hide reset ran: keep the position.
            self.hidden_since = None;
            self.hide_reset_pending = false;
        } else if self.hidden_since.is_none() {
            self.hidden_since = Some(now);
            self.hide_reset_pending = true;
        }
        if busy || (in_use && presence == HistoryPresence::Presented) {
            self.was_busy = true;
            return false;
        }
        if self.was_busy {
            self.was_busy = false;
            self.activity(now);
            return false;
        }
        if !self.enabled {
            return false;
        }
        if self.hide_reset_pending
            && self.hidden_since.is_some_and(|since| now.saturating_duration_since(since) >= HIDE_GRACE)
        {
            self.hide_reset_pending = false;
            self.fired = true;
            return true;
        }
        if self.fired {
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
    use HistoryPresence::{Concealed, Hidden, Presented};

    fn enabled(now: Instant) -> HistoryIdle {
        let mut idle = HistoryIdle::default();
        idle.configure(true, 60, now);
        idle
    }

    fn secs(now: Instant, seconds: u64) -> Instant {
        now + Duration::from_secs(seconds)
    }

    #[test]
    fn disabled_by_default_and_after_being_disabled() {
        let now = Instant::now();
        let mut idle = HistoryIdle::default();
        assert!(!idle.poll(secs(now, 3_600), Presented, false, false));
        assert!(!idle.poll(secs(now, 3_601), Hidden, false, false));
        assert!(!idle.poll(secs(now, 3_700), Hidden, false, false));
        idle.configure(true, 60, now);
        idle.configure(false, 60, secs(now, 10));
        assert!(!idle.poll(secs(now, 3_800), Presented, false, false));
    }

    #[test]
    fn exact_deadline_fires_only_once_while_unused() {
        let now = Instant::now();
        let mut idle = enabled(now);
        assert!(!idle.poll(now + Duration::from_millis(59_999), Presented, false, false));
        assert!(idle.poll(secs(now, 60), Presented, false, false));
        assert!(!idle.poll(secs(now, 60), Presented, false, false));
        assert!(!idle.poll(secs(now, 3_600), Presented, false, false));
    }

    #[test]
    fn hiding_resets_after_the_grace_even_before_the_idle_deadline() {
        let now = Instant::now();
        let mut idle = enabled(now);
        assert!(!idle.poll(secs(now, 5), Hidden, false, false));
        assert_eq!(idle.next_hidden_deadline(), Some(secs(now, 5) + HIDE_GRACE));
        assert!(!idle.poll(secs(now, 5) + Duration::from_millis(499), Hidden, false, false));
        assert!(idle.poll(secs(now, 5) + HIDE_GRACE, Hidden, false, false));
        assert_eq!(idle.next_hidden_deadline(), None);
        assert!(!idle.poll(secs(now, 30), Hidden, false, false));
        assert!(!idle.poll(secs(now, 31), Presented, false, false));
    }

    #[test]
    fn protected_hidden_interaction_disarms_expired_deadline_until_it_finishes() {
        let now = Instant::now();
        let mut idle = enabled(now);
        assert!(!idle.poll(now, Hidden, true, false));
        assert_eq!(idle.next_hidden_deadline(), None);
        assert!(!idle.poll(secs(now, 2), Hidden, false, false));
        assert_eq!(idle.next_hidden_deadline(), Some(now + HIDE_GRACE));
        assert!(idle.poll(secs(now, 2), Hidden, false, false));
        assert_eq!(idle.next_hidden_deadline(), None);
    }

    #[test]
    fn every_hide_resets_again_without_list_activity() {
        let now = Instant::now();
        let mut idle = enabled(now);
        assert!(!idle.poll(secs(now, 1), Hidden, false, false));
        assert!(idle.poll(secs(now, 2), Hidden, false, false));
        assert!(!idle.poll(secs(now, 3), Presented, false, true));
        assert!(!idle.poll(secs(now, 4), Hidden, false, false));
        assert!(idle.poll(secs(now, 5), Hidden, false, false));
    }

    #[test]
    fn quick_reopen_within_the_grace_keeps_the_position() {
        let now = Instant::now();
        let mut idle = enabled(now);
        assert!(!idle.poll(secs(now, 5), Hidden, false, false));
        assert!(!idle.poll(secs(now, 5) + Duration::from_millis(200), Presented, false, true));
        assert!(!idle.poll(secs(now, 10), Presented, false, false));
        assert!(!idle.poll(secs(now, 10), Hidden, false, false));
        assert!(idle.poll(secs(now, 11), Hidden, false, false));
    }

    #[test]
    fn busy_hide_waits_for_the_interaction_and_drops_if_reopened_first() {
        let now = Instant::now();
        let mut idle = enabled(now);
        assert!(!idle.poll(secs(now, 5), Hidden, true, false));
        assert!(!idle.poll(secs(now, 9), Hidden, true, false));
        assert!(!idle.poll(secs(now, 10), Hidden, false, false));
        assert!(idle.poll(secs(now, 11), Hidden, false, false));

        let mut idle = enabled(now);
        assert!(!idle.poll(secs(now, 5), Hidden, true, false));
        assert!(!idle.poll(secs(now, 9), Presented, true, false));
        assert!(!idle.poll(secs(now, 10), Presented, false, false));
        assert!(!idle.poll(secs(now, 69), Presented, false, false));
        assert!(idle.poll(secs(now, 70), Presented, false, false));
    }

    #[test]
    fn concealed_list_returns_when_idle_without_waiting_to_be_presented() {
        let now = Instant::now();
        let mut idle = enabled(now);
        assert!(!idle.poll(secs(now, 30), Concealed, false, false));
        assert!(!idle.poll(secs(now, 59), Concealed, false, true));
        assert!(idle.poll(secs(now, 60), Concealed, false, true));
        assert!(!idle.poll(secs(now, 61), Presented, false, false));
    }

    #[test]
    fn short_fold_or_edge_peek_keeps_the_position() {
        let now = Instant::now();
        let mut idle = enabled(now);
        assert!(!idle.poll(secs(now, 10), Concealed, false, false));
        assert!(!idle.poll(secs(now, 20), Presented, false, true));
        // Folding again after use starts a fresh interval.
        assert!(!idle.poll(secs(now, 40), Concealed, false, false));
        assert!(!idle.poll(secs(now, 99), Concealed, false, false));
        assert!(idle.poll(secs(now, 100), Concealed, false, false));
    }

    #[test]
    fn pinned_window_in_use_never_returns_and_restarts_after_use() {
        let now = Instant::now();
        let mut idle = enabled(now);
        assert!(!idle.poll(secs(now, 30), Presented, false, true));
        assert!(!idle.poll(secs(now, 300), Presented, false, true));
        assert!(!idle.poll(secs(now, 310), Presented, false, false));
        assert!(!idle.poll(secs(now, 369), Presented, false, false));
        assert!(idle.poll(secs(now, 370), Presented, false, false));
    }

    #[test]
    fn busy_interaction_grants_a_full_interval_after_finishing() {
        let now = Instant::now();
        let mut idle = enabled(now);
        assert!(!idle.poll(secs(now, 55), Presented, true, false));
        assert!(!idle.poll(secs(now, 100), Presented, true, false));
        assert!(!idle.poll(secs(now, 110), Presented, false, false));
        assert!(!idle.poll(secs(now, 169), Presented, false, false));
        assert!(idle.poll(secs(now, 170), Presented, false, false));
    }

    #[test]
    fn new_activity_rearms_a_consumed_interval() {
        let now = Instant::now();
        let mut idle = enabled(now);
        assert!(idle.poll(secs(now, 60), Presented, false, false));
        idle.activity(secs(now, 100));
        assert!(!idle.poll(secs(now, 159), Presented, false, false));
        assert!(idle.poll(secs(now, 160), Presented, false, false));
    }

    #[test]
    fn changed_settings_restart_timing_but_same_settings_preserve_it() {
        let now = Instant::now();
        let mut idle = enabled(now);
        idle.configure(true, 60, secs(now, 40));
        assert!(idle.poll(secs(now, 60), Presented, false, false));
        idle.configure(true, 90, secs(now, 70));
        assert!(!idle.poll(secs(now, 159), Presented, false, false));
        assert!(idle.poll(secs(now, 160), Presented, false, false));
    }

    #[test]
    fn enabling_starts_now_instead_of_inheriting_previous_idle_time() {
        let now = Instant::now();
        let mut idle = HistoryIdle::default();
        idle.activity(now);
        idle.configure(true, 60, secs(now, 600));
        assert!(!idle.poll(secs(now, 659), Presented, false, false));
        assert!(idle.poll(secs(now, 660), Presented, false, false));
    }
}
