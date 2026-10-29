//! Framework-free hover cancellation and delay ledger. No window, DB or codec I/O.
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

pub const HOVER_DELAY: Duration = Duration::from_millis(500);
/// 2026-10-24: once a preview has shown, moving to another row of the same
/// owner switches at once; leaving rows for longer than this restores the
/// full delay. Scroll, menus and forced hides end the warm state immediately.
pub const WARM_GRACE: Duration = Duration::from_millis(400);

/// The UI must assign a new session when an owner is hidden/reopened or rebuilt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Owner {
    pub hwnd: isize,
    pub session: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Ticket(pub(crate) u64);
impl Ticket {
    pub fn id(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Binding {
    pub owner: Owner,
    epoch: u64,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Hide {
    pub owner: Owner,
    pub ticket: Ticket,
    pub force: bool,
}

struct OwnerState {
    binding: Binding,
    menu_open: bool,
}
struct Active {
    binding: Binding,
    ticket: Ticket,
    image: bool,
    deadline: Instant,
    warm: bool,
}

#[derive(Default)]
pub(super) struct Ledger {
    owners: HashMap<isize, OwnerState>,
    next_epoch: u64,
    next_ticket: u64,
    active: Option<Active>,
    native_owner: Option<Binding>,
    warm_until: Option<(Binding, Instant)>,
    closed: bool,
}

impl Ledger {
    fn ticket(&mut self) -> Ticket {
        self.next_ticket = self
            .next_ticket
            .checked_add(1)
            .expect("hover ticket exhausted");
        Ticket(self.next_ticket)
    }
    fn hide(&mut self, binding: Binding, force: bool) -> Hide {
        Hide {
            owner: binding.owner,
            ticket: self.ticket(),
            force,
        }
    }
    pub fn bind(&mut self, owner: Owner) -> Result<(Binding, Option<Hide>), String> {
        if self.closed {
            return Err("Hover service is closed".into());
        }
        if owner.hwnd == 0 {
            return Err("Hover owner is unavailable".into());
        }
        // A fresh epoch also invalidates an accidental reuse of the same session.
        self.next_epoch = self
            .next_epoch
            .checked_add(1)
            .expect("hover epoch exhausted");
        let old = self.owners.remove(&owner.hwnd);
        // Rebinding a destination retires requests, but an open menu still
        // belongs to this display session. The caller also replays its menu
        // lease when it has explicitly retired/dropped the previous owner.
        let menu_open = old
            .as_ref()
            .is_some_and(|entry| entry.binding.owner == owner && entry.menu_open);
        let hide = old.and_then(|entry| self.cancel(entry.binding, true));
        let binding = Binding {
            owner,
            epoch: self.next_epoch,
        };
        self.owners
            .insert(owner.hwnd, OwnerState { binding, menu_open });
        Ok((binding, hide))
    }
    pub fn is_bound(&self, binding: Binding) -> bool {
        !self.closed
            && self
                .owners
                .get(&binding.owner.hwnd)
                .is_some_and(|entry| entry.binding == binding)
    }
    pub fn begin(
        &mut self,
        binding: Binding,
        image: bool,
        now: Instant,
    ) -> Result<Option<(Ticket, Option<Hide>)>, String> {
        if !self.is_bound(binding) {
            return Err("Hover owner session is retired".into());
        }
        if image && self.owners[&binding.owner.hwnd].menu_open {
            return Ok(None);
        }
        let shown = self
            .active
            .as_ref()
            .is_some_and(|active| active.binding == binding && now >= active.deadline);
        let warm = shown
            || self
                .warm_until
                .is_some_and(|(owner, until)| owner == binding && now <= until);
        let old = self.native_owner.take();
        // A warm switch keeps the visible popup; the new content replaces it.
        let hide = old
            .filter(|old| !warm || *old != binding)
            .map(|old| self.hide(old, true));
        let ticket = self.ticket();
        self.active = Some(Active {
            binding,
            ticket,
            image,
            deadline: if warm { now } else { now + HOVER_DELAY },
            warm,
        });
        self.native_owner = Some(binding);
        Ok(Some((ticket, hide)))
    }
    pub fn due(&self, binding: Binding, ticket: Ticket, now: Instant) -> bool {
        self.is_bound(binding)
            && self.active.as_ref().is_some_and(|active| {
                active.binding == binding
                    && active.ticket == ticket
                    && now >= active.deadline
                    && (!active.image || !self.owners[&binding.owner.hwnd].menu_open)
            })
    }
    /// UI card commands can wait behind another owner's Hide or an aim delay.
    /// Match the actual active lease, not a global maximum hide ticket: retiring
    /// an unrelated owner must not cancel the current owner's valid request.
    pub fn card_request_current(&self, owner: isize, ticket: u64, now: Instant) -> bool {
        self.active.as_ref().is_some_and(|active| {
            active.binding.owner.hwnd == owner
                && active.ticket.id() == ticket
                && self.due(active.binding, active.ticket, now)
        })
    }
    /// The scheduled request skipped the delay and may replace visible content.
    pub fn warm(&self, binding: Binding, ticket: Ticket) -> bool {
        self.active
            .as_ref()
            .is_some_and(|active| active.binding == binding && active.ticket == ticket && active.warm)
    }
    pub fn leave(&mut self, binding: Binding, ticket: Ticket) -> Option<Hide> {
        self.leave_at(binding, ticket, Instant::now())
    }
    pub fn leave_at(&mut self, binding: Binding, ticket: Ticket, now: Instant) -> Option<Hide> {
        if !self.is_bound(binding)
            || !self
                .active
                .as_ref()
                .is_some_and(|active| active.binding == binding && active.ticket == ticket)
        {
            return None;
        }
        let shown = self.active.take().is_some_and(|active| now >= active.deadline);
        if shown {
            self.warm_until = Some((binding, now + WARM_GRACE));
        }
        // Keep the native owner's identity during the existing 180ms gap grace.
        Some(self.hide(binding, false))
    }
    pub fn cancel(&mut self, binding: Binding, force: bool) -> Option<Hide> {
        if self.warm_until.is_some_and(|(owner, _)| owner == binding) {
            self.warm_until = None;
        }
        let active = self
            .active
            .as_ref()
            .is_some_and(|active| active.binding == binding);
        let native = self.native_owner == Some(binding);
        if !active && !native {
            return None;
        }
        if active {
            self.active = None;
        }
        if native {
            self.native_owner = None;
        }
        Some(self.hide(binding, force))
    }
    pub fn menu(&mut self, binding: Binding, open: bool) -> Result<Option<Hide>, String> {
        if !self.is_bound(binding) {
            return Err("Hover owner session is retired".into());
        }
        self.owners.get_mut(&binding.owner.hwnd).unwrap().menu_open = open;
        Ok(if open {
            self.cancel(binding, true)
        } else {
            None
        })
    }
    pub fn retire(&mut self, binding: Binding) -> Option<Hide> {
        if !self.is_bound(binding) {
            return None;
        }
        let hide = self.cancel(binding, true);
        self.owners.remove(&binding.owner.hwnd);
        // Insert a native ticket tombstone even when its show has not started.
        Some(hide.unwrap_or_else(|| self.hide(binding, true)))
    }
    pub fn destroy(&mut self, hwnd: isize) -> Option<Hide> {
        let binding = self.owners.get(&hwnd)?.binding;
        self.retire(binding)
    }
    pub fn cancel_window(&mut self, hwnd: isize) -> Option<Hide> {
        let binding = self.owners.get(&hwnd)?.binding;
        self.cancel(binding, true)
    }
    pub fn close(&mut self) -> Option<Hide> {
        let binding = self.native_owner.take();
        self.warm_until = None;
        self.closed = true;
        self.active = None;
        self.owners.clear();
        binding.map(|binding| self.hide(binding, true))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn owner(hwnd: isize, session: u64) -> Owner {
        Owner { hwnd, session }
    }
    #[test]
    fn delay_does_not_allow_lookup_before_500ms() {
        let mut ledger = Ledger::default();
        let (binding, _) = ledger.bind(owner(1, 1)).unwrap();
        let now = Instant::now();
        let (ticket, _) = ledger.begin(binding, true, now).unwrap().unwrap();
        assert!(!ledger.due(binding, ticket, now + Duration::from_millis(499)));
        assert!(ledger.due(binding, ticket, now + HOVER_DELAY));
    }
    #[test]
    fn menu_cancels_inflight_image_and_requires_a_new_hover_after_close() {
        let mut ledger = Ledger::default();
        let (binding, _) = ledger.bind(owner(1, 2)).unwrap();
        let now = Instant::now();
        let (ticket, _) = ledger.begin(binding, true, now).unwrap().unwrap();
        let hide = ledger.menu(binding, true).unwrap().unwrap();
        assert!(hide.force && hide.ticket.0 > ticket.0);
        assert!(ledger.begin(binding, true, now).unwrap().is_none());
        ledger.menu(binding, false).unwrap();
        assert!(!ledger.due(binding, ticket, now + HOVER_DELAY));
        assert!(ledger.begin(binding, true, now).unwrap().is_some());
    }
    #[test]
    fn leave_cancels_pending_decode_without_reviving_another_item() {
        let mut ledger = Ledger::default();
        let (binding, _) = ledger.bind(owner(1, 1)).unwrap();
        let now = Instant::now();
        let (first, _) = ledger.begin(binding, true, now).unwrap().unwrap();
        let (second, hide) = ledger.begin(binding, false, now).unwrap().unwrap();
        assert!(hide.unwrap().force);
        assert!(ledger.leave(binding, first).is_none());
        let leave = ledger.leave(binding, second).unwrap();
        assert!(!leave.force && leave.ticket.0 > second.0);
        assert!(!ledger.due(binding, second, now + HOVER_DELAY));
        let (_, hide) = ledger.begin(binding, true, now).unwrap().unwrap();
        assert!(hide.unwrap().force);
    }
    #[test]
    fn rebind_is_a_new_epoch_even_if_hwnd_and_session_are_reused() {
        let mut ledger = Ledger::default();
        let now = Instant::now();
        let (first, _) = ledger.bind(owner(1, 8)).unwrap();
        let (ticket, _) = ledger.begin(first, true, now).unwrap().unwrap();
        let (second, hide) = ledger.bind(owner(1, 8)).unwrap();
        assert!(hide.unwrap().ticket.0 > ticket.0);
        assert!(!ledger.is_bound(first));
        assert!(!ledger.due(first, ticket, now + HOVER_DELAY));
        assert!(ledger.retire(first).is_none());
        assert!(ledger.is_bound(second));
    }
    #[test]
    fn owner_destruction_and_shutdown_reject_late_work() {
        let mut ledger = Ledger::default();
        let now = Instant::now();
        let (binding, _) = ledger.bind(owner(1, 3)).unwrap();
        let (ticket, _) = ledger.begin(binding, true, now).unwrap().unwrap();
        assert!(ledger.destroy(1).unwrap().ticket.0 > ticket.0);
        assert!(!ledger.due(binding, ticket, now + HOVER_DELAY));
        let (new, _) = ledger.bind(owner(1, 4)).unwrap();
        let (ticket, _) = ledger.begin(new, false, now).unwrap().unwrap();
        assert!(ledger.close().unwrap().ticket.0 > ticket.0);
        assert!(!ledger.due(new, ticket, now + HOVER_DELAY));
        assert!(ledger.bind(owner(1, 5)).is_err());
    }
    #[test]
    fn switching_owners_cancels_only_the_previous_preview() {
        let mut ledger = Ledger::default();
        let now = Instant::now();
        let (first, _) = ledger.bind(owner(1, 1)).unwrap();
        let (second, _) = ledger.bind(owner(2, 1)).unwrap();
        let (old, _) = ledger.begin(first, true, now).unwrap().unwrap();
        let (new, hide) = ledger.begin(second, false, now).unwrap().unwrap();
        assert_eq!(hide.unwrap().owner, first.owner);
        ledger.retire(first);
        assert!(!ledger.due(first, old, now + HOVER_DELAY));
        assert!(ledger.due(second, new, now + HOVER_DELAY));
    }
    #[test]
    fn same_session_rebind_preserves_menu_and_old_menu_reply_cannot_close_it() {
        let mut ledger = Ledger::default();
        let now = Instant::now();
        let identity = owner(1, 1);
        let (first, _) = ledger.bind(identity).unwrap();
        ledger.menu(first, true).unwrap();
        let (second, _) = ledger.bind(identity).unwrap();
        assert!(!ledger.is_bound(first));
        assert!(ledger.begin(second, true, now).unwrap().is_none());
        assert!(ledger.menu(first, false).is_err());
        assert!(ledger.begin(second, true, now).unwrap().is_none());
        ledger.menu(second, false).unwrap();
        assert!(ledger.begin(second, true, now).unwrap().is_some());
    }
    #[test]
    fn retire_rejects_retained_callback_and_new_session_resets_old_menu() {
        let mut ledger = Ledger::default();
        let now = Instant::now();
        let (first, _) = ledger.bind(owner(1, 1)).unwrap();
        ledger.menu(first, true).unwrap();
        assert!(ledger.retire(first).is_some());
        assert!(ledger.retire(first).is_none());
        assert!(ledger.begin(first, false, now).is_err());
        let (next, _) = ledger.bind(owner(1, 2)).unwrap();
        assert!(ledger.begin(next, true, now).unwrap().is_some());
    }
    #[test]
    fn shown_preview_warms_row_switches_until_the_grace_expires() {
        let mut ledger = Ledger::default();
        let (binding, _) = ledger.bind(owner(1, 1)).unwrap();
        let start = Instant::now();
        let (first, _) = ledger.begin(binding, false, start).unwrap().unwrap();
        assert!(!ledger.warm(binding, first));
        // Moving straight to the next row while the first is visible.
        let shown = start + HOVER_DELAY;
        let (second, hide) = ledger.begin(binding, true, shown).unwrap().unwrap();
        assert!(hide.is_none(), "a warm switch keeps the visible popup");
        assert!(ledger.warm(binding, second));
        assert!(ledger.due(binding, second, shown));
        // Crossing a gap between rows: leave, then enter within the grace.
        let leave = ledger.leave_at(binding, second, shown).unwrap();
        assert!(!leave.force);
        let (third, _) = ledger.begin(binding, false, shown + WARM_GRACE).unwrap().unwrap();
        assert!(ledger.due(binding, third, shown + WARM_GRACE));
        ledger.leave_at(binding, third, shown + WARM_GRACE);
        let late = shown + WARM_GRACE * 2 + Duration::from_millis(1);
        let (cold, _) = ledger.begin(binding, false, late).unwrap().unwrap();
        assert!(!ledger.warm(binding, cold));
        assert!(!ledger.due(binding, cold, late));
    }
    #[test]
    fn unshown_leave_scroll_and_menu_do_not_warm_the_next_row() {
        let mut ledger = Ledger::default();
        let (binding, _) = ledger.bind(owner(1, 1)).unwrap();
        let start = Instant::now();
        let (pending, _) = ledger.begin(binding, false, start).unwrap().unwrap();
        ledger.leave_at(binding, pending, start + Duration::from_millis(100));
        let (next, _) = ledger.begin(binding, false, start + Duration::from_millis(150)).unwrap().unwrap();
        assert!(!ledger.warm(binding, next));
        let shown = start + Duration::from_millis(150) + HOVER_DELAY;
        ledger.leave_at(binding, next, shown);
        ledger.cancel(binding, true);
        let (after_scroll, _) = ledger.begin(binding, false, shown).unwrap().unwrap();
        assert!(!ledger.warm(binding, after_scroll));
        let (other, _) = ledger.bind(owner(2, 1)).unwrap();
        let (shown_ticket, _) = ledger.begin(binding, false, shown).unwrap().unwrap();
        ledger.leave_at(binding, shown_ticket, shown + HOVER_DELAY);
        let (foreign, _) = ledger.begin(other, false, shown + HOVER_DELAY).unwrap().unwrap();
        assert!(!ledger.warm(other, foreign), "warmth belongs to one owner window");
    }

    #[test]
    fn card_ui_admission_rechecks_retirement_without_canceling_another_owner() {
        let mut ledger = Ledger::default();
        let (first, _) = ledger.bind(owner(1, 1)).unwrap();
        let (second, _) = ledger.bind(owner(2, 1)).unwrap();
        let start = Instant::now();
        let shown = start + HOVER_DELAY;
        let (old_card, _) = ledger.begin(first, true, start).unwrap().unwrap();
        assert!(ledger.card_request_current(1, old_card.id(), shown));
        let (queued_card, _) = ledger.begin(second, true, start).unwrap().unwrap();
        assert!(!ledger.card_request_current(1, old_card.id(), shown));
        assert!(ledger.card_request_current(2, queued_card.id(), shown));

        // A Hide for the old owner has a newer ticket but cannot invalidate
        // the second owner's Show already waiting in the UI queue.
        let unrelated_hide = ledger.retire(first).unwrap();
        assert!(unrelated_hide.ticket.id() > queued_card.id());
        assert!(ledger.card_request_current(2, queued_card.id(), shown));

        // If the second owner retires before that Show or an AIM_HOLD expires,
        // its stale command must never recreate a card after the Hide.
        ledger.retire(second);
        assert!(!ledger.card_request_current(2, queued_card.id(), shown));
        let (reopened, _) = ledger.bind(owner(2, 2)).unwrap();
        let (fresh, _) = ledger.begin(reopened, true, shown).unwrap().unwrap();
        assert!(!ledger.card_request_current(2, queued_card.id(), shown + HOVER_DELAY));
        assert!(ledger.card_request_current(2, fresh.id(), shown + HOVER_DELAY));
        ledger.close();
        assert!(!ledger.card_request_current(2, fresh.id(), shown + HOVER_DELAY));
    }
}
