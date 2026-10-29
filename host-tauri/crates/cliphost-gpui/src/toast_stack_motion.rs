//! Anchor-relative repositioning for the copy-toast stack. Display only;
//! timers, card lifetimes and native window placement stay with their owners.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

const REPOSITION: Duration = Duration::from_millis(220);

pub(super) fn height_report_needed(
    height: f32,
    scale: f32,
    reported_height: f32,
    reported_scale: f32,
) -> bool {
    height >= 1.0
        && ((height - reported_height).abs() >= 0.5
            || (scale - reported_scale).abs() > f32::EPSILON)
}

/// The screen edge the toast HWND is pinned to. The window grows and shrinks
/// away from it, so card positions measured from it survive every resize.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum StackAnchor {
    #[default]
    Bottom,
    Top,
}

impl StackAnchor {
    pub fn for_placement(placement: Option<&str>) -> Self {
        if placement == Some("top-right") {
            Self::Top
        } else {
            Self::Bottom
        }
    }

    /// Layout Y measured from the pinned edge (downward positive). Growing a
    /// bottom-pinned HWND upward shifts `top`/`bottom` but not this value.
    pub fn anchored_y(self, top: f32, bottom: f32, viewport_height: f32) -> f32 {
        match self {
            Self::Bottom => bottom - viewport_height,
            Self::Top => top,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct MotionFrame {
    pub offset_y: f32,
    pub active: bool,
}

#[derive(Clone, Copy)]
struct Transition {
    started: Instant,
    from_y: f32,
}

impl Transition {
    fn frame(self, now: Instant) -> MotionFrame {
        let elapsed = now.saturating_duration_since(self.started);
        if elapsed >= REPOSITION {
            return MotionFrame::default();
        }
        let progress = elapsed.as_secs_f64() / REPOSITION.as_secs_f64();
        MotionFrame {
            offset_y: self.from_y * (1.0 - reposition_easing(progress)) as f32,
            active: true,
        }
    }
}

struct Row {
    anchored_y: f32,
    transition: Option<Transition>,
}

#[derive(Default)]
pub(super) struct StackMotion {
    enabled: bool,
    anchor: StackAnchor,
    scale: f32,
    rows: HashMap<u64, Row>,
}

impl StackMotion {
    /// Reduced motion snaps directly. DPI or anchor changes start a fresh
    /// layout snapshot instead of mixing positions from different layouts.
    pub fn set_mode(&mut self, enabled: bool, anchor: StackAnchor, scale: f32) {
        let enabled = enabled && scale.is_finite() && scale > 0.0;
        if enabled != self.enabled
            || anchor != self.anchor
            || (scale - self.scale).abs() > f32::EPSILON
        {
            self.rows.clear();
        }
        self.enabled = enabled;
        self.anchor = anchor;
        self.scale = scale;
    }

    pub fn clear(&mut self) {
        self.rows.clear();
    }

    pub fn retain(&mut self, keys: &[u64]) {
        self.rows.retain(|key, _| keys.contains(key));
    }

    /// Observe the *raw* anchored Y before the child is translated. Resizing
    /// the HWND away from its anchor does not move it and cannot restart motion.
    pub fn observe(&mut self, key: u64, anchored_y: f32, now: Instant) -> MotionFrame {
        if !anchored_y.is_finite() {
            self.rows.remove(&key);
            return MotionFrame::default();
        }
        if !self.enabled {
            return MotionFrame::default();
        }
        let row = self.rows.entry(key).or_insert(Row {
            anchored_y,
            transition: None,
        });
        let current_y = row
            .transition
            .map(|motion| motion.frame(now).offset_y)
            .unwrap_or(0.0);
        let difference = row.anchored_y - anchored_y;
        if difference.abs() > 0.5 {
            // Web FLIP: continue from the currently painted Y when another
            // copy/removal changes the destination during an unfinished move.
            let from_y = difference + current_y;
            row.transition = (from_y.abs() > f32::EPSILON).then_some(Transition {
                started: now,
                from_y,
            });
        }
        row.anchored_y = anchored_y;
        if let Some(motion) = row.transition {
            let frame = motion.frame(now);
            if !frame.active {
                row.transition = None;
            }
            frame
        } else {
            MotionFrame::default()
        }
    }

    pub fn defer_shrink(&self, raw_height: f32, viewport_height: f32, now: Instant) -> bool {
        self.enabled
            && raw_height + 0.5 < viewport_height
            && self.rows.values().any(|row| {
                row.transition
                    .is_some_and(|motion| motion.frame(now).active)
            })
    }
}

/// CSS cubic-bezier(0.22, 1, 0.36, 1). Invert the x curve before evaluating y;
/// applying an ease-out polynomial directly to time would not match Web FLIP.
fn reposition_easing(progress: f64) -> f64 {
    let progress = progress.clamp(0.0, 1.0);
    if progress == 0.0 || progress == 1.0 {
        return progress;
    }
    let (mut low, mut high) = (0.0, 1.0);
    for _ in 0..28 {
        let t = (low + high) * 0.5;
        let x = 0.66 * t - 0.24 * t * t + 0.58 * t * t * t;
        if x < progress {
            low = t;
        } else {
            high = t;
        }
    }
    let t = (low + high) * 0.5;
    1.0 - (1.0 - t).powi(3)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled() -> StackMotion {
        let mut motion = StackMotion::default();
        motion.set_mode(true, StackAnchor::Bottom, 1.0);
        motion
    }

    /// Bottom-pinned card whose bottom edge sits `distance` above the
    /// viewport's bottom edge.
    fn observe_d(motion: &mut StackMotion, key: u64, distance: f32, now: Instant) -> MotionFrame {
        let viewport = 240.0;
        let bottom = viewport - distance;
        motion.observe(key, StackAnchor::Bottom.anchored_y(bottom - 50.0, bottom, viewport), now)
    }

    #[test]
    fn initial_layout_and_viewport_growth_keep_existing_screen_positions() {
        let now = Instant::now();
        let mut motion = enabled();
        // The same card bottom moves 80 -> 200 as the viewport grows 100 ->
        // 220; its bottom-relative distance remains 20 in either layout.
        assert!(!observe_d(&mut motion, 1, 100.0 - 80.0, now).active);
        assert!(!observe_d(&mut motion, 1, 220.0 - 200.0, now).active);
        assert!(!observe_d(&mut motion, 2, 90.0, now).active);
    }

    #[test]
    fn reposition_preserves_old_visual_position_and_finishes_in_220ms() {
        let now = Instant::now();
        let mut motion = enabled();
        observe_d(&mut motion, 1, 120.0, now);
        let moved = observe_d(&mut motion, 1, 40.0, now);
        assert_eq!(moved.offset_y, -80.0);
        assert!(moved.active);
        assert_eq!(-40.0 + moved.offset_y, -120.0);
        let completed = observe_d(&mut motion, 1, 40.0, now + REPOSITION);
        assert_eq!(completed.offset_y, 0.0);
        assert!(!completed.active);
    }

    #[test]
    fn rapid_retarget_starts_at_the_current_visual_y() {
        let now = Instant::now();
        let mut motion = enabled();
        observe_d(&mut motion, 1, 120.0, now);
        observe_d(&mut motion, 1, 40.0, now);
        let halfway = now + Duration::from_millis(90);
        let current = observe_d(&mut motion, 1, 40.0, halfway);
        let redirected = observe_d(&mut motion, 1, 72.0, halfway);
        assert!((-72.0 + redirected.offset_y - (-40.0 + current.offset_y)).abs() < 0.0001);
        assert!(redirected.active);
        assert!(!observe_d(&mut motion, 1, 72.0, halfway + REPOSITION).active);
    }

    #[test]
    fn consecutive_removals_retarget_the_surviving_card_without_a_jump() {
        let now = Instant::now();
        let mut motion = enabled();
        for (key, distance) in [(1, 180.0), (2, 90.0), (3, 0.0)] {
            observe_d(&mut motion, key, distance, now);
        }
        motion.retain(&[1, 2]);
        observe_d(&mut motion, 1, 116.0, now);
        observe_d(&mut motion, 2, 26.0, now);
        let next = now + Duration::from_millis(60);
        let previous = observe_d(&mut motion, 1, 116.0, next).offset_y;
        motion.retain(&[1]);
        let redirected = observe_d(&mut motion, 1, 0.0, next);
        assert!((redirected.offset_y - (-116.0 + previous)).abs() < 0.0001);
        assert!(redirected.active);
        assert!(!motion.defer_shrink(80.0, 240.0, next + REPOSITION));
    }

    #[test]
    fn unchanged_animation_frames_do_not_restart_the_move() {
        let now = Instant::now();
        let mut motion = enabled();
        observe_d(&mut motion, 1, 100.0, now);
        observe_d(&mut motion, 1, 30.0, now);
        for ms in [16, 32, 100, 219] {
            assert!(
                observe_d(&mut motion, 1, 30.0, now + Duration::from_millis(ms))
                    .active
            );
        }
        assert!(
            !observe_d(&mut motion, 1, 30.0, now + Duration::from_millis(221))
                .active
        );
    }

    #[test]
    fn raw_height_shrink_waits_for_motion_but_growth_does_not() {
        let now = Instant::now();
        let mut motion = enabled();
        observe_d(&mut motion, 1, 120.0, now);
        observe_d(&mut motion, 1, 40.0, now);
        assert!(motion.defer_shrink(160.0, 240.0, now));
        assert!(!motion.defer_shrink(300.0, 240.0, now));
        assert!(!motion.defer_shrink(240.0, 240.0, now));
        assert!(!motion.defer_shrink(160.0, 240.0, now + REPOSITION));
    }

    #[test]
    fn hiding_dpi_changes_and_other_placements_retire_motion() {
        let now = Instant::now();
        let mut motion = enabled();
        observe_d(&mut motion, 1, 120.0, now);
        observe_d(&mut motion, 1, 40.0, now);
        motion.clear();
        assert!(!observe_d(&mut motion, 1, 10.0, now).active);
        observe_d(&mut motion, 1, 40.0, now);
        motion.set_mode(true, StackAnchor::Bottom, 1.5);
        assert!(!observe_d(&mut motion, 1, 80.0, now).active);
        observe_d(&mut motion, 1, 40.0, now);
        motion.set_mode(false, StackAnchor::Bottom, 1.5);
        assert!(!observe_d(&mut motion, 1, 0.0, now).active);
        assert!(!motion.defer_shrink(80.0, 160.0, now));
        motion.set_mode(true, StackAnchor::Bottom, 1.5);
        assert!(!observe_d(&mut motion, 1, 20.0, now).active);
    }

    #[test]
    fn removed_or_unmeasurable_cards_cannot_keep_the_window_large() {
        let now = Instant::now();
        let mut motion = enabled();
        observe_d(&mut motion, 1, 120.0, now);
        observe_d(&mut motion, 1, 40.0, now);
        motion.retain(&[2]);
        assert!(!motion.defer_shrink(80.0, 160.0, now));
        observe_d(&mut motion, 1, 120.0, now);
        observe_d(&mut motion, 1, 40.0, now);
        assert!(motion.defer_shrink(80.0, 160.0, now));
        assert!(!observe_d(&mut motion, 1, f32::NAN, now).active);
        assert!(!motion.defer_shrink(80.0, 160.0, now));
        assert!(!observe_d(&mut motion, 1, 40.0, now).active);
    }

    #[test]
    fn easing_matches_the_css_curve_at_a_known_parametric_point() {
        // At Bezier parameter t=.5, x=.3425 and y=.875 exactly.
        assert!((reposition_easing(0.3425) - 0.875).abs() < 0.0000001);
        assert_eq!(reposition_easing(0.0), 0.0);
        assert_eq!(reposition_easing(1.0), 1.0);
    }

    #[test]
    fn same_logical_height_reports_a_changed_dpi_once() {
        assert!(!height_report_needed(160.0, 1.0, 160.0, 1.0));
        assert!(height_report_needed(160.0, 1.5, 160.0, 1.0));
        assert!(!height_report_needed(160.0, 1.5, 160.0, 1.5));
    }

    #[test]
    fn anchored_y_ignores_growth_away_from_the_pinned_edge() {
        // Bottom pin: HWND grows 100 -> 220 upward; the card keeps 20 DIP.
        let bottom = StackAnchor::Bottom;
        assert_eq!(bottom.anchored_y(30.0, 80.0, 100.0), bottom.anchored_y(150.0, 200.0, 220.0));
        // Top pin: HWND grows downward; the card top is unchanged.
        let top = StackAnchor::Top;
        assert_eq!(top.anchored_y(12.0, 92.0, 104.0), top.anchored_y(12.0, 92.0, 300.0));
        assert_eq!(StackAnchor::for_placement(Some("top-right")), StackAnchor::Top);
        assert_eq!(StackAnchor::for_placement(Some("bottom-right")), StackAnchor::Bottom);
        assert_eq!(StackAnchor::for_placement(Some("bottom-center")), StackAnchor::Bottom);
        assert_eq!(StackAnchor::for_placement(None), StackAnchor::Bottom);
    }

    #[test]
    fn top_pinned_removal_slides_cards_up_from_their_old_position() {
        let now = Instant::now();
        let mut motion = StackMotion::default();
        motion.set_mode(true, StackAnchor::Top, 1.0);
        let anchor = StackAnchor::Top;
        motion.observe(2, anchor.anchored_y(104.0, 184.0, 196.0), now);
        // The card above leaves; this one now lays out at the top.
        let moved = motion.observe(2, anchor.anchored_y(12.0, 92.0, 196.0), now);
        assert_eq!(moved.offset_y, 92.0);
        assert!(moved.active);
        let halfway = motion.observe(2, 12.0, now + Duration::from_millis(110));
        assert!(halfway.offset_y > 0.0 && halfway.offset_y < 92.0);
        assert!(!motion.observe(2, 12.0, now + REPOSITION).active);
        // Switching anchors drops the old snapshot instead of animating it.
        motion.set_mode(true, StackAnchor::Bottom, 1.0);
        assert!(!motion.observe(2, -40.0, now).active);
    }

    #[test]
    fn reduced_motion_snaps_without_keeping_the_window_large() {
        let now = Instant::now();
        let mut motion = StackMotion::default();
        motion.set_mode(false, StackAnchor::Bottom, 1.0);
        motion.observe(1, -120.0, now);
        assert!(!motion.observe(1, -40.0, now).active);
        assert!(!motion.defer_shrink(80.0, 240.0, now));
    }
}
