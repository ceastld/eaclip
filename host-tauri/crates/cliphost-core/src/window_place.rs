//! C# `ShowWindowLocation` / `WindowHelper.ShowLocParser` placement.

use crate::edge_hide::Rect;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShowWindowLocation {
    Na,
    TopLeft,
    TopCenter,
    TopRight,
    LeftCenter,
    CenterScreen,
    RightCenter,
    BottomLeft,
    BottomCenter,
    BottomRight,
    MouseAround,
    MouseRightBottom,
    InScreen,
}

impl ShowWindowLocation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Na => "NA",
            Self::TopLeft => "TopLeft",
            Self::TopCenter => "TopCenter",
            Self::TopRight => "TopRight",
            Self::LeftCenter => "LeftCenter",
            Self::CenterScreen => "CenterScreen",
            Self::RightCenter => "RightCenter",
            Self::BottomLeft => "BottomLeft",
            Self::BottomCenter => "BottomCenter",
            Self::BottomRight => "BottomRight",
            Self::MouseAround => "MouseAround",
            Self::MouseRightBottom => "MouseRightBottom",
            Self::InScreen => "InScreen",
        }
    }

    pub fn parse(value: &str) -> Self {
        parse_loc(value).unwrap_or(Self::MouseRightBottom)
    }

    pub fn parse_cli(value: &str) -> Option<Self> {
        parse_loc(value)
    }

    fn from_grid(loc: i32) -> Self {
        match loc {
            1 => Self::TopLeft,
            2 => Self::TopCenter,
            3 => Self::TopRight,
            4 => Self::LeftCenter,
            5 => Self::CenterScreen,
            6 => Self::RightCenter,
            7 => Self::BottomLeft,
            8 => Self::BottomCenter,
            9 => Self::BottomRight,
            _ => Self::CenterScreen,
        }
    }
}

/// C# `WindowHelper.ShowLocParser`: `m1`/`m2`, names, numbers, or `[lrtbc]{1,2}`.
pub fn parse_loc(raw: &str) -> Option<ShowWindowLocation> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    match trimmed {
        "m1" | "10" | "MouseAround" => return Some(ShowWindowLocation::MouseAround),
        "m2" | "11" | "MouseRightBottom" => return Some(ShowWindowLocation::MouseRightBottom),
        "0" | "NA" | "Na" | "na" => return Some(ShowWindowLocation::Na),
        "5" | "CenterScreen" | "c" => return Some(ShowWindowLocation::CenterScreen),
        "1" | "TopLeft" => return Some(ShowWindowLocation::TopLeft),
        "2" | "TopCenter" => return Some(ShowWindowLocation::TopCenter),
        "3" | "TopRight" => return Some(ShowWindowLocation::TopRight),
        "4" | "LeftCenter" => return Some(ShowWindowLocation::LeftCenter),
        "6" | "RightCenter" => return Some(ShowWindowLocation::RightCenter),
        "7" | "BottomLeft" => return Some(ShowWindowLocation::BottomLeft),
        "8" | "BottomCenter" => return Some(ShowWindowLocation::BottomCenter),
        "9" | "BottomRight" => return Some(ShowWindowLocation::BottomRight),
        "12" | "InScreen" => return Some(ShowWindowLocation::InScreen),
        _ => {}
    }
    // C# `Enum.TryParse<ShowWindowLocation>(..., ignoreCase: true)` from settings form keys.
    match trimmed.to_ascii_lowercase().as_str() {
        "mousearound" => return Some(ShowWindowLocation::MouseAround),
        "mouserightbottom" => return Some(ShowWindowLocation::MouseRightBottom),
        "na" => return Some(ShowWindowLocation::Na),
        "centerscreen" => return Some(ShowWindowLocation::CenterScreen),
        "topleft" => return Some(ShowWindowLocation::TopLeft),
        "topcenter" => return Some(ShowWindowLocation::TopCenter),
        "topright" => return Some(ShowWindowLocation::TopRight),
        "leftcenter" => return Some(ShowWindowLocation::LeftCenter),
        "rightcenter" => return Some(ShowWindowLocation::RightCenter),
        "bottomleft" => return Some(ShowWindowLocation::BottomLeft),
        "bottomcenter" => return Some(ShowWindowLocation::BottomCenter),
        "bottomright" => return Some(ShowWindowLocation::BottomRight),
        "inscreen" => return Some(ShowWindowLocation::InScreen),
        _ => {}
    }
    if !trimmed.chars().all(|c| matches!(c, 'l' | 'r' | 't' | 'b' | 'c')) || trimmed.len() > 2 {
        return None;
    }
    let mut loc = 5;
    for c in trimmed.chars() {
        loc += match c {
            'l' => -1,
            'r' => 1,
            't' => -3,
            'b' => 3,
            _ => 0,
        };
    }
    loc = loc.clamp(1, 9);
    Some(ShowWindowLocation::from_grid(loc))
}

pub fn compute(
    location: ShowWindowLocation,
    mouse: (i32, i32),
    work: Rect,
    width: i32,
    height: i32,
) -> Option<(i32, i32)> {
    let (left, top) = match location {
        ShowWindowLocation::Na | ShowWindowLocation::InScreen => return None,
        ShowWindowLocation::TopLeft => (work.left as i32, work.top as i32),
        ShowWindowLocation::TopCenter => (
            (work.left + (work.width - width as f64) / 2.0).round() as i32,
            work.top as i32,
        ),
        ShowWindowLocation::TopRight => ((work.right() - width as f64).round() as i32, work.top as i32),
        ShowWindowLocation::LeftCenter => (
            work.left as i32,
            (work.top + (work.height - height as f64) / 2.0).round() as i32,
        ),
        ShowWindowLocation::CenterScreen => (
            (work.left + (work.width - width as f64) / 2.0).round() as i32,
            (work.top + (work.height - height as f64) / 2.0).round() as i32,
        ),
        ShowWindowLocation::RightCenter => (
            (work.right() - width as f64).round() as i32,
            (work.top + (work.height - height as f64) / 2.0).round() as i32,
        ),
        ShowWindowLocation::BottomLeft => (
            work.left as i32,
            (work.bottom() - height as f64).round() as i32,
        ),
        ShowWindowLocation::BottomCenter => (
            (work.left + (work.width - width as f64) / 2.0).round() as i32,
            (work.bottom() - height as f64).round() as i32,
        ),
        ShowWindowLocation::BottomRight => (
            (work.right() - width as f64).round() as i32,
            (work.bottom() - height as f64).round() as i32,
        ),
        ShowWindowLocation::MouseAround => (mouse.0 - width / 2, mouse.1 - height / 2),
        ShowWindowLocation::MouseRightBottom => (mouse.0, mouse.1),
    };
    Some(clamp(left, top, work, width, height))
}

pub fn clamp(left: i32, top: i32, work: Rect, width: i32, height: i32) -> (i32, i32) {
    let max_left = (work.right() - width as f64).floor() as i32;
    let max_top = (work.bottom() - height as f64).floor() as i32;
    (
        left.clamp(work.left as i32, max_left.max(work.left as i32)),
        top.clamp(work.top as i32, max_top.max(work.top as i32)),
    )
}

/// Monitor work area + effective DPI scales (dpi/96) at a physical screen point.
/// C# `ClipHostOverlayWorkAreaHelper.ResolveOverlayWorkArea` + `DpiScale`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverlayWorkArea {
    pub work: Rect,
    pub dpi_scale_x: f64,
    pub dpi_scale_y: f64,
}

pub fn overlay_work_area_from_point(x: i32, y: i32) -> Option<OverlayWorkArea> {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::Foundation::POINT;
        use windows::Win32::Graphics::Gdi::{
            GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
        };
        use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
        let pt = POINT { x, y };
        let monitor = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        GetMonitorInfoW(monitor, &mut info).ok().ok()?;
        let work = info.rcWork;
        let mut dpi_x = 0u32;
        let mut dpi_y = 0u32;
        let (dpi_scale_x, dpi_scale_y) =
            if GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y).is_ok()
                && dpi_x > 0
            {
                let sx = dpi_x as f64 / 96.0;
                let sy = if dpi_y > 0 { dpi_y as f64 / 96.0 } else { sx };
                (sx, sy)
            } else {
                (1.0, 1.0)
            };
        Some(OverlayWorkArea {
            work: Rect {
                left: work.left as f64,
                top: work.top as f64,
                width: (work.right - work.left) as f64,
                height: (work.bottom - work.top) as f64,
            },
            dpi_scale_x,
            dpi_scale_y,
        })
    }
    #[cfg(not(windows))]
    {
        let _ = (x, y);
        None
    }
}

pub fn work_area_from_point(x: i32, y: i32) -> Option<(Rect, f64)> {
    overlay_work_area_from_point(x, y).map(|area| (area.work, area.dpi_scale_x))
}

pub fn cursor_pos() -> Option<(i32, i32)> {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::Foundation::POINT;
        use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
        let mut pt = POINT::default();
        GetCursorPos(&mut pt).ok()?;
        Some((pt.x, pt.y))
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// C# `DpiUtil.GetTextScaleFactor`: HKCU Accessibility `TextScaleFactor` percent (100–225).
pub fn text_scale_factor_from_percent(percent: i32) -> f64 {
    if percent < 100 {
        1.0
    } else {
        (percent as f64 / 100.0).min(2.25)
    }
}

pub fn apply_text_scale(dpi_x: f64, dpi_y: f64, text_scale: f64) -> (f64, f64) {
    if text_scale > 1.0001 {
        (dpi_x * text_scale, dpi_y * text_scale)
    } else {
        (dpi_x, dpi_y)
    }
}

pub fn text_scale_factor() -> f64 {
    #[cfg(windows)]
    {
        text_scale_factor_from_percent(read_text_scale_percent().unwrap_or(100))
    }
    #[cfg(not(windows))]
    {
        1.0
    }
}

#[cfg(windows)]
fn read_text_scale_percent() -> Option<i32> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY_CURRENT_USER, KEY_READ, REG_DWORD,
        REG_SZ,
    };
    unsafe {
        let sub: Vec<u16> = "Software\\Microsoft\\Accessibility"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let name: Vec<u16> = "TextScaleFactor"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mut key = Default::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(sub.as_ptr()), 0, KEY_READ, &mut key).0
            != ERROR_SUCCESS.0
        {
            return None;
        }
        let mut kind = REG_DWORD;
        let mut dword = 0u32;
        let mut size = 4u32;
        let dword_status = RegQueryValueExW(
            key,
            PCWSTR(name.as_ptr()),
            None,
            Some(&mut kind),
            Some((&mut dword as *mut u32).cast()),
            Some(&mut size),
        );
        if dword_status.0 == ERROR_SUCCESS.0 {
            let _ = RegCloseKey(key);
            return Some(dword as i32);
        }
        kind = REG_SZ;
        size = 0;
        let _ = RegQueryValueExW(
            key,
            PCWSTR(name.as_ptr()),
            None,
            Some(&mut kind),
            None,
            Some(&mut size),
        );
        if size == 0 {
            let _ = RegCloseKey(key);
            return None;
        }
        let mut buf = vec![0u8; size as usize];
        let status = RegQueryValueExW(
            key,
            PCWSTR(name.as_ptr()),
            None,
            Some(&mut kind),
            Some(buf.as_mut_ptr()),
            Some(&mut size),
        );
        let _ = RegCloseKey(key);
        if status.0 != ERROR_SUCCESS.0 {
            return None;
        }
        let wide = std::slice::from_raw_parts(buf.as_ptr() as *const u16, (size as usize) / 2);
        let text = String::from_utf16_lossy(wide).trim_end_matches('\0').to_string();
        text.parse().ok()
    }
}

pub fn cursor_and_work_area() -> Option<((i32, i32), Rect)> {
    let (x, y) = cursor_pos()?;
    let (work, _) = work_area_from_point(x, y)?;
    Some(((x, y), work))
}

pub fn window_screen_rect(hwnd: isize) -> Option<Rect> {
    if hwnd == 0 {
        return None;
    }
    #[cfg(windows)]
    unsafe {
        use windows::Win32::Foundation::{HWND, RECT};
        use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;
        let mut rc = RECT::default();
        GetWindowRect(HWND(hwnd as *mut _), &mut rc).ok()?;
        Some(Rect {
            left: rc.left as f64,
            top: rc.top as f64,
            width: (rc.right - rc.left) as f64,
            height: (rc.bottom - rc.top) as f64,
        })
    }
    #[cfg(not(windows))]
    {
        let _ = hwnd;
        None
    }
}

pub fn place_for_cursor(location: ShowWindowLocation, width: i32, height: i32) -> Option<(i32, i32)> {
    let (mouse, work) = cursor_and_work_area()?;
    compute(location, mouse, work, width, height)
}

/// Resolve logical client dimensions against the cursor's destination monitor,
/// rather than the window's previous monitor scale.
pub fn place_for_cursor_logical(location: ShowWindowLocation, width: f64, height: f64) -> Option<(i32, i32)> {
    let mouse = cursor_pos()?;
    let area = overlay_work_area_from_point(mouse.0, mouse.1)?;
    compute_for_logical_size(location, mouse, area, width, height)
}

pub fn compute_for_logical_size(
    location: ShowWindowLocation,
    mouse: (i32, i32),
    area: OverlayWorkArea,
    width: f64,
    height: f64,
) -> Option<(i32, i32)> {
    let width = width * area.dpi_scale_x;
    let height = height * area.dpi_scale_y;
    if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
        return None;
    }
    compute(location, mouse, area.work, width.round() as i32, height.round() as i32)
}

/// C# `ClipHostQuickPasteOverlayWebViewWindow` bounded HWND layout (DIP constants).
pub const QP_PANEL_WIDTH_DIP: f64 = 380.0;
pub const QP_PREVIEW_WIDTH_DIP: f64 = 560.0;
pub const QP_CLUSTER_GAP_DIP: f64 = 10.0;
pub const QP_EDGE_SAFE_MARGIN_DIP: f64 = 14.0;
pub const QP_WINDOW_CLUSTER_MARGIN_DIP: f64 = 16.0;
pub const QP_FIXED_OVERLAY_HEIGHT_DIP: f64 = 560.0;
pub const QP_ITEM_HEIGHT_DIP: f64 = 48.0;
pub const QP_CHROME_PADDING_DIP: f64 = 32.0;
/// Frontend `CURSOR_GAP_PX`: the list sits this far from the pointer.
pub const QP_CURSOR_GAP_DIP: f64 = 12.0;
/// Narrowest preview lane kept when the side beside the list is cramped.
pub const QP_MIN_PREVIEW_WIDTH_DIP: f64 = 240.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QuickPasteOverlayPlacement {
    pub window_left: i32,
    pub window_top: i32,
    pub window_width: i32,
    pub window_height: i32,
    pub work: Rect,
    pub panel_width_px: i32,
    pub panel_height_px: i32,
    pub preview_width_px: i32,
    pub cluster_gap_px: i32,
    pub cluster_offset_left: i32,
    pub cluster_offset_top: i32,
    pub preview_on_left: bool,
    pub cursor_x: i32,
    pub cursor_y: i32,
    pub dpi_scale_x: f64,
    pub dpi_scale_y: f64,
}

fn ceil_px(dip: f64, scale: f64) -> i32 {
    (dip * scale).ceil() as i32
}

fn qp_min_overlay_width_dip(item_count: usize) -> f64 {
    let inner = if item_count > 0 {
        QP_PANEL_WIDTH_DIP + QP_CLUSTER_GAP_DIP + QP_PREVIEW_WIDTH_DIP
    } else {
        QP_PANEL_WIDTH_DIP
    };
    inner + QP_WINDOW_CLUSTER_MARGIN_DIP * 2.0
}

fn qp_overlay_origin(
    location: ShowWindowLocation,
    has_anchor: bool,
    cursor: (i32, i32),
    window_width: i32,
    window_height: i32,
    work: Rect,
) -> (i32, i32) {
    let area_left = work.left as i32;
    let area_top = work.top as i32;
    let area_width = work.width.max(0.0) as i32;
    let area_height = work.height.max(0.0) as i32;
    if !has_anchor || location == ShowWindowLocation::CenterScreen {
        return (
            area_left + (area_width - window_width).max(0) / 2,
            area_top + (area_height - window_height).max(0) / 2,
        );
    }
    if location == ShowWindowLocation::MouseAround {
        return (cursor.0 - window_width / 2, cursor.1 - window_height / 2);
    }
    (cursor.0, cursor.1)
}

fn clamp_qp_overlay_window(
    left: i32,
    top: i32,
    width: i32,
    height: i32,
    work: Rect,
    edge_x: i32,
    edge_y: i32,
) -> (i32, i32, i32, i32) {
    let area_left = work.left as i32;
    let area_top = work.top as i32;
    let area_right = (work.left + work.width) as i32;
    let area_bottom = (work.top + work.height) as i32;
    let min_left = area_left + edge_x;
    let min_top = area_top + edge_y;
    let width = width.min((area_right - area_left).max(1));
    let height = height.min((area_bottom - area_top).max(1));
    let max_left = min_left.max(area_right - width - edge_x);
    let max_top = min_top.max(area_bottom - height - edge_y);
    (
        left.clamp(min_left, max_left),
        top.clamp(min_top, max_top),
        width,
        height,
    )
}

/// C# `ComputePanelPlacement` + `ClampBoundedOverlayWindow`.
pub fn compute_quick_paste_overlay(
    item_count: usize,
    location: ShowWindowLocation,
    anchor: Option<(i32, i32)>,
    work: Rect,
    dpi_x: f64,
    dpi_y: f64,
) -> QuickPasteOverlayPlacement {
    let location = if location == ShowWindowLocation::Na {
        ShowWindowLocation::MouseRightBottom
    } else {
        location
    };
    let dpi_x = if dpi_x > 0.0 { dpi_x } else { 1.0 };
    let dpi_y = if dpi_y > 0.0 { dpi_y } else { 1.0 };
    let visible_rows = item_count.clamp(1, 8);
    let panel_height_dip = QP_CHROME_PADDING_DIP + visible_rows as f64 * QP_ITEM_HEIGHT_DIP;
    let panel_width_px = ceil_px(QP_PANEL_WIDTH_DIP, dpi_x);
    let panel_height_px = ceil_px(panel_height_dip, dpi_y);
    let preview_width_px = if item_count > 0 {
        ceil_px(QP_PREVIEW_WIDTH_DIP, dpi_x)
    } else {
        0
    };
    let cluster_gap_px = if item_count > 0 {
        ceil_px(QP_CLUSTER_GAP_DIP, dpi_x)
    } else {
        0
    };
    let edge_x = ceil_px(QP_EDGE_SAFE_MARGIN_DIP, dpi_x);
    let edge_y = ceil_px(QP_EDGE_SAFE_MARGIN_DIP, dpi_y);
    let margin_x = ceil_px(QP_WINDOW_CLUSTER_MARGIN_DIP, dpi_x);
    let margin_y = ceil_px(QP_WINDOW_CLUSTER_MARGIN_DIP, dpi_y);
    let window_height = ceil_px(QP_FIXED_OVERLAY_HEIGHT_DIP, dpi_y);
    let has_anchor = anchor.is_some();
    let cursor = anchor.unwrap_or((0, 0));
    let mut preview_width_px = preview_width_px;
    let mut preview_on_left = false;
    let (left, top, window_width) = if has_anchor && location != ShowWindowLocation::CenterScreen {
        // Frontend `computeListPlacement`: the list hugs the pointer and the
        // preview takes the roomier side, so the bounded HWND is built around
        // the list instead of putting its top-left corner at the pointer.
        let cursor_gap = ceil_px(QP_CURSOR_GAP_DIP, dpi_x);
        let area_left = work.left as i32;
        let area_right = (work.left + work.width) as i32;
        preview_on_left = crate::overlay_row_actions::preview_on_left(
            cursor.0 as f64,
            work.left,
            work.width,
            panel_width_px as f64,
            preview_width_px as f64,
            cluster_gap_px as f64,
            edge_x as f64,
            cursor_gap as f64,
        );
        let cursor_x = cursor.0 - area_left;
        let area_width = area_right - area_left;
        let mut list_left = if cursor_x * 2 >= area_width {
            cursor_x - panel_width_px - cursor_gap
        } else {
            cursor_x + cursor_gap
        };
        if list_left + panel_width_px > area_width - edge_x {
            list_left = cursor_x - panel_width_px - cursor_gap;
        }
        if list_left < edge_x {
            list_left = cursor_x + cursor_gap;
        }
        let list_left = area_left
            + list_left
                .min(area_width - panel_width_px - edge_x)
                .max(edge_x);
        if preview_width_px > 0 {
            let room = if preview_on_left {
                list_left - cluster_gap_px - area_left - edge_x - margin_x
            } else {
                area_right - edge_x - margin_x - (list_left + panel_width_px + cluster_gap_px)
            };
            preview_width_px = preview_width_px
                .min(room)
                .max(ceil_px(QP_MIN_PREVIEW_WIDTH_DIP, dpi_x));
        }
        let preview_lane = if preview_width_px > 0 {
            cluster_gap_px + preview_width_px
        } else {
            0
        };
        let left = list_left - margin_x - if preview_on_left { preview_lane } else { 0 };
        // The list never covers the pointer horizontally, so start its first
        // row at pointer height and let the work-area clamp lift it near the
        // bottom edge instead of flipping the whole column above the pointer.
        (
            left,
            cursor.1 - margin_y,
            panel_width_px + preview_lane + margin_x * 2,
        )
    } else {
        let cluster_width_px = panel_width_px + if preview_width_px > 0 {
            cluster_gap_px + preview_width_px
        } else {
            0
        };
        let min_width_px = ceil_px(qp_min_overlay_width_dip(item_count), dpi_x);
        let window_width = min_width_px.max(cluster_width_px + margin_x * 2);
        let (left, top) = qp_overlay_origin(
            location,
            has_anchor,
            cursor,
            window_width,
            window_height,
            work,
        );
        (left, top, window_width)
    };
    let (left, top, window_width, window_height) =
        clamp_qp_overlay_window(left, top, window_width, window_height, work, edge_x, edge_y);
    QuickPasteOverlayPlacement {
        window_left: left,
        window_top: top,
        window_width,
        window_height,
        work,
        panel_width_px,
        panel_height_px,
        preview_width_px,
        cluster_gap_px,
        cluster_offset_left: margin_x,
        cluster_offset_top: margin_y,
        preview_on_left,
        cursor_x: cursor.0,
        cursor_y: cursor.1,
        dpi_scale_x: dpi_x,
        dpi_scale_y: dpi_y,
    }
}

/// C# `ClipHostToastOverlayWebViewWindow` first-show HWND (lane + padding).
pub const TOAST_COLUMN_LANE_WIDTH_DIP: f64 = 420.0;
pub const TOAST_COLUMN_HORIZONTAL_PADDING_DIP: f64 = 16.0;
pub const TOAST_COLUMN_WIDTH_DIP: f64 =
    TOAST_COLUMN_LANE_WIDTH_DIP + TOAST_COLUMN_HORIZONTAL_PADDING_DIP * 2.0;
pub const TOAST_STRIP_HEIGHT_DIP: f64 = 140.0;
pub const TOAST_EDGE_MARGIN_DIP: f64 = 14.0;
pub const TOAST_CONTENT_PAD_DIP: f64 = 8.0;
pub const TOAST_CONTENT_BOUNDS_MIN_DELTA_PX: i32 = 8;
/// C# `ClipHostWebWindowBridge.PushToast` default / clamp.
pub const TOAST_DEFAULT_DURATION_MS: i32 = 3200;
pub const TOAST_MIN_DURATION_MS: i32 = 1200;
pub const TOAST_MAX_DURATION_MS: i32 = 15000;
/// C# `ClipHostToastOverlayWebViewWindow.BottomToastExitFallbackMs`.
pub const BOTTOM_TOAST_EXIT_FALLBACK_MS: i32 = 400;

/// C# `ScheduleIdleCollapseFallback`: clamp(duration ?? 3200, 1200, 15000) + 400ms.
pub fn toast_idle_collapse_delay_ms(duration_ms: Option<i32>) -> u64 {
    let clamped = duration_ms
        .unwrap_or(TOAST_DEFAULT_DURATION_MS)
        .clamp(TOAST_MIN_DURATION_MS, TOAST_MAX_DURATION_MS);
    (clamped + BOTTOM_TOAST_EXIT_FALLBACK_MS) as u64
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastPlacement {
    BottomCenter,
    TopRight,
    BottomRight,
}

pub fn parse_toast_placement(raw: &str) -> ToastPlacement {
    match raw.replace(['_', '-'], "").to_ascii_lowercase().as_str() {
        "topright" => ToastPlacement::TopRight,
        "bottomright" => ToastPlacement::BottomRight,
        _ => ToastPlacement::BottomCenter,
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ToastOverlayPlacement {
    pub window_left: i32,
    pub window_top: i32,
    pub window_width: i32,
    pub window_height: i32,
    pub work: Rect,
    pub placement: ToastPlacement,
    pub dpi_scale_x: f64,
    pub dpi_scale_y: f64,
}

fn toast_origin(
    placement: ToastPlacement,
    work: Rect,
    window_width: i32,
    window_height: i32,
    edge_x: i32,
    edge_y: i32,
) -> (i32, i32) {
    let area_left = work.left as i32;
    let area_top = work.top as i32;
    let area_right = (work.left + work.width) as i32;
    let area_bottom = (work.top + work.height) as i32;
    let centered_left = area_left + (area_right - area_left - window_width).max(0) / 2;
    let right_left = area_right - edge_x - window_width;
    let bottom_top = area_bottom - edge_y - window_height;
    let top_top = area_top + edge_y;
    match placement {
        ToastPlacement::TopRight => (right_left, top_top),
        ToastPlacement::BottomRight => (right_left, bottom_top),
        ToastPlacement::BottomCenter => (centered_left, bottom_top),
    }
}

fn clamp_toast_window(
    left: i32,
    top: i32,
    width: i32,
    height: i32,
    work: Rect,
) -> (i32, i32, i32, i32) {
    let area_left = work.left as i32;
    let area_top = work.top as i32;
    let area_right = (work.left + work.width) as i32;
    let area_bottom = (work.top + work.height) as i32;
    let width = width.min((area_right - area_left).max(1)).max(1);
    let height = height.min((area_bottom - area_top).max(1)).max(1);
    let max_left = area_left.max(area_right - width);
    let max_top = area_top.max(area_bottom - height);
    (
        left.clamp(area_left, max_left),
        top.clamp(area_top, max_top),
        width,
        height,
    )
}

/// C# `ApplyBoundedToastWindow` first show (strip height, optional preserve).
pub fn compute_toast_overlay(
    placement: ToastPlacement,
    work: Rect,
    dpi_x: f64,
    dpi_y: f64,
    height_px: Option<i32>,
) -> ToastOverlayPlacement {
    let dpi_x = if dpi_x > 0.0 { dpi_x } else { 1.0 };
    let dpi_y = if dpi_y > 0.0 { dpi_y } else { 1.0 };
    let strip = ceil_px(TOAST_STRIP_HEIGHT_DIP, dpi_y);
    let mut width = ceil_px(TOAST_COLUMN_WIDTH_DIP, dpi_x);
    let mut height = match height_px.filter(|h| *h > 0) {
        Some(existing) => existing.max(strip),
        None => strip,
    };
    let edge_x = ceil_px(TOAST_EDGE_MARGIN_DIP, dpi_x);
    let edge_y = ceil_px(TOAST_EDGE_MARGIN_DIP, dpi_y);
    width = width.min((work.width as i32).max(1));
    height = height.min((work.height as i32).max(1));
    let (left, top) = toast_origin(placement, work, width, height, edge_x, edge_y);
    let (left, top, width, height) = clamp_toast_window(left, top, width, height, work);
    ToastOverlayPlacement {
        window_left: left,
        window_top: top,
        window_width: width,
        window_height: height,
        work,
        placement,
        dpi_scale_x: dpi_x,
        dpi_scale_y: dpi_y,
    }
}

/// A copy arriving while the stack is visible keeps the window's current
/// logical size and only re-anchors it. Resetting a fitted (GPUI) window to
/// the legacy column for one frame made every card jump sideways and reflow.
pub fn reanchor_toast_overlay(
    previous: ToastOverlayPlacement,
    placement: ToastPlacement,
    work: Rect,
    dpi_x: f64,
    dpi_y: f64,
) -> ToastOverlayPlacement {
    let fresh = compute_toast_overlay(placement, work, dpi_x, dpi_y, None);
    let rescale = |px: i32, from: f64, to: f64| {
        if !from.is_finite() || from <= 0.0 || (from - to).abs() < 1e-6 {
            px
        } else {
            // Tolerate float noise so 100% -> 150% -> 100% cannot creep.
            (f64::from(px) / from * to - 1e-6).ceil() as i32
        }
    };
    let width = rescale(previous.window_width, previous.dpi_scale_x, fresh.dpi_scale_x).max(1);
    let height = rescale(previous.window_height, previous.dpi_scale_y, fresh.dpi_scale_y).max(1);
    let edge_x = ceil_px(TOAST_EDGE_MARGIN_DIP, fresh.dpi_scale_x);
    let edge_y = ceil_px(TOAST_EDGE_MARGIN_DIP, fresh.dpi_scale_y);
    let (left, top) = toast_origin(placement, work, width, height, edge_x, edge_y);
    let (left, top, width, height) = clamp_toast_window(left, top, width, height, work);
    ToastOverlayPlacement {
        window_left: left,
        window_top: top,
        window_width: width,
        window_height: height,
        ..fresh
    }
}

/// C# `ApplyContentBoundsFromWeb`: grow-only height, keep column width.
pub fn grow_toast_overlay_height(
    current: ToastOverlayPlacement,
    height_css: f64,
    device_pixel_ratio: f64,
) -> ToastOverlayPlacement {
    let scale_x = if device_pixel_ratio > 0.1 {
        device_pixel_ratio
    } else {
        current.dpi_scale_x
    };
    let scale_y = if device_pixel_ratio > 0.1 {
        device_pixel_ratio
    } else {
        current.dpi_scale_y
    };
    let pad_y = ceil_px(TOAST_CONTENT_PAD_DIP, scale_y);
    let edge_y = ceil_px(TOAST_EDGE_MARGIN_DIP, scale_y);
    let edge_x = ceil_px(TOAST_EDGE_MARGIN_DIP, scale_x);
    let content_height = (height_css * scale_y).ceil() as i32;
    let measured = (content_height.max(1) + pad_y).max(1);
    let max_height = ((current.work.height as i32) - edge_y).max(1);
    let measured = measured.clamp(1, max_height);
    let height = if current.window_height > 0 {
        current.window_height.max(measured)
    } else {
        measured
    };
    let width = current.window_width.max(1);
    let (left, top) = toast_origin(current.placement, current.work, width, height, edge_x, edge_y);
    let (left, top, width, height) = clamp_toast_window(left, top, width, height, current.work);
    ToastOverlayPlacement {
        window_left: left,
        window_top: top,
        window_width: width,
        window_height: height,
        work: current.work,
        placement: current.placement,
        dpi_scale_x: current.dpi_scale_x,
        dpi_scale_y: current.dpi_scale_y,
    }
}

/// GPUI bounds include their own padding; fit both dimensions and keep the
/// configured screen anchor when cards enter or leave the stack.
pub fn fit_toast_overlay_bounds(
    current: ToastOverlayPlacement,
    width_dip: f64,
    height_dip: f64,
    device_pixel_ratio: f64,
) -> ToastOverlayPlacement {
    if !width_dip.is_finite() || width_dip <= 0.0 || !height_dip.is_finite() || height_dip <= 0.0 {
        return current;
    }
    let valid_scale = |scale: f64| scale.is_finite() && scale > 0.0;
    let scale = |fallback: f64| {
        if valid_scale(device_pixel_ratio) {
            device_pixel_ratio
        } else if valid_scale(fallback) {
            fallback
        } else {
            1.0
        }
    };
    let scale_x = scale(current.dpi_scale_x);
    let scale_y = scale(current.dpi_scale_y);
    let edge_x = ceil_px(TOAST_EDGE_MARGIN_DIP, scale_x);
    let edge_y = ceil_px(TOAST_EDGE_MARGIN_DIP, scale_y);
    let max_width = (current.work.width as i32)
        .saturating_sub(edge_x.saturating_mul(2))
        .max(1);
    let max_height = (current.work.height as i32)
        .saturating_sub(edge_y.saturating_mul(2))
        .max(1);
    let width = ceil_px(width_dip, scale_x).clamp(1, max_width);
    let height = ceil_px(height_dip, scale_y).clamp(1, max_height);
    let (left, top) = toast_origin(
        current.placement,
        current.work,
        width,
        height,
        edge_x,
        edge_y,
    );
    let (left, top, width, height) = clamp_toast_window(left, top, width, height, current.work);
    ToastOverlayPlacement {
        window_left: left,
        window_top: top,
        window_width: width,
        window_height: height,
        dpi_scale_x: scale_x,
        dpi_scale_y: scale_y,
        ..current
    }
}

pub fn toast_overlay_delta_ignorable(a: ToastOverlayPlacement, b: ToastOverlayPlacement) -> bool {
    (a.window_left - b.window_left).abs() < TOAST_CONTENT_BOUNDS_MIN_DELTA_PX
        && (a.window_top - b.window_top).abs() < TOAST_CONTENT_BOUNDS_MIN_DELTA_PX
        && (a.window_width - b.window_width).abs() < TOAST_CONTENT_BOUNDS_MIN_DELTA_PX
        && (a.window_height - b.window_height).abs() < TOAST_CONTENT_BOUNDS_MIN_DELTA_PX
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_alignment_logical_panel_placement_uses_destination_dpi() {
        let area = OverlayWorkArea {
            work: Rect { left: -1920.0, top: 0.0, width: 1920.0, height: 1080.0 },
            dpi_scale_x: 1.5,
            dpi_scale_y: 1.5,
        };
        for location in [ShowWindowLocation::MouseRightBottom, ShowWindowLocation::BottomRight] {
            // The existing HWND can come from a 100% monitor; placement only
            // uses the destination's physical work area and 150% scale.
            let (x, y) = compute_for_logical_size(location, (-10, 1060), area, 420.0, 520.0).unwrap();
            assert_eq!((x, y), (-630, 300));
            assert_eq!(f64::from(x) + 420.0 * area.dpi_scale_x, area.work.right());
            assert_eq!(f64::from(y) + 520.0 * area.dpi_scale_y, area.work.bottom());
        }
    }

    fn work() -> Rect {
        Rect {
            left: 0.0,
            top: 0.0,
            width: 1920.0,
            height: 1080.0,
        }
    }

    #[test]
    fn mouse_right_bottom_uses_cursor_as_top_left() {
        let pos = compute(ShowWindowLocation::MouseRightBottom, (100, 80), work(), 420, 760).unwrap();
        assert_eq!(pos, (100, 80));
    }

    #[test]
    fn clamps_to_work_area() {
        let pos = compute(ShowWindowLocation::MouseRightBottom, (1800, 900), work(), 420, 760).unwrap();
        assert_eq!(pos, (1500, 320));
    }

    #[test]
    fn na_keeps_current_position() {
        assert_eq!(compute(ShowWindowLocation::Na, (10, 10), work(), 100, 100), None);
    }

    #[test]
    fn loc_parser_matches_csharp_tokens() {
        assert_eq!(parse_loc(""), None);
        assert_eq!(parse_loc("m1"), Some(ShowWindowLocation::MouseAround));
        assert_eq!(parse_loc("m2"), Some(ShowWindowLocation::MouseRightBottom));
        assert_eq!(parse_loc("lt"), Some(ShowWindowLocation::TopLeft));
        assert_eq!(parse_loc("rb"), Some(ShowWindowLocation::BottomRight));
        assert_eq!(parse_loc("c"), Some(ShowWindowLocation::CenterScreen));
        assert_eq!(
            parse_loc("mouserightbottom"),
            Some(ShowWindowLocation::MouseRightBottom)
        );
        assert_eq!(parse_loc("CenterScreen"), Some(ShowWindowLocation::CenterScreen));
        assert_eq!(parse_loc("centerscreen"), Some(ShowWindowLocation::CenterScreen));
    }

    #[test]
    fn quick_paste_overlay_mouse_right_bottom_matches_csharp() {
        let place = compute_quick_paste_overlay(
            5,
            ShowWindowLocation::Na,
            Some((100, 80)),
            work(),
            1.0,
            1.0,
        );
        assert_eq!(place.window_width, 982);
        assert_eq!(place.window_height, 560);
        // List starts 12px right of the pointer; its first row is at pointer height.
        assert_eq!(place.window_left, 96);
        assert_eq!(place.window_top, 64);
        assert!(!place.preview_on_left);
        assert_eq!(place.preview_width_px, 560);
        assert_eq!(place.cluster_offset_left, 16);
        let far = compute_quick_paste_overlay(
            5,
            ShowWindowLocation::MouseRightBottom,
            Some((1800, 1000)),
            work(),
            1.0,
            1.0,
        );
        // Right half: list ends 12px left of the pointer, preview on its left.
        assert_eq!(far.window_left, 1800 - 12 - 380 - 16 - 570);
        assert_eq!(far.window_top, 506);
        assert!(far.preview_on_left);
        let cramped = compute_quick_paste_overlay(
            5,
            ShowWindowLocation::MouseRightBottom,
            Some((500, 80)),
            Rect {
                left: 0.0,
                top: 0.0,
                width: 1000.0,
                height: 1080.0,
            },
            1.0,
            1.0,
        );
        // Neither side fits 560px: the preview lane shrinks, the list stays put.
        assert_eq!(cramped.window_left, 500 - 12 - 380 - 16);
        assert_eq!(cramped.preview_width_px, 1000 - 14 - 16 - (108 + 380 + 10));
        assert_eq!(cramped.window_width, 380 + 10 + 472 + 32);
        let empty = compute_quick_paste_overlay(
            0,
            ShowWindowLocation::MouseRightBottom,
            Some((100, 80)),
            work(),
            1.0,
            1.0,
        );
        assert_eq!(empty.preview_width_px, 0);
        assert_eq!(empty.window_width, 412);
        let center = compute_quick_paste_overlay(
            3,
            ShowWindowLocation::CenterScreen,
            Some((100, 80)),
            work(),
            1.0,
            1.0,
        );
        assert_eq!(center.window_left, (1920 - 982) / 2);
        assert_eq!(center.window_top, (1080 - 560) / 2);
    }

    #[test]
    fn text_scale_factor_matches_csharp_percent_clamp() {
        assert_eq!(text_scale_factor_from_percent(80), 1.0);
        assert_eq!(text_scale_factor_from_percent(100), 1.0);
        assert_eq!(text_scale_factor_from_percent(150), 1.5);
        assert_eq!(text_scale_factor_from_percent(300), 2.25);
        assert_eq!(apply_text_scale(1.25, 1.25, 1.0), (1.25, 1.25));
        assert_eq!(apply_text_scale(1.0, 1.0, 1.5), (1.5, 1.5));
        let scaled = compute_quick_paste_overlay(
            5,
            ShowWindowLocation::MouseRightBottom,
            Some((100, 80)),
            work(),
            1.5,
            1.5,
        );
        assert_eq!(scaled.panel_width_px, 570);
        assert_eq!(scaled.window_height, 840);
    }

    #[test]
    fn toast_overlay_bottom_center_matches_csharp() {
        let place = compute_toast_overlay(ToastPlacement::BottomCenter, work(), 1.0, 1.0, None);
        assert_eq!(place.window_width, 452);
        assert_eq!(place.window_height, 140);
        assert_eq!(place.window_left, (1920 - 452) / 2);
        assert_eq!(place.window_top, 1080 - 14 - 140);
        let top_right = compute_toast_overlay(ToastPlacement::TopRight, work(), 1.0, 1.0, None);
        assert_eq!(top_right.window_left, 1920 - 14 - 452);
        assert_eq!(top_right.window_top, 14);
        let grown = grow_toast_overlay_height(place, 200.0, 1.0);
        assert_eq!(grown.window_height, 208);
        assert_eq!(grown.window_width, 452);
        assert_eq!(grow_toast_overlay_height(grown, 80.0, 1.0), grown);
        assert!(toast_overlay_delta_ignorable(place, place));
        assert!(!toast_overlay_delta_ignorable(place, grown));
        assert_eq!(toast_idle_collapse_delay_ms(None), 3600);
        assert_eq!(toast_idle_collapse_delay_ms(Some(2500)), 2900);
        assert_eq!(toast_idle_collapse_delay_ms(Some(100)), 1600);
        assert_eq!(toast_idle_collapse_delay_ms(Some(20_000)), 15_400);
    }

    #[test]
    fn toast_reanchor_keeps_the_fitted_size_instead_of_the_legacy_column() {
        let first = compute_toast_overlay(ToastPlacement::BottomRight, work(), 1.0, 1.0, None);
        // GPUI fit: 384 x 104 DIP, anchored bottom-right.
        let fitted = fit_toast_overlay_bounds(first, 384.0, 104.0, 1.0);
        assert_eq!((fitted.window_width, fitted.window_height), (384, 104));
        let next = reanchor_toast_overlay(fitted, ToastPlacement::BottomRight, work(), 1.0, 1.0);
        assert_eq!(next, fitted);
        // WebView keeps its grown 452 column exactly as before.
        let grown = grow_toast_overlay_height(first, 200.0, 1.0);
        assert_eq!(
            reanchor_toast_overlay(grown, ToastPlacement::BottomRight, work(), 1.0, 1.0),
            grown
        );
    }

    #[test]
    fn toast_reanchor_rescales_between_monitors_without_creeping() {
        let first = compute_toast_overlay(ToastPlacement::BottomCenter, work(), 1.0, 1.0, None);
        let fitted = fit_toast_overlay_bounds(first, 384.0, 104.0, 1.0);
        let other = Rect { left: 1920.0, top: 0.0, width: 2880.0, height: 1620.0 };
        let scaled = reanchor_toast_overlay(fitted, ToastPlacement::BottomCenter, other, 1.5, 1.5);
        assert_eq!((scaled.window_width, scaled.window_height), (576, 156));
        assert_eq!(scaled.window_left, 1920 + (2880 - 576) / 2);
        assert_eq!(scaled.window_top, 1620 - 21 - 156);
        assert_eq!(scaled.dpi_scale_x, 1.5);
        let back = reanchor_toast_overlay(scaled, ToastPlacement::BottomCenter, work(), 1.0, 1.0);
        assert_eq!(back, fitted);
        let top_right = reanchor_toast_overlay(fitted, ToastPlacement::TopRight, work(), 1.0, 1.0);
        assert_eq!((top_right.window_left, top_right.window_top), (1920 - 14 - 384, 14));
    }

    #[test]
    fn toast_fit_shrinks_and_grows_without_legacy_padding_or_minimum() {
        let initial = compute_toast_overlay(ToastPlacement::BottomRight, work(), 1.0, 1.0, None);
        let tall = fit_toast_overlay_bounds(initial, 384.0, 420.0, 1.0);
        assert_eq!((tall.window_width, tall.window_height), (384, 420));
        let compact = fit_toast_overlay_bounds(tall, 384.0, 82.0, 1.0);
        assert_eq!((compact.window_width, compact.window_height), (384, 82));
        assert_eq!(compact.window_left + compact.window_width, 1920 - 14);
        assert_eq!(compact.window_top + compact.window_height, 1080 - 14);
        assert_eq!(fit_toast_overlay_bounds(compact, 384.0, 82.0, 1.0), compact);
    }

    #[test]
    fn toast_fit_preserves_anchors_and_scales_negative_monitor_bounds() {
        let area = Rect {
            left: -1920.0,
            top: -100.0,
            width: 1920.0,
            height: 1080.0,
        };
        let initial = compute_toast_overlay(ToastPlacement::TopRight, area, 1.5, 1.5, None);
        let top = fit_toast_overlay_bounds(initial, 384.0, 82.5, 1.5);
        assert_eq!((top.window_width, top.window_height), (576, 124));
        assert_eq!((top.window_left, top.window_top), (-597, -79));
        let center = fit_toast_overlay_bounds(
            ToastOverlayPlacement {
                placement: ToastPlacement::BottomCenter,
                ..initial
            },
            384.0,
            82.5,
            1.5,
        );
        assert_eq!((center.window_left, center.window_top), (-1248, 835));
        let bounded = fit_toast_overlay_bounds(initial, 10_000.0, 10_000.0, 1.5);
        assert_eq!((bounded.window_width, bounded.window_height), (1878, 1038));
        assert_eq!((bounded.window_left, bounded.window_top), (-1899, -79));
    }

    #[test]
    fn toast_fit_rejects_invalid_measurements_and_falls_back_to_monitor_dpi() {
        let initial = compute_toast_overlay(ToastPlacement::BottomRight, work(), 1.25, 1.5, None);
        for (width, height) in [
            (f64::NAN, 80.0),
            (384.0, f64::INFINITY),
            (0.0, 80.0),
            (384.0, -1.0),
        ] {
            assert_eq!(
                fit_toast_overlay_bounds(initial, width, height, 1.0),
                initial
            );
        }
        for dpr in [f64::NAN, f64::INFINITY, 0.0, -1.0] {
            let fitted = fit_toast_overlay_bounds(initial, 384.0, 80.0, dpr);
            assert_eq!((fitted.window_width, fitted.window_height), (480, 120));
            assert_eq!((fitted.dpi_scale_x, fitted.dpi_scale_y), (1.25, 1.5));
        }
        let invalid_monitor = ToastOverlayPlacement {
            dpi_scale_x: f64::NAN,
            dpi_scale_y: f64::INFINITY,
            ..initial
        };
        let fitted = fit_toast_overlay_bounds(invalid_monitor, 384.0, 80.0, 0.0);
        assert_eq!((fitted.window_width, fitted.window_height), (384, 80));
        assert_eq!((fitted.dpi_scale_x, fitted.dpi_scale_y), (1.0, 1.0));
    }
}
