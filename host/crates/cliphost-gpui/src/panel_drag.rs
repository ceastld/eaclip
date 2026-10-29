//! Pure panel drop validation. The backend repeats these checks at commit time.

use std::collections::HashSet;

use serde_json::Value;

use crate::favorite_drag::{ContentDrag, DragSource, DropTarget, Placement};

pub(super) fn current_pointer(window: &gpui::Window) -> gpui::Point<gpui::Pixels> {
    #[cfg(windows)]
    {
        use crate::windows_api::Win32::{Foundation::{HWND, POINT}, Graphics::Gdi::ScreenToClient, UI::WindowsAndMessaging::GetCursorPos};
        if let Ok(hwnd) = crate::platform::hwnd(window) {
            unsafe {
                let mut point = POINT::default();
                if GetCursorPos(&mut point).is_ok() && ScreenToClient(HWND(hwnd.0 as *mut _), &mut point).as_bool() {
                    return gpui::point(gpui::px(point.x as f32), gpui::px(point.y as f32)) / window.scale_factor();
                }
            }
        }
    }
    window.mouse_position()
}

/// Routes opened externally only identify a directory. Pick its first valid
/// occurrence under the chosen panel root without altering shared relations.
pub(super) fn occurrence_path(tree: &Value, root: &str, target: &str) -> Option<Vec<String>> {
    fn visit(tree: &Value, id: &str, target: &str, path: &mut Vec<String>, visited: &mut HashSet<String>) -> bool {
        if !visited.insert(id.into()) { return false; }
        path.push(id.into());
        if id == target { return true; }
        for child in tree["childrenById"][id].as_array().into_iter().flatten().filter_map(Value::as_str) {
            if visit(tree, child, target, path, visited) { return true; }
        }
        path.pop();
        false
    }
    let mut path = Vec::new();
    visit(tree, root, target, &mut path, &mut HashSet::new()).then_some(path)
}

pub(super) fn placement_bounds(
    mut bounds: gpui::Bounds<gpui::Pixels>, folder: bool, vertical: bool, can_order: bool, placement: Placement,
) -> gpui::Bounds<gpui::Pixels> {
    if !can_order { return bounds; }
    let length = f32::from(if vertical { bounds.size.height } else { bounds.size.width });
    let edge = if folder { 10.0_f32.min(length / 3.0) } else { length / 2.0 };
    let (offset, size) = match placement {
        Placement::Before => (0.0, edge),
        Placement::After => (length - edge, edge),
        Placement::Into => (edge, (length - edge * 2.0).max(0.0)),
    };
    if vertical { bounds.origin.y += gpui::px(offset); bounds.size.height = gpui::px(size); }
    else { bounds.origin.x += gpui::px(offset); bounds.size.width = gpui::px(size); }
    bounds
}

pub(super) fn valid_target(tree: &Value, drag: &ContentDrag, target: &DropTarget, copy: bool) -> bool {
    if drag.item_ids.is_empty() || target.parent_id.as_deref() == Some(super::ORPHANS) {
        return false;
    }
    if let Some(parent) = &target.parent_id {
        if tree["items"][parent]["isFolder"].as_bool() != Some(true) {
            return false;
        }
    }
    if target.placement != Placement::Into && target.anchor_id.is_none() {
        return false;
    }
    if matches!(drag.source, DragSource::Favorite { .. }) {
        if !copy && target.anchor_id.as_ref().is_some_and(|anchor| drag.item_ids.contains(anchor)) {
            return false;
        }
        for id in &drag.item_ids {
            if tree["items"][id].is_null() {
                return false;
            }
            // Moving and copying into a selected folder's reachable graph are
            // both rejected, including ParentId-only legacy fallback edges.
            if target.parent_id.as_ref().is_some_and(|parent| reachable(tree, id, parent)) {
                return false;
            }
        }
        if target.parent_id.is_none()
            && drag.item_ids.iter().any(|id| tree["items"][id]["isFolder"].as_bool() != Some(true)) {
            return false;
        }
    } else if target.parent_id.is_none() {
        return false;
    }
    true
}

fn reachable(tree: &Value, source: &str, target: &str) -> bool {
    let mut pending = vec![source];
    let mut visited = HashSet::new();
    while let Some(id) = pending.pop() {
        if id == target { return true; }
        if !visited.insert(id) { continue; }
        pending.extend(tree["childrenById"][id].as_array().into_iter().flatten().filter_map(Value::as_str));
    }
    false
}

pub(super) fn edge_velocity(position: f32, start: f32, length: f32) -> f32 {
    if !position.is_finite() || !start.is_finite() || !length.is_finite() || length <= 0.0
        || position < start || position >= start + length { return 0.0; }
    let band = 28.0_f32.min(length / 3.0);
    let relative = position - start;
    if relative < band { -420.0 * (1.0 - relative / band) }
    else if relative > length - band { 420.0 * (1.0 - (length - relative) / band) }
    else { 0.0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn native_folder_offer_bounds_match_centre_and_edge_in_both_panel_orientations() {
        use gpui::{point, px, size, Bounds};
        let bounds = Bounds::new(point(px(20.0), px(30.0)), size(px(60.0), px(30.0)));
        let centre = placement_bounds(bounds, true, true, true, Placement::Into);
        assert_eq!(centre.origin.y, px(40.0));
        assert_eq!(centre.size.height, px(10.0));
        assert_eq!(placement_bounds(bounds, true, true, true, Placement::Before).size.height, px(10.0));
        assert_eq!(placement_bounds(bounds, true, true, true, Placement::After).origin.y, px(50.0));
        let grid = placement_bounds(bounds, true, false, true, Placement::Into);
        assert_eq!(grid.origin.x, px(30.0));
        assert_eq!(grid.size.width, px(40.0));
        assert_eq!(placement_bounds(bounds, true, true, false, Placement::Into), bounds);
    }

    #[test]
    fn panel_edges_accelerate_only_inside_the_active_scroll_container() {
        assert_eq!(edge_velocity(99.0, 100.0, 400.0), 0.0);
        assert_eq!(edge_velocity(501.0, 100.0, 400.0), 0.0);
        assert_eq!(edge_velocity(300.0, 100.0, 400.0), 0.0);
        assert!(edge_velocity(101.0, 100.0, 400.0) < edge_velocity(115.0, 100.0, 400.0));
        assert!(edge_velocity(499.0, 100.0, 400.0) > edge_velocity(485.0, 100.0, 400.0));
    }

    #[test]
    fn panel_rejects_cycles_and_virtual_targets_without_losing_valid_shared_edges() {
        let tree = json!({"items":{
            "root":{"isFolder":true},"folder":{"isFolder":true},
            "child":{"isFolder":true},"text":{"isFolder":false}
        },"childrenById":{"root":["folder","text"],"folder":["child"],"child":["folder"]}});
        let drag = ContentDrag::favorite(Some("root".into()), vec!["folder".into()], vec![], "r".into(), 1, 1, "Folder".into());
        let into = |id: &str| DropTarget { parent_id:Some(id.into()), anchor_id:None, placement:Placement::Into };
        assert!(!valid_target(&tree, &drag, &into("child"), false));
        assert!(!valid_target(&tree, &drag, &into("folder"), false));
        assert!(!valid_target(&tree, &drag, &into(super::super::ORPHANS), false));
        assert!(valid_target(&tree, &drag, &into("root"), false));
        assert!(!valid_target(&tree, &drag, &into("text"), false));
        assert!(valid_target(&tree, &drag, &DropTarget { parent_id:None, anchor_id:None, placement:Placement::Into }, false));
        assert!(!valid_target(&tree, &drag, &DropTarget { parent_id:Some("root".into()), anchor_id:Some("folder".into()), placement:Placement::Before }, false));
        let mixed = ContentDrag::favorite(Some("root".into()), vec!["folder".into(), "text".into()], vec![], "r".into(), 1, 1, "Mixed".into());
        assert!(!valid_target(&tree, &mixed, &DropTarget { parent_id:None, anchor_id:None, placement:Placement::Into }, false));
        assert_eq!(occurrence_path(&tree, "root", "child"), Some(vec!["root".into(), "folder".into(), "child".into()]));
        assert_eq!(occurrence_path(&tree, "folder", "missing"), None);
    }

    #[test]
    fn panel_self_anchor_is_copy_only_and_uses_the_current_control_state() {
        let tree = json!({"items":{"parent":{"isFolder":true},"text":{"isFolder":false}},
            "childrenById":{"parent":["text"]}});
        let drag = ContentDrag::favorite(Some("parent".into()), vec!["text".into()], vec![], "r".into(), 1, 1, "Text".into());
        for placement in [Placement::Before, Placement::After] {
            let target = DropTarget { parent_id:Some("parent".into()), anchor_id:Some("text".into()), placement };
            assert!(!valid_target(&tree, &drag, &target, false));
            assert!(valid_target(&tree, &drag, &target, true));
            assert!(!valid_target(&tree, &drag, &target, false), "releasing Ctrl restores move validation");
        }
    }
}
