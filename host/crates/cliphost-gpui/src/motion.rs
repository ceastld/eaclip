//! Shared motion tokens mirroring `frontend/src/ui/*.css` transitions.
//! Durations and curves come from the frontend CSS unless a GPUI surface has
//! already been tuned and accepted; see `.agents/skills/gpui-frontend-align`.
//! Display only: timers, lifetimes and HWND placement stay with their owners.

use std::{
    cell::Cell,
    collections::HashMap,
    hash::Hash,
    sync::OnceLock,
    time::{Duration, Instant},
};

/// CSS `cubic-bezier(x1, y1, x2, y2)`; endpoints are fixed at (0,0) and (1,1).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CubicBezier {
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
}

impl CubicBezier {
    pub const fn new(x1: f32, y1: f32, x2: f32, y2: f32) -> Self {
        Self { x1, y1, x2, y2 }
    }

    /// Progress (0..=1) along the curve for elapsed fraction `x` (0..=1).
    pub fn sample(self, x: f32) -> f32 {
        let x = x.clamp(0.0, 1.0);
        if x == 0.0 || x == 1.0 {
            return x;
        }
        let bez = |t: f32, p1: f32, p2: f32| {
            let u = 1.0 - t;
            3.0 * u * u * t * p1 + 3.0 * u * t * t * p2 + t * t * t
        };
        // x(t) is monotonic for valid CSS curves (x1, x2 in 0..=1).
        let (mut low, mut high) = (0.0f32, 1.0f32);
        for _ in 0..24 {
            let t = (low + high) * 0.5;
            if bez(t, self.x1, self.x2) < x {
                low = t;
            } else {
                high = t;
            }
        }
        bez((low + high) * 0.5, self.y1, self.y2)
    }

    /// For `gpui::Animation::with_easing`.
    pub fn easing(self) -> impl Fn(f32) -> f32 + 'static {
        move |delta| self.sample(delta)
    }
}

/// CSS `ease`.
pub const EASE: CubicBezier = CubicBezier::new(0.25, 0.1, 0.25, 1.0);
/// CSS `ease-out`.
pub const EASE_OUT: CubicBezier = CubicBezier::new(0.0, 0.0, 0.58, 1.0);
/// Frontend enter curve `cubic-bezier(0.22, 1, 0.36, 1)` (toasts, hover preview).
pub const EMPHASIZED_ENTER: CubicBezier = CubicBezier::new(0.22, 1.0, 0.36, 1.0);
/// Frontend exit curve `cubic-bezier(0.4, 0, 0.2, 1)` (toast bottom exit).
pub const STANDARD_EXIT: CubicBezier = CubicBezier::new(0.4, 0.0, 0.2, 1.0);

/// Hover/press background and color changes (`120ms ease`).
pub const HOVER: Duration = Duration::from_millis(120);
/// Focus ring, border and small opacity changes (`140ms ease`).
pub const FOCUS: Duration = Duration::from_millis(140);
/// Control surface background/border/shadow (`160ms ease`).
pub const SURFACE: Duration = Duration::from_millis(160);
/// Small popups and menu columns entering (`120ms ease-out`).
pub const POPUP_ENTER: Duration = Duration::from_millis(120);
/// Footer list-settings proposal `.list-settings-popover` (`140ms ease-out`).
pub const LIST_SETTINGS_ENTER: Duration = Duration::from_millis(140);
/// `.keyboard-paste-status__bubble`: 140ms ease-out, no activation or input handoff.
pub const KEYBOARD_PASTE_HINT_ENTER: Duration = Duration::from_millis(140);
/// First pinned/active keyboard hint; hovering pauses its remaining lifetime.
pub const KEYBOARD_PASTE_HINT_DURATION: Duration = Duration::from_millis(4000);
/// Favorite cascade: trigger dwell, branch opening, diagonal grace and exit.
pub const FAVORITE_SAVE_OPEN: Duration = Duration::from_millis(200);
pub const FAVORITE_SAVE_BRANCH: Duration = Duration::from_millis(180);
pub const FAVORITE_SAVE_SWITCH: Duration = Duration::from_millis(360);
pub const FAVORITE_SAVE_DISMISS: Duration = Duration::from_millis(360);
/// `GpuiHistoryContextMenuPreview.tsx`: LAN submenu dwell and continuous leave.
pub const HISTORY_MENU_DEVICE_OPEN: Duration = Duration::from_millis(180);
pub const HISTORY_MENU_DEVICE_LEAVE: Duration = Duration::from_millis(360);
/// `GpuiAlignmentFavoriteSaveStage.tsx` NoticeClock: saved feedback lifetime.
pub const FAVORITE_FEEDBACK_SAVE: Duration = Duration::from_millis(2500);
/// `GpuiAlignmentFavoriteSaveStage.tsx` NoticeClock: undo feedback lifetime.
pub const FAVORITE_FEEDBACK_UNDO: Duration = Duration::from_millis(1500);
/// `GpuiAlignmentFavoriteSaveStage.tsx` NoticeClock: minimum after engagement ends.
pub const FAVORITE_FEEDBACK_RESUME: Duration = Duration::from_millis(1000);
/// Drawers and larger panels entering (`160ms ease-out`).
pub const PANEL_ENTER: Duration = Duration::from_millis(160);
/// Overlay scrollbar fade (tokens `scrollbar.fadeInMs` / `fadeOutMs`,
/// frontend `.next-scrollbar` opacity transition).
pub const SCROLLBAR_FADE_IN: Duration = Duration::from_millis(90);
pub const SCROLLBAR_FADE_OUT: Duration = Duration::from_millis(240);
/// Mouse-wheel distance per line. WebView2 moves about 100 px for the default
/// three-line notch; GPUI's List uses a fixed 20 px per line.
pub const WHEEL_LINE_PX: f32 = 100.0 / 3.0;
/// Keep wheel input close to the pointer: 90% of a notch in ~58 ms and a
/// 100 px notch settled within ~133 ms, while retaining smooth movement.
pub const SMOOTH_SCROLL_TIME_CONSTANT: Duration = Duration::from_millis(25);

/// One frame of smooth scrolling toward `target`; snaps within half a pixel.
pub fn smooth_scroll_step(current: f32, target: f32, elapsed: Duration) -> f32 {
    // Follow wall time even after a slow frame. Capping this at 50 ms made
    // input accumulate behind rendering and kept scrolling after the user
    // had stopped, precisely when image/raster work made a frame late.
    let elapsed = elapsed.as_secs_f32();
    let tau = SMOOTH_SCROLL_TIME_CONSTANT.as_secs_f32();
    let next = current + (target - current) * (1.0 - (-elapsed / tau).exp());
    if (target - next).abs() < 0.5 {
        target
    } else {
        next
    }
}

/// One-shot enter animation for `with_animation`, honoring the system
/// "show animations" setting. GPUI divides by the duration, so reduced motion
/// uses a 1ms floor instead of zero.
pub fn enter(duration: Duration, curve: CubicBezier) -> gpui::Animation {
    let duration = if reduced_motion() {
        Duration::from_millis(1)
    } else {
        duration
    };
    gpui::Animation::new(duration).with_easing(curve.easing())
}

/// Linear interpolation between two HSLA colors, taking the short hue path.
/// Fully transparent endpoints borrow the other side's hue, saturation and
/// lightness, so fading a color changes only its opacity rather than passing
/// through transparent black and darkening intermediate frames.
pub fn mix_hsla(from: gpui::Hsla, to: gpui::Hsla, t: f32) -> gpui::Hsla {
    let t = t.clamp(0.0, 1.0);
    let (from, to) = match (from.a <= f32::EPSILON, to.a <= f32::EPSILON) {
        (true, false) => (gpui::Hsla { a: from.a, ..to }, to),
        (false, true) => (from, gpui::Hsla { a: to.a, ..from }),
        _ => (from, to),
    };
    let mut dh = to.h - from.h;
    if dh > 0.5 {
        dh -= 1.0;
    } else if dh < -0.5 {
        dh += 1.0;
    }
    let lerp = |a: f32, b: f32| a + (b - a) * t;
    gpui::Hsla {
        h: (from.h + dh * t).rem_euclid(1.0),
        s: lerp(from.s, to.s),
        l: lerp(from.l, to.l),
        a: lerp(from.a, to.a),
    }
}

/// Plain linear interpolation of a scalar.
pub fn lerp(from: f32, to: f32, t: f32) -> f32 {
    from + (to - from) * t
}

/// One scalar transition from `from` to `to`. Callers use a 0..=1 "progress"
/// and map it onto colors with `mix_hsla` or onto offsets with `lerp`.
#[derive(Clone, Copy, Debug)]
pub struct Tween {
    from: f32,
    to: f32,
    started: Instant,
    duration: Duration,
    curve: CubicBezier,
}

impl Tween {
    /// A tween that is already at `value` and never animates.
    pub fn settled(value: f32, now: Instant) -> Self {
        Self {
            from: value,
            to: value,
            started: now,
            duration: Duration::ZERO,
            curve: EASE,
        }
    }

    pub fn new(
        from: f32,
        to: f32,
        started: Instant,
        duration: Duration,
        curve: CubicBezier,
    ) -> Self {
        Self { from, to, started, duration, curve }
    }

    pub fn target(&self) -> f32 {
        self.to
    }

    /// Current value; exactly the target once the duration has elapsed.
    pub fn sample(&self, now: Instant) -> f32 {
        if !self.active(now) {
            return self.to;
        }
        let elapsed = now.saturating_duration_since(self.started);
        let x = elapsed.as_secs_f32() / self.duration.as_secs_f32();
        lerp(self.from, self.to, self.curve.sample(x))
    }

    pub fn active(&self, now: Instant) -> bool {
        self.from != self.to
            && !self.duration.is_zero()
            && now.saturating_duration_since(self.started) < self.duration
    }
}

/// Keyed scalar transitions for state changes GPUI has no CSS `transition`
/// for (hover, focus, selection). Read with `value` while rendering, then
/// request another animation frame while `any_active`.
#[derive(Debug)]
pub struct Tweens<K: Eq + Hash> {
    tweens: HashMap<K, Tween>,
}

impl<K: Eq + Hash> Default for Tweens<K> {
    fn default() -> Self {
        Self { tweens: HashMap::new() }
    }
}

impl<K: Eq + Hash + Clone> Tweens<K> {
    /// Current value for `key` moving toward `target`. A key seen for the
    /// first time returns `target` (no first-frame animation); a changed
    /// target restarts from the current value over the full `duration`.
    /// Honors the system "reduce animations" setting.
    pub fn value(
        &mut self,
        key: K,
        target: f32,
        duration: Duration,
        curve: CubicBezier,
        now: Instant,
    ) -> f32 {
        self.value_with(key, target, duration, curve, now, reduced_motion())
    }

    /// `value` with an explicit reduced-motion flag (pure; used by tests).
    pub fn value_with(
        &mut self,
        key: K,
        target: f32,
        duration: Duration,
        curve: CubicBezier,
        now: Instant,
        reduced: bool,
    ) -> f32 {
        if reduced {
            self.tweens.insert(key, Tween::settled(target, now));
            return target;
        }
        match self.tweens.get_mut(&key) {
            None => {
                self.tweens.insert(key, Tween::settled(target, now));
                target
            }
            Some(tween) => {
                if (tween.target() - target).abs() > f32::EPSILON {
                    *tween = Tween::new(tween.sample(now), target, now, duration, curve);
                }
                tween.sample(now)
            }
        }
    }

    /// Whether any tween still needs another frame at `now`.
    pub fn any_active(&self, now: Instant) -> bool {
        self.tweens.values().any(|tween| tween.active(now))
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&K) -> bool) {
        self.tweens.retain(|key, _| keep(key));
    }

    pub fn clear(&mut self) {
        self.tweens.clear();
    }

    pub fn len(&self) -> usize {
        self.tweens.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tweens.is_empty()
    }
}

/// Whether the user turned off system animations (the WebView2
/// `prefers-reduced-motion` source). Read once and cached; never writes the
/// system setting. Always false off Windows.
pub fn reduced_motion() -> bool {
    static REDUCED: OnceLock<bool> = OnceLock::new();
    *REDUCED.get_or_init(system_reduced_motion)
}

#[cfg(windows)]
fn system_reduced_motion() -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{
        SystemParametersInfoW, SPI_GETCLIENTAREAANIMATION, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    };
    // BOOL is a 32-bit integer; animations count as enabled when the query fails.
    let mut enabled: i32 = 1;
    let result = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some((&mut enabled as *mut i32).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    result.is_ok() && enabled == 0
}

#[cfg(not(windows))]
fn system_reduced_motion() -> bool {
    false
}

/// Counts how many times a transient surface (menu, drawer, action bar) has
/// newly appeared. Feed it the surface's visibility from every render and use
/// the returned epoch in a `with_animation` id: the entrance plays once per
/// appearance and never on an ordinary redraw.
#[derive(Debug, Default)]
pub struct OpenEpoch {
    open: Cell<bool>,
    epoch: Cell<u64>,
}

impl OpenEpoch {
    pub fn observe(&self, open: bool) -> u64 {
        if open && !self.open.get() {
            self.epoch.set(self.epoch.get().wrapping_add(1));
        }
        self.open.set(open);
        self.epoch.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_epoch_advances_once_per_appearance_and_not_on_redraw() {
        let epoch = OpenEpoch::default();
        let closed = epoch.observe(false);
        assert_eq!(epoch.observe(false), closed);
        let first = epoch.observe(true);
        assert_ne!(first, closed);
        assert_eq!(epoch.observe(true), first, "redraw must not replay");
        assert_eq!(epoch.observe(false), first);
        let second = epoch.observe(true);
        assert_ne!(second, first, "reopening replays");
    }

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn first_sight_of_a_key_is_the_target_without_animation() {
        let base = Instant::now();
        let mut tweens = Tweens::<&str>::default();
        assert_eq!(tweens.value_with("a", 1.0, HOVER, EASE, base, false), 1.0);
        assert!(!tweens.any_active(base));
        assert_eq!(tweens.value_with("a", 1.0, HOVER, EASE, at(base, 10), false), 1.0);
    }

    #[test]
    fn target_change_animates_for_the_full_duration_then_settles() {
        let base = Instant::now();
        let mut tweens = Tweens::<&str>::default();
        tweens.value_with("a", 0.0, HOVER, EASE, base, false);
        let start = tweens.value_with("a", 1.0, HOVER, EASE, base, false);
        assert_eq!(start, 0.0);
        assert!(tweens.any_active(base));
        let mid = tweens.value_with("a", 1.0, HOVER, EASE, at(base, 60), false);
        assert!(mid > 0.0 && mid < 1.0, "mid {mid}");
        assert!(tweens.any_active(at(base, 119)));
        assert!(!tweens.any_active(at(base, 120)));
        assert_eq!(tweens.value_with("a", 1.0, HOVER, EASE, at(base, 500), false), 1.0);
    }

    #[test]
    fn reversing_midway_continues_from_the_current_value_without_a_jump() {
        let base = Instant::now();
        let mut tweens = Tweens::<&str>::default();
        tweens.value_with("a", 0.0, HOVER, EASE, base, false);
        tweens.value_with("a", 1.0, HOVER, EASE, base, false);
        let before = tweens.value_with("a", 1.0, HOVER, EASE, at(base, 50), false);
        let after = tweens.value_with("a", 0.0, HOVER, EASE, at(base, 50), false);
        assert!((before - after).abs() < 1e-5, "{before} vs {after}");
        // The reversal runs a full duration from the reversal point.
        assert!(tweens.any_active(at(base, 50 + 119)));
        assert!(!tweens.any_active(at(base, 50 + 120)));
        assert_eq!(tweens.value_with("a", 0.0, HOVER, EASE, at(base, 400), false), 0.0);
    }

    #[test]
    fn reduced_motion_jumps_straight_to_the_target() {
        let base = Instant::now();
        let mut tweens = Tweens::<&str>::default();
        tweens.value_with("a", 0.0, HOVER, EASE, base, true);
        assert_eq!(tweens.value_with("a", 1.0, HOVER, EASE, base, true), 1.0);
        assert!(!tweens.any_active(base));
    }

    #[test]
    fn retain_and_clear_drop_tweens_and_a_cleared_key_is_first_sight_again() {
        let base = Instant::now();
        let mut tweens = Tweens::<&str>::default();
        for key in ["keep", "drop"] {
            tweens.value_with(key, 0.0, HOVER, EASE, base, false);
            tweens.value_with(key, 1.0, HOVER, EASE, base, false);
        }
        tweens.retain(|key| *key == "keep");
        assert_eq!(tweens.len(), 1);
        assert!(tweens.any_active(base));
        tweens.clear();
        assert!(tweens.is_empty() && !tweens.any_active(base));
        assert_eq!(tweens.value_with("keep", 0.0, HOVER, EASE, base, false), 0.0);
    }

    #[test]
    fn tween_is_inactive_when_equal_or_zero_length_and_clamped_after_the_end() {
        let base = Instant::now();
        assert!(!Tween::new(0.5, 0.5, base, HOVER, EASE).active(base));
        let instant = Tween::new(0.0, 1.0, base, Duration::ZERO, EASE);
        assert!(!instant.active(base));
        assert_eq!(instant.sample(base), 1.0);
        let tween = Tween::new(0.0, 1.0, base, HOVER, EASE);
        assert_eq!(tween.sample(base), 0.0);
        assert_eq!(tween.sample(at(base, 1000)), 1.0);
        assert_eq!(lerp(2.0, 4.0, 0.5), 3.0);
    }

    #[test]
    fn curves_hit_endpoints_and_stay_monotonic() {
        for curve in [EASE, EASE_OUT, EMPHASIZED_ENTER, STANDARD_EXIT] {
            assert_eq!(curve.sample(0.0), 0.0);
            assert_eq!(curve.sample(1.0), 1.0);
            let mut last = 0.0;
            for step in 1..=20 {
                let value = curve.sample(step as f32 / 20.0);
                assert!(value + 1e-4 >= last, "{curve:?} regressed at {step}");
                last = value;
            }
        }
    }

    #[test]
    fn css_ease_matches_reference_midpoint() {
        // Chromium: cubic-bezier(0.25, 0.1, 0.25, 1) at x=0.5 ≈ 0.8024.
        assert!((EASE.sample(0.5) - 0.8024).abs() < 0.002);
    }

    #[test]
    fn color_mix_takes_short_hue_path_and_keeps_hue_from_transparent() {
        let red = gpui::hsla(0.95, 1.0, 0.5, 1.0);
        let orange = gpui::hsla(0.05, 1.0, 0.5, 1.0);
        let mid = mix_hsla(red, orange, 0.5);
        assert!(mid.h < 0.01 || mid.h > 0.99, "hue {}", mid.h);

        let clear = gpui::hsla(0.0, 0.0, 0.0, 0.0);
        let blue = gpui::hsla(0.6, 0.8, 0.5, 1.0);
        let half = mix_hsla(clear, blue, 0.5);
        assert!((half.h - 0.6).abs() < 1e-5);
        assert!((half.a - 0.5).abs() < 1e-5);
    }

    #[test]
    fn transparent_fades_only_change_opacity_in_both_directions() {
        for visible in [
            gpui::hsla(0.6, 0.8, 0.5, 1.0),
            gpui::hsla(0.1, 0.3, 0.98, 1.0),
            gpui::hsla(0.9, 0.6, 0.7, 0.4),
        ] {
            let expected: gpui::Rgba = visible.into();
            for clear in [gpui::transparent_black(), gpui::hsla(0.2, 0.9, 0.1, 0.0)] {
                for progress in [0.0, 0.1, 0.25, 0.5, 0.75, 1.0] {
                    let fade_in: gpui::Rgba = mix_hsla(clear, visible, progress).into();
                    let fade_out: gpui::Rgba = mix_hsla(visible, clear, progress).into();
                    for actual in [fade_in, fade_out] {
                        for (channel, expected) in [
                            (actual.r, expected.r),
                            (actual.g, expected.g),
                            (actual.b, expected.b),
                        ] {
                            assert!((channel - expected).abs() < 1e-5);
                        }
                    }
                    assert!((fade_in.a - expected.a * progress).abs() < 1e-5);
                    assert!((fade_out.a - expected.a * (1.0 - progress)).abs() < 1e-5);
                }
            }
        }
    }

    #[test]
    fn light_panel_hover_does_not_flash_darker_than_its_background() {
        let tokens = crate::preview::DesignTokens::from_json(crate::preview::TOKENS_JSON).unwrap();
        let light = &tokens.themes.light;
        let background: gpui::Rgba = crate::view::color(&light.background).into();
        let hover = crate::view::color(&light.hover);
        let endpoint: gpui::Rgba = hover.into();
        // A panel row is transparent at rest. Every intermediate frame must
        // stay between the page background and the settled hover color.
        for progress in [0.0, 0.1, 0.25, 0.5, 0.75, 0.9, 1.0] {
            for faded in [
                mix_hsla(gpui::transparent_black(), hover, progress),
                mix_hsla(hover, gpui::transparent_black(), progress),
            ] {
                let faded: gpui::Rgba = faded.into();
                for (page, color, end) in [
                    (background.r, faded.r, endpoint.r),
                    (background.g, faded.g, endpoint.g),
                    (background.b, faded.b, endpoint.b),
                ] {
                    let composited = color * faded.a + page * (1.0 - faded.a);
                    assert!(composited >= page.min(end) - 1e-5, "dark flash at {progress}");
                    assert!(composited <= page.max(end) + 1e-5);
                }
            }
        }
    }
}
