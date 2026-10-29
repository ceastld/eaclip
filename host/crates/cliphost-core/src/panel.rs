//! Super-panel catalog/layout/page aligned with C# `ClipHostPanelAppService` DTOs.

use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};

use crate::extra_settings::{PanelSettings, UserIcon};
use crate::model::FavorRecord;

pub const ROOT_FOLDER_ID: &str = "root";
pub const DETACHED_ORPHANS_FOLDER_ID: &str = "__cea:detached-orphans";
pub const DETACHED_ORPHANS_TITLE: &str = "独立收藏";

/// Shared read projection for all favorite surfaces and structure commands.
/// Raw/shared/missing edges keep their order; old ParentId-only edges follow.
pub(crate) fn effective_children(favors: &[FavorRecord]) -> BTreeMap<String, Vec<String>> {
    let mut children: BTreeMap<_, _> = favors.iter().map(|item| (item.id.clone(), item.children.clone())).collect();
    let folders: HashSet<_> = favors.iter().filter(|item| item.is_folder).map(|item| item.id.as_str()).collect();
    let mut fallback: Vec<_> = favors.iter().collect();
    fallback.sort_by(|a, b| a.sub_index.cmp(&b.sub_index).then(a.create_time.cmp(&b.create_time)).then(a.id.cmp(&b.id)));
    for item in fallback {
        if let Some(parent) = item.parent_id.as_deref().filter(|parent| folders.contains(parent)) {
            let edges = children.get_mut(parent).unwrap();
            if !edges.contains(&item.id) { edges.push(item.id.clone()); }
        }
    }
    children
}

pub fn root_panel_folders(favors: &[FavorRecord]) -> Vec<&FavorRecord> {
    favors
        .iter()
        .filter(|i| i.is_folder && i.parent_id.is_none())
        .collect()
}

pub fn ordered_root_panels<'a>(favors: &'a [FavorRecord], ordered_ids: &[String]) -> Vec<&'a FavorRecord> {
    let mut roots = root_panel_folders(favors);
    roots.sort_by(|a, b| {
        let ia = ordered_ids.iter().position(|id| id == &a.id).unwrap_or(usize::MAX);
        let ib = ordered_ids.iter().position(|id| id == &b.id).unwrap_or(usize::MAX);
        ia.cmp(&ib)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    roots
}

pub fn catalog(favors: &[FavorRecord], ordered_root_ids: &[String], user_icon: &UserIcon) -> Value {
    let panels: Vec<Value> = ordered_root_panels(favors, ordered_root_ids)
        .into_iter()
        .map(|i| {
            json!({
                "favorItemId": i.id,
                "title": i.visual_title(),
                "icon": i.host_icon_with(user_icon),
            })
        })
        .collect();
    json!({ "panels": panels })
}

pub fn layout(
    favors: &[FavorRecord],
    ordered_root_ids: &[String],
    default_panel_favor_id: &str,
    user_icon: &UserIcon,
) -> Option<Value> {
    let roots = ordered_root_panels(favors, ordered_root_ids);
    let orphans = detached_orphans(favors, user_icon).len();
    if roots.is_empty() {
        return None;
    }
    if roots.len() == 1 {
        let panel = roots[0];
        return Some(json!({
            "mode": "single",
            "defaultFavorItemId": panel.id,
            "rootTitle": panel.visual_title(),
            "rootIcon": panel.host_icon_with(user_icon),
            "showPanelHomeNav": true,
            "navEntries": folder_nav_tree(favors, panel, user_icon),
            "detachedOrphansCount": orphans,
        }));
    }
    let configured = default_panel_favor_id.trim();
    let pick = roots
        .iter()
        .find(|i| !configured.is_empty() && i.id == configured)
        .copied()
        .unwrap_or(roots[0]);
    Some(json!({
        "mode": "multiRoot",
        "defaultFavorItemId": pick.id,
        "rootTitle": "超级面板",
        "rootIcon": Value::Null,
        "showPanelHomeNav": false,
        "navEntries": roots.iter().map(|r| json!({
            "favorItemId": r.id,
            "title": r.visual_title(),
            "icon": r.host_icon_with(user_icon),
            "children": folder_nav_tree(favors, r, user_icon),
        })).collect::<Vec<_>>(),
        "detachedOrphansCount": orphans,
    }))
}

pub fn page(
    favors: &[FavorRecord],
    favor_id: &str,
    folder_id: Option<&str>,
    appearance: Value,
    user_icon: &UserIcon,
) -> Result<Value, String> {
    if folder_id == Some(DETACHED_ORPHANS_FOLDER_ID) || favor_id == DETACHED_ORPHANS_FOLDER_ID {
        return Ok(json!({
            "rootFavorId": DETACHED_ORPHANS_FOLDER_ID,
            "currentFavorId": DETACHED_ORPHANS_FOLDER_ID,
            "parentFolderId": Value::Null,
            "title": DETACHED_ORPHANS_TITLE,
            "canGoBack": false,
            "canReorderChildren": false,
            "behavior": { "doubleClickClose": false },
            "appearance": appearance,
            "items": detached_orphans(favors, user_icon),
        }));
    }
    let current_id = folder_id.filter(|s| !s.is_empty()).unwrap_or(favor_id);
    let current = favors
        .iter()
        .find(|i| i.id == current_id && i.is_folder)
        .ok_or_else(|| "Panel page not found.".to_string())?;
    let children = effective_children(favors);
    let items: Vec<Value> = children[&current.id]
        .iter()
        .filter_map(|id| favors.iter().find(|i| i.id == *id))
        .map(|item| panel_item(item, user_icon))
        .collect();
    Ok(json!({
        "rootFavorId": favor_id,
        "currentFavorId": current_id,
        "parentFolderId": current.parent_id,
        "title": current.visual_title(),
        "canGoBack": current_id != favor_id,
        "canReorderChildren": true,
        "behavior": { "doubleClickClose": false },
        "appearance": appearance,
        "items": items,
    }))
}

pub fn appearance(cfg: &PanelSettings) -> Value {
    json!({
        "panelStyle": cfg.panel_style,
        "windowBackground": cfg.window_background,
        "fontColor": cfg.font_color,
        "defaultClickAction": cfg.default_click_action,
        "folderDefaultClickAction": crate::extra_settings::panel_item_click_action(&cfg.folder_default_click_action),
        "fileDefaultClickAction": crate::extra_settings::panel_item_click_action(&cfg.file_default_click_action),
        "bigIconWidth": cfg.big_icon_width,
        "bigIconHeight": cfg.big_icon_height,
        "bigIconFontSize": cfg.big_icon_font_size,
        "smallIconFontSize": cfg.small_icon_font_size,
        "smallWidth": cfg.small_width,
        "smallMinHeight": cfg.small_min_height,
        "onlyIconSize": cfg.only_icon_size,
    })
}

pub fn insert_child_id(children: &mut Vec<String>, id: &str, before_item_id: Option<&str>) {
    children.retain(|c| c != id);
    if let Some(before) = before_item_id.filter(|s| !s.is_empty()) {
        if let Some(i) = children.iter().position(|c| c == before) {
            children.insert(i, id.to_string());
            return;
        }
    }
    children.push(id.to_string());
}

pub fn is_root_folder_id(folder_id: &str) -> bool {
    folder_id.eq_ignore_ascii_case(ROOT_FOLDER_ID)
}

pub fn is_orphans_folder_id(folder_id: &str) -> bool {
    folder_id == DETACHED_ORPHANS_FOLDER_ID
}

pub fn walk_to_root_panel<'a>(
    favors: &'a [FavorRecord],
    start_id: &str,
) -> Option<(&'a FavorRecord, &'a FavorRecord)> {
    let start = favors.iter().find(|i| i.id == start_id)?;
    let mut current = start;
    let mut visited = HashSet::new();
    while let Some(pid) = current.parent_id.as_ref().filter(|id| !id.is_empty()) {
        if !visited.insert(current.id.as_str()) { return None; }
        current = favors.iter().find(|i| i.id == *pid)?;
    }
    if current.is_folder {
        Some((current, start))
    } else {
        None
    }
}

fn folder_nav_tree(favors: &[FavorRecord], parent: &FavorRecord, user_icon: &UserIcon) -> Vec<Value> {
    fn walk(favors: &[FavorRecord], parent: &FavorRecord, user_icon: &UserIcon,
        children: &BTreeMap<String, Vec<String>>, path: &mut HashSet<String>) -> Vec<Value> {
        if !path.insert(parent.id.clone()) { return Vec::new(); }
        let result = children.get(&parent.id).into_iter().flatten()
            .filter_map(|id| favors.iter().find(|item| item.id == *id && item.is_folder))
            .map(|item| json!({
                "favorItemId":item.id,"title":item.visual_title(),"icon":item.host_icon_with(user_icon),
                "children":walk(favors,item,user_icon,children,path),
            })).collect();
        path.remove(&parent.id);
        result
    }
    walk(favors, parent, user_icon, &effective_children(favors), &mut HashSet::new())
}

fn detached_orphans(favors: &[FavorRecord], user_icon: &UserIcon) -> Vec<Value> {
    let mut items: Vec<&FavorRecord> = favors
        .iter()
        .filter(|i| !i.is_folder && i.parent_id.is_none())
        .collect();
    items.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    items
        .into_iter()
        .map(|item| panel_item(item, user_icon))
        .collect()
}

fn panel_item(item: &FavorRecord, user_icon: &UserIcon) -> Value {
    json!({
        "id": item.id,
        "title": item.visual_title(),
        "icon": item.host_icon_with(user_icon),
        "isFolder": item.is_folder,
        "kind": item.obj_type_name(),
        "summary": item.summary(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use crate::model::ClipKind;

    fn folder(id: &str, name: &str, children: Vec<String>) -> FavorRecord {
        FavorRecord {
            storage_revision: None,
            id: id.into(),
            parent_id: None,
            is_folder: true,
            name: name.into(),
            kind: ClipKind::FavoriteClipItem,
            body_text: None,
            file_paths: Vec::new(),
            image_path: None,
            children,
            create_time: Utc::now(),
            update_time: Utc::now(),
            source_clip_id: None,
            html_data: None,
            html_source_url: None,
            qq_rich_text: None,
            source_process_name: String::new(),
            sub_index: 0,
            icon: String::new(),
            execute_param: String::new(),
        }
    }

    #[test]
    fn single_root_layout_matches_csharp_mode() {
        let root = folder("p1", "工作", vec!["c1".into()]);
        let child = FavorRecord {
            storage_revision: None,
            parent_id: Some("p1".into()),
            ..folder("c1", "子夹", Vec::new())
        };
        let layout = layout(&[root, child], &["p1".into()], "", &UserIcon::default()).unwrap();
        assert_eq!(layout["mode"], "single");
        assert_eq!(layout["defaultFavorItemId"], "p1");
        assert_eq!(layout["showPanelHomeNav"], true);
        assert_eq!(layout["navEntries"][0]["favorItemId"], "c1");
    }

    #[test]
    fn empty_roots_have_no_layout() {
        assert!(layout(&[], &[], "", &UserIcon::default()).is_none());
    }

    #[test]
    fn multi_root_uses_configured_default_panel() {
        let a = folder("a", "A", Vec::new());
        let b = folder("b", "B", Vec::new());
        let layout = layout(&[a, b], &["b".into(), "a".into()], "a", &UserIcon::default()).unwrap();
        assert_eq!(layout["mode"], "multiRoot");
        assert_eq!(layout["defaultFavorItemId"], "a");
        assert_eq!(layout["navEntries"][0]["favorItemId"], "b");
    }

    #[test]
    fn page_missing_folder_matches_csharp() {
        let err = page(&[], "missing", None, json!({}), &UserIcon::default()).unwrap_err();
        assert_eq!(err, "Panel page not found.");
    }

    #[test]
    fn catalog_layout_resolve_icon_like_csharp() {
        let root = folder("p1", "工作", vec!["c1".into()]);
        let child = FavorRecord {
            storage_revision: None,
            parent_id: Some("p1".into()),
            ..folder("c1", "子夹", Vec::new())
        };
        let cat = catalog(&[root.clone(), child.clone()], &["p1".into()], &UserIcon::default());
        assert_eq!(cat["panels"][0]["icon"], "fa:Light_Folder");
        let first = super::layout(&[root.clone(), child], &["p1".into()], "", &UserIcon::default()).unwrap();
        assert_eq!(first["rootIcon"], "fa:Light_Folder");
        assert_eq!(first["navEntries"][0]["icon"], "fa:Light_Folder");

        let mut starred = folder("p1", "工作", Vec::new());
        starred.icon = "fa:Solid_Star".into();
        let starred_layout = super::layout(&[starred], &["p1".into()], "", &UserIcon::default()).unwrap();
        assert_eq!(starred_layout["rootIcon"], "fa:Solid_Star");

        let a = folder("a", "A", Vec::new());
        let b = folder("b", "B", Vec::new());
        let multi = super::layout(&[a, b], &["a".into(), "b".into()], "", &UserIcon::default()).unwrap();
        assert_eq!(multi["rootIcon"], Value::Null);
        assert_eq!(multi["navEntries"][0]["icon"], "fa:Light_Folder");
    }

    #[test]
    fn insert_child_id_honors_before() {
        let mut children = vec!["a".into(), "b".into(), "c".into()];
        insert_child_id(&mut children, "x", Some("c"));
        assert_eq!(children, vec!["a", "b", "x", "c"]);
        insert_child_id(&mut children, "b", Some("a"));
        assert_eq!(children, vec!["b", "a", "x", "c"]);
    }

    #[test]
    fn effective_panel_page_and_navigation_include_parent_only_edges_without_repair() {
        let root = folder("root-panel", "Root", vec!["explicit".into()]);
        let mut explicit = folder("explicit", "Explicit", Vec::new());
        explicit.parent_id = Some(root.id.clone());
        let mut fallback = folder("fallback", "Fallback", Vec::new());
        fallback.parent_id = Some(root.id.clone());
        let records = vec![root, explicit, fallback];
        let before = serde_json::to_value(&records).unwrap();
        let page = page(&records, "root-panel", None, json!({}), &UserIcon::default()).unwrap();
        assert_eq!(page["items"].as_array().unwrap().iter().map(|item| item["id"].as_str().unwrap()).collect::<Vec<_>>(), ["explicit", "fallback"]);
        let layout = layout(&records, &[], "", &UserIcon::default()).unwrap();
        assert_eq!(layout["navEntries"][1]["favorItemId"], "fallback");
        assert_eq!(serde_json::to_value(&records).unwrap(), before);
    }

    #[test]
    fn effective_panel_navigation_bounds_existing_shared_cycles() {
        let root = folder("p", "Root", vec!["nested".into()]);
        let mut nested = folder("nested", "Nested", vec!["p".into()]);
        nested.parent_id = Some(root.id.clone());
        let records = vec![root, nested];
        let layout = layout(&records, &[], "", &UserIcon::default()).unwrap();
        assert_eq!(layout["navEntries"][0]["children"][0]["favorItemId"], "p");
        assert_eq!(layout["navEntries"][0]["children"][0]["children"], json!([]));
    }
}
