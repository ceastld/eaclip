//! Polling must not turn the previous presentation's deactivation into a
//! dismissal of a newly shown no-activate window.

pub(super) struct ActivationObservation<S> {
    stamp: S,
    active: Option<bool>,
}

impl<S: Copy + Eq> ActivationObservation<S> {
    pub fn new(stamp: S, active: Option<bool>) -> Self {
        Self { stamp, active }
    }

    pub fn observe(&mut self, stamp: S, active: Option<bool>) -> Option<bool> {
        let new_session = self.stamp != stamp;
        let previous = self.active;
        self.stamp = stamp;
        self.active = active;
        if new_session {
            // SWP_NOACTIVATE intentionally leaves the root inactive. Its first
            // sample is a baseline, never evidence of leaving this session.
            active.filter(|active| *active)
        } else if active != previous {
            active
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hide_and_reopen_between_polls_discards_old_deactivation() {
        let mut observation = ActivationObservation::new(1, Some(true));
        assert_eq!(observation.observe(2, Some(false)), None);
        assert_eq!(observation.observe(2, Some(false)), None);
        assert_eq!(observation.observe(2, Some(true)), Some(true));
        assert_eq!(observation.observe(2, Some(false)), Some(false));
    }

    #[test]
    fn repeated_show_rebases_even_without_a_hidden_poll() {
        let mut observation = ActivationObservation::new(1, Some(true));
        for session in 2..10 {
            assert_eq!(observation.observe(session, Some(false)), None);
            assert_eq!(observation.observe(session, Some(true)), Some(true));
        }
    }

    #[test]
    fn new_activated_session_still_reports_activation() {
        let mut observation = ActivationObservation::new(1, Some(false));
        assert_eq!(observation.observe(2, Some(true)), Some(true));
        assert_eq!(observation.observe(2, Some(true)), None);
        assert_eq!(observation.observe(2, Some(false)), Some(false));
    }
}
