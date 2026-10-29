//! Pure state of the settings color popover (no GPUI types, no window).
//!
//! The draft is kept as HSV + alpha. Hue is never re-derived from RGB while the
//! user drags, otherwise s == 0 or v == 0 would snap the hue bar back to red.

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use super::settings_color::ArgbColor;

/// Preset swatches, one row of twelve. Kept in one place so the product owner can
/// adjust them; each entry is a WPF `#AARRGGBB` string.
pub(super) const PRESET_COLORS: [&str; 12] = [
    "#FFFFFFFF",
    "#FFF3F3F3",
    "#FF8A8A8A",
    "#FF1F1F1F",
    "#FF000000",
    "#FF0078D4",
    "#FF107C10",
    "#FFFFB900",
    "#FFD83B01",
    "#FFE81123",
    "#FF881798",
    "#00FFFFFF",
];

/// Largest hue the bar can hold. 360 is the same red as 0, but would make the
/// thumb jump back to the left edge.
pub(super) const HUE_MAX: f32 = 359.99;

/// Opaque white: the starting point when the text box holds an invalid value.
pub(super) const WHITE: ArgbColor = ArgbColor {
    a: 255,
    r: 255,
    g: 255,
    b: 255,
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Adjust {
    Saturation,
    Value,
    Alpha,
    Hue,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct ColorModel {
    h: f32,
    s: f32,
    v: f32,
    a: f32,
}

impl ColorModel {
    pub(super) fn from_argb(color: ArgbColor) -> Self {
        Self::from_argb_keeping_hue(color, 0.0)
    }

    /// Hue is undefined for greys and black; keep `fallback_hue` there.
    pub(super) fn from_argb_keeping_hue(color: ArgbColor, fallback_hue: f32) -> Self {
        let r = f32::from(color.r) / 255.0;
        let g = f32::from(color.g) / 255.0;
        let b = f32::from(color.b) / 255.0;
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let delta = max - min;
        let h = if delta <= f32::EPSILON {
            fallback_hue
        } else if max == r {
            60.0 * (((g - b) / delta).rem_euclid(6.0))
        } else if max == g {
            60.0 * ((b - r) / delta + 2.0)
        } else {
            60.0 * ((r - g) / delta + 4.0)
        };
        Self {
            h: h.clamp(0.0, HUE_MAX),
            s: if max <= f32::EPSILON {
                0.0
            } else {
                delta / max
            },
            v: max,
            a: f32::from(color.a) / 255.0,
        }
    }

    pub(super) fn to_argb(self) -> ArgbColor {
        let (r, g, b) = self.rgb_unit();
        let byte = |value: f32| (value * 255.0).round().clamp(0.0, 255.0) as u8;
        ArgbColor {
            a: byte(self.a),
            r: byte(r),
            g: byte(g),
            b: byte(b),
        }
    }

    fn rgb_unit(self) -> (f32, f32, f32) {
        let c = self.v * self.s;
        let sector = self.h / 60.0;
        let x = c * (1.0 - (sector.rem_euclid(2.0) - 1.0).abs());
        let m = self.v - c;
        let (r, g, b) = match sector.floor() as i32 {
            0 => (c, x, 0.0),
            1 => (x, c, 0.0),
            2 => (0.0, c, x),
            3 => (0.0, x, c),
            4 => (x, 0.0, c),
            _ => (c, 0.0, x),
        };
        (r + m, g + m, b + m)
    }

    pub(super) fn hue(self) -> f32 {
        self.h
    }
    pub(super) fn saturation(self) -> f32 {
        self.s
    }
    pub(super) fn value(self) -> f32 {
        self.v
    }
    pub(super) fn alpha(self) -> f32 {
        self.a
    }

    /// `#AARRGGBB`, the text shown in the popover.
    pub(super) fn hex(self) -> String {
        self.to_argb().serialize()
    }

    pub(super) fn set_hue(&mut self, hue: f32) {
        if hue.is_finite() {
            self.h = hue.clamp(0.0, HUE_MAX);
        }
    }

    pub(super) fn set_saturation_value(&mut self, saturation: f32, value: f32) {
        if saturation.is_finite() && value.is_finite() {
            self.s = saturation.clamp(0.0, 1.0);
            self.v = value.clamp(0.0, 1.0);
        }
    }

    pub(super) fn set_alpha(&mut self, alpha: f32) {
        if alpha.is_finite() {
            self.a = alpha.clamp(0.0, 1.0);
        }
    }

    /// Hex text replaces the whole draft; invalid input leaves it untouched.
    pub(super) fn apply_hex(&mut self, input: &str) -> bool {
        match ArgbColor::parse(input) {
            Some(color) => {
                self.apply_argb(color);
                true
            }
            None => false,
        }
    }

    /// Presets and recents: replace the draft but keep the hue for greys.
    pub(super) fn apply_argb(&mut self, color: ArgbColor) {
        *self = Self::from_argb_keeping_hue(color, self.h);
    }

    /// Eyedropper result: the sampled RGB replaces the draft but the draft's own
    /// alpha is kept exactly, as for every RGB-only picker. Greys keep the hue.
    pub(super) fn apply_rgb_keeping_alpha(&mut self, (r, g, b): (u8, u8, u8)) {
        let alpha = self.a;
        *self = Self::from_argb_keeping_hue(ArgbColor { a: 255, r, g, b }, self.h);
        self.a = alpha;
    }

    /// Keyboard step: 1% (hue 1 degree), 10% / 10 degrees with Shift.
    /// Returns whether the draft moved.
    pub(super) fn nudge(&mut self, what: Adjust, direction: i32, large: bool) -> bool {
        let before = *self;
        let unit = if large { 0.10 } else { 0.01 };
        let sign = direction.signum() as f32;
        match what {
            Adjust::Saturation => self.set_saturation_value(self.s + sign * unit, self.v),
            Adjust::Value => self.set_saturation_value(self.s, self.v + sign * unit),
            Adjust::Alpha => self.set_alpha(self.a + sign * unit),
            Adjust::Hue => self.set_hue(self.h + sign * if large { 10.0 } else { 1.0 }),
        }
        *self != before
    }
}

/// Position of a pointer inside the saturation / value square (top-left origin).
pub(super) fn square_sample(x: f32, y: f32, width: f32, height: f32) -> Option<(f32, f32)> {
    (width > 0.0 && height > 0.0 && x.is_finite() && y.is_finite()).then(|| {
        (
            (x / width).clamp(0.0, 1.0),
            (1.0 - y / height).clamp(0.0, 1.0),
        )
    })
}

/// Thumb inset of the hue and alpha bars: the thumb travels `[inset, width - inset]`.
pub(super) const BAR_INSET: f32 = 7.0;

/// Fraction along a bar for a pointer `x` relative to its left edge.
pub(super) fn bar_ratio(x: f32, width: f32) -> Option<f32> {
    let travel = width - 2.0 * BAR_INSET;
    (travel > 0.0 && x.is_finite()).then(|| ((x - BAR_INSET) / travel).clamp(0.0, 1.0))
}

/// Draft + the value the popover was opened with. All commit rules live here so
/// they can be tested without a window.
#[derive(Debug)]
pub(super) struct PopupState {
    pub(super) model: ColorModel,
    opened: ArgbColor,
    open: bool,
}

impl PopupState {
    /// An invalid text box value opens at opaque white.
    pub(super) fn new(text: &str) -> Self {
        let opened = ArgbColor::parse(text).unwrap_or(WHITE);
        Self {
            model: ColorModel::from_argb(opened),
            opened,
            open: true,
        }
    }

    pub(super) fn is_open(&self) -> bool {
        self.open
    }

    pub(super) fn opened(&self) -> ArgbColor {
        self.opened
    }

    pub(super) fn draft(&self) -> ArgbColor {
        self.model.to_argb()
    }

    pub(super) fn changed(&self) -> bool {
        self.draft() != self.opened
    }

    /// Enter or an outside click. Closes; yields the color only if it changed.
    pub(super) fn confirm(&mut self) -> Option<ArgbColor> {
        if !std::mem::replace(&mut self.open, false) {
            return None;
        }
        self.changed().then(|| self.draft())
    }

    /// Escape and every lifecycle close: never commits. True if it was open.
    pub(super) fn cancel(&mut self) -> bool {
        std::mem::replace(&mut self.open, false)
    }
}

/// What a finished eyedropper run means for the popover.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum EyedropperOutcome {
    /// Replace the draft's RGB (alpha untouched). Never committed by itself.
    Apply((u8, u8, u8)),
    /// Esc, right click, focus loss, timeout: nothing changes.
    Cancelled,
    /// The overlay could not run; the message is kept for the popover.
    Failed,
    /// An old or retired run: ignored completely.
    Stale,
}

/// Handed to the blocking worker so it can ask whether its run still matters.
#[derive(Clone, Debug)]
pub(super) struct EyedropperTicket {
    id: u64,
    current: Arc<AtomicU64>,
}

impl EyedropperTicket {
    pub(super) fn id(&self) -> u64 {
        self.id
    }

    /// False once the run finished, the popover closed or it was dropped.
    pub(super) fn is_current(&self) -> bool {
        self.current.load(Ordering::Acquire) == self.id
    }
}

/// Lifecycle of the screen-eyedropper runs of one popover (at most one at a
/// time). Pure so busy / stale / closed handling is testable without a window.
#[derive(Debug)]
pub(super) struct EyedropperRuns {
    current: Arc<AtomicU64>,
    last: u64,
    active: Option<u64>,
    error: Option<String>,
}

impl Default for EyedropperRuns {
    fn default() -> Self {
        Self {
            current: Arc::new(AtomicU64::new(0)),
            last: 0,
            active: None,
            error: None,
        }
    }
}

impl EyedropperRuns {
    pub(super) fn is_busy(&self) -> bool {
        self.active.is_some()
    }

    pub(super) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// `None` while another run is in flight. A new run clears the old error.
    pub(super) fn begin(&mut self) -> Option<EyedropperTicket> {
        if self.active.is_some() {
            return None;
        }
        self.error = None;
        self.last += 1;
        self.active = Some(self.last);
        self.current.store(self.last, Ordering::Release);
        Some(EyedropperTicket {
            id: self.last,
            current: self.current.clone(),
        })
    }

    /// Records a failure that happened before a run could start.
    pub(super) fn fail(&mut self, message: impl Into<String>) {
        self.error = Some(message.into());
    }

    pub(super) fn finish(
        &mut self,
        run: u64,
        result: Result<Option<(u8, u8, u8)>, String>,
    ) -> EyedropperOutcome {
        if self.active != Some(run) {
            return EyedropperOutcome::Stale;
        }
        self.active = None;
        self.current.store(0, Ordering::Release);
        match result {
            Ok(Some(rgb)) => EyedropperOutcome::Apply(rgb),
            Ok(None) => EyedropperOutcome::Cancelled,
            Err(message) => {
                self.error = Some(message);
                EyedropperOutcome::Failed
            }
        }
    }

    /// The popover closed: whatever is in flight no longer matters and the
    /// overlay closes itself at its next poll.
    pub(super) fn retire(&mut self) {
        self.active = None;
        self.current.store(0, Ordering::Release);
    }
}

impl Drop for EyedropperRuns {
    fn drop(&mut self) {
        self.retire();
    }
}

/// Identity of the form state a popover was opened for. Any mismatch means the
/// popover is stale and must close without committing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ColorSessionLease {
    pub(super) generation: u64,
    pub(super) leave_epoch: u64,
    pub(super) config_id: String,
}

impl ColorSessionLease {
    pub(super) fn is_current(
        &self,
        generation: u64,
        leave_epoch: u64,
        config_id: &str,
        leave_locked: bool,
    ) -> bool {
        !leave_locked
            && self.generation == generation
            && self.leave_epoch == leave_epoch
            && self.config_id == config_id
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OpenDecision {
    Open,
    /// IME composition in the text box: show the existing message.
    RefuseComposing,
    Ignore,
}

pub(super) fn open_decision(
    leave_locked: bool,
    field_is_color: bool,
    composing: bool,
) -> OpenDecision {
    if leave_locked || !field_is_color {
        OpenDecision::Ignore
    } else if composing {
        OpenDecision::RefuseComposing
    } else {
        OpenDecision::Open
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct PopupLayout {
    pub(super) left: f32,
    pub(super) anchor_y: f32,
    pub(super) above: bool,
}

/// Anchored under the swatch button; flips above when the space below is too
/// small and the space above is larger. `anchor` is `[x, y, width, height]`.
pub(super) fn popup_layout(
    anchor: [f32; 4],
    viewport: [f32; 2],
    width: f32,
    estimate: f32,
) -> PopupLayout {
    let [x, y, _, height] = anchor;
    let margin = 8.0_f32.min((viewport[0] / 2.0).max(0.0));
    let left = x.clamp(margin, (viewport[0] - margin - width).max(margin));
    let below = (viewport[1] - y - height - 4.0 - margin).max(0.0);
    let above = (y - 4.0 - margin).max(0.0);
    let place_above = below < estimate && below < above;
    PopupLayout {
        left,
        anchor_y: if place_above {
            y - 4.0
        } else {
            y + height + 4.0
        },
        above: place_above,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argb(a: u8, r: u8, g: u8, b: u8) -> ArgbColor {
        ArgbColor { a, r, g, b }
    }

    #[test]
    fn hsv_round_trips_primaries_greys_black_white_and_every_alpha() {
        let mut samples = vec![
            argb(255, 255, 0, 0),
            argb(255, 0, 255, 0),
            argb(255, 0, 0, 255),
            argb(255, 255, 255, 0),
            argb(255, 0, 255, 255),
            argb(255, 255, 0, 255),
            argb(255, 0, 0, 0),
            argb(255, 255, 255, 255),
            argb(0, 0x12, 0x34, 0x56),
            argb(0x7f, 0x80, 0x40, 0xc0),
        ];
        samples.extend((0..=255u8).map(|grey| argb(255, grey, grey, grey)));
        samples.extend((0..=255u8).map(|alpha| argb(alpha, 0x01, 0x80, 0xfe)));
        for step in (0..=255u16).step_by(15) {
            for other in (0..=255u16).step_by(17) {
                let (a, b) = (step as u8, other as u8);
                samples.push(argb(255, a, b, 255 - a));
                samples.push(argb(255, b, 255 - a, a));
                samples.push(argb(255, 255 - a, a, b));
            }
        }
        for color in samples {
            assert_eq!(
                ColorModel::from_argb(color).to_argb(),
                color,
                "{}",
                color.serialize()
            );
        }
    }

    #[test]
    fn hue_survives_greys_and_black_while_dragging() {
        let mut model = ColorModel::from_argb(argb(255, 255, 0, 0));
        model.set_hue(200.0);
        model.set_saturation_value(0.0, 0.5);
        assert_eq!(model.hue(), 200.0);
        model.set_saturation_value(0.7, 0.0);
        assert_eq!(model.hue(), 200.0);
        model.set_saturation_value(0.7, 0.6);
        assert_eq!(model.hue(), 200.0);
        // Presets and recents that are grey keep the current hue; chromatic ones replace it.
        model.apply_argb(argb(255, 0x8a, 0x8a, 0x8a));
        assert_eq!(model.hue(), 200.0);
        model.apply_argb(argb(255, 0, 0x78, 0xd4));
        assert!((model.hue() - 206.04).abs() < 0.1, "{}", model.hue());
    }

    #[test]
    fn hue_is_bounded_below_360_and_known_hues_map_to_expected_colors() {
        let mut model = ColorModel::from_argb(argb(255, 255, 0, 0));
        model.set_hue(360.0);
        assert!(model.hue() < 360.0);
        assert_eq!(model.to_argb(), argb(255, 255, 0, 0));
        model.set_hue(120.0);
        assert_eq!(model.to_argb(), argb(255, 0, 255, 0));
        model.set_hue(240.0);
        assert_eq!(model.to_argb(), argb(255, 0, 0, 255));
        model.set_hue(f32::NAN);
        assert_eq!(model.hue(), 240.0);
    }

    #[test]
    fn hex_input_accepts_six_and_eight_digits_and_ignores_invalid_text() {
        let mut model = ColorModel::from_argb(argb(255, 1, 2, 3));
        assert!(model.apply_hex("#112233"));
        assert_eq!(model.to_argb(), argb(255, 0x11, 0x22, 0x33));
        assert!(model.apply_hex(" #80AABBCC "));
        assert_eq!(model.to_argb(), argb(0x80, 0xaa, 0xbb, 0xcc));
        let before = model;
        for bad in [
            "",
            "#",
            "#12345",
            "#1234567",
            "112233",
            "#GGGGGG",
            "#11223344 55",
        ] {
            assert!(!model.apply_hex(bad), "{bad}");
            assert_eq!(model, before, "{bad}");
        }
        assert_eq!(before.hex(), "#80AABBCC");
    }

    #[test]
    fn keyboard_steps_are_one_percent_or_ten_with_shift_and_clamp() {
        let mut model = ColorModel::from_argb(argb(255, 255, 0, 0));
        model.set_saturation_value(0.50, 0.50);
        assert!(model.nudge(Adjust::Saturation, 1, false));
        assert!((model.saturation() - 0.51).abs() < 1e-6);
        assert!(model.nudge(Adjust::Value, -1, true));
        assert!((model.value() - 0.40).abs() < 1e-6);
        model.set_alpha(0.95);
        assert!(model.nudge(Adjust::Alpha, 1, true));
        assert_eq!(model.alpha(), 1.0);
        assert!(!model.nudge(Adjust::Alpha, 1, false)); // Already at the limit.
        model.set_hue(10.0);
        assert!(model.nudge(Adjust::Hue, 1, false));
        assert_eq!(model.hue(), 11.0);
        assert!(model.nudge(Adjust::Hue, -1, true));
        assert_eq!(model.hue(), 1.0);
        assert!(model.nudge(Adjust::Hue, -1, true));
        assert_eq!(model.hue(), 0.0);
        assert!(!model.nudge(Adjust::Hue, -1, false));
    }

    #[test]
    fn pointer_samples_clamp_to_the_controls() {
        assert_eq!(square_sample(110.0, 35.0, 220.0, 140.0), Some((0.5, 0.75)));
        assert_eq!(square_sample(-9.0, 500.0, 220.0, 140.0), Some((0.0, 0.0)));
        assert_eq!(square_sample(900.0, -9.0, 220.0, 140.0), Some((1.0, 1.0)));
        assert_eq!(square_sample(1.0, 1.0, 0.0, 140.0), None);
        assert_eq!(bar_ratio(7.0, 220.0), Some(0.0));
        assert_eq!(bar_ratio(110.0, 220.0), Some(0.5));
        assert_eq!(bar_ratio(500.0, 220.0), Some(1.0));
        assert_eq!(bar_ratio(5.0, 10.0), None);
    }

    #[test]
    fn invalid_start_value_opens_as_opaque_white_without_counting_as_a_change() {
        let state = PopupState::new("not a color");
        assert_eq!(state.opened(), WHITE);
        assert_eq!(state.draft(), WHITE);
        assert!(!state.changed());
    }

    #[test]
    fn escape_closes_without_a_commit_even_after_changes() {
        let mut state = PopupState::new("#FF112233");
        state.model.set_alpha(0.5);
        assert!(state.changed());
        assert!(state.cancel());
        assert_eq!(state.confirm(), None); // A late outside click cannot commit.
        assert!(!state.cancel());
    }

    #[test]
    fn outside_or_enter_commits_exactly_once_and_only_when_changed() {
        let mut state = PopupState::new("#FF112233");
        state.model.apply_hex("#FF445566");
        assert_eq!(state.confirm(), Some(argb(255, 0x44, 0x55, 0x66)));
        assert!(!state.is_open());
        assert_eq!(state.confirm(), None);

        let mut untouched = PopupState::new("#80112233");
        assert_eq!(untouched.confirm(), None);
        assert!(!untouched.is_open());

        // Moving away and back to the opened value is not a change either.
        let mut returned = PopupState::new("#FF112233");
        returned.model.apply_hex("#FF000000");
        returned.model.apply_hex("#FF112233");
        assert_eq!(returned.confirm(), None);
    }

    #[test]
    fn stale_sessions_are_rejected_for_generation_epoch_page_and_leave_lock() {
        let lease = ColorSessionLease {
            generation: 3,
            leave_epoch: 1,
            config_id: "global".into(),
        };
        assert!(lease.is_current(3, 1, "global", false));
        assert!(!lease.is_current(4, 1, "global", false)); // load_schema bumps generation
        assert!(!lease.is_current(3, 2, "global", false));
        assert!(!lease.is_current(3, 1, "clip", false));
        assert!(!lease.is_current(3, 1, "global", true));
    }

    #[test]
    fn composing_text_boxes_refuse_to_open_and_locked_forms_ignore_the_click() {
        assert_eq!(open_decision(false, true, false), OpenDecision::Open);
        assert_eq!(
            open_decision(false, true, true),
            OpenDecision::RefuseComposing
        );
        assert_eq!(open_decision(true, true, true), OpenDecision::Ignore);
        assert_eq!(open_decision(false, false, false), OpenDecision::Ignore);
    }

    #[test]
    fn popup_layout_aligns_left_flips_above_and_clamps_to_the_window() {
        let below = popup_layout([40.0, 100.0, 90.0, 32.0], [640.0, 600.0], 242.0, 290.0);
        assert!(!below.above);
        assert_eq!((below.left, below.anchor_y), (40.0, 136.0));
        let above = popup_layout([40.0, 500.0, 90.0, 32.0], [640.0, 600.0], 242.0, 290.0);
        assert!(above.above);
        assert_eq!(above.anchor_y, 496.0);
        let right = popup_layout([600.0, 100.0, 90.0, 32.0], [640.0, 600.0], 242.0, 290.0);
        assert_eq!(right.left, 640.0 - 8.0 - 242.0);
        // More room below than above keeps the popover below even if it is tight.
        let tight = popup_layout([40.0, 60.0, 90.0, 32.0], [640.0, 300.0], 242.0, 290.0);
        assert!(!tight.above);
    }

    #[test]
    fn eyedropper_replaces_rgb_but_keeps_the_draft_alpha_and_never_commits() {
        let mut state = PopupState::new("#80112233");
        state.model.set_alpha(0.37); // Not a whole byte: must survive untouched.
        state.model.apply_rgb_keeping_alpha((0x12, 0x80, 0xfe));
        assert_eq!(state.model.alpha(), 0.37);
        let draft = state.draft();
        assert_eq!((draft.r, draft.g, draft.b), (0x12, 0x80, 0xfe));
        assert_eq!(draft.a, (0.37_f32 * 255.0).round() as u8);
        // Applying the sample is a draft change only; the popover stays open.
        assert!(state.is_open());
        assert!(state.changed());
        // A grey sample keeps the hue chosen so far.
        state.model.set_hue(200.0);
        state.model.apply_rgb_keeping_alpha((0x40, 0x40, 0x40));
        assert_eq!(state.model.hue(), 200.0);
        assert_eq!(state.draft().a, draft.a);
    }

    #[test]
    fn eyedropper_runs_one_at_a_time_and_apply_only_the_current_result() {
        let mut runs = EyedropperRuns::default();
        assert!(!runs.is_busy());
        let first = runs.begin().expect("first run");
        assert!(runs.is_busy());
        assert!(first.is_current());
        assert!(runs.begin().is_none(), "a second click while busy is ignored");
        // A reply carrying another run id is stale and changes nothing.
        assert_eq!(
            runs.finish(first.id() + 1, Ok(Some((1, 2, 3)))),
            EyedropperOutcome::Stale
        );
        assert!(runs.is_busy());
        assert_eq!(
            runs.finish(first.id(), Ok(Some((1, 2, 3)))),
            EyedropperOutcome::Apply((1, 2, 3))
        );
        assert!(!runs.is_busy());
        assert!(!first.is_current(), "a finished run reads as not current");
        // The same reply cannot apply twice.
        assert_eq!(
            runs.finish(first.id(), Ok(Some((1, 2, 3)))),
            EyedropperOutcome::Stale
        );
        let second = runs.begin().expect("run after the first finished");
        assert_ne!(second.id(), first.id());
        assert!(second.is_current());
        assert!(!first.is_current());
    }

    #[test]
    fn eyedropper_cancel_and_failure_keep_the_draft_and_report_the_error() {
        let mut runs = EyedropperRuns::default();
        let run = runs.begin().unwrap();
        assert_eq!(runs.finish(run.id(), Ok(None)), EyedropperOutcome::Cancelled);
        assert_eq!(runs.error(), None);
        let run = runs.begin().unwrap();
        assert_eq!(
            runs.finish(run.id(), Err("overlay failed".into())),
            EyedropperOutcome::Failed
        );
        assert_eq!(runs.error(), Some("overlay failed"));
        assert!(!runs.is_busy());
        // Trying again clears the old message.
        let _ = runs.begin().unwrap();
        assert_eq!(runs.error(), None);
        runs.fail("no window handle");
        assert_eq!(runs.error(), Some("no window handle"));
    }

    #[test]
    fn closing_or_dropping_the_popover_retires_the_run_so_the_overlay_stops() {
        let mut runs = EyedropperRuns::default();
        let run = runs.begin().unwrap();
        runs.retire();
        assert!(!run.is_current(), "the worker's lease check must turn false");
        assert!(!runs.is_busy());
        // The late reply of the retired run is ignored.
        assert_eq!(
            runs.finish(run.id(), Ok(Some((9, 9, 9)))),
            EyedropperOutcome::Stale
        );
        let next = runs.begin().unwrap();
        assert!(next.is_current() && !run.is_current());
        drop(runs);
        assert!(!next.is_current(), "dropping the entity ends the run too");
    }

    #[test]
    fn presets_are_valid_wpf_colors_with_a_transparent_last_entry() {
        for preset in PRESET_COLORS {
            assert_eq!(ArgbColor::parse(preset).unwrap().serialize(), preset);
        }
        assert_eq!(PRESET_COLORS[11], "#00FFFFFF");
    }
}
