//! C# `WindowStateService` / `MyWindowState` JSON under `{dataRoot}/WindowState/{Name}.json`.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

pub const CLIP_WINDOW: &str = "clip_window_webview";
pub const PANEL_WINDOW: &str = "panel_window_webview";
pub const MAIN_WINDOW: &str = "main_host_window_webview";
pub const APP_WINDOW: &str = "app_host_window_webview";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub struct SavedWindowState {
    pub name: String,
    pub left: f64,
    pub top: f64,
    pub width: f64,
    pub height: f64,
    pub last_update_time: DateTime<Local>,
    #[serde(default)]
    pub monitor_left: bool,
    #[serde(default)]
    pub monitor_top: bool,
    #[serde(default)]
    pub monitor_width: bool,
    #[serde(default)]
    pub monitor_height: bool,
    #[serde(default)]
    pub properties: HashMap<String, String>,
    // Rust records the screen-pixel position as well as the C# logical fields.
    // Dividing a global coordinate by a display's DPI loses its display identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub physical_position: Option<(i32, i32)>,
    /// Clipboard size before folding; legacy records may contain only the live rectangle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub normal_size: Option<(f64, f64)>,
}

impl SavedWindowState {
    pub fn new(name: &str, left: f64, top: f64, width: f64, height: f64) -> Self {
        let (monitor_left, monitor_top, monitor_width, monitor_height) = monitor_flags(name);
        Self {
            name: name.to_string(),
            left,
            top,
            width,
            height,
            last_update_time: Local::now(),
            monitor_left,
            monitor_top,
            monitor_width,
            monitor_height,
            properties: HashMap::new(),
            physical_position: None,
            normal_size: None,
        }
    }
}

pub fn clipboard_normal_size(state: &SavedWindowState) -> Option<(f64, f64)> {
    let valid = |&(width, height): &(f64, f64)| {
        width.is_finite() && height.is_finite() && width >= 120.0 && height >= 80.0
    };
    state.normal_size.filter(valid)
        .or_else(|| Some((state.width, state.height)).filter(valid))
}

/// C# `Register` flags: clipboard/panel `WithLocation`; main `WithLocation`+`WithSize`.
pub fn monitor_flags(name: &str) -> (bool, bool, bool, bool) {
    match name {
        MAIN_WINDOW | APP_WINDOW => (true, true, true, true),
        CLIP_WINDOW | PANEL_WINDOW => (true, true, false, false),
        _ => (true, true, false, false),
    }
}

pub fn name_for_label(label: &str) -> Option<&'static str> {
    match label {
        "clipboard" => Some(CLIP_WINDOW),
        "panel" => Some(PANEL_WINDOW),
        "main" => Some(MAIN_WINDOW),
        _ => None,
    }
}

pub fn folder(data_root: &Path) -> PathBuf {
    data_root.join("WindowState")
}

fn file_path(data_root: &Path, name: &str) -> PathBuf {
    folder(data_root).join(format!("{name}.json"))
}

pub fn load(data_root: &Path, name: &str) -> Option<SavedWindowState> {
    let raw = fs::read_to_string(file_path(data_root, name)).ok()?;
    serde_json::from_str(&raw).ok()
}

pub fn save(data_root: &Path, state: &SavedWindowState) -> Result<(), String> {
    crate::config_file::save(&file_path(data_root, &state.name), state)
}

/// C# `ClampWindowRectToWorkArea` (`SystemParameters.WorkArea`, min 120×80).
pub fn clamp_to_work_area(
    left: f64,
    top: f64,
    width: f64,
    height: f64,
    work_left: f64,
    work_top: f64,
    work_width: f64,
    work_height: f64,
) -> (f64, f64, f64, f64) {
    let min_width = 120.0;
    let min_height = 80.0;
    let width = width.max(min_width).min(work_width.max(min_width));
    let height = height.max(min_height).min(work_height.max(min_height));
    let max_left = work_left + (work_width - width).max(0.0);
    let max_top = work_top + (work_height - height).max(0.0);
    let left = left.max(work_left).min(max_left);
    let top = top.max(work_top).min(max_top);
    (left, top, width, height)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowPosition {
    Logical(i32, i32),
    Physical(i32, i32),
}

pub type RestoredWindowRect = (Option<WindowPosition>, Option<(i32, i32)>);

/// Size stays logical; native positions are selected/clamped in screen-pixel space.
/// Old C#-shaped records retain their logical-position interpretation.
pub fn restored_rect(state: &SavedWindowState) -> RestoredWindowRect {
    restored_rect_with_lookup(state, crate::window_place::work_area_from_point)
}

/// Location-only windows keep their live client size. Their saved dimensions
/// may describe a folded title bar and must not determine the restored position.
pub fn restored_rect_with_client_size(
    state: &SavedWindowState,
    client_size: (f64, f64),
) -> RestoredWindowRect {
    restored_rect_with_client_size_and_lookup(state, Some(client_size), crate::window_place::work_area_from_point)
}

fn restored_rect_with_lookup(
    state: &SavedWindowState,
    lookup: impl FnOnce(i32, i32) -> Option<(crate::edge_hide::Rect, f64)>,
) -> RestoredWindowRect {
    restored_rect_with_client_size_and_lookup(state, None, lookup)
}

fn restored_rect_with_client_size_and_lookup(
    state: &SavedWindowState,
    client_size: Option<(f64, f64)>,
    lookup: impl FnOnce(i32, i32) -> Option<(crate::edge_hide::Rect, f64)>,
) -> RestoredWindowRect {
    let (mut left, mut top, mut width, mut height) =
        (state.left, state.top, state.width, state.height);
    if let Some((client_width, client_height)) = client_size {
        if !state.monitor_width && client_width.is_finite() && client_width > 0.0 {
            width = client_width;
        }
        if !state.monitor_height && client_height.is_finite() && client_height > 0.0 {
            height = client_height;
        }
    }
    let point = state
        .physical_position
        .unwrap_or((left.round() as i32, top.round() as i32));
    let physical = state.physical_position.is_some();
    let mut position_scale = 1.0;
    if physical {
        left = f64::from(point.0);
        top = f64::from(point.1);
    }
    if let Some((work, scale)) = lookup(point.0, point.1) {
        let scale = scale.max(0.1);
        if physical {
            position_scale = scale;
            left /= scale;
            top /= scale;
        }
        let (cl, ct, cw, ch) = clamp_to_work_area(
            left,
            top,
            width,
            height,
            work.left / scale,
            work.top / scale,
            work.width / scale,
            work.height / scale,
        );
        left = cl;
        top = ct;
        width = cw;
        height = ch;
    }
    let pos = if state.monitor_left || state.monitor_top {
        let (x, y) = (
            (left * position_scale).round() as i32,
            (top * position_scale).round() as i32,
        );
        Some(if physical {
            WindowPosition::Physical(x, y)
        } else {
            WindowPosition::Logical(x, y)
        })
    } else {
        None
    };
    let size = if state.monitor_width || state.monitor_height {
        Some((
            width.round().max(1.0) as i32,
            height.round().max(1.0) as i32,
        ))
    } else {
        None
    };
    (pos, size)
}

pub fn property_bool(state: &SavedWindowState, key: &str) -> Option<bool> {
    let raw = state.properties.get(key)?;
    let trimmed = raw.trim();
    match trimmed {
        "true" | "True" => Some(true),
        "false" | "False" => Some(false),
        other => serde_json::from_str(other).ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn roundtrip_matches_csharp_file_name_and_flags() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let mut state = SavedWindowState::new(PANEL_WINDOW, 40.0, 80.0, 420.0, 520.0);
        state.properties.insert("IsPinned".into(), "true".into());
        save(root, &state).unwrap();
        let loaded = load(root, PANEL_WINDOW).unwrap();
        assert_eq!(loaded.name, PANEL_WINDOW);
        assert_eq!(loaded.left, 40.0);
        assert_eq!(loaded.top, 80.0);
        assert_eq!(loaded.monitor_left, true);
        assert_eq!(loaded.monitor_width, false);
        assert_eq!(property_bool(&loaded, "IsPinned"), Some(true));
        let (pos, size) = restored_rect(&loaded);
        assert_eq!(pos, Some(WindowPosition::Logical(40, 80)));
        assert_eq!(size, None);
        assert_eq!(name_for_label("panel"), Some(PANEL_WINDOW));
        assert_eq!(name_for_label("clipboard"), Some(CLIP_WINDOW));
        let main = SavedWindowState::new(MAIN_WINDOW, 1.0, 2.0, 760.0, 580.0);
        let (_, main_size) = restored_rect(&main);
        assert_eq!(main_size, Some((760, 580)));
        let clamped = clamp_to_work_area(-80.0, -40.0, 2000.0, 40.0, 0.0, 0.0, 800.0, 600.0);
        assert_eq!(clamped, (0.0, 0.0, 800.0, 80.0));
    }

    fn work(left: f64, top: f64, width: f64, height: f64) -> crate::edge_hide::Rect {
        crate::edge_hide::Rect {
            left,
            top,
            width,
            height,
        }
    }

    #[test]
    fn clipboard_normal_size_prefers_saved_expanded_size_and_roundtrips() {
        let mut state = SavedWindowState::new(CLIP_WINDOW, 40.0, 80.0, 420.0, 36.0);
        state.normal_size = Some((480.0, 820.0));
        assert_eq!(clipboard_normal_size(&state), Some((480.0, 820.0)));
        let value = serde_json::to_value(&state).unwrap();
        assert_eq!(value["NormalSize"], serde_json::json!([480.0, 820.0]));
        let loaded: SavedWindowState = serde_json::from_value(value).unwrap();
        assert_eq!(clipboard_normal_size(&loaded), Some((480.0, 820.0)));
        assert!(!loaded.monitor_width && !loaded.monitor_height);
    }

    #[test]
    fn clipboard_normal_size_uses_legacy_expanded_dimensions_but_rejects_folded_height() {
        let state = SavedWindowState::new(CLIP_WINDOW, 40.0, 80.0, 420.0, 760.0);
        let value = serde_json::to_value(&state).unwrap();
        assert!(value.get("NormalSize").is_none());
        let mut legacy: SavedWindowState = serde_json::from_value(value).unwrap();
        assert_eq!(legacy.normal_size, None);
        assert_eq!(clipboard_normal_size(&legacy), Some((420.0, 760.0)));
        legacy.height = 36.0;
        assert_eq!(clipboard_normal_size(&legacy), None);
    }

    #[test]
    fn clipboard_normal_size_rejects_invalid_values_before_legacy_fallback() {
        let mut state = SavedWindowState::new(CLIP_WINDOW, 40.0, 80.0, 420.0, 760.0);
        for size in [(f64::NAN, 820.0), (480.0, f64::INFINITY), (119.0, 820.0), (480.0, 79.0)] {
            state.normal_size = Some(size);
            assert_eq!(clipboard_normal_size(&state), Some((420.0, 760.0)));
            state.height = 36.0;
            assert_eq!(clipboard_normal_size(&state), None);
            state.height = 760.0;
        }
        state.normal_size = Some((120.0, 80.0));
        assert_eq!(clipboard_normal_size(&state), Some((120.0, 80.0)));
        state.normal_size = None;
        state.width = f64::NAN;
        assert_eq!(clipboard_normal_size(&state), None);
    }

    #[test]
    fn live_client_restore_ignores_folded_height_at_bottom_of_screen() {
        let mut state = SavedWindowState::new(CLIP_WINDOW, 40.0, 1044.0, 420.0, 36.0);
        state.physical_position = Some((40, 1044));
        let restored = restored_rect_with_client_size_and_lookup(&state, Some((420.0, 760.0)), |_, _| {
            Some((work(0.0, 0.0, 1920.0, 1080.0), 1.0))
        });
        assert_eq!(restored, (Some(WindowPosition::Physical(40, 320)), None));
        assert_eq!(state.height, 36.0, "location restoration must not rewrite saved size policy");
        assert!(!state.monitor_width && !state.monitor_height);
    }

    #[test]
    fn live_client_restore_handles_legacy_panel_and_preserves_main_saved_size() {
        let panel = SavedWindowState::new(PANEL_WINDOW, -20.0, 700.0, 80.0, 36.0);
        let restored = restored_rect_with_client_size_and_lookup(&panel, Some((420.0, 520.0)), |_, _| {
            Some((work(-1920.0, 0.0, 1920.0, 1080.0), 1.5))
        });
        assert_eq!(restored, (Some(WindowPosition::Logical(-420, 200)), None));
        let mut main = SavedWindowState::new(MAIN_WINDOW, 1850.0, 1000.0, 760.0, 580.0);
        main.physical_position = Some((1850, 1000));
        let restored = restored_rect_with_client_size_and_lookup(&main, Some((420.0, 520.0)), |_, _| {
            Some((work(0.0, 0.0, 1920.0, 1080.0), 1.0))
        });
        assert_eq!(restored, (Some(WindowPosition::Physical(1160, 500)), Some((760, 580))));
    }

    #[test]
    fn scaled_secondary_display_uses_saved_physical_point_and_keeps_logical_size() {
        let mut state = SavedWindowState::new(MAIN_WINDOW, 2680.0 / 1.25, 80.0, 760.0, 580.0);
        state.physical_position = Some((2680, 100));
        let restored = restored_rect_with_lookup(&state, |x, y| {
            assert_eq!((x, y), (2680, 100));
            Some((work(2560.0, 0.0, 1920.0, 1140.0), 1.25))
        });
        assert_eq!(
            restored,
            (Some(WindowPosition::Physical(2680, 100)), Some((760, 580)))
        );
        // A restart after a DPI change uses the target display's new scale only
        // for size/clamping, never to multiply the saved physical position again.
        let changed_dpi = restored_rect_with_lookup(&state, |_, _| {
            Some((work(2560.0, 0.0, 1920.0, 1140.0), 1.5))
        });
        assert_eq!(changed_dpi, restored);
    }

    #[test]
    fn removed_display_clamps_native_position_and_size_to_remaining_work_area() {
        let mut state = SavedWindowState::new(MAIN_WINDOW, 3200.0, 700.0, 1800.0, 900.0);
        state.physical_position = Some((4000, 875));
        let restored = restored_rect_with_lookup(&state, |x, y| {
            assert_eq!((x, y), (4000, 875));
            Some((work(0.0, 0.0, 1920.0, 1040.0), 1.25))
        });
        assert_eq!(
            restored,
            (Some(WindowPosition::Physical(0, 0)), Some((1536, 832)))
        );
    }

    #[test]
    fn negative_native_position_roundtrips_and_obeys_location_and_size_flags() {
        let dir = tempdir().unwrap();
        let mut state = SavedWindowState::new(PANEL_WINDOW, -1200.0, 40.0, 420.0, 520.0);
        state.physical_position = Some((-1800, 60));
        save(dir.path(), &state).unwrap();
        let mut loaded = load(dir.path(), PANEL_WINDOW).unwrap();
        assert_eq!(loaded, state);
        assert_eq!(
            restored_rect_with_lookup(&loaded, |_, _| Some((
                work(-1920.0, 0.0, 1920.0, 1040.0),
                1.5
            ))),
            (Some(WindowPosition::Physical(-1800, 60)), None)
        );
        loaded.monitor_left = false;
        loaded.monitor_top = false;
        assert_eq!(
            restored_rect_with_lookup(&loaded, |_, _| None),
            (None, None)
        );
        let legacy = SavedWindowState::new(MAIN_WINDOW, 12.0, 34.0, 760.0, 580.0);
        let json = serde_json::to_value(&legacy).unwrap();
        assert!(json.get("PhysicalPosition").is_none());
        let decoded: SavedWindowState = serde_json::from_value(json).unwrap();
        assert_eq!(
            restored_rect_with_lookup(&decoded, |_, _| None),
            (Some(WindowPosition::Logical(12, 34)), Some((760, 580)))
        );
    }
}
