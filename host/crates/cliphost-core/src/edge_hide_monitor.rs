//! Per-window screen geometry. All coordinates are in the same screen-pixel space.

use crate::edge_hide::{self, Rect};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct MonitorArea {
    pub bounds: Rect,
    pub work: Rect,
}

pub(crate) struct WindowEdgeGeometry {
    window: Rect,
    outer: Rect,
    monitor: MonitorArea,
    working_areas: Vec<Rect>,
    snap_tolerance: f64,
}

impl WindowEdgeGeometry {
    #[cfg(test)]
    pub(crate) fn new(window: Rect, monitors: &[MonitorArea]) -> Option<Self> {
        Self::with_visible_frame(window, window, monitors, 1.0)
    }

    fn with_visible_frame(outer: Rect, window: Rect, monitors: &[MonitorArea], scale: f64) -> Option<Self> {
        let monitor = *select_monitor(window, monitors)?;
        Some(Self {
            window,
            outer,
            monitor,
            working_areas: monitors.iter().map(|monitor| monitor.work).collect(),
            snap_tolerance: edge_hide::SNAP_EDGE_TOLERANCE_DIP * scale,
        })
    }

    pub(crate) fn can_tuck(&self) -> bool {
        edge_hide::window_can_tuck_with_tolerance(
            self.window,
            self.monitor.work,
            self.monitor.bounds,
            &self.working_areas,
            self.snap_tolerance,
        )
    }

    pub(crate) fn tuck_position(&self) -> Option<(f64, f64)> {
        if !self.can_tuck() {
            return None;
        }
        let taskbar = edge_hide::determine_taskbar_type(self.monitor.bounds, self.monitor.work);
        let hide = edge_hide::determine_hide_type_with_tolerance(
            self.window.extend(edge_hide::AREA_EXT),
            self.monitor.work,
            taskbar,
            &self.working_areas,
            self.snap_tolerance,
        );
        self.outer_position(edge_hide::tuck_position(self.window, self.monitor.work, hide))
    }

    pub(crate) fn restore_position(&self) -> Option<(f64, f64)> {
        self.outer_position(edge_hide::show_restore_position(
            self.window, self.monitor.work, &self.working_areas))
    }

    // Win32/Tauri moves the outer HWND. Translate the desired visible-frame
    // position back to that origin so the exposed pixel is actually on screen.
    fn outer_position(&self, position: Option<(f64, f64)>) -> Option<(f64, f64)> {
        position.map(|(x, y)| (x + self.outer.left - self.window.left,
            y + self.outer.top - self.window.top))
    }
}

// Screen.FromHandle / MonitorFromRect select the largest intersection, then the
// nearest monitor when a window is off screen (including a one-pixel edge tuck).
fn select_monitor(window: Rect, monitors: &[MonitorArea]) -> Option<&MonitorArea> {
    fn intersection(a: Rect, b: Rect) -> f64 {
        (a.right().min(b.right()) - a.left.max(b.left)).max(0.0)
            * (a.bottom().min(b.bottom()) - a.top.max(b.top)).max(0.0)
    }
    fn distance(a: Rect, b: Rect) -> f64 {
        let dx = (a.left - b.right()).max(b.left - a.right()).max(0.0);
        let dy = (a.top - b.bottom()).max(b.top - a.bottom()).max(0.0);
        dx * dx + dy * dy
    }
    monitors.iter().max_by(|a, b| {
        intersection(window, a.bounds)
            .total_cmp(&intersection(window, b.bounds))
            .then_with(|| distance(window, b.bounds).total_cmp(&distance(window, a.bounds)))
    })
}

pub(crate) fn window_geometry(hwnd: isize) -> Option<WindowEdgeGeometry> {
    let outer = edge_hide::window_rect(hwnd)?;
    let visible = edge_hide::visible_window_rect(hwnd).unwrap_or(outer);
    #[cfg(windows)]
    let scale = {
        use windows::Win32::{Foundation::HWND, UI::HiDpi::GetDpiForWindow};
        let dpi = unsafe { GetDpiForWindow(HWND(hwnd as *mut _)) };
        if dpi == 0 { 1.0 } else { dpi as f64 / 96.0 }
    };
    #[cfg(not(windows))]
    let scale = 1.0;
    WindowEdgeGeometry::with_visible_frame(outer, visible, &system_monitors(), scale)
}

#[cfg(windows)]
pub(crate) fn system_monitors() -> Vec<MonitorArea> {
    use std::mem::size_of;
    use windows::Win32::Foundation::{BOOL, LPARAM, RECT};
    use windows::Win32::Graphics::Gdi::{
        EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO,
    };

    fn rect(rc: RECT) -> Rect {
        Rect {
            left: rc.left as f64,
            top: rc.top as f64,
            width: (rc.right - rc.left) as f64,
            height: (rc.bottom - rc.top) as f64,
        }
    }
    unsafe extern "system" fn callback(
        monitor: HMONITOR,
        _hdc: HDC,
        _rc: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        let out = &mut *(data.0 as *mut Vec<MonitorArea>);
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(monitor, &mut info).as_bool() {
            out.push(MonitorArea {
                bounds: rect(info.rcMonitor),
                work: rect(info.rcWork),
            });
        }
        BOOL(1)
    }
    let mut monitors = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            HDC::default(),
            None,
            Some(callback),
            LPARAM(&mut monitors as *mut _ as isize),
        );
    }
    monitors
}

#[cfg(not(windows))]
pub(crate) fn system_monitors() -> Vec<MonitorArea> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(left: f64, top: f64, width: f64, height: f64) -> Rect {
        Rect {
            left,
            top,
            width,
            height,
        }
    }
    fn screen(bounds: Rect) -> MonitorArea {
        MonitorArea {
            bounds,
            work: bounds,
        }
    }

    fn edge_windows(area: Rect, gap: f64) -> [(edge_hide::HideType, Rect); 4] {
        use edge_hide::HideType::{Left, Top, Right, Bottom};
        [
            (Left, rect(area.left + gap, area.top + 100.0, 420.0, 500.0)),
            (Top, rect(area.left + 600.0, area.top + gap, 420.0, 500.0)),
            (Right, rect(area.right() - 420.0 - gap, area.top + 100.0, 420.0, 500.0)),
            (Bottom, rect(area.left + 600.0, area.bottom() - 500.0 - gap, 420.0, 500.0)),
        ]
    }

    #[test]
    fn snap_band_scales_with_dpi_and_accepts_shadow_contact_and_overshoot() {
        let monitor = screen(rect(0.0, 0.0, 1920.0, 1080.0));
        for scale in [1.0, 1.25, 2.0] {
            // Content is still inside the screen when its 15 DIP shadow touches.
            let limit = 20.0 * scale + 3.0;
            for (gap, accepted) in [(0.0, true), (15.0 * scale, true),
                (limit, true), (limit + 1.0, false), (-2.0, true), (-40.0, true)] {
                for (edge, window) in edge_windows(monitor.work, gap) {
                    let geometry = WindowEdgeGeometry::with_visible_frame(
                        window, window, &[monitor], scale).unwrap();
                    assert_eq!(geometry.can_tuck(), accepted, "{edge:?}, {scale}x, gap={gap}");
                    assert_eq!(geometry.tuck_position().is_some(), accepted,
                        "policy and target must agree: {edge:?}, {scale}x, gap={gap}");
                }
            }
        }
    }

    #[test]
    fn widened_snap_band_keeps_taskbar_and_monitor_junctions_excluded() {
        use edge_hide::HideType::{Left, Top, Right, Bottom};
        let bounds = rect(0.0, 0.0, 1920.0, 1080.0);
        for (edge, work) in [
            (Left, rect(40.0, 0.0, 1880.0, 1080.0)),
            (Top, rect(0.0, 40.0, 1920.0, 1040.0)),
            (Right, rect(0.0, 0.0, 1880.0, 1080.0)),
            (Bottom, rect(0.0, 0.0, 1920.0, 1040.0)),
        ] {
            for gap in [19.0, -2.0] {
                let window = edge_windows(work, gap).into_iter().find(|(side, _)| *side == edge).unwrap().1;
                let geometry = WindowEdgeGeometry::with_visible_frame(
                    window, window, &[MonitorArea { bounds, work }], 1.25).unwrap();
                assert!(!geometry.can_tuck(), "taskbar {edge:?}, gap={gap}");
                assert_eq!(geometry.tuck_position(), None);
            }
        }
        let main = screen(bounds);
        for (edge, neighbor) in [
            (Left, screen(rect(-1920.0, 0.0, 1920.0, 1080.0))),
            (Top, screen(rect(0.0, -1080.0, 1920.0, 1080.0))),
            (Right, screen(rect(1920.0, 0.0, 1920.0, 1080.0))),
            (Bottom, screen(rect(0.0, 1080.0, 1920.0, 1080.0))),
        ] {
            for gap in [19.0, -2.0] {
                let window = edge_windows(bounds, gap).into_iter().find(|(side, _)| *side == edge).unwrap().1;
                let geometry = WindowEdgeGeometry::with_visible_frame(
                    window, window, &[main, neighbor], 1.25).unwrap();
                assert!(!geometry.can_tuck(), "monitor junction {edge:?}, gap={gap}");
                assert_eq!(geometry.tuck_position(), None);
            }
        }
    }

    #[test]
    fn edge_tuck_and_restore_use_the_windows_monitor_in_either_enumeration_order() {
        let left = screen(rect(0.0, 0.0, 1920.0, 1080.0));
        let right = screen(rect(1920.0, 0.0, 2560.0, 1440.0));
        for monitors in [[left, right], [right, left]] {
            let window = rect(4060.0, 100.0, 420.0, 760.0);
            let geometry = WindowEdgeGeometry::new(window, &monitors).unwrap();
            assert!(geometry.can_tuck(), "right-hand display's external edge");
            assert_eq!(geometry.tuck_position(), Some((4479.0, 100.0)));
            let tucked = rect(4479.0, 100.0, 420.0, 760.0);
            assert_eq!(
                WindowEdgeGeometry::new(tucked, &monitors)
                    .unwrap()
                    .restore_position(),
                Some((4060.0, 100.0))
            );
            let interior = rect(2400.0, 100.0, 420.0, 760.0);
            let geometry = WindowEdgeGeometry::new(interior, &monitors).unwrap();
            assert!(!geometry.can_tuck());
            assert_eq!(
                geometry.restore_position(),
                None,
                "do not pull normal windows onto another display"
            );
        }
    }

    #[test]
    fn invisible_resize_borders_leave_a_visible_pixel_on_every_external_edge() {
        let monitor = screen(rect(0.0, 0.0, 1920.0, 1080.0));
        // Asymmetric borders observed on the real 125% Tauri window.
        let (width, height) = (525.0, 650.0);
        for (outer_x, outer_y, tucked_x, tucked_y, restore_x, restore_y) in [
            (0.0, 80.0, -516.0, 80.0, -8.0, 80.0),
            (80.0, 0.0, 80.0, -641.0, 80.0, 0.0),
            (1395.0, 80.0, 1911.0, 80.0, 1403.0, 80.0),
            (80.0, 430.0, 80.0, 1079.0, 80.0, 438.0),
        ] {
            let outer = rect(outer_x, outer_y, width, height);
            let frame = rect(outer_x+8.0, outer_y, width-16.0, height-8.0);
            let geometry = WindowEdgeGeometry::with_visible_frame(outer, frame, &[monitor], 1.25).unwrap();
            assert!(geometry.can_tuck());
            assert_eq!(geometry.tuck_position(), Some((tucked_x, tucked_y)));
            let outer = rect(tucked_x, tucked_y, width, height);
            let frame = rect(tucked_x+8.0, tucked_y, width-16.0, height-8.0);
            let visible_width = (frame.right().min(1920.0)-frame.left.max(0.0)).max(0.0);
            let visible_height = (frame.bottom().min(1080.0)-frame.top.max(0.0)).max(0.0);
            assert_eq!(visible_width.min(visible_height), edge_hide::DISPLAY_WIDTH);
            assert_eq!(WindowEdgeGeometry::with_visible_frame(outer, frame, &[monitor], 1.25).unwrap()
                .restore_position(), Some((restore_x, restore_y)));
        }
    }

    #[test]
    fn negative_coordinate_monitor_restores_to_its_own_edge() {
        let monitors = [
            screen(rect(0.0, 0.0, 1920.0, 1080.0)),
            screen(rect(-1920.0, -200.0, 1920.0, 1080.0)),
        ];
        let window = rect(-1920.0, 0.0, 420.0, 600.0);
        assert_eq!(
            WindowEdgeGeometry::new(window, &monitors)
                .unwrap()
                .tuck_position(),
            Some((-2339.0, 0.0))
        );
        let tucked = rect(-2339.0, 0.0, 420.0, 600.0);
        assert_eq!(
            WindowEdgeGeometry::new(tucked, &monitors)
                .unwrap()
                .restore_position(),
            Some((-1920.0, 0.0))
        );
    }

    #[test]
    fn taskbar_side_is_excluded_from_automatic_tuck() {
        let bounds = rect(0.0, 0.0, 1920.0, 1080.0);
        for (work, window) in [
            (
                rect(0.0, 0.0, 1920.0, 1040.0),
                rect(600.0, 540.0, 420.0, 500.0),
            ),
            (
                rect(0.0, 40.0, 1920.0, 1040.0),
                rect(600.0, 40.0, 420.0, 500.0),
            ),
            (
                rect(40.0, 0.0, 1880.0, 1080.0),
                rect(40.0, 100.0, 420.0, 500.0),
            ),
            (
                rect(0.0, 0.0, 1880.0, 1080.0),
                rect(1460.0, 100.0, 420.0, 500.0),
            ),
        ] {
            let geometry =
                WindowEdgeGeometry::new(window, &[MonitorArea { bounds, work }]).unwrap();
            assert!(!geometry.can_tuck(), "reserved desktop space: {work:?}");
            assert_eq!(geometry.tuck_position(), None);
        }
    }

    #[test]
    fn screen_junction_and_spanning_windows_are_not_external_edges() {
        let left = screen(rect(0.0, 0.0, 1920.0, 1080.0));
        let right = screen(rect(1920.0, 0.0, 1920.0, 1080.0));
        for monitors in [[left, right], [right, left]] {
            for window in [
                rect(1500.0, 100.0, 420.0, 600.0),
                rect(1920.0, 100.0, 420.0, 600.0),
                rect(1820.0, 100.0, 420.0, 600.0),
            ] {
                let geometry = WindowEdgeGeometry::new(window, &monitors).unwrap();
                assert!(!geometry.can_tuck());
                assert_eq!(geometry.tuck_position(), None);
                assert_eq!(geometry.restore_position(), None);
            }
            let spanning = rect(1820.0, 100.0, 420.0, 600.0);
            assert_eq!(select_monitor(spanning, &monitors), Some(&right));
        }
    }

    #[test]
    fn missing_display_restores_to_nearest_remaining_screen() {
        let monitors = [
            screen(rect(0.0, 0.0, 1920.0, 1080.0)),
            screen(rect(1920.0, 0.0, 2560.0, 1440.0)),
        ];
        let offscreen = rect(6000.0, 100.0, 420.0, 760.0);
        let geometry = WindowEdgeGeometry::new(offscreen, &monitors).unwrap();
        assert!(
            !geometry.can_tuck(),
            "entirely disconnected coordinates cannot auto-tuck"
        );
        assert_eq!(geometry.restore_position(), Some((4060.0, 100.0)));
        let left = rect(-5000.0, 100.0, 420.0, 760.0);
        assert_eq!(
            WindowEdgeGeometry::new(left, &monitors)
                .unwrap()
                .restore_position(),
            Some((0.0, 100.0))
        );
        assert!(WindowEdgeGeometry::new(offscreen, &[]).is_none());
    }

    #[test]
    fn vertical_display_tucks_and_restores_in_physical_coordinates() {
        let monitors = [
            screen(rect(0.0, 0.0, 1920.0, 1080.0)),
            screen(rect(0.0, -2160.0, 3840.0, 2160.0)),
        ];
        let window = rect(500.0, -2160.0, 630.0, 900.0);
        assert_eq!(
            WindowEdgeGeometry::new(window, &monitors)
                .unwrap()
                .tuck_position(),
            Some((500.0, -3059.0))
        );
        let tucked = rect(500.0, -3059.0, 630.0, 900.0);
        assert_eq!(
            WindowEdgeGeometry::new(tucked, &monitors)
                .unwrap()
                .restore_position(),
            Some((500.0, -2160.0))
        );
    }

    #[cfg(windows)]
    #[test]
    fn native_hidden_windows_match_windows_monitor_selection_and_host_geometry() {
        use windows::core::w;
        use windows::Win32::Foundation::HWND;
        use windows::Win32::Graphics::Gdi::{
            GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
        };
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::HiDpi::{
            SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT,
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        };
        use windows::Win32::UI::WindowsAndMessaging::*;

        struct DpiGuard(DPI_AWARENESS_CONTEXT);
        impl Drop for DpiGuard {
            fn drop(&mut self) {
                unsafe {
                    SetThreadDpiAwarenessContext(self.0);
                }
            }
        }
        struct WindowGuard(HWND);
        impl Drop for WindowGuard {
            fn drop(&mut self) {
                unsafe {
                    let _ = DestroyWindow(self.0);
                }
            }
        }
        crate::clipboard::force_headless(true);
        let dir = tempfile::tempdir().unwrap();
        let host = crate::HostState::open(
            crate::AppPaths::from_data_root(dir.path().to_path_buf()),
            28965,
        )
        .unwrap();
        unsafe {
            let previous = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            assert!(!previous.0.is_null());
            let _dpi = DpiGuard(previous);
            let monitors = system_monitors();
            assert!(!monitors.is_empty());
            let mut checked = 0;
            for area in &monitors {
                let width = 420.0_f64.min(area.work.width);
                let height = 500.0_f64.min(area.work.height);
                for (x, y) in [
                    (
                        area.work.right() - width,
                        area.work.top + (area.work.height - height) / 2.0,
                    ),
                    (
                        area.work.left + (area.work.width - width) / 2.0,
                        area.work.bottom() - height,
                    ),
                ] {
                    let hwnd = CreateWindowExW(
                        WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                        w!("STATIC"),
                        w!("ClipHost hidden monitor geometry probe"),
                        WS_POPUP,
                        x as i32,
                        y as i32,
                        width as i32,
                        height as i32,
                        None,
                        None,
                        GetModuleHandleW(None).unwrap(),
                        None,
                    )
                    .unwrap();
                    let _window = WindowGuard(hwnd);
                    assert!(!IsWindowVisible(hwnd).as_bool());
                    assert_ne!(GetForegroundWindow(), hwnd);
                    let mut info = MONITORINFO {
                        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                        ..Default::default()
                    };
                    assert!(GetMonitorInfoW(
                        MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST),
                        &mut info
                    )
                    .as_bool());
                    let geometry = window_geometry(hwnd.0 as isize).unwrap();
                    assert_eq!(
                        geometry.monitor.bounds,
                        rect(
                            info.rcMonitor.left as f64,
                            info.rcMonitor.top as f64,
                            (info.rcMonitor.right - info.rcMonitor.left) as f64,
                            (info.rcMonitor.bottom - info.rcMonitor.top) as f64
                        )
                    );
                    assert_eq!(
                        geometry.monitor.work,
                        rect(
                            info.rcWork.left as f64,
                            info.rcWork.top as f64,
                            (info.rcWork.right - info.rcWork.left) as f64,
                            (info.rcWork.bottom - info.rcWork.top) as f64
                        )
                    );
                    host.set_host_hwnd(hwnd.0 as isize);
                    assert_eq!(host.host_can_tuck(), geometry.can_tuck());
                    assert_eq!(host.edge_tuck_position(), geometry.tuck_position());
                    assert_eq!(
                        host.edge_show_restore_position(),
                        geometry.restore_position()
                    );
                    host.set_host_hwnd(0);
                    assert_ne!(GetForegroundWindow(), hwnd);
                    checked += 1;
                }
            }
            eprintln!("Native geometry: {} monitors, {checked} hidden HWNDs; no window shown or desktop input sent", monitors.len());
        }
    }
}
