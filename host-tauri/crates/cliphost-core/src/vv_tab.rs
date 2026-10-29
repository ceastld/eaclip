//! C# `EnableVvTabTextInstruction`: type `vvv` or `vv`+Tab to show the clip window.

use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(2);
const VK_V: u32 = 0x56;
const VK_TAB: u32 = 0x09;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VvTabAction {
    Show { backspaces: u8 },
}

#[derive(Debug, Default)]
pub struct VvTabMachine {
    matched_v: u8,
    last: Option<Instant>,
}

impl VvTabMachine {
    pub fn on_keydown(&mut self, vk: u32, modifier_down: bool, now: Instant) -> Option<VvTabAction> {
        if modifier_down {
            self.reset();
            return None;
        }
        let within = self
            .last
            .is_some_and(|last| now.duration_since(last) <= TIMEOUT);
        if vk == VK_V {
            self.last = Some(now);
            if !within {
                self.matched_v = 1;
                return None;
            }
            return match self.matched_v {
                1 => {
                    self.matched_v = 2;
                    None
                }
                2 => {
                    self.reset();
                    // The triggering third V is swallowed by the hook.
                    Some(VvTabAction::Show { backspaces: 2 })
                }
                _ => {
                    self.matched_v = 1;
                    None
                }
            };
        }
        if vk == VK_TAB && self.matched_v == 2 && within {
            self.reset();
            return Some(VvTabAction::Show { backspaces: 2 });
        }
        self.reset();
        None
    }

    pub(crate) fn reset(&mut self) {
        self.matched_v = 0;
        self.last = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vvv_only_erases_two_delivered_letters() {
        let mut machine = VvTabMachine::default();
        let t0 = Instant::now();
        assert_eq!(machine.on_keydown(VK_V, false, t0), None);
        assert_eq!(machine.on_keydown(VK_V, false, t0 + Duration::from_millis(20)), None);
        assert_eq!(
            machine.on_keydown(VK_V, false, t0 + Duration::from_millis(40)),
            Some(VvTabAction::Show { backspaces: 2 })
        );
    }

    #[test]
    fn vv_tab_shows_and_swallows_two_letters() {
        let mut machine = VvTabMachine::default();
        let t0 = Instant::now();
        assert_eq!(machine.on_keydown(VK_V, false, t0), None);
        assert_eq!(machine.on_keydown(VK_V, false, t0 + Duration::from_millis(20)), None);
        assert_eq!(
            machine.on_keydown(VK_TAB, false, t0 + Duration::from_millis(40)),
            Some(VvTabAction::Show { backspaces: 2 })
        );
    }

    #[test]
    fn modifiers_or_timeout_reset() {
        let mut machine = VvTabMachine::default();
        let t0 = Instant::now();
        assert_eq!(machine.on_keydown(VK_V, false, t0), None);
        assert_eq!(machine.on_keydown(VK_V, true, t0 + Duration::from_millis(10)), None);
        assert_eq!(machine.on_keydown(VK_V, false, t0 + Duration::from_millis(20)), None);
        assert_eq!(
            machine.on_keydown(VK_TAB, false, t0 + Duration::from_secs(3)),
            None
        );
    }
}
