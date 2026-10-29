//! EdgeHide geometry aligned with C# `MonitorEdgeHelper` + `EdgeHideAutoDismissPolicy`.

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub left: f64,
    pub top: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub fn right(self) -> f64 {
        self.left + self.width
    }

    pub fn bottom(self) -> f64 {
        self.top + self.height
    }

    pub fn extend(self, thickness: f64) -> Self {
        Self {
            left: self.left - thickness,
            top: self.top - thickness,
            width: self.width + thickness * 2.0,
            height: self.height + thickness * 2.0,
        }
    }

    pub fn intersects(self, other: Self) -> bool {
        self.left < other.right()
            && self.right() > other.left
            && self.top < other.bottom()
            && self.bottom() > other.top
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HideType {
    None,
    Left,
    Top,
    Right,
    Bottom,
    OnTaskbar,
}

const EDGE_TOLERANCE: f64 = 1.0;
// Allow the native 15 DIP shadow to touch the screen before the content does.
// Real windows convert this independent snap tolerance to screen pixels.
pub(crate) const SNAP_EDGE_TOLERANCE_DIP: f64 = 20.0;
pub const AREA_EXT: f64 = 3.0;
pub const DISPLAY_WIDTH: f64 = 1.0;
pub const ANIMATION_MS: u64 = 150;
pub const ANIMATION_STEPS: usize = 15;

pub fn animation_frames(from: (f64, f64), to: (f64, f64)) -> Vec<(f64, f64)> {
    (0..=ANIMATION_STEPS)
        .map(|step| {
            let t = step as f64 / ANIMATION_STEPS as f64;
            let eased = 1.0 - (1.0 - t).powi(3);
            (
                from.0 + (to.0 - from.0) * eased,
                from.1 + (to.1 - from.1) * eased,
            )
        })
        .collect()
}

pub fn can_apply_edge_tuck(hide_type: HideType) -> bool {
    !matches!(hide_type, HideType::None | HideType::OnTaskbar)
}

/// C# `EdgeHideAutoDismissPolicy.ShouldFallBackToFullHide`.
pub fn should_fall_back_to_full_hide(can_apply_edge_tuck: bool) -> bool {
    !can_apply_edge_tuck
}

pub fn determine_hide_type(
    win_area: Rect,
    primary: Rect,
    taskbar: HideType,
    all_working_areas: &[Rect],
) -> HideType {
    determine_hide_type_with_tolerance(win_area, primary, taskbar, all_working_areas,
        SNAP_EDGE_TOLERANCE_DIP)
}

pub(crate) fn determine_hide_type_with_tolerance(
    win_area: Rect,
    primary: Rect,
    taskbar: HideType,
    all_working_areas: &[Rect],
    snap_tolerance: f64,
) -> HideType {
    let mut hide = HideType::None;
    if win_area.left < primary.left && is_external(primary, HideType::Left, all_working_areas) {
        hide = HideType::Left;
    }
    if win_area.bottom() > primary.bottom() && is_external(primary, HideType::Bottom, all_working_areas)
    {
        hide = HideType::Bottom;
    }
    if win_area.top < primary.top && is_external(primary, HideType::Top, all_working_areas) {
        if hide == HideType::None || win_area.height < win_area.width {
            hide = HideType::Top;
        }
    }
    if win_area.right() > primary.right() && is_external(primary, HideType::Right, all_working_areas) {
        if hide == HideType::None || win_area.height > win_area.width {
            hide = HideType::Right;
        }
    }
    if hide == HideType::None {
        hide = snapped_hide_type(win_area, primary, all_working_areas, snap_tolerance);
    }
    if hide != HideType::None && hide == taskbar {
        return HideType::OnTaskbar;
    }
    hide
}

pub fn determine_taskbar_type(bounds: Rect, working: Rect) -> HideType {
    if (bounds.left - working.left).abs() > EDGE_TOLERANCE {
        return HideType::Left;
    }
    if (bounds.top - working.top).abs() > EDGE_TOLERANCE {
        return HideType::Top;
    }
    if (bounds.right() - working.right()).abs() > EDGE_TOLERANCE {
        return HideType::Right;
    }
    if (bounds.bottom() - working.bottom()).abs() > EDGE_TOLERANCE {
        return HideType::Bottom;
    }
    HideType::None
}

pub fn tuck_position(window: Rect, working: Rect, hide_type: HideType) -> Option<(f64, f64)> {
    match hide_type {
        HideType::Left => Some((working.left + DISPLAY_WIDTH - window.width, window.top)),
        HideType::Top => Some((window.left, working.top + DISPLAY_WIDTH - window.height)),
        HideType::Right => Some((working.right() - DISPLAY_WIDTH, window.top)),
        HideType::Bottom => Some((window.left, working.bottom() - DISPLAY_WIDTH)),
        _ => None,
    }
}

/// C# `EdgeHider.ApplyShowAnimation` with default `AreaMargin` 0.
pub fn show_restore_position(window: Rect, working: Rect, all: &[Rect]) -> Option<(f64, f64)> {
    let win_area = window.extend(AREA_EXT);
    let mut left = window.left;
    let mut top = window.top;
    let mut moved = false;
    if win_area.left < working.left && is_external(working, HideType::Left, all) {
        left = working.left;
        moved = true;
    }
    if win_area.top < working.top && is_external(working, HideType::Top, all) {
        top = working.top;
        moved = true;
    }
    if win_area.right() > working.right() && is_external(working, HideType::Right, all) {
        left = working.right() - window.width;
        moved = true;
    }
    if win_area.bottom() > working.bottom() && is_external(working, HideType::Bottom, all) {
        top = working.bottom() - window.height;
        moved = true;
    }
    moved.then_some((left, top))
}

pub fn window_can_tuck(window: Rect, primary: Rect, bounds: Rect, all: &[Rect]) -> bool {
    window_can_tuck_with_tolerance(window, primary, bounds, all, SNAP_EDGE_TOLERANCE_DIP)
}

pub(crate) fn window_can_tuck_with_tolerance(
    window: Rect, primary: Rect, bounds: Rect, all: &[Rect], snap_tolerance: f64,
) -> bool {
    if !all.iter().any(|area| area.intersects(window.extend(AREA_EXT))) {
        return false;
    }
    let taskbar = determine_taskbar_type(bounds, primary);
    can_apply_edge_tuck(determine_hide_type_with_tolerance(
        window.extend(AREA_EXT), primary, taskbar, all, snap_tolerance))
}

fn snapped_hide_type(win: Rect, primary: Rect, all: &[Rect], tolerance: f64) -> HideType {
    let mut hide = HideType::None;
    if snapped(win.left, primary.left, tolerance) && is_external(primary, HideType::Left, all) {
        hide = HideType::Left;
    }
    if snapped(win.bottom(), primary.bottom(), tolerance) && is_external(primary, HideType::Bottom, all) {
        hide = HideType::Bottom;
    }
    if snapped(win.top, primary.top, tolerance) && is_external(primary, HideType::Top, all) {
        if hide == HideType::None || win.height < win.width {
            hide = HideType::Top;
        }
    }
    if snapped(win.right(), primary.right(), tolerance) && is_external(primary, HideType::Right, all) {
        if hide == HideType::None || win.height > win.width {
            hide = HideType::Right;
        }
    }
    hide
}

fn is_external(working: Rect, edge: HideType, all: &[Rect]) -> bool {
    for other in all {
        if rects_equal(working, *other) {
            continue;
        }
        let adjacent = match edge {
            HideType::Left => {
                adjacent(other.right(), working.left)
                    && ranges_overlap(other.top, other.bottom(), working.top, working.bottom())
            }
            HideType::Right => {
                adjacent(other.left, working.right())
                    && ranges_overlap(other.top, other.bottom(), working.top, working.bottom())
            }
            HideType::Top => {
                adjacent(other.bottom(), working.top)
                    && ranges_overlap(other.left, other.right(), working.left, working.right())
            }
            HideType::Bottom => {
                adjacent(other.top, working.bottom())
                    && ranges_overlap(other.left, other.right(), working.left, working.right())
            }
            _ => false,
        };
        if adjacent {
            return false;
        }
    }
    true
}

fn adjacent(a: f64, b: f64) -> bool {
    (a - b).abs() <= EDGE_TOLERANCE
}

fn snapped(a: f64, b: f64, tolerance: f64) -> bool {
    (a - b).abs() <= tolerance
}

fn ranges_overlap(a0: f64, a1: f64, b0: f64, b1: f64) -> bool {
    a0.max(b0) < a1.min(b1) + EDGE_TOLERANCE
}

fn rects_equal(a: Rect, b: Rect) -> bool {
    (a.left - b.left).abs() <= EDGE_TOLERANCE
        && (a.top - b.top).abs() <= EDGE_TOLERANCE
        && (a.width - b.width).abs() <= EDGE_TOLERANCE
        && (a.height - b.height).abs() <= EDGE_TOLERANCE
}

pub fn system_work_areas() -> Vec<Rect> {
    crate::edge_hide_monitor::system_monitors().into_iter().map(|monitor| monitor.work).collect()
}

#[cfg(windows)]
pub fn window_rect(hwnd: isize) -> Option<Rect> {
    use windows::Win32::Foundation::{HWND, RECT};
    use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;
    if hwnd == 0 {
        return None;
    }
    unsafe {
        let mut rc = RECT::default();
        GetWindowRect(HWND(hwnd as *mut _), &mut rc).ok()?;
        if rc.right <= rc.left || rc.bottom <= rc.top {
            return None;
        }
        Some(Rect {
            left: rc.left as f64,
            top: rc.top as f64,
            width: (rc.right - rc.left) as f64,
            height: (rc.bottom - rc.top) as f64,
        })
    }
}

/// DWM excludes the invisible resize borders included by GetWindowRect.
/// Tauri is per-monitor DPI aware, so both rectangles use physical screen pixels.
#[cfg(windows)]
pub(crate) fn visible_window_rect(hwnd: isize) -> Option<Rect> {
    use windows::Win32::Foundation::{HWND, RECT};
    use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
    if hwnd == 0 {
        return None;
    }
    unsafe {
        let mut rc = RECT::default();
        DwmGetWindowAttribute(HWND(hwnd as *mut _), DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut rc as *mut _ as *mut _, std::mem::size_of_val(&rc) as u32).ok()?;
        (rc.right > rc.left && rc.bottom > rc.top).then_some(Rect {
            left: rc.left as f64, top: rc.top as f64,
            width: (rc.right - rc.left) as f64, height: (rc.bottom - rc.top) as f64,
        })
    }
}

#[cfg(not(windows))]
pub(crate) fn visible_window_rect(_hwnd: isize) -> Option<Rect> {
    None
}

#[cfg(not(windows))]
pub fn window_rect(_hwnd: isize) -> Option<Rect> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapped_right_edge_tucks() {
        let work = Rect {
            left: 0.0,
            top: 0.0,
            width: 1920.0,
            height: 1080.0,
        };
        let win = Rect {
            left: 1912.0,
            top: 100.0,
            width: 420.0,
            height: 760.0,
        };
        let hide = determine_hide_type(win, work, HideType::None, &[work]);
        assert_eq!(hide, HideType::Right);
        assert!(can_apply_edge_tuck(hide));
        let pos = tuck_position(win, work, hide).unwrap();
        assert!((pos.0 - (1920.0 - 1.0)).abs() < f64::EPSILON);
        let restored = show_restore_position(
            Rect {
                left: pos.0,
                top: pos.1,
                width: win.width,
                height: win.height,
            },
            work,
            &[work],
        )
        .unwrap();
        assert!((restored.0 - (1920.0 - win.width)).abs() < 0.5, "{restored:?}");
        assert!((restored.1 - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn show_restore_slides_left_tuck_back_like_csharp() {
        let work = Rect {
            left: 0.0,
            top: 0.0,
            width: 1920.0,
            height: 1080.0,
        };
        let tucked = Rect {
            left: DISPLAY_WIDTH - 420.0,
            top: 100.0,
            width: 420.0,
            height: 760.0,
        };
        let pos = show_restore_position(tucked, work, &[work]).unwrap();
        assert!((pos.0 - work.left).abs() < f64::EPSILON);
        assert!((pos.1 - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn internal_monitor_junction_is_not_external() {
        let left = Rect {
            left: 0.0,
            top: 0.0,
            width: 1920.0,
            height: 1080.0,
        };
        let right = Rect {
            left: 1920.0,
            top: 0.0,
            width: 1920.0,
            height: 1080.0,
        };
        assert!(!is_external(left, HideType::Right, &[left, right]));
        assert!(is_external(left, HideType::Left, &[left, right]));
    }

    #[test]
    fn taskbar_edge_does_not_tuck() {
        let bounds = Rect {
            left: 0.0,
            top: 0.0,
            width: 1920.0,
            height: 1080.0,
        };
        let work = Rect {
            left: 0.0,
            top: 0.0,
            width: 1920.0,
            height: 1040.0,
        };
        assert_eq!(determine_taskbar_type(bounds, work), HideType::Bottom);
        let win = Rect {
            left: 10.0,
            top: 1035.0,
            width: 420.0,
            height: 760.0,
        };
        let hide = determine_hide_type(win.extend(AREA_EXT), work, HideType::Bottom, &[work]);
        assert!(!can_apply_edge_tuck(hide) || hide == HideType::OnTaskbar);
    }

    #[test]
    fn should_fall_back_to_full_hide_matches_csharp() {
        assert!(should_fall_back_to_full_hide(false));
        assert!(!should_fall_back_to_full_hide(true));
    }

    #[test]
    fn cubic_ease_out_reaches_target() {
        let frames = animation_frames((0.0, 0.0), (100.0, 40.0));
        assert_eq!(frames.first().copied(), Some((0.0, 0.0)));
        assert_eq!(frames.last().copied(), Some((100.0, 40.0)));
        assert!(frames[frames.len() / 2].0 > 50.0);
    }
}
