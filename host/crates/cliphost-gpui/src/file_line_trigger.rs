//! Top-edge reveal timing for the file line (2026-10-28, proposal D).
//!
//! Rest the pointer at the screen's top edge and a click-through thread
//! appears; rest a moment longer and the whole line drops. Sliding along the
//! edge (across browser tabs, say) keeps restarting the rest, so it never
//! triggers. Pure state over pointer samples so the timing is unit tested.

use std::time::{Duration, Instant};

/// Rest before the line is down; the thread shows two fifths of the way in.
/// User adjustable since 2026-10-30, when ~0.46 s felt sluggish.
pub const DEFAULT_DELAY: Duration = Duration::from_millis(200);
pub const RETRACT_AFTER: Duration = Duration::from_millis(320);
/// Physical pixels the pointer may drift while still "resting" at the edge.
pub const STILL_PX: i32 = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Hidden,
    /// The thread shows; clicks still reach whatever is under it.
    Peek,
    Down,
}

/// One poll of the pointer.
#[derive(Debug, Clone, Copy)]
pub struct Sample {
    pub now: Instant,
    /// Physical x, compared only against itself while resting.
    pub x: i32,
    /// On the top pixel row of its monitor.
    pub at_edge: bool,
    /// Close enough under the edge to stay on the peeking thread.
    pub in_peek_band: bool,
    /// Over the dropped line or one of its sheets.
    pub in_line: bool,
    /// A full-screen program is in front, or something else owns the pointer.
    pub blocked: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    None,
    Peek,
    Drop,
    /// Back to hidden, from the thread or the dropped line.
    Hide,
}

#[derive(Debug)]
pub struct EdgeTrigger {
    phase: Phase,
    rest: Option<(i32, Instant)>,
    peek_at: Option<Instant>,
    left_at: Option<Instant>,
    /// The pointer has been on the line since it dropped. A line dropped
    /// without the pointer (a key, a drag) waits for it to arrive and leave.
    visited: bool,
    delay: Duration,
}

impl Default for EdgeTrigger {
    fn default() -> Self {
        Self { phase: Phase::Hidden, rest: None, peek_at: None, left_at: None, visited: false, delay: DEFAULT_DELAY }
    }
}

impl EdgeTrigger {
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// Total rest at the edge before the line drops; zero drops on the next poll.
    pub fn set_delay(&mut self, delay: Duration) {
        self.delay = delay;
    }

    fn peek_after(&self) -> Duration {
        self.delay * 2 / 5
    }

    fn drop_after(&self) -> Duration {
        self.delay - self.peek_after()
    }

    pub fn step(&mut self, sample: Sample) -> Change {
        if sample.blocked {
            self.rest = None;
            return if self.phase == Phase::Hidden { Change::None } else { self.hide() };
        }
        match self.phase {
            Phase::Hidden => {
                if !sample.at_edge {
                    self.rest = None;
                    return Change::None;
                }
                match self.rest {
                    Some((x, since)) if (sample.x - x).abs() <= STILL_PX => {
                        if sample.now.duration_since(since) < self.peek_after() {
                            return Change::None;
                        }
                        self.rest = Some((sample.x, sample.now));
                        self.phase = Phase::Peek;
                        self.peek_at = Some(sample.now);
                        Change::Peek
                    }
                    _ => {
                        self.rest = Some((sample.x, sample.now));
                        Change::None
                    }
                }
            }
            Phase::Peek => {
                if !sample.in_peek_band {
                    return self.hide();
                }
                if self.rest.is_some_and(|(x, _)| (sample.x - x).abs() > STILL_PX) {
                    let change = self.hide();
                    if sample.at_edge {
                        self.rest = Some((sample.x, sample.now));
                    }
                    return change;
                }
                if self.peek_at.is_some_and(|at| sample.now.duration_since(at) >= self.drop_after()) {
                    self.drop_now(true);
                    return Change::Drop;
                }
                Change::None
            }
            Phase::Down => {
                if sample.in_line {
                    self.visited = true;
                    self.left_at = None;
                    return Change::None;
                }
                if !self.visited {
                    return Change::None;
                }
                let left = *self.left_at.get_or_insert(sample.now);
                if sample.now.duration_since(left) >= RETRACT_AFTER { self.hide() } else { Change::None }
            }
        }
    }

    /// Drop without the rest: a key, or a folder carried to the edge.
    pub fn drop_now(&mut self, pointer_on_line: bool) {
        self.phase = Phase::Down;
        self.rest = None;
        self.peek_at = None;
        self.left_at = None;
        self.visited = pointer_on_line;
    }

    /// Click outside, a click that goes somewhere, or Esc.
    pub fn retract(&mut self) {
        let _ = self.hide();
    }

    fn hide(&mut self) -> Change {
        *self = Self { delay: self.delay, ..Self::default() };
        Change::Hide
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    fn edge(now: Instant, x: i32) -> Sample {
        Sample { now, x, at_edge: true, in_peek_band: true, in_line: true, blocked: false }
    }

    fn slow() -> EdgeTrigger {
        let mut trigger = EdgeTrigger::default();
        trigger.set_delay(Duration::from_millis(500));
        trigger
    }

    fn away(now: Instant) -> Sample {
        Sample { now, x: 500, at_edge: false, in_peek_band: false, in_line: false, blocked: false }
    }

    #[test]
    fn resting_shows_the_thread_then_drops_the_line() {
        let base = Instant::now();
        let mut trigger = slow();
        assert_eq!(trigger.step(edge(at(base, 0), 400)), Change::None);
        assert_eq!(trigger.step(edge(at(base, 120), 403)), Change::None, "small drift still rests");
        assert_eq!(trigger.step(edge(at(base, 200), 402)), Change::Peek);
        assert_eq!(trigger.phase(), Phase::Peek);
        assert_eq!(trigger.step(edge(at(base, 400), 402)), Change::None);
        assert_eq!(trigger.step(edge(at(base, 500), 402)), Change::Drop);
        assert_eq!(trigger.phase(), Phase::Down);
    }

    #[test]
    fn sliding_along_the_edge_never_triggers() {
        let base = Instant::now();
        let mut trigger = slow();
        for step in 0..40 {
            let change = trigger.step(edge(at(base, step * 40), 100 + step as i32 * 20));
            assert_eq!(change, Change::None, "step {step}");
        }
        assert_eq!(trigger.phase(), Phase::Hidden);
    }

    #[test]
    fn sliding_after_the_thread_appears_restarts_the_entire_rest() {
        let base = Instant::now();
        let mut trigger = slow();
        trigger.step(edge(at(base, 0), 300));
        assert_eq!(trigger.step(edge(at(base, 200), 300)), Change::Peek);
        assert_eq!(trigger.step(edge(at(base, 430), 340)), Change::Hide);
        assert_eq!(trigger.phase(), Phase::Hidden);
        assert_eq!(trigger.step(edge(at(base, 460), 340)), Change::None,
            "the old drop deadline must not survive pointer movement");
        assert_eq!(trigger.step(edge(at(base, 630), 340)), Change::Peek);
        assert_eq!(trigger.step(edge(at(base, 929), 343)), Change::None);
        assert_eq!(trigger.step(edge(at(base, 930), 342)), Change::Drop);
    }

    #[test]
    fn leaving_the_thread_cancels_without_dropping() {
        let base = Instant::now();
        let mut trigger = slow();
        trigger.step(edge(at(base, 0), 300));
        assert_eq!(trigger.step(edge(at(base, 220), 300)), Change::Peek);
        assert_eq!(trigger.step(away(at(base, 260))), Change::Hide);
        assert_eq!(trigger.phase(), Phase::Hidden);
        assert_eq!(trigger.step(edge(at(base, 280), 300)), Change::None, "a fresh rest starts over");
    }

    #[test]
    fn the_default_drops_within_a_fifth_of_a_second() {
        let base = Instant::now();
        let mut trigger = EdgeTrigger::default();
        trigger.step(edge(at(base, 0), 300));
        assert_eq!(trigger.step(edge(at(base, 40), 300)), Change::None);
        assert_eq!(trigger.step(edge(at(base, 80), 300)), Change::Peek);
        assert_eq!(trigger.step(edge(at(base, 160), 300)), Change::None);
        assert_eq!(trigger.step(edge(at(base, 200), 300)), Change::Drop);
    }

    #[test]
    fn zero_delay_still_needs_a_resting_pointer_and_survives_hiding() {
        let base = Instant::now();
        let mut trigger = EdgeTrigger::default();
        trigger.set_delay(Duration::ZERO);
        assert_eq!(trigger.step(edge(at(base, 0), 300)), Change::None, "the first sample only starts the rest");
        assert_eq!(trigger.step(edge(at(base, 40), 360)), Change::None, "sliding still restarts it");
        assert_eq!(trigger.step(edge(at(base, 80), 360)), Change::Peek);
        assert_eq!(trigger.step(edge(at(base, 120), 360)), Change::Drop);
        trigger.retract();
        trigger.step(edge(at(base, 200), 360));
        assert_eq!(trigger.step(edge(at(base, 240), 360)), Change::Peek, "hiding keeps the chosen delay");
    }

    #[test]
    fn the_line_retracts_after_the_pointer_has_been_away_a_moment() {
        let base = Instant::now();
        let mut trigger = EdgeTrigger::default();
        trigger.drop_now(true);
        assert_eq!(trigger.step(away(at(base, 0))), Change::None);
        assert_eq!(trigger.step(away(at(base, 200))), Change::None);
        assert_eq!(trigger.step(edge(at(base, 250), 10)), Change::None, "coming back cancels");
        assert_eq!(trigger.step(away(at(base, 300))), Change::None);
        assert_eq!(trigger.step(away(at(base, 640))), Change::Hide);
    }

    #[test]
    fn a_line_dropped_by_key_waits_for_the_pointer_to_visit() {
        let base = Instant::now();
        let mut trigger = EdgeTrigger::default();
        trigger.drop_now(false);
        assert_eq!(trigger.step(away(at(base, 0))), Change::None);
        assert_eq!(trigger.step(away(at(base, 2_000))), Change::None);
        assert_eq!(trigger.phase(), Phase::Down);
    }

    #[test]
    fn a_full_screen_program_blocks_and_folds_the_line() {
        let base = Instant::now();
        let mut trigger = EdgeTrigger::default();
        let blocked = |now| Sample { blocked: true, ..edge(now, 300) };
        for ms in [0, 250, 600] {
            assert_eq!(trigger.step(blocked(at(base, ms))), Change::None);
        }
        trigger.drop_now(true);
        assert_eq!(trigger.step(blocked(at(base, 700))), Change::Hide);
    }
}
