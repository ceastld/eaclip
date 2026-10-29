//! Super Menu sessions aligned with C# `ClipHostMenuSessionService`.

use serde_json::{json, Value};
use std::collections::HashMap;
use uuid::Uuid;

use crate::bookmarks::BookmarkNode;
use crate::explorer::OpenedFolder;
use crate::extra_settings::{FileMenuSettings, MenuGlobalSettings, UserIcon};
use crate::favor_menu::{FavorMenuConfigItem, FavorMenuData, MenuTargetWindow};
use crate::file_menu::{CustomEntry, FileMenuPaths};
use crate::file_menu_extract;
use crate::model::{ClipKind, FavorRecord};
use crate::window_menu::{self, WindowEntry, WindowInfo};

pub const CONTINUE_CURSOR: &str = "more";
pub const CLIP_FILES_ROLE: &str = "clip-files";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CollectionKind {
    Children,
    ContextMenu,
}

#[derive(Debug, Clone)]
pub enum LazyKind {
    FileDir { path: String },
    ThisPc,
    RecentFiles,
    OpenedFolders,
    AddOpenedFolders,
    WindowOps { hwnd: i64 },
    WindowGroup { hwnds: Vec<i64> },
    FavorAdder,
    OpenBrowserTabs,
    BrowserTabItems { tabs: Vec<crate::browser::BrowserTab>, parent: Value, error: Option<String> },
    ContextFile { path: String },
    ContextWindow { hwnd: i64 },
    ContextOpened,
    BookmarkFolder { children: Vec<BookmarkNode> },
    ReopenFolders,
    FileMenuRoot,
    TextMenuRoot,
    /// Paths of one clipboard history item, shown as ordinary file-menu nodes.
    ClipFiles { paths: Vec<String> },
}

struct LazySessionBits {
    node_payload: Value,
    file_cfg: FileMenuSettings,
    menu_cfg: MenuGlobalSettings,
    opened: Vec<OpenedFolder>,
    reopen_paths: Vec<String>,
    reopen_recorded_at: HashMap<String, i64>,
    reopen_undo: Option<Value>,
    custom_paths: Vec<String>,
    extra_files: Vec<String>,
    clipboard_path: Option<String>,
    text_actions: Vec<Value>,
    bookmarks: Option<BookmarkNode>,
    skip_hwnd: isize,
    file_menu: FileMenuPaths,
    existing_children: Vec<String>,
}

impl LazySessionBits {
    fn from_node(session: &MenuSession, node: &MenuItemNode) -> Self {
        Self {
            node_payload: node.payload.clone(),
            file_cfg: session.file_cfg.clone(),
            menu_cfg: session.menu_cfg.clone(),
            opened: session.opened.clone(),
            reopen_paths: session.reopen_paths.clone(),
            reopen_recorded_at: session.reopen_recorded_at.clone(),
            reopen_undo: session.reopen_undo.clone(),
            custom_paths: session.custom_paths.clone(),
            extra_files: session.extra_files.clone(),
            clipboard_path: session.clipboard_path.clone(),
            text_actions: session.text_actions.clone(),
            bookmarks: session.bookmarks.clone(),
            skip_hwnd: session.skip_hwnd,
            file_menu: session.file_menu.clone(),
            existing_children: node.child_ids.clone(),
        }
    }

    fn to_build_context(&self) -> MenuBuildContext {
        MenuBuildContext {
            file_menu: self.file_menu.clone(),
            file_cfg: self.file_cfg.clone(),
            menu_cfg: self.menu_cfg.clone(),
            extra_files: self.extra_files.clone(),
            favors: Vec::new(),
            skip_hwnd: self.skip_hwnd,
            clipboard_path: self.clipboard_path.clone(),
            opened: self.opened.clone(),
            reopen_paths: self.reopen_paths.clone(),
            reopen_recorded_at: self.reopen_recorded_at.clone(),
            reopen_undo: self.reopen_undo.clone(),
            text_actions: self.text_actions.clone(),
            bookmarks: self.bookmarks.clone(),
            source_window_handle: 0,
            target_window_handle: 0,
            favor_menu: FavorMenuData::default(),
            target: MenuTargetWindow::default(),
            user_icon: UserIcon::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MenuItemNode {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub description: String,
    pub icon: String,
    pub is_enabled: bool,
    pub invoke_mode: String,
    pub keep_open_on_invoke: bool,
    pub has_children: bool,
    pub has_context_menu: bool,
    pub role: Option<String>,
    pub payload: Value,
    pub child_ids: Vec<String>,
    pub context_ids: Vec<String>,
    pub check: Option<bool>,
    pub value: Option<(f64, f64, f64, f64)>,
    pub visual: Option<Value>,
    pub lazy: Option<LazyKind>,
    pub pending: Vec<String>,
    pub served: usize,
}

#[derive(Debug, Clone)]
pub struct MenuSession {
    pub id: String,
    pub action: String,
    pub style: Value,
    pub message: String,
    pub root_ids: Vec<String>,
    pub nodes: HashMap<String, MenuItemNode>,
    pub file_cfg: FileMenuSettings,
    pub menu_cfg: MenuGlobalSettings,
    pub opened: Vec<OpenedFolder>,
    pub reopen_paths: Vec<String>,
    pub reopen_recorded_at: HashMap<String, i64>,
    pub reopen_undo: Option<Value>,
    pub custom_paths: Vec<String>,
    pub extra_files: Vec<String>,
    pub clipboard_path: Option<String>,
    pub text_actions: Vec<Value>,
    pub bookmarks: Option<BookmarkNode>,
    pub skip_hwnd: isize,
    pub file_menu: FileMenuPaths,
    pub source_window_handle: i64,
    pub target_window_handle: i64,
}

#[derive(Debug, Default)]
pub struct MenuRegistry {
    sessions: HashMap<String, MenuSession>,
    node_load_generations: HashMap<(String, String, CollectionKind), u64>,
}

pub(crate) struct MenuNodesLoad {
    session_id: String,
    node_id: String,
    kind: CollectionKind,
    generation: u64,
    reset: bool,
    materialization: MenuNodesMaterialization,
}

enum MenuNodesMaterialization {
    Lazy { lazy: LazyKind, bits: LazySessionBits },
    NativeContext {
        role: Option<String>, payload: Value, custom_paths: Vec<String>,
        target_hwnd: i64, clipboard_path: Option<String>,
    },
}

pub(crate) struct MenuNodesResult {
    load: MenuNodesLoad,
    pub(crate) built: Vec<MenuItemNode>,
    extra: HashMap<String, MenuItemNode>,
}

impl MenuNodesLoad {
    pub(crate) fn materialize(self) -> MenuNodesResult {
        let (built, extra) = match &self.materialization {
            MenuNodesMaterialization::Lazy { lazy, bits } => materialize_lazy(lazy, bits),
            MenuNodesMaterialization::NativeContext { role, payload, custom_paths, target_hwnd, clipboard_path } =>
                context_for(role.as_deref(), payload, custom_paths, *target_hwnd, clipboard_path.as_deref()),
        };
        MenuNodesResult { load: self, built, extra }
    }

    /// Supply native-query results without touching the interactive desktop.
    #[cfg(test)]
    pub(crate) fn fixture_result(self, built: Vec<MenuItemNode>) -> MenuNodesResult {
        MenuNodesResult { load: self, built, extra: HashMap::new() }
    }
}

pub struct MenuBuildContext {
    pub file_menu: FileMenuPaths,
    pub file_cfg: FileMenuSettings,
    pub menu_cfg: MenuGlobalSettings,
    pub extra_files: Vec<String>,
    pub favors: Vec<FavorRecord>,
    pub skip_hwnd: isize,
    pub clipboard_path: Option<String>,
    pub opened: Vec<OpenedFolder>,
    pub reopen_paths: Vec<String>,
    pub reopen_recorded_at: HashMap<String, i64>,
    pub reopen_undo: Option<Value>,
    pub text_actions: Vec<Value>,
    pub bookmarks: Option<BookmarkNode>,
    pub source_window_handle: i64,
    pub target_window_handle: i64,
    pub favor_menu: FavorMenuData,
    pub target: MenuTargetWindow,
    pub user_icon: UserIcon,
}

impl Default for MenuBuildContext {
    fn default() -> Self {
        Self {
            file_menu: FileMenuPaths::default(),
            file_cfg: FileMenuSettings::default(),
            menu_cfg: MenuGlobalSettings::default(),
            extra_files: Vec::new(),
            favors: Vec::new(),
            skip_hwnd: 0,
            clipboard_path: None,
            opened: Vec::new(),
            reopen_paths: Vec::new(),
            reopen_recorded_at: HashMap::new(),
            reopen_undo: None,
            text_actions: Vec::new(),
            bookmarks: None,
            source_window_handle: 0,
            target_window_handle: 0,
            favor_menu: FavorMenuData::default(),
            target: MenuTargetWindow::default(),
            user_icon: UserIcon::default(),
        }
    }
}

impl MenuRegistry {
    /// Construct an owned registry fixture without enumerating desktop windows.
    #[cfg(test)]
    pub(crate) fn create_fixture_session(&mut self, ctx: &MenuBuildContext, nodes: Vec<MenuItemNode>) -> Value {
        self.insert_session("window", json!({}), nodes, HashMap::new(), "", ctx)
    }

    pub fn create_from_context(&mut self, action: &str, style: Value, ctx: &MenuBuildContext) -> Value {
        let (items, extra, message) = match action {
            "window" => (window_root(ctx, true), HashMap::new(), String::new()),
            "file" => file_root(ctx),
            "text" => text_root(&ctx.text_actions),
            "clipboard" => (Vec::new(), HashMap::new(), String::new()),
            "bookmarks" => match &ctx.bookmarks {
                Some(tree) => {
                    let (items, extra) = bookmark_children(tree);
                    let message = if items.is_empty() {
                        "书签为空".into()
                    } else {
                        String::new()
                    };
                    (items, extra, message)
                }
                None => (
                    Vec::new(),
                    HashMap::new(),
                    "暂时无法获取浏览器书签，请稍后重试；若持续失败再检查扩展连接与书签权限".into(),
                ),
            },
            "reopen" => (
                reopen_verb_root(&ctx.opened, &ctx.reopen_paths, &ctx.reopen_recorded_at, ctx.reopen_undo.as_ref()),
                HashMap::new(),
                String::new(),
            ),
            _ => favor_root(ctx),
        };
        self.insert_session(action, style, items, extra, &message, ctx)
    }

    fn insert_session(
        &mut self,
        action: &str,
        style: Value,
        items: Vec<MenuItemNode>,
        extra_nodes: HashMap<String, MenuItemNode>,
        message: &str,
        ctx: &MenuBuildContext,
    ) -> Value {
        let mut nodes = extra_nodes;
        let mut root_ids = Vec::new();
        for item in items {
            root_ids.push(item.id.clone());
            nodes.insert(item.id.clone(), item);
        }
        let root = page_from_ids(&nodes, &root_ids, message, false, None);
        let id = Uuid::new_v4().simple().to_string();
        self.sessions.insert(
            id.clone(),
            MenuSession {
                id: id.clone(),
                action: action.to_string(),
                style: style.clone(),
                message: message.to_string(),
                root_ids,
                nodes,
                file_cfg: ctx.file_cfg.clone(),
                menu_cfg: ctx.menu_cfg.clone(),
                opened: ctx.opened.clone(),
                reopen_paths: ctx.reopen_paths.clone(),
                reopen_recorded_at: ctx.reopen_recorded_at.clone(),
                reopen_undo: ctx.reopen_undo.clone(),
                custom_paths: ctx.file_menu.paths(),
                extra_files: ctx.extra_files.clone(),
                clipboard_path: ctx.clipboard_path.clone(),
                text_actions: ctx.text_actions.clone(),
                bookmarks: ctx.bookmarks.clone(),
                skip_hwnd: ctx.skip_hwnd,
                file_menu: ctx.file_menu.clone(),
                source_window_handle: ctx.source_window_handle,
                target_window_handle: ctx.target_window_handle,
            },
        );
        json!({
            "sessionId": id,
            "action": action,
            "style": style,
            "root": root,
            "message": message,
        })
    }

    pub fn close(&mut self, session_id: &str) -> bool {
        self.node_load_generations.retain(|(id, _, _), _| id != session_id);
        self.sessions.remove(session_id).is_some()
    }

    pub fn session_target_handle(&self, session_id: &str) -> i64 {
        self.sessions
            .get(session_id)
            .map(|s| s.target_window_handle)
            .unwrap_or(0)
    }

    pub fn session_opened_and_reopen(&self, session_id: &str) -> (Vec<OpenedFolder>, Vec<String>) {
        self.sessions
            .get(session_id)
            .map(|s| (s.opened.clone(), s.reopen_paths.clone()))
            .unwrap_or_default()
    }

    pub fn set_session_handles(&mut self, session_id: &str, source: i64, target: i64) {
        if let Some(session) = self.sessions.get_mut(session_id) {
            if source != 0 {
                session.source_window_handle = source;
            }
            if target != 0 {
                session.target_window_handle = target;
            }
        }
    }

    pub fn get_nodes(
        &mut self,
        session_id: &str,
        node_id: &str,
        kind: CollectionKind,
        cursor: Option<&str>,
    ) -> Result<Value, String> {
        if node_id.is_empty() || node_id.eq_ignore_ascii_case("root") {
            let session = self.sessions.get(session_id).ok_or_else(|| "Session not found".to_string())?;
            return Ok(page_from_ids(
                &session.nodes,
                &session.root_ids,
                &session.message,
                false,
                None,
            ));
        }
        if kind == CollectionKind::ContextMenu {
            self.ensure_context(session_id, node_id)?;
            let session = self.sessions.get(session_id).unwrap();
            let node = session.nodes.get(node_id).ok_or_else(|| "Node not found".to_string())?;
            return Ok(page_from_ids(&session.nodes, &node.context_ids, "", false, None));
        }
        let reset = cursor.map(str::trim).filter(|c| !c.is_empty()).is_none();
        self.ensure_children(session_id, node_id, reset)?;
        self.page_children(session_id, node_id, reset)
    }

    fn page_children(&mut self, session_id: &str, node_id: &str, reset: bool) -> Result<Value, String> {
        let session = self.sessions.get_mut(session_id).ok_or_else(|| "Session not found".to_string())?;
        let page_size = page_size_of(&session.menu_cfg);
        let node = session.nodes.get_mut(node_id).ok_or_else(|| "Node not found".to_string())?;
        if reset {
            node.served = 0;
        }
        let start = node.served;
        let end = (start + page_size).min(node.child_ids.len());
        let page_ids = node.child_ids[start..end].to_vec();
        node.served = end;
        let has_more = end < node.child_ids.len() || !node.pending.is_empty();
        if has_more && end == node.child_ids.len() && !node.pending.is_empty() {
            // pending consumed on next ensure
        }
        Ok(page_from_ids(
            &session.nodes,
            &page_ids,
            "",
            has_more,
            has_more.then_some(CONTINUE_CURSOR),
        ))
    }

    pub(crate) fn prepare_nodes(
        &mut self,
        session_id: &str,
        node_id: &str,
        kind: CollectionKind,
        cursor: Option<&str>,
    ) -> Result<Option<MenuNodesLoad>, String> {
        if node_id.is_empty() || node_id.eq_ignore_ascii_case("root") {
            return Ok(None);
        }
        let session = self.sessions.get(session_id).ok_or("Session not found")?;
        let node = session.nodes.get(node_id).ok_or("Node not found")?;
        let reset = cursor.map(str::trim).filter(|c| !c.is_empty()).is_none();
        let materialization = match kind {
            CollectionKind::Children => {
                let lazy = match &node.lazy {
                    Some(lazy @ (LazyKind::FileDir { .. } | LazyKind::ThisPc | LazyKind::RecentFiles
                        | LazyKind::ClipFiles { .. }
                        | LazyKind::WindowOps { .. } | LazyKind::WindowGroup { .. }
                        | LazyKind::ContextWindow { .. })) => lazy.clone(),
                    _ => return Ok(None),
                };
                if !reset && !node.child_ids.is_empty() {
                    return Ok(None);
                }
                MenuNodesMaterialization::Lazy { lazy, bits: LazySessionBits::from_node(session, node) }
            }
            CollectionKind::ContextMenu => {
                let window_context = matches!(node.role.as_deref(), Some("window" | "window-group"));
                // File context menus inspect the receiving application for
                // "open in process" / paste actions, so they share the same
                // native-message lock boundary as the window menus.
                let target_context = session.target_window_handle != 0
                    && matches!(node.role.as_deref(), Some("file" | "recent-files" | "reopen" | "opened-folder"));
                if !node.context_ids.is_empty() || !(window_context || target_context) {
                    return Ok(None);
                }
                MenuNodesMaterialization::NativeContext {
                    role: node.role.clone(), payload: node.payload.clone(),
                    custom_paths: session.custom_paths.clone(),
                    target_hwnd: session.target_window_handle, clipboard_path: session.clipboard_path.clone(),
                }
            }
        };
        let generation = self.node_load_generations
            .entry((session_id.into(), node_id.into(), kind)).or_default();
        *generation = generation.checked_add(1).ok_or("Menu load generation exhausted")?;
        Ok(Some(MenuNodesLoad {
            session_id: session_id.into(), node_id: node_id.into(),
            kind, generation: *generation, reset, materialization,
        }))
    }

    pub(crate) fn commit_nodes(&mut self, result: MenuNodesResult) -> Result<Value, String> {
        let MenuNodesResult { load, built, extra } = result;
        let session = self.sessions.get(&load.session_id).ok_or("Session not found")?;
        if !session.nodes.contains_key(&load.node_id) {
            return Err("Node not found".into());
        }
        if self.node_load_generations.get(&(load.session_id.clone(), load.node_id.clone(), load.kind))
            != Some(&load.generation)
        {
            return Err("Node load superseded".into());
        }
        // Validate before inserting any generated nodes or advancing the page.
        let session = self.sessions.get_mut(&load.session_id).unwrap();
        let ids = built.iter().map(|node| node.id.clone()).collect();
        for child in built {
            session.nodes.insert(child.id.clone(), child);
        }
        for (id, node) in extra {
            session.nodes.insert(id, node);
        }
        let node = session.nodes.get_mut(&load.node_id).unwrap();
        match load.kind {
            CollectionKind::Children => {
                node.child_ids = ids;
                node.pending.clear();
                node.has_children = !node.child_ids.is_empty();
                self.page_children(&load.session_id, &load.node_id, load.reset)
            }
            CollectionKind::ContextMenu => {
                node.context_ids = ids;
                node.has_context_menu = !node.context_ids.is_empty();
                let node = session.nodes.get(&load.node_id).unwrap();
                // Empty native context results are final for this load too;
                // get_nodes would materialize them again under the lock.
                Ok(page_from_ids(&session.nodes, &node.context_ids, "", false, None))
            }
        }
    }

    fn ensure_children(&mut self, session_id: &str, node_id: &str, reset: bool) -> Result<(), String> {
        let (lazy, session_copy) = {
            let session = self.sessions.get(session_id).ok_or_else(|| "Session not found".to_string())?;
            let node = session.nodes.get(node_id).ok_or_else(|| "Node not found".to_string())?;
            (
                node.lazy.clone(),
                LazySessionBits::from_node(session, node),
            )
        };
        if !session_copy.existing_children.is_empty() && !reset {
            if self
                .sessions
                .get(session_id)
                .and_then(|s| s.nodes.get(node_id))
                .is_some_and(|n| n.served < n.child_ids.len() || n.pending.is_empty())
            {
                return Ok(());
            }
        }
        if session_copy.existing_children.is_empty() || reset {
            if let Some(lazy) = lazy {
                let (built, extra) = materialize_lazy(&lazy, &session_copy);
                let session = self.sessions.get_mut(session_id).unwrap();
                let child_ids: Vec<String> = built.iter().map(|n| n.id.clone()).collect();
                for child in built {
                    session.nodes.insert(child.id.clone(), child);
                }
                for (id, node) in extra {
                    session.nodes.insert(id, node);
                }
                if let Some(node) = session.nodes.get_mut(node_id) {
                    node.child_ids = child_ids;
                    node.pending.clear();
                    node.has_children = !node.child_ids.is_empty();
                }
            }
        }
        Ok(())
    }

    fn ensure_context(&mut self, session_id: &str, node_id: &str) -> Result<(), String> {
        let (role, payload, existing, custom_paths, target, clipboard_path) = {
            let session = self.sessions.get(session_id).ok_or_else(|| "Session not found".to_string())?;
            let node = session.nodes.get(node_id).ok_or_else(|| "Node not found".to_string())?;
            (
                node.role.clone(),
                node.payload.clone(),
                node.context_ids.clone(),
                session.custom_paths.clone(),
                session.target_window_handle,
                session.clipboard_path.clone(),
            )
        };
        if !existing.is_empty() {
            return Ok(());
        }
        let (items, extra) = context_for(
            role.as_deref(),
            &payload,
            &custom_paths,
            target,
            clipboard_path.as_deref(),
        );
        let session = self.sessions.get_mut(session_id).unwrap();
        let mut ids = Vec::new();
        for item in items {
            ids.push(item.id.clone());
            session.nodes.insert(item.id.clone(), item);
        }
        for (id, node) in extra {
            session.nodes.insert(id, node);
        }
        if let Some(node) = session.nodes.get_mut(node_id) {
            node.context_ids = ids;
            node.has_context_menu = !node.context_ids.is_empty();
        }
        Ok(())
    }

    pub(crate) fn set_browser_tabs(&mut self, session_id: &str, node_id: &str,
        result: Result<Vec<crate::browser::BrowserTab>, String>) -> Result<(), String> {
        let session = self.sessions.get_mut(session_id).ok_or("Session not found")?;
        let node = session.nodes.get_mut(node_id).ok_or("Node not found")?;
        let (tabs, error) = match result { Ok(tabs) => (tabs, None), Err(error) => (Vec::new(), Some(error)) };
        node.lazy = Some(LazyKind::BrowserTabItems { tabs, parent: favor_parent_payload(&node.payload), error });
        Ok(())
    }

    /// Registers a clipboard file item as a folder whose children are the same
    /// file nodes, folders and context menus the file category uses.
    pub fn register_clip_files(&mut self, session_id: &str, clip_id: &str, paths: Vec<String>) -> Result<Value, String> {
        let session = self.sessions.get_mut(session_id).ok_or("Session not found")?;
        let existing = session.nodes.values().find(|node| {
            node.role.as_deref() == Some(CLIP_FILES_ROLE) && node.payload["clipId"].as_str() == Some(clip_id)
                && matches!(&node.lazy, Some(LazyKind::ClipFiles { paths: old }) if *old == paths)
        });
        if let Some(node) = existing {
            return Ok(node_to_dto(node));
        }
        let node = folder_lazy(String::new(), CLIP_FILES_ROLE, json!({ "clipId": clip_id }), "",
            LazyKind::ClipFiles { paths });
        let dto = node_to_dto(&node);
        session.nodes.insert(node.id.clone(), node);
        Ok(dto)
    }

    pub fn node(&self, session_id: &str, node_id: &str) -> Result<MenuItemNode, String> {
        let session = self
            .sessions
            .get(session_id)
            .ok_or_else(|| "Session not found".to_string())?;
        session
            .nodes
            .get(node_id)
            .cloned()
            .ok_or_else(|| "Node not found".into())
    }

    pub fn patch_check(&mut self, session_id: &str, node_id: &str, checked: bool) -> Result<Value, String> {
        let session = self.sessions.get_mut(session_id).ok_or_else(|| "Session not found".to_string())?;
        let node = session.nodes.get_mut(node_id).ok_or_else(|| "Node not found".to_string())?;
        node.check = Some(checked);
        Ok(node_to_dto(node))
    }

    pub fn patch_value(&mut self, session_id: &str, node_id: &str, current: f64) -> Result<Value, String> {
        let session = self.sessions.get_mut(session_id).ok_or_else(|| "Session not found".to_string())?;
        let node = session.nodes.get_mut(node_id).ok_or_else(|| "Node not found".to_string())?;
        if let Some((min, max, step, _)) = node.value {
            node.value = Some((min, max, step, current));
        }
        Ok(node_to_dto(node))
    }

    pub fn list(&self) -> Value {
        json!({
            "sessions": self.sessions.values().map(|s| json!({
                "sessionId": s.id,
                "action": s.action,
            })).collect::<Vec<_>>()
        })
    }
}

fn page_size_of(cfg: &MenuGlobalSettings) -> usize {
    if cfg.quick_load_submenu {
        10_000
    } else {
        cfg.max_single_load_count.clamp(5, 50) as usize
    }
}

pub fn style_from_config(cfg: &MenuGlobalSettings) -> Value {
    json!({
        "menuFontSize": cfg.menu_font_size,
        "iconSize": icon_size_from_font(cfg.menu_font_size),
        "menuMaxWidth": cfg.menu_max_width,
        "showMenuLocation": show_menu_location_code(&cfg.show_menu_location),
        "windowThumbnail": cfg.show_window_thumbnail,
    })
}

/// C# `GlobalMenuConfig.IconSize` = `Math.Max(20, MenuFontSize * 1.3)`.
pub fn icon_size_from_font(menu_font_size: f64) -> f64 {
    (menu_font_size * 1.3).max(20.0)
}

fn show_menu_location_code(value: &str) -> i32 {
    match value.to_ascii_lowercase().as_str() {
        "centerscreen" => 5,
        "mousearound" => 10,
        "mouserightbottom" => 11,
        _ => 0,
    }
}

fn page_from_ids(
    nodes: &HashMap<String, MenuItemNode>,
    ids: &[String],
    message: &str,
    has_more: bool,
    next_cursor: Option<&str>,
) -> Value {
    let items: Vec<Value> = ids.iter().filter_map(|id| nodes.get(id).map(node_to_dto)).collect();
    let status = if items.is_empty() {
        if message.is_empty() {
            "Empty"
        } else {
            "Error"
        }
    } else {
        "Ready"
    };
    json!({
        "items": items,
        "status": status,
        "message": message,
        "hasMore": has_more,
        "nextCursor": next_cursor,
    })
}

fn node_to_dto(node: &MenuItemNode) -> Value {
    let mut dto = json!({
        "id": node.id,
        "kind": node.kind,
        "title": node.title,
        "description": node.description,
        "icon": node.icon,
        "isEnabled": node.is_enabled,
        "invokeMode": node.invoke_mode,
        "keepOpenOnInvoke": node.keep_open_on_invoke,
        "hasChildren": node.has_children,
        "hasContextMenu": node.has_context_menu,
        "role": node.role,
        "payload": node.payload,
    });
    if let Some(checked) = node.check {
        dto["check"] = json!({ "isChecked": checked });
    }
    if let Some((min, max, step, current)) = node.value {
        dto["value"] = json!({ "min": min, "max": max, "step": step, "current": current });
    }
    if let Some(visual) = &node.visual {
        dto["visual"] = visual.clone();
    }
    dto
}

fn blank_node() -> MenuItemNode {
    MenuItemNode {
        id: Uuid::new_v4().simple().to_string(),
        kind: "Command".into(),
        title: String::new(),
        description: String::new(),
        icon: String::new(),
        is_enabled: true,
        invoke_mode: "Invoke".into(),
        keep_open_on_invoke: false,
        has_children: false,
        has_context_menu: false,
        role: None,
        payload: Value::Null,
        child_ids: Vec::new(),
        context_ids: Vec::new(),
        check: None,
        value: None,
        visual: None,
        lazy: None,
        pending: Vec::new(),
        served: 0,
    }
}

pub fn command(title: impl Into<String>, role: &str, payload: Value) -> MenuItemNode {
    let mut node = blank_node();
    node.title = title.into();
    node.role = Some(role.into());
    node.payload = payload;
    node
}

fn command_icon(title: impl Into<String>, role: &str, payload: Value, icon: &str) -> MenuItemNode {
    let mut node = command(title, role, payload);
    node.icon = icon.into();
    node
}

fn keep_open(mut node: MenuItemNode) -> MenuItemNode {
    node.keep_open_on_invoke = true;
    node
}

fn separator() -> MenuItemNode {
    let mut node = blank_node();
    node.kind = "Separator".into();
    node.is_enabled = false;
    node.invoke_mode = "None".into();
    node
}

fn setting_item(page: &str) -> MenuItemNode {
    command_icon(
        match page {
            "menu_file" => "文件菜单设置",
            "menu_global" | "menu" | "menu_window" => "全局菜单设置",
            "textAction" | "textaction" => "文本操作设置",
            _ => "设置",
        },
        "setting",
        json!({ "page": page }),
        "fa:Light_Cog",
    )
}

fn folder_lazy(title: impl Into<String>, role: &str, payload: Value, icon: &str, lazy: LazyKind) -> MenuItemNode {
    let mut node = blank_node();
    node.kind = "Folder".into();
    node.title = title.into();
    node.icon = icon.into();
    // C# `HostInvokeMode` default for folders: `GetClickRun` is false → expand only (`None`).
    node.invoke_mode = "None".into();
    node.keep_open_on_invoke = true;
    node.has_children = true;
    node.role = Some(role.into());
    node.payload = payload;
    node.lazy = Some(lazy);
    node
}

fn file_node(path: &str, cfg: &FileMenuSettings) -> MenuItemNode {
    file_node_with_status(path, cfg, file_menu_extract::menu_file_path_status(path))
}

fn file_node_from_snapshot(entry: file_menu_extract::FileMenuEntry, cfg: &FileMenuSettings) -> MenuItemNode {
    let path = entry.path.to_string_lossy();
    let Some(metadata) = entry.metadata.as_ref() else {
        // Non-Windows enumeration can retain an entry whose metadata is unavailable.
        return file_node(&path, cfg);
    };
    let is_dir = metadata.is_dir();
    let title = file_menu_extract::file_name_for_type(&path, is_dir, cfg.show_extension);
    let icon = if is_dir || path.trim().is_empty() {
        "icon:.".into()
    } else {
        file_menu_extract::file_icon_spec_without_probe(&path)
    };
    let hidden = file_menu_extract::hidden_from_metadata(&path, metadata);
    file_node_for_present_path(&path, is_dir, title, icon, hidden)
}

fn file_node_with_status(
    path: &str,
    cfg: &FileMenuSettings,
    status: file_menu_extract::MenuPathStatus,
) -> MenuItemNode {
    use file_menu_extract::MenuPathStatus;
    if crate::explorer::is_shell_path(path) {
        return shell_location_node(path, "file", json!({ "path": path }));
    }
    // Keep a timed-out path actionable without doing any more synchronous I/O.
    // Its file/directory presentation can be resolved when the root is reloaded.
    if status == MenuPathStatus::Unknown
        || (status == MenuPathStatus::Missing && file_menu_extract::is_network_path(path))
    {
        let title = file_menu_extract::path_get_file_name(path);
        let icon = file_menu_extract::file_icon_spec_without_probe(path);
        let mut node = command_icon(title, "file", json!({ "path": path }), &icon);
        node.description = path.to_string();
        node.has_context_menu = true;
        if status == MenuPathStatus::Missing {
            node.is_enabled = false;
            node.invoke_mode = "None".into();
        }
        return node;
    }
    // C# `MenuFactory.GetFileMenuWrapper`: missing local path is a disabled empty item.
    if status == MenuPathStatus::Missing {
        // C# `GetEmptyMenuWrapper("", Path.GetFileName(path))`.
        let title = file_menu_extract::path_get_file_name(path);
        let mut node = blank_node();
        node.title = if title.is_empty() {
            path.to_string()
        } else {
            title.to_string()
        };
        node.is_enabled = false;
        node.invoke_mode = "None".into();
        node.has_context_menu = false;
        return node;
    }
    let is_dir = file_menu_extract::is_directory(path);
    let title = file_menu_extract::extra_file_name(path, cfg.show_extension);
    let icon = file_menu_extract::shell_icon_spec(path);
    let hidden = file_menu_extract::is_hidden(path);
    file_node_for_present_path(path, is_dir, title, icon, hidden)
}

fn file_node_for_present_path(
    path: &str, is_dir: bool, title: String, icon: String, hidden: bool,
) -> MenuItemNode {
    if is_dir && file_menu_extract::has_sub_entries(path) {
        let mut node = folder_lazy(
            title,
            "file",
            json!({ "path": path, "isDirectory": true }),
            &icon,
            LazyKind::FileDir { path: path.to_string() },
        );
        // C# `FileMenuItemWrapper.GetClickRun` is true → `InvokeAndExpand`.
        node.invoke_mode = "InvokeAndExpand".into();
        node.has_context_menu = true;
        node.description = path.to_string();
        if hidden {
            node.visual = Some(json!({ "opacity": 0.5 }));
        }
        node
    } else {
        let mut node = command_icon(title, "file", json!({ "path": path, "isDirectory": is_dir }), &icon);
        node.has_context_menu = true;
        node.description = path.to_string();
        if hidden {
            node.visual = Some(json!({ "opacity": 0.5 }));
        }
        node
    }
}

fn apply_file_menu_enabled(node: &mut MenuItemNode, path: &str) {
    if file_menu_extract::file_menu_item_enabled(path) {
        return;
    }
    node.is_enabled = false;
    node.invoke_mode = "None".into();
}

fn shell_location_node(path: &str, role: &str, payload: Value) -> MenuItemNode {
    let title = file_menu_extract::extra_file_name(path, true);
    let icon = file_menu_extract::shell_icon_spec(path);
    let mut node = if crate::explorer::is_this_pc(path) {
        let mut node = folder_lazy(title, role, payload, &icon, LazyKind::ThisPc);
        node.invoke_mode = "InvokeAndExpand".into();
        node
    } else {
        command_icon(title, role, payload, &icon)
    };
    node.has_context_menu = true;
    node.description = path.to_string();
    node
}

/// C# `OpenedFolderMenuItemWrapper : FileMenuItemWrapper` — ExtraFileName title,
/// HasSubEntries expand, HostPayload path + HWND so invoke can `WinProperty.Show`.
fn opened_node(folder: &OpenedFolder, cfg: &FileMenuSettings) -> MenuItemNode {
    let path = if folder.path.is_empty() {
        folder.title.clone()
    } else {
        folder.path.clone()
    };
    if path.trim().is_empty() {
        let mut node = command_icon(
            folder.title.clone(),
            "opened-folder",
            folder.payload(),
            "fa:Light_FolderOpen",
        );
        node.has_context_menu = true;
        return node;
    }
    if crate::explorer::is_shell_path(&path) {
        return shell_location_node(&path, "opened-folder", folder.payload());
    }
    let title = file_menu_extract::extra_file_name(&path, cfg.show_extension);
    let icon = file_menu_extract::shell_icon_spec(&path);
    let hidden = file_menu_extract::is_hidden(&path);
    let mut node = if file_menu_extract::is_directory(&path) && file_menu_extract::has_sub_entries(&path)
    {
        folder_lazy(
            title,
            "opened-folder",
            folder.payload(),
            &icon,
            LazyKind::FileDir { path: path.clone() },
        )
    } else {
        command_icon(title, "opened-folder", folder.payload(), &icon)
    };
    node.has_context_menu = true;
    node.description = path.clone();
    if matches!(node.lazy, Some(LazyKind::FileDir { .. })) {
        // C# `OpenedFolderMenuItemWrapper.GetClickRun` is true.
        node.invoke_mode = "InvokeAndExpand".into();
    }
    if hidden {
        node.visual = Some(json!({ "opacity": 0.5 }));
    }
    apply_file_menu_enabled(&mut node, &path);
    node
}

fn window_node(info: &WindowInfo) -> MenuItemNode {
    let mut node = folder_lazy(
        info.menu_title(),
        "window",
        info.payload(),
        &info.icon(),
        LazyKind::WindowOps { hwnd: info.hwnd },
    );
    // A primary click activates this window; hover still exposes its submenu.
    node.invoke_mode = "InvokeAndExpand".into();
    node.description = info.title.clone();
    node.has_context_menu = true;
    // GPUI draws the plain title with state chips; `title` keeps the C# prefixes.
    node.visual = Some(json!({ "window": window_visual(info) }));
    node
}

/// Row and submenu-header data for the native window menu (2026-10-23 design).
fn window_visual(info: &WindowInfo) -> Value {
    json!({
        "title": info.short_title(),
        "app": info.app_name(),
        "exe": info.exe_file_name(),
        "pid": info.pid,
        "foreground": info.is_foreground,
        "topmost": info.topmost,
        "minimized": info.minimized,
        "maximized": info.maximized,
        "opacity": (f64::from(info.opacity) * 100.0 / 255.0).round() as i64,
        "penetrate": info.mouse_penetration,
    })
}

fn window_group_node(wins: &[WindowInfo]) -> MenuItemNode {
    let focus = crate::window_menu::last_focus_window(wins).unwrap_or(&wins[0]);
    let hwnds: Vec<i64> = wins.iter().map(|w| w.hwnd).collect();
    let mut node = folder_lazy(
        focus.group_title(),
        "window-group",
        json!({ "windowHandle": focus.hwnd }),
        &focus.icon(),
        LazyKind::WindowGroup { hwnds },
    );
    // Use the group's existing last-focus target for the same primary action.
    node.invoke_mode = "InvokeAndExpand".into();
    node.description = focus.title.clone();
    node.has_context_menu = true;
    node.visual = Some(json!({ "windowGroup": {
        "app": focus.app_name(),
        "count": wins.len(),
        "recent": focus.short_title(),
        "foreground": wins.iter().any(|win| win.is_foreground),
    } }));
    node
}

pub fn file_root(ctx: &MenuBuildContext) -> (Vec<MenuItemNode>, HashMap<String, MenuItemNode>, String) {
    let mut items = Vec::new();
    // 2026-10-26: the docked browser opens from a row's ⇢ / context menu;
    // the root keeps only locations and maintenance entries.
    items.push(file_add_dir_node());
    items.push(file_add_opened_root_node());
    if let Some(path) = ctx.clipboard_path.as_deref().filter(|p| !p.is_empty()) {
        items.push(file_add_clipboard_node(path));
    }
    items.push(separator());

    if ctx.file_cfg.expand_opened_folder {
        for folder in &ctx.opened {
            items.push(opened_node(folder, &ctx.file_cfg));
        }
        if !ctx.opened.is_empty() {
            items.push(separator());
        }
    } else {
        items.push(folder_lazy(
            "打开的文件夹",
            "opened-folder",
            json!({}),
            "fa:Light_FolderOpen",
            LazyKind::OpenedFolders,
        ));
    }

    for entry in ctx.file_menu.custom_entries() {
        match entry {
            CustomEntry::Separator => items.push(separator()),
            CustomEntry::Path(path) => {
                let mut node = file_node(&path, &ctx.file_cfg);
                // A missing saved path still needs its configuration context menu.
                // Keep primary invocation disabled until the path is available.
                node.role = Some("file".into());
                if !node.payload.is_object() { node.payload = json!({}); }
                node.payload["path"] = json!(path);
                node.has_context_menu = true;
                items.push(node);
            }
        }
    }
    for path in &ctx.extra_files {
        items.push(file_node(path, &ctx.file_cfg));
    }
    if !ctx.file_menu.paths().is_empty() || !ctx.extra_files.is_empty() {
        items.push(separator());
    }

    for drive in file_menu_extract::list_drives() {
        items.push(file_node(&drive, &ctx.file_cfg));
    }
    if !file_menu_extract::list_drives().is_empty() {
        items.push(separator());
    }

    items.push(recent_files_root_node());
    items.push(separator());
    items.push(setting_item("menu_file"));
    (items, HashMap::new(), String::new())
}

pub fn window_root(ctx: &MenuBuildContext, include_setting: bool) -> Vec<MenuItemNode> {
    let mut items = Vec::new();
    let listed = if include_setting {
        // C# `MenuVerbAction.window`: `withMouse: true` and always append global setting.
        let target = if ctx.target_window_handle != 0 {
            ctx.target_window_handle
        } else {
            window_menu::cursor_root_hwnd()
        };
        window_menu::list_windows_with_mouse(ctx.skip_hwnd, ctx.menu_cfg.grouping, target)
    } else {
        window_menu::list_windows(ctx.skip_hwnd, ctx.menu_cfg.grouping)
    };
    for entry in listed {
        match entry {
            WindowEntry::Single(info) => items.push(window_node(&info)),
            WindowEntry::Group(wins) => items.push(window_group_node(&wins)),
        }
    }
    if include_setting {
        items.push(separator());
        items.push(setting_item("menu_global"));
    }
    items
}

pub fn favor_root(ctx: &MenuBuildContext) -> (Vec<MenuItemNode>, HashMap<String, MenuItemNode>, String) {
    let mut items = Vec::new();
    let mut extra = HashMap::new();
    let matched = ctx.favor_menu.match_configs(&ctx.target, &ctx.favors);
    let root = matched
        .first()
        .and_then(|cfg| cfg.resolve_favor(&ctx.favors))
        .or_else(|| ctx.favor_menu.default_favorite(&ctx.favors));

    if ctx.target.is_save_as() {
        items.push(opened_folder_root_node(&ctx.opened, &ctx.reopen_paths));
        items.push(recent_files_root_node());
    } else {
        if ctx.menu_cfg.show_adder_menu_item {
            let mut adder = folder_lazy(
                "添加选中",
                "favor-add",
                json!({"parentId": root.map(|item| &item.id)}),
                "fa:Light_PlusCircle",
                LazyKind::FavorAdder,
            );
            adder.description = "剪贴板或已打开的页面".into();
            adder.has_context_menu = true;
            items.push(adder);
        }
        if ctx.menu_cfg.show_window_menu_item {
            if ctx.menu_cfg.expand_window_menu {
                items.extend(window_root(ctx, false));
            } else {
                let fg = window_menu::foreground_hwnd() as i64;
                let mut node = folder_lazy(
                    "窗口管理",
                    "window",
                    json!({ "windowHandle": fg, "root": true }),
                    "fa:Light_WindowRestore",
                    LazyKind::WindowGroup { hwnds: Vec::new() },
                );
                node.has_context_menu = true;
                items.push(node);
            }
        }
    }

    if ctx.menu_cfg.show_book_marks_menu {
        items.push(bookmark_session_root(ctx.bookmarks.as_ref()));
    }

    if let Some(root) = root {
        if matched.len() > 1 {
            for cfg in &matched {
                let Some(item) = cfg.resolve_favor(&ctx.favors) else {
                    continue;
                };
                let (nodes, more) = favor_config_nodes(item, &ctx.favors, ctx, cfg);
                extra.extend(more);
                items.extend(nodes);
            }
        } else {
            let (children, more) = favor_children_of(&ctx.favors, &root.id, ctx);
            extra.extend(more);
            items.extend(children);
        }
    }

    if ctx.menu_cfg.show_setting_item {
        items.push(separator());
        items.push(setting_item("menu_global"));
    }
    (items, extra, String::new())
}

fn opened_folder_root_node(opened: &[OpenedFolder], reopen_paths: &[String]) -> MenuItemNode {
    let mut node = folder_lazy(
        "打开的文件夹",
        "opened-folder",
        json!({}),
        "fa:Light_FolderOpen",
        LazyKind::OpenedFolders,
    );
    node.description = opened
        .first()
        .map(|f| f.path.clone())
        .filter(|p| !p.is_empty())
        .or_else(|| reopen_paths.first().cloned())
        .unwrap_or_default();
    node
}

fn recent_files_root_node() -> MenuItemNode {
    let mut node = folder_lazy(
        "最近使用的文件",
        "recent-files",
        json!({ "path": file_menu_extract::recent_folder().to_string_lossy() }),
        "https://files.getquicker.net/_icons/10C23A09F0F68CFFD19515E9177AEE476940C0CB.svg",
        LazyKind::RecentFiles,
    );
    node.description = "最近使用的文件或者文件夹".into();
    // C# `RecentFileMenuItemWrapper.CanExecute` is false → `HostInvokeMode.None`.
    node.invoke_mode = "None".into();
    node.keep_open_on_invoke = false;
    node
}

fn file_add_dir_node() -> MenuItemNode {
    let mut node = keep_open(command_icon(
        "添加目录…",
        "file-add-dir",
        json!({}),
        "fa:Light_FolderPlus",
    ));
    node.description = "选择文件夹写入文件菜单自定义列表（与收藏无关）".into();
    node
}

fn file_add_opened_root_node() -> MenuItemNode {
    let mut node = folder_lazy(
        "添加已打开的文件夹…",
        "file-add-opened-root",
        json!({}),
        "fa:Light_Folders",
        LazyKind::AddOpenedFolders,
    );
    node.keep_open_on_invoke = true;
    node.description = "从当前资源管理器窗口挑选目录加入自定义列表".into();
    node
}

fn file_add_clipboard_node(path: &str) -> MenuItemNode {
    let mut node = keep_open(command_icon(
        "粘贴路径添加",
        "file-add-clipboard",
        json!({ "path": path }),
        "fa:Light_Clipboard",
    ));
    node.description = "将剪贴板中的路径加入文件菜单自定义列表".into();
    node
}

/// C# `FileMenuCustomPathActions.GetAddOpenedFolderMenuItems`.
fn add_opened_folder_nodes(opened: &[OpenedFolder], custom_paths: &[String]) -> Vec<MenuItemNode> {
    if opened.is_empty() {
        let mut empty = command_icon(
            "（当前没有打开的文件夹）",
            "file-add-opened-empty",
            json!({}),
            "fa:Light_InfoCircle",
        );
        empty.keep_open_on_invoke = true;
        empty.invoke_mode = "None".into();
        return vec![empty];
    }
    opened
        .iter()
        .map(|folder| {
            let already = custom_paths
                .iter()
                .any(|p| p.eq_ignore_ascii_case(&folder.path));
            let base = add_opened_folder_title(&folder.path);
            let title = if already {
                format!("已添加 · {base}")
            } else {
                base
            };
            let icon = if already {
                "fa:Light_Check"
            } else {
                "fa:Light_FolderPlus"
            };
            let mut node = keep_open(command_icon(
                title,
                "file-add-opened",
                folder.payload(),
                icon,
            ));
            node.description = folder.path.clone();
            node
        })
        .collect()
}

/// C# `Path.GetFileName(path.TrimEnd('\\', '/'))`, fallback to the raw path.
fn add_opened_folder_title(path: &str) -> String {
    if crate::explorer::is_shell_path(path) {
        return file_menu_extract::extra_file_name(path, true);
    }
    let trimmed = path.trim_end_matches(['\\', '/']);
    let name = file_menu_extract::path_get_file_name(trimmed);
    if name.is_empty() {
        path.to_string()
    } else {
        name.to_string()
    }
}

fn favor_config_nodes(
    item: &FavorRecord,
    favors: &[FavorRecord],
    ctx: &MenuBuildContext,
    cfg: &FavorMenuConfigItem,
) -> (Vec<MenuItemNode>, HashMap<String, MenuItemNode>) {
    if cfg.always_expand || !item.is_folder {
        if item.is_folder {
            return favor_children_of(favors, &item.id, ctx);
        }
        return (favor_leaf_nodes(item, ctx), HashMap::new());
    }
    let (children, extra) = favor_children_of(favors, &item.id, ctx);
    let child_ids: Vec<String> = children.iter().map(|c| c.id.clone()).collect();
    let mut extra = extra;
    for child in children {
        extra.insert(child.id.clone(), child);
    }
    (vec![favor_folder_node(item, child_ids, &ctx.user_icon)], extra)
}

fn reopen_root_node(paths: &[String]) -> MenuItemNode {
    let last = paths
        .iter()
        .map(|s| s.trim())
        .find(|s| !s.is_empty())
        .unwrap_or("");
    let title = if last.is_empty() {
        "重开".to_string()
    } else {
        format!("重开：{}", add_opened_folder_title(last))
    };
    let mut node = folder_lazy(
        title,
        "reopen-root",
        json!({ "path": last }),
        "fa:Light_ArrowAltToLeft",
        LazyKind::ReopenFolders,
    );
    node.has_context_menu = true;
    node.is_enabled = true;
    node.has_children = true;
    node.invoke_mode = if last.is_empty() { "Expand" } else { "InvokeAndExpand" }.into();
    node
}

pub fn reopen_items(paths: &[String]) -> Vec<MenuItemNode> {
    paths
        .iter()
        .filter(|p| !p.trim().is_empty())
        .map(|path| {
            if crate::explorer::is_shell_path(path) {
                return shell_location_node(path, "reopen", json!({ "path": path }));
            }
            let title = file_menu_extract::path_get_file_name(path).to_string();
            let mut node = command(title, "reopen", json!({ "path": path }));
            // History is editable even while its volume is offline. Opening
            // reports its own error; building a list must not probe storage.
            node.has_context_menu = true;
            node
        })
        .collect()
}

/// C# `MenuVerbAction.reopen`: `GetAllOpenedFolder().AppendSeperator().Concat(GetAllReOpenMenus())`.
fn reopen_verb_root(opened: &[OpenedFolder], reopen_paths: &[String], versions: &HashMap<String, i64>, undo: Option<&Value>) -> Vec<MenuItemNode> {
    let mut items: Vec<MenuItemNode> = opened
        .iter()
        .filter(|folder| opened_tab_is_live_directory(folder))
        .map(|folder| opened_node(folder, &FileMenuSettings::default()))
        .collect();
    if !items.is_empty() {
        items.push(separator());
    }
    items.extend(reopen_menu_items(reopen_paths, versions, undo));
    items
}

fn reopen_menu_items(paths: &[String], versions: &HashMap<String, i64>, undo: Option<&Value>) -> Vec<MenuItemNode> {
    let mut items = reopen_items(paths);
    for node in &mut items {
        if let Some(path) = node.payload.get("path").and_then(Value::as_str) {
            if let Some(recorded_at) = versions.get(path) { node.payload["recordedAt"] = json!(recorded_at); }
        }
    }
    if !items.is_empty() { items.push(separator()); }
    if let Some(undo) = undo {
        let mut item = command("撤销移除", "reopen-undo", json!({ "undo": undo }));
        item.keep_open_on_invoke = true;
        items.push(item);
    }
    items.push(command("管理重开历史…", "reopen-manage", json!({})));
    items
}

/// C# `GetAllOpenedFolderFromTabs`: skip empty path, `Directory.Exists` (not shell / files).
fn opened_tab_is_live_directory(folder: &OpenedFolder) -> bool {
    opened_tab_is_live_directory_with(folder, file_menu_extract::menu_directory_path_status)
}

fn opened_tab_is_live_directory_with(
    folder: &OpenedFolder,
    probe: impl FnOnce(&str) -> file_menu_extract::MenuPathStatus,
) -> bool {
    if folder.hwnd == 0 {
        return false;
    }
    let path = folder.path.trim();
    if path.is_empty() || crate::explorer::is_shell_path(path) {
        return false;
    }
    probe(path) != file_menu_extract::MenuPathStatus::Missing
}

pub fn text_root(actions: &[Value]) -> (Vec<MenuItemNode>, HashMap<String, MenuItemNode>, String) {
    let mut named: Vec<(String, Vec<(usize, &Value)>)> = Vec::new();
    let mut ungrouped = Vec::new();
    for (index, action) in actions.iter().enumerate() {
        // C# `GroupBy(x => x.Group)` does not trim; only "" is ungrouped.
        let group = action
            .get("Group")
            .or_else(|| action.get("group"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if group.is_empty() {
            ungrouped.push((index, action));
        } else if let Some((_, bucket)) = named.iter_mut().find(|(name, _)| name == &group) {
            bucket.push((index, action));
        } else {
            named.push((group, vec![(index, action)]));
        }
    }
    let mut extra = HashMap::new();
    let mut items = Vec::new();
    for (name, group) in &named {
        items.push(text_group_node(name, group, &mut extra));
    }
    if named.len() < 5 {
        items.extend(ungrouped.iter().map(|(index, action)| text_action_node(*index, action)));
    } else if !ungrouped.is_empty() {
        items.push(text_group_node("*未分组*", &ungrouped, &mut extra));
    }
    items.push(separator());
    items.push(setting_item("textAction"));
    (items, extra, String::new())
}

fn text_group_node(
    title: &str,
    actions: &[(usize, &Value)],
    extra: &mut HashMap<String, MenuItemNode>,
) -> MenuItemNode {
    let children: Vec<MenuItemNode> = actions
        .iter()
        .map(|(index, action)| text_action_node(*index, action))
        .collect();
    let child_ids: Vec<String> = children.iter().map(|c| c.id.clone()).collect();
    for child in children {
        extra.insert(child.id.clone(), child);
    }
    let mut node = blank_node();
    node.kind = "Folder".into();
    node.title = title.to_string();
    node.icon = "fa:Light_Text".into();
    // C# `GroupedTextActionMenuItemWrapper.CanExecute` is false → `HostInvokeMode.None`.
    node.invoke_mode = "None".into();
    node.keep_open_on_invoke = false;
    node.has_children = !child_ids.is_empty();
    node.has_context_menu = false;
    node.role = Some("text".into());
    node.payload = json!({ "group": title });
    node.child_ids = child_ids;
    node
}

fn text_action_node(index: usize, action: &Value) -> MenuItemNode {
    let mut node = command(
        crate::text_actions::visual_title(action),
        "text",
        json!({ "actionId": index.to_string() }),
    );
    node.has_context_menu = true;
    node.description = crate::text_actions::visual_tips(action);
    node
}

fn bookmark_node(node: &BookmarkNode) -> MenuItemNode {
    if node.is_folder() {
        let mut item = folder_lazy(
            if node.title.is_empty() { "书签".into() } else { node.title.clone() },
            "bookmark-folder",
            json!({ "url": node.url, "id": node.id, "title": node.title }),
            &node.icon(),
            LazyKind::BookmarkFolder {
                children: node.children.clone(),
            },
        );
        item.has_children = !node.children.is_empty();
        item.has_context_menu = false;
        item.description = node.url.clone();
        item
    } else {
        let mut item = command_icon(
            node.title.clone(),
            "bookmark",
            json!({ "url": node.url, "id": node.id, "title": node.title }),
            &node.icon(),
        );
        item.has_context_menu = true;
        item.description = node.url.clone();
        item
    }
}

fn bookmark_children(node: &BookmarkNode) -> (Vec<MenuItemNode>, HashMap<String, MenuItemNode>) {
    (node.children.iter().map(bookmark_node).collect(), HashMap::new())
}

pub fn favor_tree(favors: &[FavorRecord]) -> (Vec<MenuItemNode>, HashMap<String, MenuItemNode>) {
    favor_tree_with(favors, &FileMenuSettings::default())
}

pub fn favor_tree_with(
    favors: &[FavorRecord],
    file_cfg: &FileMenuSettings,
) -> (Vec<MenuItemNode>, HashMap<String, MenuItemNode>) {
    let ctx = MenuBuildContext {
        file_cfg: file_cfg.clone(),
        favors: favors.to_vec(),
        ..MenuBuildContext::default()
    };
    let mut extra = HashMap::new();
    let roots = build_favor_level(&by_parent_map(favors), None, &mut extra, &ctx);
    (roots, extra)
}

fn by_parent_map(favors: &[FavorRecord]) -> HashMap<Option<String>, Vec<&FavorRecord>> {
    let mut by_parent: HashMap<Option<String>, Vec<&FavorRecord>> = HashMap::new();
    for item in favors {
        by_parent.entry(item.parent_id.clone()).or_default().push(item);
    }
    by_parent
}

fn favor_command_node(item: &FavorRecord, user_icon: &UserIcon) -> MenuItemNode {
    let mut node = command(item.visual_title(), "favor", json!({ "favorId": item.id }));
    node.has_context_menu = true;
    node.icon = item.host_icon_with(user_icon);
    node.description = item.summary();
    node
}

/// C# `LinkMenuItemWrapper` icon: `VisualIcon` else favicon from absolute `Uri.Authority`, else `fa:Light_Link`.
fn link_menu_item_icon(url: &str, visual_icon: &str) -> String {
    if !visual_icon.trim().is_empty() {
        return visual_icon.to_string();
    }
    match url.split_once("://") {
        Some((scheme, rest)) if !scheme.is_empty() && !rest.is_empty() => {}
        _ => return "fa:Light_Link".into(),
    }
    crate::bookmarks::url_authority(url)
        .map(|host| format!("https://helperservice.getquicker.cn/favicon/get/{host}"))
        .unwrap_or_else(|| "fa:Light_Link".into())
}

/// C# `FavorMenuItemWrapper.GetChildMenuItems`: FileClipItem → file wrappers,
/// LinkClipItem → `LinkMenuItemWrapper`, ActionClipItem template → menu roots,
/// else favor items.
fn favor_leaf_nodes(item: &FavorRecord, ctx: &MenuBuildContext) -> Vec<MenuItemNode> {
    match item.kind {
        ClipKind::FileClipItem => {
            let paths = if item.file_paths.is_empty() {
                item.body_text
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| vec![s.to_string()])
                    .unwrap_or_default()
            } else {
                item.file_paths.clone()
            };
            if paths.is_empty() {
                return vec![favor_command_node(item, &ctx.user_icon)];
            }
            paths.iter().map(|path| file_node(path, &ctx.file_cfg)).collect()
        }
        ClipKind::LinkClipItem => {
            let url = item.body_text.clone().unwrap_or_default();
            // C# `new LinkMenuItemWrapper(item.VisualTitle, l_item.Url, item.VisualIcon)`.
            let title = item.visual_title();
            let icon = link_menu_item_icon(&url, &item.icon);
            let mut node = command_icon(
                title.clone(),
                "favor-link",
                json!({ "favorId": item.id, "url": url, "title": title }),
                &icon,
            );
            node.has_context_menu = true;
            node.description = url;
            vec![node]
        }
        ClipKind::ActionClipItem => {
            if let Some(node) = favor_action_template_node(item, ctx) {
                vec![node]
            } else {
                vec![favor_command_node(item, &ctx.user_icon)]
            }
        }
        _ => vec![favor_command_node(item, &ctx.user_icon)],
    }
}

fn favor_action_template_node(item: &FavorRecord, ctx: &MenuBuildContext) -> Option<MenuItemNode> {
    let action_id = item.body_text.as_deref().unwrap_or("");
    match crate::quicker_action::payload_menu_action_template(action_id, &item.execute_param) {
        Some(crate::quicker_action::MenuActionTemplate::Window) => Some(window_manage_folder_node()),
        Some(crate::quicker_action::MenuActionTemplate::Bookmarks) => {
            Some(bookmark_root_folder_node(ctx, item))
        }
        Some(crate::quicker_action::MenuActionTemplate::File) => Some(file_menu_root_folder_node(item)),
        Some(crate::quicker_action::MenuActionTemplate::Reopen) => {
            let mut node = reopen_root_node(&ctx.reopen_paths);
            if !item.name.trim().is_empty() {
                node.title = item.name.clone();
            }
            Some(node)
        }
        Some(crate::quicker_action::MenuActionTemplate::Text) => Some(text_menu_root_folder_node(item)),
        Some(crate::quicker_action::MenuActionTemplate::Adder) => {
            let parent = item.parent_id.clone().unwrap_or_default();
            let mut node = folder_lazy(
                "添加选中",
                "favor-add",
                json!({ "favorId": parent }),
                "fa:Light_PlusCircle",
                LazyKind::FavorAdder,
            );
            node.description = "剪贴板或已打开的页面".into();
            node.has_context_menu = true;
            Some(node)
        }
        None => None,
    }
}

fn window_manage_folder_node() -> MenuItemNode {
    let fg = window_menu::foreground_hwnd() as i64;
    let mut node = folder_lazy(
        "窗口管理",
        "window",
        json!({ "windowHandle": fg, "root": true }),
        "fa:Light_WindowRestore",
        LazyKind::WindowGroup { hwnds: Vec::new() },
    );
    node.has_context_menu = true;
    node
}

fn bookmark_root_folder_node(ctx: &MenuBuildContext, item: &FavorRecord) -> MenuItemNode {
    let mut node = bookmark_session_root(ctx.bookmarks.as_ref());
    if !item.name.trim().is_empty() {
        node.title = item.name.clone();
    }
    node
}

/// C# `ChromeBookMarkRootMenuItemWrapper`: HostRole is always `bookmark`, ICON_EARTH when icon empty.
fn bookmark_session_root(tree: Option<&BookmarkNode>) -> MenuItemNode {
    const ICON_EARTH: &str =
        "https://files.getquicker.net/_icons/99A6D79D46C1F1E67869507DFA7D7032F6EA63E4.svg";
    if let Some(tree) = tree {
        let mut node = bookmark_node(tree);
        node.role = Some("bookmark".into());
        if node.icon.is_empty() || node.icon == "fa:Light_Folder" {
            node.icon = ICON_EARTH.into();
        }
        node
    } else {
        command_icon("浏览器书签", "bookmark", json!({}), ICON_EARTH)
    }
}

fn file_menu_root_folder_node(item: &FavorRecord) -> MenuItemNode {
    let mut node = folder_lazy(
        if item.name.trim().is_empty() {
            "文件菜单".into()
        } else {
            item.name.clone()
        },
        "file",
        json!({ "root": true }),
        "https://files.getquicker.net/_icons/011D8B5F6BB051200D70A0CEBC95304483C6AADD.svg",
        LazyKind::FileMenuRoot,
    );
    node.has_context_menu = true;
    node
}

fn text_menu_root_folder_node(item: &FavorRecord) -> MenuItemNode {
    let mut node = folder_lazy(
        if item.name.trim().is_empty() {
            "文本替换".into()
        } else {
            item.name.clone()
        },
        "text",
        json!({ "root": true }),
        "fa:Light_Text",
        LazyKind::TextMenuRoot,
    );
    node.has_context_menu = true;
    node
}

fn build_favor_level(
    by_parent: &HashMap<Option<String>, Vec<&FavorRecord>>,
    parent: Option<String>,
    extra: &mut HashMap<String, MenuItemNode>,
    ctx: &MenuBuildContext,
) -> Vec<MenuItemNode> {
    let Some(items) = by_parent.get(&parent) else {
        return Vec::new();
    };
    let mut nodes = Vec::new();
    for item in items {
        if item.is_folder {
            let children = build_favor_level(by_parent, Some(item.id.clone()), extra, ctx);
            let child_ids: Vec<String> = children.iter().map(|c| c.id.clone()).collect();
            for child in children {
                extra.insert(child.id.clone(), child);
            }
            nodes.push(favor_folder_node(item, child_ids, &ctx.user_icon));
        } else {
            nodes.extend(favor_leaf_nodes(item, ctx));
        }
    }
    nodes
}

fn favor_folder_node(item: &FavorRecord, child_ids: Vec<String>, user_icon: &UserIcon) -> MenuItemNode {
    MenuItemNode {
        id: Uuid::new_v4().simple().to_string(),
        kind: "Folder".into(),
        title: item.visual_title(),
        description: item.summary(),
        icon: item.host_icon_with(user_icon),
        is_enabled: true,
        invoke_mode: "None".into(),
        keep_open_on_invoke: true,
        has_children: !child_ids.is_empty(),
        has_context_menu: true,
        role: Some("favor-folder".into()),
        payload: json!({ "favorId": item.id }),
        child_ids,
        context_ids: Vec::new(),
        check: None,
        value: None,
        visual: None,
        lazy: None,
        pending: Vec::new(),
        served: 0,
    }
}

fn favor_children_of(
    favors: &[FavorRecord],
    parent_id: &str,
    ctx: &MenuBuildContext,
) -> (Vec<MenuItemNode>, HashMap<String, MenuItemNode>) {
    let mut extra = HashMap::new();
    let by_parent = by_parent_map(favors);
    if let Some(folder) = favors.iter().find(|f| f.id == parent_id) {
        if !folder.children.is_empty() {
            let mut nodes = Vec::new();
            for id in &folder.children {
                let Some(item) = favors.iter().find(|f| f.id == *id) else {
                    continue;
                };
                if item.is_folder {
                    let children = build_favor_level(&by_parent, Some(item.id.clone()), &mut extra, ctx);
                    let child_ids: Vec<String> = children.iter().map(|c| c.id.clone()).collect();
                    for child in children {
                        extra.insert(child.id.clone(), child);
                    }
                    nodes.push(favor_folder_node(item, child_ids, &ctx.user_icon));
                } else {
                    nodes.extend(favor_leaf_nodes(item, ctx));
                }
            }
            return (nodes, extra);
        }
    }
    let nodes = build_favor_level(&by_parent, Some(parent_id.to_string()), &mut extra, ctx);
    (nodes, extra)
}

fn materialize_lazy(lazy: &LazyKind, bits: &LazySessionBits) -> (Vec<MenuItemNode>, HashMap<String, MenuItemNode>) {
    let file_cfg = &bits.file_cfg;
    let opened = bits.opened.as_slice();
    let reopen_paths = bits.reopen_paths.as_slice();
    let custom_paths = bits.custom_paths.as_slice();
    match lazy {
        LazyKind::FileDir { path } => (
            file_menu_extract::list_entry_snapshots(path, file_cfg)
                .into_iter()
                .map(|entry| file_node_from_snapshot(entry, file_cfg))
                .collect(),
            HashMap::new(),
        ),
        LazyKind::ClipFiles { paths } => (
            paths.iter().map(|path| file_node(path, file_cfg)).collect(),
            HashMap::new(),
        ),
        LazyKind::ThisPc => (
            file_menu_extract::list_drives()
                .into_iter()
                .map(|p| file_node(&p, file_cfg))
                .collect(),
            HashMap::new(),
        ),
        LazyKind::RecentFiles => (
            // C# `WhenReadyAsync(5s)` then `GetRecentFiles()`.
            file_menu_extract::list_recent_files()
                .into_iter()
                .map(|p| file_node(&p, file_cfg))
                .collect(),
            HashMap::new(),
        ),
        LazyKind::OpenedFolders => {
            let mut items: Vec<MenuItemNode> = opened
                .iter()
                .map(|folder| opened_node(folder, file_cfg))
                .collect();
            if !items.is_empty() {
                items.push(separator());
            }
            items.push(reopen_root_node(reopen_paths));
            (items, HashMap::new())
        }
        LazyKind::ReopenFolders => (reopen_menu_items(reopen_paths, &bits.reopen_recorded_at, bits.reopen_undo.as_ref()), HashMap::new()),
        LazyKind::AddOpenedFolders => (add_opened_folder_nodes(opened, custom_paths), HashMap::new()),
        LazyKind::WindowOps { hwnd } => (window_ops(*hwnd), HashMap::new()),
        LazyKind::WindowGroup { hwnds } => {
            if hwnds.is_empty() {
                let listed = window_menu::list_windows(bits.skip_hwnd, bits.menu_cfg.grouping);
                (
                    listed
                        .into_iter()
                        .map(|e| match e {
                            WindowEntry::Single(info) => window_node(&info),
                            WindowEntry::Group(wins) => window_group_node(&wins),
                        })
                        .collect(),
                    HashMap::new(),
                )
            } else {
                (
                    hwnds
                        .iter()
                        .filter_map(|h| window_menu::inspect(*h).map(|info| window_node(&info)))
                        .collect(),
                    HashMap::new(),
                )
            }
        }
        LazyKind::FavorAdder => {
            let parent = favor_parent_payload(&bits.node_payload);
            let mut clipboard = command_icon(
                "从剪贴板添加",
                "favor-add-clipboard",
                parent.clone(),
                "fa:Light_Clipboard",
            );
            clipboard.description = "聚焦上一窗口并 Ctrl+C".into();
            let mut open_tabs = folder_lazy(
                "从打开的页面添加…",
                "favor-add-open-tabs",
                parent.clone(),
                "fa:Light_Globe",
                LazyKind::OpenBrowserTabs,
            );
            open_tabs.description = "需 Quicker 浏览器扩展在线".into();
            (
                vec![
                    clipboard,
                    command_icon("添加当前网址", "favor-add-link", parent, "fa:Light_Link"),
                    open_tabs,
                ],
                HashMap::new(),
            )
        }
        LazyKind::BrowserTabItems { tabs, parent, .. } if !tabs.is_empty() => {
            let items = tabs.iter().map(|tab| {
                let mut payload = parent.clone();
                payload["url"] = json!(tab.url);
                payload["title"] = json!(tab.title);
                payload["icon"] = json!(tab.icon);
                let mut node = command_icon(&tab.title, "favor-add-open-tab", payload,
                    if tab.icon.is_empty() { "fa:Light_Globe" } else { &tab.icon });
                node.description = tab.url.clone();
                node
            }).collect();
            (items, HashMap::new())
        }
        LazyKind::OpenBrowserTabs | LazyKind::BrowserTabItems { .. } => {
            // C# FavorAdderMenuItemWrapper.GetOpenTabMenuItems uses Quicker ChromeControl.
            // Without the plugin the empty-state node matches the catch path.
            let mut empty = command_icon(
                "（无可用标签）",
                "favor-add-open-tabs-empty",
                json!({}),
                "",
            );
            empty.description = match lazy {
                LazyKind::BrowserTabItems { error: Some(error), .. } => error.clone(),
                _ => "确认 Quicker 插件与浏览器扩展已连接".into(),
            };
            empty.payload = json!({"message": empty.description});
            (vec![empty], HashMap::new())
        }
        LazyKind::ContextFile { path } => file_context(path, &[], 0),
        LazyKind::ContextWindow { hwnd } => (window_context(*hwnd), HashMap::new()),
        LazyKind::ContextOpened => (Vec::new(), HashMap::new()),
        LazyKind::BookmarkFolder { children } => (children.iter().map(bookmark_node).collect(), HashMap::new()),
        LazyKind::FileMenuRoot => {
            let (items, extra, _) = file_root(&bits.to_build_context());
            (items, extra)
        }
        LazyKind::TextMenuRoot => {
            let (items, extra, _) = text_root(&bits.text_actions);
            (items, extra)
        }
    }
}

fn window_ops(hwnd: i64) -> Vec<MenuItemNode> {
    let info = window_menu::inspect(hwnd);
    let topmost = info.as_ref().is_some_and(|i| i.topmost);
    let minimized = info.as_ref().is_some_and(|i| i.minimized);
    let maximized = info.as_ref().is_some_and(|i| i.maximized);
    let opacity = info.as_ref().map(|i| i.opacity).unwrap_or(255);
    let explorer = info.as_ref().is_some_and(|i| i.is_explorer_process());
    let penetrate = info.as_ref().is_some_and(|i| i.mouse_penetration);
    // 2026-10-23 order: window flags, then size/position, then destructive commands.
    let mut items = vec![with_icon(check_item("置顶", "window-topmost", hwnd, topmost), "fa:Light_Thumbtack")];
    if !explorer {
        items.push(with_icon(check_item("鼠标穿透", "window-penetrate", hwnd, penetrate), "fa:Light_MousePointer"));
    }
    items.push(separator());
    items.push(with_icon(check_item("最小化", "window-min", hwnd, minimized), "fa:Light_WindowMinimize"));
    items.push(with_icon(
        check_item("最大化", "window-max", hwnd, maximized),
        if maximized { "fa:Light_WindowRestore" } else { "fa:Light_WindowMaximize" },
    ));
    let mut center = command_icon(
        "居中",
        "window-center",
        json!({ "windowHandle": hwnd }),
        "fa:Regular_SquareFull",
    );
    center.description = "移动到当前窗口中心".into();
    items.push(center);
    items.push(with_icon(
        value_item("不透明度", "window-opacity", hwnd, 26.0, 255.0, 1.0, opacity as f64),
        "fa:Light_Adjust",
    ));
    items.push(separator());
    items.push(command_icon(
        "关闭窗口",
        "window-close",
        json!({ "windowHandle": hwnd }),
        "fa:Light_Times",
    ));
    let mut kill = command_icon(
        "结束进程",
        "window-kill",
        json!({ "windowHandle": hwnd }),
        "fa:Light_TimesCircle:red",
    );
    if let Some(info) = info.as_ref().filter(|info| info.pid != 0) {
        kill.description = format!("PID {}", info.pid);
    }
    items.push(kill);
    items
}

fn with_icon(mut node: MenuItemNode, icon: &str) -> MenuItemNode {
    node.icon = icon.into();
    node
}

fn check_item(title: &str, role: &str, hwnd: i64, checked: bool) -> MenuItemNode {
    let mut node = command(title, role, json!({ "windowHandle": hwnd }));
    node.kind = "Check".into();
    node.check = Some(checked);
    node.keep_open_on_invoke = true;
    node
}

fn value_item(title: &str, role: &str, hwnd: i64, min: f64, max: f64, step: f64, current: f64) -> MenuItemNode {
    let mut node = command(title, role, json!({ "windowHandle": hwnd }));
    node.kind = "Value".into();
    node.value = Some((min, max, step, current));
    node.keep_open_on_invoke = true;
    node
}

fn file_context(path: &str, custom_paths: &[String], target_hwnd: i64) -> (Vec<MenuItemNode>, HashMap<String, MenuItemNode>) {
    if crate::explorer::is_shell_path(path) {
        let in_custom = custom_paths.iter().any(|p| p.eq_ignore_ascii_case(path));
        let mut maintenance = keep_open(command_icon(
            if in_custom { "从文件菜单移除" } else { "添加到文件菜单" },
            if in_custom { "file-remove-custom" } else { "file-add-custom" },
            json!({ "path": path }),
            if in_custom { "fa:Light_FolderMinus" } else { "fa:Light_FolderPlus" },
        ));
        maintenance.description = if in_custom {
            "从文件菜单自定义列表移除（与收藏无关）"
        } else {
            "写入文件菜单自定义列表（与收藏无关）"
        }.into();
        let mut items = vec![
            command_icon("复制完整路径", "file-copy-path", json!({ "path": path }), "fa:Light_Copy"),
            separator(),
            maintenance,
        ];
        if crate::explorer::is_this_pc(path) {
            items.insert(0, command_icon("在浏览栏打开", "file-browser", json!({ "path": "" }), "fa:Light_Desktop"));
        }
        return (items, HashMap::new());
    }
    // C# `FileMenuItemWrapper.GetContextMenuItems`: FileExists 50ms + DirectoryExistsWithTimeout 50ms.
    let is_dir = file_menu_extract::context_menu_is_directory(path);
    let is_file = file_menu_extract::context_menu_is_file(path);
    file_context_with_types(path, custom_paths, target_hwnd, is_dir, is_file)
}

fn file_context_with_types(
    path: &str,
    custom_paths: &[String],
    target_hwnd: i64,
    is_dir: bool,
    is_file: bool,
) -> (Vec<MenuItemNode>, HashMap<String, MenuItemNode>) {
    let in_custom = custom_paths.iter().any(|p| p.eq_ignore_ascii_case(path));
    let mut extra = HashMap::new();
    let mut items = vec![
        command_icon("定位文件", "file-locate", json!({ "path": path }), "fa:Light_MapMarker"),
        command_icon("复制文件", "file-copy", json!({ "path": path }), "fa:Light_Copy"),
        command_icon("复制并粘贴", "file-copy-paste", json!({ "path": path }), "fa:Light_Copy"),
        command_icon("剪切文件", "file-cut", json!({ "path": path }), "fa:Light_Cut"),
    ];
    if is_dir {
        items.insert(0, command_icon("在浏览栏打开", "file-browser", json!({ "path": path }), "fa:Light_FolderOpen"));
        // C# `FileMenuItemWrapper` directory context: MoveSelectedFileToDir.
        items.push(command_icon(
            "移动选中文件到此目录",
            "file-move-selected",
            json!({ "path": path }),
            "fa:Light_ArrowAltToLeft",
        ));
    }
    items.push(command_icon("复制完整路径", "file-copy-path", json!({ "path": path }), "fa:Light_Copy"));
    let copy_more_children = vec![
        command("复制带引号路径,例如:\"abc\"", "file-copy-quoted", json!({ "path": path })),
        command("复制文件名", "file-copy-name", json!({ "path": path })),
        command("复制文件名无扩展名", "file-copy-stem", json!({ "path": path })),
        command("复制所在文件夹路径", "file-copy-parent", json!({ "path": path })),
    ];
    let mut copy_more = blank_node();
    copy_more.kind = "Folder".into();
    copy_more.title = "复制更多".into();
    copy_more.icon = "fa:Light_Copy".into();
    copy_more.invoke_mode = "Expand".into();
    copy_more.keep_open_on_invoke = true;
    copy_more.has_children = true;
    copy_more.role = Some("file-copy-more".into());
    copy_more.payload = json!({ "path": path });
    for child in copy_more_children {
        copy_more.child_ids.push(child.id.clone());
        extra.insert(child.id.clone(), child);
    }
    items.push(copy_more);
    if in_custom || is_dir || is_file {
        items.push(separator());
        if in_custom {
            let title = if is_dir {
                "从文件菜单移除目录"
            } else if is_file {
                "从文件菜单移除文件"
            } else {
                "从文件菜单移除"
            };
            let mut node = keep_open(command_icon(
                title,
                "file-remove-custom",
                json!({ "path": path }),
                "fa:Light_FolderMinus",
            ));
            node.description = "从文件菜单自定义列表移除（与收藏无关）".into();
            items.push(node);
        } else {
            let title = if is_dir {
                "添加到文件菜单"
            } else {
                "添加文件到文件菜单"
            };
            let mut node = keep_open(command_icon(
                title,
                "file-add-custom",
                json!({ "path": path }),
                "fa:Light_FolderPlus",
            ));
            node.description = "写入文件菜单自定义列表，可直接展开（与收藏无关）".into();
            items.push(node);
        }
    }
    if target_hwnd != 0 {
        if let Some(info) = window_menu::inspect(target_hwnd) {
            let proc = if info.process_name.is_empty() {
                "当前程序".into()
            } else {
                info.process_name.clone()
            };
            let mut open_in = command_icon(
                format!("用 {proc} 打开"),
                "file-open-in-process",
                json!({ "path": path, "exe": info.exe_path, "windowHandle": target_hwnd }),
                "fa:Light_PaperPlane",
            );
            open_in.description = "用当前程序打开".into();
            items.push(open_in);
            if is_file {
                let mut paste = command_icon(
                    format!("在 {proc} 中粘贴文件"),
                    "file-paste-on-window",
                    json!({ "path": path, "windowHandle": target_hwnd }),
                    "fa:Light_PaperPlane",
                );
                paste.description = "当前窗口粘贴文件".into();
                items.push(paste);
            }
        }
    }
    items.push(command_icon("属性", "file-properties", json!({ "path": path }), "fa:Light_Wrench"));
    if is_file {
        items.push(command_icon(
            "打开方式",
            "file-open-as",
            json!({ "path": path }),
            "fa:Light_FolderOpen",
        ));
    }
    items.push(command_icon("移动到", "file-move-to", json!({ "path": path }), "fa:Light_ArrowsAlt"));
    let mut recycle = command_icon(
        "删除文件",
        "file-recycle",
        json!({ "path": path }),
        "fa:Light_Times:red",
    );
    recycle.description = "若误删，可在资源管理器 Ctrl+Z 撤销".into();
    items.push(recycle);
    (items, extra)
}

fn favor_parent_payload(payload: &Value) -> Value {
    let id = payload
        .get("parentId")
        .or_else(|| payload.get("favorId"))
        .cloned()
        .unwrap_or(Value::Null);
    json!({ "parentId": id.clone(), "favorId": id })
}

/// Super Menu `FavorMenuItemWrapper.GetAdderMenu` (folder context) plus optional
/// delete / setting. Skips C# `生成打开此收藏夹的动作` (Quicker `ActionItem` clipboard object).
fn favor_adder_context(
    payload: &Value,
    include_delete: bool,
    include_setting: bool,
) -> (Vec<MenuItemNode>, HashMap<String, MenuItemNode>) {
    let p = favor_parent_payload(payload);
    let mut open_tabs = folder_lazy(
        "从打开的页面添加…",
        "favor-add-open-tabs",
        p.clone(),
        "fa:Light_Globe",
        LazyKind::OpenBrowserTabs,
    );
    open_tabs.description = "需 Quicker 浏览器扩展在线".into();
    let mut items = vec![
        command("添加收藏夹", "favor-add-folder", p.clone()),
        command_icon("添加文件夹", "favor-add-dirs", p.clone(), "fa:Light_Folder"),
        command_icon("添加文件", "favor-add-files", p.clone(), "fa:Light_File"),
        command("添加当前网址", "favor-add-link", p.clone()),
        open_tabs,
    ];
    if include_delete {
        items.push(command("删除", "favor-delete", p.clone()));
    }
    if include_setting {
        items.push(setting_item("menu"));
    }
    (items, HashMap::new())
}

fn window_context(hwnd: i64) -> Vec<MenuItemNode> {
    window_menu::inspect(hwnd)
        .map(|info| window_context_from_info(&info))
        .unwrap_or_default()
}

/// C# `WindowRootMenuItemWrapper.GetContextMenuItems` vs window/group `GetContextMenuItems`.
fn window_role_context(
    role: Option<&str>,
    payload: &Value,
) -> (Vec<MenuItemNode>, HashMap<String, MenuItemNode>) {
    let hwnd = payload
        .get("windowHandle")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let is_root = role == Some("window")
        && (payload.get("root").and_then(|v| v.as_bool()).unwrap_or(false) || hwnd == 0);
    if is_root {
        let mut items = vec![setting_item("menu_global")];
        let handle = if hwnd == 0 {
            window_menu::foreground_hwnd() as i64
        } else {
            hwnd
        };
        items.extend(window_context(handle));
        return (items, HashMap::new());
    }
    (
        if hwnd == 0 {
            Vec::new()
        } else {
            window_context(hwnd)
        },
        HashMap::new(),
    )
}

/// C# `WindowMenuItemWrapper.GetContextMenuItems`, ordered by the 2026-10-23
/// design: copyable fields first, then process commands, destructive last.
fn window_context_from_info(info: &WindowInfo) -> Vec<MenuItemNode> {
    let icon = info.icon();
    let hwnd = info.hwnd;
    let mut items = Vec::new();
    let mut fields = window_menu::summary_pairs(info);
    // Display order: title, handle, class, process, PID, path, position.
    fields.sort_by_key(|(key, _)| match key.as_str() {
        "标题" => 0,
        "句柄" => 1,
        "类名" => 2,
        "进程名" => 3,
        "进程路径" => 5,
        _ => 6,
    });
    if info.pid != 0 {
        fields.insert(4, ("PID".into(), info.pid.to_string()));
    }
    for (key, value) in fields {
        let label = match key.as_str() {
            "进程名" => "进程",
            "进程路径" => "路径",
            key => key,
        };
        let display = if key == "位置" {
            format!("{}, {} · {} × {}", info.left, info.top, info.width, info.height)
        } else {
            value.clone()
        };
        let mut node = command_icon(
            format!("复制{key}:  {value}"),
            "window-copy",
            json!({ "text": value, "windowHandle": hwnd }),
            "fa:Light_Copy",
        );
        node.description = value.clone();
        node.visual = Some(json!({ "field": { "label": label, "value": display } }));
        if value.is_empty() {
            node.is_enabled = false;
        }
        items.push(node);
    }
    let summary = window_menu::summary_text(info);
    let mut view = command_icon(
        "复制全部信息",
        "window-copy",
        json!({ "text": summary, "windowHandle": hwnd }),
        "fa:Light_Copy",
    );
    view.description = summary;
    items.push(view);
    items.push(separator());
    let mut locate = command_icon(
        "打开进程所在文件夹",
        "window-locate",
        json!({ "path": info.exe_path, "windowHandle": hwnd }),
        "fa:Light_FolderOpen",
    );
    locate.description = info.exe_path.clone();
    items.push(locate);
    if info.is_explorer_process() {
        items.push(command_icon(
            format!("打开新的 {} 窗口", info.process_name),
            "window-open-exe",
            json!({ "path": info.exe_path, "windowHandle": hwnd }),
            &icon,
        ));
    }
    if !window_menu::is_random_class_name(&info.class_name) {
        let mut close_all = command_icon(
            format!("关闭所有 {} 窗口", info.class_name),
            "window-close-class",
            json!({ "className": info.class_name, "windowHandle": hwnd }),
            "fa:Light_Times",
        );
        close_all.description = "用类名匹配窗口，然后 close".into();
        items.push(close_all);
    }
    items.push(separator());
    let kill_title = if info.is_explorer_process() {
        format!("重启{}", info.process_name)
    } else {
        format!("结束所有 {} 进程", info.process_name)
    };
    let mut kill_all = command_icon(
        kill_title,
        "window-kill-all",
        json!({ "processName": info.process_name, "windowHandle": hwnd }),
        "fa:Light_TimesCircle:red",
    );
    kill_all.description = "用进程名匹配进程，然后 kill".into();
    items.push(kill_all);
    items
}

fn file_root_context(clipboard_path: Option<&str>) -> (Vec<MenuItemNode>, HashMap<String, MenuItemNode>) {
    let mut items = vec![file_add_dir_node(), file_add_opened_root_node()];
    if let Some(path) = clipboard_path.filter(|p| !p.is_empty()) {
        items.push(file_add_clipboard_node(path));
    }
    items.push(separator());
    items.push(setting_item("menu_file"));
    (items, HashMap::new())
}

/// C# `ChromeBookMarksMenuItemWrapper.GetContextMenuItems`: folders yield break.
fn bookmark_leaf_context(payload: &Value) -> (Vec<MenuItemNode>, HashMap<String, MenuItemNode>) {
    let url = payload
        .get("url")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if url.is_empty() {
        return (Vec::new(), HashMap::new());
    }
    let mut copy_title = command("复制标题+链接", "bookmark-copy-title", payload.clone());
    copy_title.description = "标题...\r\n链接...".into();
    let mut delete = command_icon("删除", "bookmark-delete", payload.clone(), "fa:Light_Times:red");
    delete.is_enabled = false;
    delete.description = "当前未接通，请在浏览器中删除书签。".into();
    (
        vec![
            command("复制链接", "bookmark-copy", payload.clone()),
            copy_title,
            delete,
        ],
        HashMap::new(),
    )
}

fn text_role_context(payload: &Value) -> (Vec<MenuItemNode>, HashMap<String, MenuItemNode>) {
    if payload
        .get("actionId")
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.is_empty())
    {
        return (
            vec![
                command("编辑", "text-edit", payload.clone()),
                command("删除", "text-delete", payload.clone()),
            ],
            HashMap::new(),
        );
    }
    if payload.get("group").is_some() {
        return (Vec::new(), HashMap::new());
    }
    (
        vec![
            command("添加", "text-add", payload.clone()),
            setting_item("textAction"),
        ],
        HashMap::new(),
    )
}

fn context_for(
    role: Option<&str>,
    payload: &Value,
    custom_paths: &[String],
    target_hwnd: i64,
    clipboard_path: Option<&str>,
) -> (Vec<MenuItemNode>, HashMap<String, MenuItemNode>) {
    match role {
        Some("reopen") => {
            let mut items = vec![command("打开", "reopen", payload.clone())];
            if payload.get("recordedAt").and_then(Value::as_i64).is_some() {
                let mut remove = command("从重开历史移除", "reopen-remove", payload.clone());
                remove.keep_open_on_invoke = true;
                items.push(remove);
            }
            items.push(command("复制路径", "file-copy-path", payload.clone()));
            let mut add = command("添加到文件菜单", "file-add-custom", payload.clone());
            add.keep_open_on_invoke = true;
            items.push(add);
            items.push(command("管理重开历史…", "reopen-manage", payload.clone()));
            (items, HashMap::new())
        }
        Some("file") | Some("recent-files") | Some("opened-folder") => {
            if role == Some("file")
                && (payload.get("root").and_then(|v| v.as_bool()).unwrap_or(false)
                    || payload.get("path").and_then(|v| v.as_str()).unwrap_or("").is_empty())
            {
                file_root_context(clipboard_path)
            } else {
                payload
                    .get("path")
                    .and_then(|v| v.as_str())
                    .map(|path| file_context(path, custom_paths, target_hwnd))
                    .unwrap_or_default()
            }
        }
        Some("window") | Some("window-group") => window_role_context(role, payload),
        Some("favor") => (
            vec![command("删除", "favor-delete", favor_parent_payload(payload))],
            HashMap::new(),
        ),
        Some("favor-folder") => favor_adder_context(payload, true, false),
        Some("favor-add") => favor_adder_context(payload, false, true),
        Some("favor-link") => (
            vec![
                command("复制链接", "favor-copy-link", payload.clone()),
                command("复制链接加标题", "favor-copy-link-title", payload.clone()),
            ],
            HashMap::new(),
        ),
        Some("text") => text_role_context(payload),
        Some("reopen-root") => (vec![command("管理重开历史…", "reopen-manage", json!({}))], HashMap::new()),
        Some("bookmark") => bookmark_leaf_context(payload),
        Some("bookmark-folder") => (Vec::new(), HashMap::new()),
        _ => (Vec::new(), HashMap::new()),
    }
}

pub fn activate_window(hwnd: i64) -> Result<(), String> {
    window_menu::activate(hwnd)
}

pub fn open_or_show_window(hwnd: i64, exe_path: &str) -> Result<(), String> {
    window_menu::open_or_show(hwnd, exe_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bookmarks::parse_bookmarks_file;
    use tempfile::tempdir;

    fn lazy_bits(
        opened: Vec<OpenedFolder>,
        reopen: Vec<String>,
        custom: Vec<String>,
    ) -> LazySessionBits {
        LazySessionBits {
            node_payload: json!({}),
            file_cfg: FileMenuSettings::default(),
            menu_cfg: MenuGlobalSettings::default(),
            opened,
            reopen_paths: reopen,
            reopen_recorded_at: HashMap::new(),
            reopen_undo: None,
            custom_paths: custom.clone(),
            extra_files: Vec::new(),
            clipboard_path: None,
            text_actions: Vec::new(),
            bookmarks: None,
            skip_hwnd: 0,
            file_menu: FileMenuPaths {
                lines: custom,
                opened_folders: Vec::new(),
            },
            existing_children: Vec::new(),
        }
    }

    #[test]
    fn text_root_emits_ungrouped_folder_when_named_groups_reach_five() {
        let mut actions = Vec::new();
        for i in 0..5 {
            actions.push(json!({
                "Title": format!("g{i}"),
                "Group": format!("G{i}"),
                "Type": "Send",
                "Data": { "Text": "x" }
            }));
        }
        actions.push(json!({
            "Title": "loose",
            "Group": "",
            "Type": "Send",
            "Data": { "Text": "y" }
        }));
        let (items, extra, _) = text_root(&actions);
        assert_eq!(items.len(), 8);
        assert_eq!(items[5].title, "*未分组*");
        assert!(items[5].has_children);
        assert_eq!(items[5].invoke_mode, "None");
        assert!(!items[5].keep_open_on_invoke);
        assert_eq!(items[0].invoke_mode, "None");
        assert_eq!(items[5].child_ids.len(), 1);
        let child = extra.get(&items[5].child_ids[0]).expect("ungrouped child");
        assert_eq!(child.title, "loose");
        assert_eq!(items[6].kind, "Separator");
        assert_eq!(items[7].role.as_deref(), Some("setting"));
        assert_eq!(items[7].title, "文本操作设置");
    }

    #[test]
    fn text_root_inlines_ungrouped_when_named_groups_below_five() {
        let actions = vec![
            json!({
                "Title": "named",
                "Group": "Format",
                "Type": "Send",
                "Data": { "Text": "x" }
            }),
            json!({
                "Title": "loose",
                "Group": "",
                "Type": "Send",
                "Data": { "Text": "y" }
            }),
            json!({
                "Title": "spaces",
                "Group": "  ",
                "Type": "Send",
                "Data": { "Text": "z" }
            }),
        ];
        let (items, extra, _) = text_root(&actions);
        assert_eq!(items[0].title, "Format");
        assert_eq!(items[0].kind, "Folder");
        assert_eq!(items[0].invoke_mode, "None");
        assert_eq!(items[1].title, "  ");
        assert_eq!(items[1].kind, "Folder");
        assert_eq!(items[2].title, "loose");
        assert_eq!(items[2].role.as_deref(), Some("text"));
        assert_eq!(items[2].kind, "Command");
        assert_eq!(items[3].kind, "Separator");
        assert_eq!(items[4].role.as_deref(), Some("setting"));
        let named_child = extra.get(&items[0].child_ids[0]).expect("named child");
        assert_eq!(named_child.title, "named");
    }

    #[test]
    fn bookmarks_session_keeps_browsing_and_disables_unavailable_delete() {
        let tree = parse_bookmarks_file(
            r#"{
              "roots": {
                "bookmark_bar": {
                  "name": "书签栏",
                  "guid": "bar",
                  "children": [{ "name": "Quicker", "url": "https://getquicker.net", "guid": "1" }]
                }
              }
            }"#,
        )
        .unwrap();
        let ctx = MenuBuildContext {
            bookmarks: Some(tree),
            ..MenuBuildContext::default()
        };
        let mut registry = MenuRegistry::default();
        let session = registry.create_from_context("bookmarks", json!({}), &ctx);
        assert_eq!(session["root"]["status"], "Ready");
        assert_eq!(session["root"]["items"][0]["title"], "书签栏");
        assert_eq!(session["root"]["items"][0]["hasChildren"], true);
        assert_eq!(session["root"]["items"][0]["isEnabled"], true);

        let session_id = session["sessionId"].as_str().unwrap();
        let folder_id = session["root"]["items"][0]["id"].as_str().unwrap();
        let children = registry.get_nodes(session_id, folder_id, CollectionKind::Children, None).unwrap();
        let bookmark = &children["items"][0];
        assert_eq!(bookmark["title"], "Quicker");
        assert_eq!(bookmark["isEnabled"], true);
        assert_eq!(bookmark["invokeMode"], "Invoke");
        assert_eq!(bookmark["payload"]["url"], "https://getquicker.net");

        let context = registry.get_nodes(
            session_id, bookmark["id"].as_str().unwrap(), CollectionKind::ContextMenu, None,
        ).unwrap();
        let items = context["items"].as_array().unwrap();
        for role in ["bookmark-copy", "bookmark-copy-title"] {
            let copy = items.iter().find(|item| item["role"] == role).expect("bookmark copy action");
            assert_eq!(copy["isEnabled"], true);
        }
        let delete = items.iter().find(|item| item["role"] == "bookmark-delete").expect("bookmark delete action");
        assert_eq!(delete["isEnabled"], false);
        assert_eq!(delete["description"], "当前未接通，请在浏览器中删除书签。");
    }

    #[test]
    fn bookmarks_session_empty_tree_uses_csharp_empty_message() {
        let ctx = MenuBuildContext {
            bookmarks: Some(crate::bookmarks::BookmarkNode {
                id: "0".into(),
                title: "浏览器书签".into(),
                url: String::new(),
                children: Vec::new(),
            }),
            ..MenuBuildContext::default()
        };
        let mut registry = MenuRegistry::default();
        let session = registry.create_from_context("bookmarks", json!({}), &ctx);
        assert_eq!(session["message"], "书签为空");
        assert_eq!(session["root"]["status"], "Error");
        assert_eq!(session["root"]["message"], "书签为空");
    }

    #[test]
    fn window_verb_root_always_appends_setting_like_csharp() {
        let mut ctx = MenuBuildContext::default();
        ctx.menu_cfg.show_setting_item = false;
        let items = window_root(&ctx, true);
        assert_eq!(items.last().unwrap().role.as_deref(), Some("setting"));
        assert_eq!(items[items.len() - 2].kind, "Separator");
        let nested = window_root(&ctx, false);
        assert!(nested.iter().all(|n| n.role.as_deref() != Some("setting")));
    }

    #[test]
    fn clip_files_reuse_file_menu_nodes_cascade_and_context() {
        let dir = tempdir().unwrap();
        let folder = dir.path().join("folder");
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("inner.txt"), "x").unwrap();
        let file = dir.path().join("note.txt");
        std::fs::write(&file, "x").unwrap();
        let paths = vec![folder.to_string_lossy().into_owned(), file.to_string_lossy().into_owned()];
        let mut registry = MenuRegistry::default();
        let session = registry.create_fixture_session(&MenuBuildContext::default(), Vec::new());
        let session = session["sessionId"].as_str().unwrap().to_string();
        let node = registry.register_clip_files(&session, "clip-1", paths.clone()).unwrap();
        assert_eq!(node["role"], CLIP_FILES_ROLE);
        assert_eq!(registry.register_clip_files(&session, "clip-1", paths.clone()).unwrap()["id"], node["id"],
            "re-selecting the same clip reuses its node");
        let page = registry.get_nodes(&session, node["id"].as_str().unwrap(), CollectionKind::Children, None).unwrap();
        let items = page["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert!(items.iter().all(|item| item["role"] == "file" && item["hasContextMenu"] == true));
        assert_eq!(items[0]["kind"], "Folder", "a folder with entries expands like the file category");
        let inner = registry.get_nodes(&session, items[0]["id"].as_str().unwrap(), CollectionKind::Children, None).unwrap();
        assert_eq!(inner["items"].as_array().unwrap().len(), 1);
        let context = registry.get_nodes(&session, items[1]["id"].as_str().unwrap(), CollectionKind::ContextMenu, None).unwrap();
        assert!(context["items"].as_array().unwrap().iter().any(|item| item["role"] == "file-copy-more"));
        assert!(registry.register_clip_files("missing", "clip-1", paths).is_err());
    }

    #[test]
    fn registered_file_menu_path_can_be_removed_without_filesystem_confirmation() {
        let path = r"\\unavailable-server\share\folder";
        // Both timed-out and missing paths have no confirmed file/directory type.
        let (items, _) = file_context_with_types(path, &[path.to_string()], 0, false, false);
        let remove = items.iter().find(|item| item.role.as_deref() == Some("file-remove-custom"))
            .expect("a saved path can always be removed from configuration");
        assert!(remove.is_enabled);
        assert!(remove.keep_open_on_invoke);
        assert_eq!(remove.payload["path"], path);
        assert!(!items.iter().any(|item| matches!(item.role.as_deref(), Some("file-add-custom" | "file-move-selected" | "file-open-as"))));
    }

    #[test]
    fn file_context_matches_csharp_roles_and_custom_toggle() {
        let (missing, extra) = file_context(r"D:\share\a.txt", &[], 0);
        let missing_roles: Vec<&str> = missing.iter().filter_map(|n| n.role.as_deref()).collect();
        assert!(missing_roles.contains(&"file-cut"));
        assert!(missing_roles.contains(&"file-copy-more"));
        assert!(!missing_roles.contains(&"file-add-custom"));
        assert!(!missing_roles.contains(&"file-remove-custom"));
        assert!(!missing_roles.contains(&"file-open-as"));
        assert!(!missing_roles.contains(&"file-move-selected"));
        assert!(missing_roles.contains(&"file-properties"));
        assert!(missing_roles.contains(&"file-recycle"));
        assert_eq!(
            missing
                .iter()
                .find(|n| n.role.as_deref() == Some("file-recycle"))
                .unwrap()
                .description,
            "若误删，可在资源管理器 Ctrl+Z 撤销"
        );
        assert_eq!(extra.len(), 4);

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "x").unwrap();
        let file_s = file.to_str().unwrap();
        let (added, _) = file_context_with_types(file_s, &[], 0, false, true);
        let added_roles: Vec<&str> = added.iter().filter_map(|n| n.role.as_deref()).collect();
        assert!(added_roles.contains(&"file-add-custom"));
        assert!(!added_roles.contains(&"file-remove-custom"));
        assert!(added_roles.contains(&"file-open-as"));
        assert_eq!(
            added
                .iter()
                .find(|n| n.role.as_deref() == Some("file-add-custom"))
                .unwrap()
                .description,
            "写入文件菜单自定义列表，可直接展开（与收藏无关）"
        );

        let folder = dir.path().join("share");
        std::fs::create_dir_all(&folder).unwrap();
        let folder_s = folder.to_str().unwrap();
        let (removed, _) = file_context_with_types(folder_s, &[folder_s.to_string()], 0, true, false);
        let roles: Vec<&str> = removed.iter().filter_map(|n| n.role.as_deref()).collect();
        assert!(roles.contains(&"file-remove-custom"));
        assert!(!roles.contains(&"file-add-custom"));
        assert!(!roles.contains(&"file-open-as"));
        assert!(roles.contains(&"file-move-selected"));
        assert_eq!(
            removed
                .iter()
                .find(|n| n.role.as_deref() == Some("file-remove-custom"))
                .unwrap()
                .description,
            "从文件菜单自定义列表移除（与收藏无关）"
        );
    }

    #[test]
    fn file_node_enabled_matches_csharp_get_is_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "x").unwrap();
        let live = file_node(file.to_str().unwrap(), &FileMenuSettings::default());
        assert!(live.is_enabled);
        assert_eq!(live.invoke_mode, "Invoke");

        let missing = file_node_with_status(
            r"C:\cliphost-missing-local-file-xyz.bin", &FileMenuSettings::default(),
            file_menu_extract::MenuPathStatus::Missing,
        );
        assert!(!missing.is_enabled);
        assert_eq!(missing.invoke_mode, "None");
        assert_eq!(missing.title, "cliphost-missing-local-file-xyz.bin");
        assert!(!missing.has_context_menu);

        let unc = file_node_with_status(
            r"\\cliphost-no-such-host\share", &FileMenuSettings::default(),
            file_menu_extract::MenuPathStatus::Missing,
        );
        assert_eq!(unc.title, "share");
        assert!(!unc.is_enabled);
        assert_eq!(unc.invoke_mode, "None");
        assert!(unc.has_context_menu);
    }

    #[test]
    fn file_menu_shell_locations_are_named_actionable_and_have_valid_context() {
        let path = crate::explorer::THIS_PC_SHELL;
        let cfg = FileMenuSettings::default();
        let saved = file_node(path, &cfg);
        let opened = opened_node(&OpenedFolder {
            hwnd: 42, path: path.into(), title: "此电脑".into(),
        }, &cfg);
        let reopened = reopen_items(&[path.into()]).remove(0);
        for node in [&saved, &opened, &reopened] {
            assert_eq!(node.title, "此电脑");
            assert!(node.is_enabled);
            assert_eq!(node.invoke_mode, "InvokeAndExpand");
            assert!(matches!(node.lazy, Some(LazyKind::ThisPc)));
            assert!(node.has_context_menu);
            assert_eq!(node.payload["path"], path);
            assert_eq!(node.description, path);
            assert_eq!(node.icon, "fa:Light_Desktop");
        }
        assert_eq!(opened.payload["windowHandle"], 42);
        assert_eq!(reopen_root_node(&[path.into()]).title, "重开：此电脑");
        let added = add_opened_folder_nodes(&[OpenedFolder {
            hwnd: 42, path: path.into(), title: "此电脑".into(),
        }], &[]);
        assert_eq!(added[0].title, "此电脑");
        for custom in [Vec::new(), vec![path.to_string()]] {
            let (context, extra) = file_context(path, &custom, 0);
            assert!(extra.is_empty());
            let roles: Vec<_> = context.iter().filter_map(|n| n.role.as_deref()).collect();
            assert_eq!(roles, ["file-browser", "file-copy-path", if custom.is_empty() {
                "file-add-custom"
            } else { "file-remove-custom" }]);
            assert!(context.last().unwrap().keep_open_on_invoke);
        }
        let home = file_node(crate::explorer::HOME_SHELL, &cfg);
        assert_eq!(home.title, "主文件夹");
        assert!(home.is_enabled);
        assert_eq!(home.invoke_mode, "Invoke");
        assert!(!home.has_children);
        let node_id = saved.id.clone();
        let mut registry = MenuRegistry::default();
        let session = registry.create_fixture_session(&MenuBuildContext::default(), vec![saved]);
        assert!(registry.prepare_nodes(session["sessionId"].as_str().unwrap(), &node_id,
            CollectionKind::Children, None).unwrap().is_some(),
            "drive enumeration must run outside the menu registry lock");
    }

    #[test]
    fn directory_snapshot_nodes_preserve_file_menu_presentation() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("notes.txt");
        let app = dir.path().join("application.exe");
        let folder = dir.path().join("folder.name");
        std::fs::write(&file, "notes").unwrap();
        std::fs::write(&app, "app").unwrap();
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("child.txt"), "child").unwrap();
        let normalize = |node: MenuItemNode| {
            let mut value = node_to_dto(&node);
            value["id"] = Value::Null;
            value
        };
        for show_extension in [false, true] {
            let cfg = FileMenuSettings { show_extension, ..FileMenuSettings::default() };
            for path in [&file, &app, &folder] {
                let snapshot = file_menu_extract::FileMenuEntry {
                    path: path.clone(), metadata: Some(std::fs::metadata(path).unwrap()),
                };
                assert_eq!(normalize(file_node_from_snapshot(snapshot, &cfg)),
                    normalize(file_node(path.to_str().unwrap(), &cfg)), "{path:?}");
            }
        }
        // A captured file's type/title/icon are sufficient to construct its menu
        // row; later filesystem access belongs to invoke/context/reload paths.
        let snapshot = file_menu_extract::FileMenuEntry {
            path: file.clone(), metadata: Some(std::fs::metadata(&file).unwrap()),
        };
        std::fs::remove_file(&file).unwrap();
        let node = file_node_from_snapshot(snapshot, &FileMenuSettings::default());
        assert!(node.is_enabled);
        assert_eq!(node.title, "notes.txt");
        assert_eq!(node.icon, "icon:.txt");
        assert_eq!(node.invoke_mode, "Invoke");
    }

    #[test]
    fn timed_out_menu_paths_keep_invoke_and_context_without_more_filesystem_probes() {
        for path in [r"Z:\slow.mount\folder.name", r"\\slow-server\share\item.txt"] {
            let node = file_node_with_status(
                path, &FileMenuSettings::default(), file_menu_extract::MenuPathStatus::Unknown,
            );
            assert!(node.is_enabled);
            assert_eq!(node.invoke_mode, "Invoke");
            assert_eq!(node.role.as_deref(), Some("file"));
            assert_eq!(node.payload["path"], path);
            assert_eq!(node.title, file_menu_extract::path_get_file_name(path));
            assert!(node.has_context_menu);
            assert!(!node.has_children, "do not guess the unconfirmed path's type");
        }
    }

    #[test]
    fn file_root_captions_match_csharp_file_root_menu_item_wrapper() {
        let (items, _) = file_root_context(Some(r"C:\Windows"));
        assert_eq!(items[0].title, "添加目录…");
        assert_eq!(
            items[0].description,
            "选择文件夹写入文件菜单自定义列表（与收藏无关）"
        );
        assert!(items[0].keep_open_on_invoke);
        assert_eq!(items[1].title, "添加已打开的文件夹…");
        assert_eq!(
            items[1].description,
            "从当前资源管理器窗口挑选目录加入自定义列表"
        );
        assert_eq!(items[2].title, "粘贴路径添加");
        assert_eq!(
            items[2].description,
            "将剪贴板中的路径加入文件菜单自定义列表"
        );
        let none = file_root_context(None);
        assert!(
            !none.0.iter().any(|n| n.role.as_deref() == Some("file-add-clipboard")),
            "C# ClipboardLooksLikeExistingPath hides 粘贴路径添加"
        );
        let recent = recent_files_root_node();
        assert_eq!(recent.description, "最近使用的文件或者文件夹");
        assert_eq!(recent.invoke_mode, "None");
        assert!(!recent.keep_open_on_invoke);
        let (root, _, _) = file_root(&MenuBuildContext::default());
        assert!(!root.iter().any(|n| n.role.as_deref() == Some("file-browser")),
            "the docked browser opens from rows, not a separate root entry");
        let recent_root = root
            .iter()
            .find(|n| n.role.as_deref() == Some("recent-files"))
            .unwrap();
        assert_eq!(recent_root.invoke_mode, "None");
        assert_eq!(recent_root.description, "最近使用的文件或者文件夹");
    }

    #[test]
    fn file_browser_context_only_offers_internal_navigation_for_directories() {
        let path = r"Z:\not-probed";
        let (directory, _) = file_context_with_types(path, &[], 0, true, false);
        let entry = directory.iter().find(|node| node.role.as_deref() == Some("file-browser")).unwrap();
        assert_eq!(entry.payload["path"], path);
        assert!(!entry.keep_open_on_invoke);
        for (is_dir, is_file) in [(false, true), (false, false)] {
            let (items, _) = file_context_with_types(path, &[], 0, is_dir, is_file);
            assert!(!items.iter().any(|node| node.role.as_deref() == Some("file-browser")));
        }
    }

    #[test]
    fn add_opened_folders_marks_already_in_custom_list() {
        let opened = vec![OpenedFolder {
            hwnd: 1,
            path: r"D:\share".into(),
            title: "share".into(),
        }];
        let (marked, _) = materialize_lazy(
            &LazyKind::AddOpenedFolders,
            &lazy_bits(opened.clone(), Vec::new(), vec![r"D:\share".into()]),
        );
        assert_eq!(marked[0].title, "已添加 · share");
        assert_eq!(marked[0].icon, "fa:Light_Check");
        assert_eq!(marked[0].role.as_deref(), Some("file-add-opened"));
        let (fresh, _) = materialize_lazy(
            &LazyKind::AddOpenedFolders,
            &lazy_bits(opened, Vec::new(), Vec::new()),
        );
        assert_eq!(fresh[0].title, "share");
        assert_eq!(fresh[0].icon, "fa:Light_FolderPlus");
        assert_eq!(fresh[0].description, r"D:\share");
        assert!(fresh[0].keep_open_on_invoke);
        assert_eq!(marked[0].description, r"D:\share");

        let (empty, _) = materialize_lazy(
            &LazyKind::AddOpenedFolders,
            &lazy_bits(Vec::new(), Vec::new(), Vec::new()),
        );
        assert_eq!(empty[0].title, "（当前没有打开的文件夹）");
        assert_eq!(empty[0].icon, "fa:Light_InfoCircle");
        assert_eq!(empty[0].description, "");
        assert!(empty[0].keep_open_on_invoke);
        assert_eq!(empty[0].invoke_mode, "None");
        assert_eq!(empty[0].role.as_deref(), Some("file-add-opened-empty"));
        assert_eq!(add_opened_folder_title(r"D:\share\"), "share");
        assert_eq!(add_opened_folder_title(r"\\server\share"), "share");
    }

    #[test]
    fn opened_folders_lazy_nests_reopen_root_like_csharp() {
        let opened = vec![OpenedFolder {
            hwnd: 1,
            path: r"D:\share".into(),
            title: "share".into(),
        }];
        let (items, _) = materialize_lazy(
            &LazyKind::OpenedFolders,
            &lazy_bits(opened, vec![r"D:\closed".into()], Vec::new()),
        );
        let roles: Vec<&str> = items.iter().filter_map(|n| n.role.as_deref()).collect();
        assert_eq!(roles[0], "opened-folder");
        assert_eq!(*roles.last().unwrap(), "reopen-root");
        let reopen = items.iter().find(|n| n.role.as_deref() == Some("reopen-root")).unwrap();
        assert_eq!(reopen.title, "重开：closed");
        assert_eq!(reopen.icon, "fa:Light_ArrowAltToLeft");
        assert!(reopen.has_context_menu);
        assert!(reopen.is_enabled);
        let (children, _) = materialize_lazy(
            &LazyKind::ReopenFolders,
            &lazy_bits(Vec::new(), vec![r"D:\closed".into()], Vec::new()),
        );
        assert_eq!(children[0].role.as_deref(), Some("reopen"));
        assert_eq!(children[0].title, "closed");
        let (ctx, _) = context_for(Some("reopen-root"), &json!({}), &[], 0, None);
        assert_eq!(ctx[0].role.as_deref(), Some("reopen-manage"));
        let empty = reopen_root_node(&[]);
        assert_eq!(empty.title, "重开");
        assert!(empty.is_enabled);
        assert!(empty.has_children);
        assert_eq!(empty.invoke_mode, "Expand");
    }

    #[test]
    fn opened_folder_node_matches_csharp_file_menu_item_wrapper() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("docs");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("note.txt"), b"x").unwrap();
        let path = nested.to_str().unwrap().to_string();
        let folder = OpenedFolder {
            hwnd: 42,
            path: path.clone(),
            title: "Explorer Window Title".into(),
        };
        let cfg = FileMenuSettings::default();
        let node = opened_node(&folder, &cfg);
        assert_eq!(node.title, "docs");
        assert_eq!(node.description, path);
        assert_eq!(node.role.as_deref(), Some("opened-folder"));
        assert_eq!(node.payload["windowHandle"], 42);
        assert_eq!(node.invoke_mode, "InvokeAndExpand");
        assert!(node.has_children);
        assert!(node.has_context_menu);
        assert_eq!(node.kind, "Folder");
        match node.lazy {
            Some(LazyKind::FileDir { path: lazy_path }) => assert_eq!(lazy_path, folder.path),
            other => panic!("expected FileDir lazy, got {other:?}"),
        }

        let empty_dir = dir.path().join("empty");
        std::fs::create_dir_all(&empty_dir).unwrap();
        let empty_path = empty_dir.to_str().unwrap().to_string();
        let leaf = opened_node(
            &OpenedFolder {
                hwnd: 7,
                path: empty_path.clone(),
                title: "ignored".into(),
            },
            &cfg,
        );
        assert_eq!(leaf.title, "empty");
        assert_eq!(leaf.description, empty_path);
        assert!(!leaf.has_children);
        assert_eq!(leaf.invoke_mode, "Invoke");
        assert_eq!(leaf.role.as_deref(), Some("opened-folder"));
    }

    #[test]
    fn reopen_verb_lists_opened_then_closed_like_csharp() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("docs");
        std::fs::create_dir_all(&live).unwrap();
        let live_path = live.to_str().unwrap().to_string();
        let opened = vec![
            OpenedFolder {
                hwnd: 11,
                path: live_path.clone(),
                title: "docs".into(),
            },
            OpenedFolder {
                hwnd: 0,
                path: live_path.clone(),
                title: "persisted".into(),
            },
            OpenedFolder {
                hwnd: 12,
                path: "shell:::this-pc".into(),
                title: "This PC".into(),
            },
        ];
        let items = reopen_verb_root(&opened, &[r"D:\closed".into()], &HashMap::new(), None);
        let roles: Vec<&str> = items.iter().filter_map(|n| n.role.as_deref()).collect();
        assert_eq!(roles, ["opened-folder", "reopen", "reopen-manage"]);
        assert_eq!(items[0].payload["path"], live_path);
        assert_eq!(items[0].payload["windowHandle"], 11);
        assert_eq!(items[1].kind, "Separator");
        assert_eq!(items[2].role.as_deref(), Some("reopen"));
        assert_eq!(items[2].title, "closed");

        let closed_only = reopen_verb_root(&[], &[r"D:\closed".into()], &HashMap::new(), None);
        assert_eq!(closed_only.len(), 3);
        assert_eq!(closed_only[0].role.as_deref(), Some("reopen"));
    }

    #[test]
    fn opened_directory_probe_timeout_preserves_the_live_menu_entry() {
        use file_menu_extract::MenuPathStatus::{Present, Missing, Unknown};
        let mut folder = OpenedFolder { hwnd: 11, path: r"\\slow-share\docs".into(), title: "docs".into() };
        for status in [Present, Missing, Unknown] {
            assert_eq!(opened_tab_is_live_directory_with(&folder, |_| status), status != Missing);
        }
        folder.hwnd = 0;
        assert!(!opened_tab_is_live_directory_with(&folder, |_| panic!("persisted entry is not a live tab")));
        folder.hwnd = 11;
        folder.path = "shell:::this-pc".into();
        assert!(!opened_tab_is_live_directory_with(&folder, |_| panic!("shell path is not a directory")));
    }

    #[test]
    fn reopen_items_keep_unavailable_paths_manageable_without_probing() {
        let items = reopen_items(&[
            r"C:\cliphost-missing-reopen-xyz".into(),
            "shell:::keep".into(),
        ]);
        assert_eq!(items[0].title, "cliphost-missing-reopen-xyz");
        assert!(items[0].is_enabled);
        assert!(items[0].has_context_menu);
        assert_eq!(items[0].invoke_mode, "Invoke");
        assert_eq!(items[1].title, "shell:::keep");
        assert!(items[1].is_enabled);
        assert_eq!(items[1].invoke_mode, "Invoke");
        let root = reopen_root_node(&[r"\\server\share".into()]);
        assert_eq!(root.title, "重开：share");
    }

    #[test]
    fn reopen_context_removes_history_with_its_version_and_never_recycles_files() {
        let payload = json!({ "path": r"\\offline\share", "recordedAt": 17 });
        let (items, _) = context_for(Some("reopen"), &payload, &[], 0, None);
        let remove = items.iter().find(|node| node.role.as_deref() == Some("reopen-remove")).unwrap();
        assert_eq!(remove.payload["recordedAt"], 17);
        assert_eq!(remove.payload["path"], r"\\offline\share");
        assert!(remove.keep_open_on_invoke);
        assert!(!items.iter().any(|node| matches!(node.role.as_deref(), Some("file-recycle" | "file-cut"))));
        let (legacy, _) = context_for(Some("reopen"), &json!({ "path": r"D:\old" }), &[], 0, None);
        assert!(!legacy.iter().any(|node| node.role.as_deref() == Some("reopen-remove")));
    }

    #[test]
    fn reopen_menu_stamps_rows_and_keeps_management_and_undo_when_empty() {
        let path = "shell:::fixture".to_string();
        let versions = HashMap::from([(path.clone(), 31)]);
        let rows = reopen_menu_items(&[path], &versions, None);
        assert_eq!(rows[0].payload["recordedAt"], 31);
        let undo = json!({ "token": "fixture" });
        let empty = reopen_menu_items(&[], &HashMap::new(), Some(&undo));
        assert_eq!(empty.iter().filter_map(|node| node.role.as_deref()).collect::<Vec<_>>(), ["reopen-undo", "reopen-manage"]);
        assert_eq!(empty[0].payload["undo"], undo);
    }

    #[test]
    fn favor_adder_matches_csharp_open_tabs_child() {
        let (items, _) = materialize_lazy(&LazyKind::FavorAdder, &lazy_bits(Vec::new(), Vec::new(), Vec::new()));
        let roles: Vec<&str> = items.iter().filter_map(|n| n.role.as_deref()).collect();
        assert_eq!(
            roles,
            ["favor-add-clipboard", "favor-add-link", "favor-add-open-tabs"]
        );
        let (tabs, _) = materialize_lazy(
            &LazyKind::OpenBrowserTabs,
            &lazy_bits(Vec::new(), Vec::new(), Vec::new()),
        );
        assert_eq!(tabs[0].title, "（无可用标签）");
        assert_eq!(tabs[0].role.as_deref(), Some("favor-add-open-tabs-empty"));
        assert!(tabs[0].description.contains("浏览器扩展"));
    }

    #[test]
    fn favor_folder_context_matches_csharp_adder() {
        let (items, _) = context_for(
            Some("favor-folder"),
            &json!({ "favorId": "folder-1" }),
            &[],
            0,
            None,
        );
        let roles: Vec<&str> = items.iter().filter_map(|n| n.role.as_deref()).collect();
        assert_eq!(
            roles,
            [
                "favor-add-folder",
                "favor-add-dirs",
                "favor-add-files",
                "favor-add-link",
                "favor-add-open-tabs",
                "favor-delete"
            ]
        );
        assert_eq!(items[0].title, "添加收藏夹");
        assert_eq!(items[1].title, "添加文件夹");
        assert_eq!(items[2].title, "添加文件");
        assert_eq!(items[0].payload["parentId"], "folder-1");
        assert_eq!(items[4].description, "需 Quicker 浏览器扩展在线");
        assert!(items[4].has_children);
    }

    #[test]
    fn favor_item_context_is_delete_only() {
        let (items, _) = context_for(Some("favor"), &json!({ "favorId": "clip-1" }), &[], 0, None);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].role.as_deref(), Some("favor-delete"));
        assert_eq!(items[0].title, "删除");
        assert_eq!(items[0].payload["favorId"], "clip-1");
    }

    #[test]
    fn favor_add_context_has_setting_without_delete() {
        let (items, _) = context_for(Some("favor-add"), &json!({}), &[], 0, None);
        let roles: Vec<&str> = items.iter().filter_map(|n| n.role.as_deref()).collect();
        assert_eq!(
            roles,
            [
                "favor-add-folder",
                "favor-add-dirs",
                "favor-add-files",
                "favor-add-link",
                "favor-add-open-tabs",
                "setting"
            ]
        );
        assert!(!roles.contains(&"favor-delete"));
        assert_eq!(items.last().unwrap().payload["page"], "menu");
    }

    #[test]
    fn favor_tree_wraps_file_and_link_children_like_csharp() {
        let dir = tempdir().unwrap();
        let notes = dir.path().join("notes.txt");
        std::fs::write(&notes, "x").unwrap();
        let notes_path = notes.to_str().unwrap().to_string();
        let folder = FavorRecord {
            storage_revision: None,
            id: "folder-1".into(),
            parent_id: None,
            is_folder: true,
            name: "inbox".into(),
            kind: ClipKind::FavoriteClipItem,
            body_text: None,
            file_paths: Vec::new(),
            image_path: None,
            children: vec!["file-1".into(), "link-1".into()],
            create_time: chrono::Utc::now(),
            update_time: chrono::Utc::now(),
            source_clip_id: None,
            html_data: None,
            html_source_url: None,
            qq_rich_text: None,
            source_process_name: String::new(),
            sub_index: 0,
            icon: String::new(),
            execute_param: String::new(),
        };
        let file = FavorRecord {
            storage_revision: None,
            id: "file-1".into(),
            parent_id: Some("folder-1".into()),
            is_folder: false,
            name: "notes.txt".into(),
            kind: ClipKind::FileClipItem,
            body_text: Some(notes_path.clone()),
            file_paths: vec![notes_path],
            image_path: None,
            children: Vec::new(),
            create_time: chrono::Utc::now(),
            update_time: chrono::Utc::now(),
            source_clip_id: None,
            html_data: None,
            html_source_url: None,
            qq_rich_text: None,
            source_process_name: String::new(),
            sub_index: 0,
            icon: String::new(),
            execute_param: String::new(),
        };
        let link = FavorRecord {
            storage_revision: None,
            id: "link-1".into(),
            parent_id: Some("folder-1".into()),
            is_folder: false,
            name: "example".into(),
            kind: ClipKind::LinkClipItem,
            body_text: Some("https://example.com".into()),
            file_paths: Vec::new(),
            image_path: None,
            children: Vec::new(),
            create_time: chrono::Utc::now(),
            update_time: chrono::Utc::now(),
            source_clip_id: None,
            html_data: None,
            html_source_url: None,
            qq_rich_text: None,
            source_process_name: String::new(),
            sub_index: 0,
            icon: String::new(),
            execute_param: String::new(),
        };
        let (roots, extra) = favor_tree(&[folder, file, link]);
        assert_eq!(roots[0].role.as_deref(), Some("favor-folder"));
        let roles: Vec<&str> = roots[0]
            .child_ids
            .iter()
            .filter_map(|id| extra.get(id).and_then(|n| n.role.as_deref()))
            .collect();
        assert_eq!(roles, ["file", "favor-link"]);
        assert_eq!(
            extra
                .values()
                .find(|n| n.role.as_deref() == Some("favor-link"))
                .map(|n| n.icon.as_str()),
            Some("https://helperservice.getquicker.cn/favicon/get/example.com")
        );
        let (ctx, _) = context_for(
            Some("favor-link"),
            &json!({ "url": "https://example.com" }),
            &[],
            0,
            None,
        );
        assert_eq!(ctx[0].title, "复制链接");
        assert_eq!(ctx[1].title, "复制链接加标题");
        assert_eq!(ctx[0].role.as_deref(), Some("favor-copy-link"));
        assert_eq!(ctx[1].role.as_deref(), Some("favor-copy-link-title"));
    }

    #[test]
    fn favor_link_empty_name_uses_csharp_visual_title_and_icon() {
        let untitled = FavorRecord {
            storage_revision: None,
            id: "link-empty".into(),
            parent_id: None,
            is_folder: false,
            name: String::new(),
            kind: ClipKind::LinkClipItem,
            body_text: Some("https://example.com".into()),
            file_paths: Vec::new(),
            image_path: None,
            children: Vec::new(),
            create_time: chrono::Utc::now(),
            update_time: chrono::Utc::now(),
            source_clip_id: None,
            html_data: None,
            html_source_url: None,
            qq_rich_text: None,
            source_process_name: String::new(),
            sub_index: 0,
            icon: String::new(),
            execute_param: String::new(),
        };
        let nodes = favor_leaf_nodes(&untitled, &MenuBuildContext::default());
        assert_eq!(nodes[0].title, "\r\nhttps://example.com");
        assert_eq!(
            nodes[0].icon,
            "https://helperservice.getquicker.cn/favicon/get/example.com"
        );

        let mut custom = untitled.clone();
        custom.icon = "fa:Solid_Star".into();
        let star = favor_leaf_nodes(&custom, &MenuBuildContext::default());
        assert_eq!(star[0].icon, "fa:Solid_Star");

        let mut bad = untitled;
        bad.body_text = Some("not a url".into());
        let fallback = favor_leaf_nodes(&bad, &MenuBuildContext::default());
        assert_eq!(fallback[0].icon, "fa:Light_Link");
    }

    #[test]
    fn style_icon_size_and_default_location_match_csharp() {
        let mut cfg = MenuGlobalSettings::default();
        let style = style_from_config(&cfg);
        assert_eq!(style["showMenuLocation"], 0);
        assert_eq!(style["iconSize"], 20.0);
        cfg.menu_font_size = 20.0;
        assert_eq!(style_from_config(&cfg)["iconSize"], 26.0);
    }

    #[test]
    fn window_context_matches_csharp_get_context_menu_items() {
        let notepad = WindowInfo {
            hwnd: 42,
            title: "readme.txt - Notepad".into(),
            process_name: "notepad".into(),
            exe_path: r"C:\Windows\System32\notepad.exe".into(),
            class_name: "Notepad".into(),
            left: 10,
            top: 20,
            width: 300,
            height: 200,
            opacity: 255,
            ..WindowInfo::default()
        };
        let items = window_context_from_info(&notepad);
        let roles: Vec<&str> = items.iter().filter_map(|n| n.role.as_deref()).collect();
        assert!(!roles.contains(&"window-open-exe"));
        assert!(roles.contains(&"window-close-class"));
        assert!(roles.contains(&"window-kill-all"));
        assert!(roles.contains(&"window-locate"));
        assert!(roles.iter().filter(|r| **r == "window-copy").count() >= 7);
        assert_eq!(
            items
                .iter()
                .find(|n| n.role.as_deref() == Some("window-kill-all"))
                .unwrap()
                .title,
            "结束所有 notepad 进程"
        );
        assert_eq!(
            items
                .iter()
                .find(|n| n.role.as_deref() == Some("window-locate"))
                .unwrap()
                .description,
            r"C:\Windows\System32\notepad.exe"
        );
        let view = items
            .iter()
            .find(|n| n.title == "复制全部信息")
            .expect("copy all info");
        assert!(view.description.contains("进程名:  notepad"));
        assert!(view.description.contains("位置:  10,20,300,200"));

        let explorer = WindowInfo {
            hwnd: 7,
            process_name: "explorer".into(),
            class_name: "CabinetWClass".into(),
            exe_path: r"C:\Windows\explorer.exe".into(),
            ..WindowInfo::default()
        };
        let exp_items = window_context_from_info(&explorer);
        assert!(exp_items
            .iter()
            .any(|n| n.role.as_deref() == Some("window-open-exe")
                && n.title == "打开新的 explorer 窗口"));
        assert_eq!(
            exp_items
                .iter()
                .find(|n| n.role.as_deref() == Some("window-kill-all"))
                .unwrap()
                .title,
            "重启explorer"
        );

        let guid = WindowInfo {
            process_name: "app".into(),
            class_name: "HwndWrapper[app;;aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee]".into(),
            ..WindowInfo::default()
        };
        let guid_items = window_context_from_info(&guid);
        let guid_roles: Vec<&str> = guid_items.iter().filter_map(|n| n.role.as_deref()).collect();
        assert!(!guid_roles.contains(&"window-close-class"));
    }

    fn rec(
        id: &str,
        parent: Option<&str>,
        is_folder: bool,
        name: &str,
        children: &[&str],
    ) -> FavorRecord {
        FavorRecord {
            storage_revision: None,
            id: id.into(),
            parent_id: parent.map(|s| s.to_string()),
            is_folder,
            name: name.into(),
            kind: if is_folder {
                ClipKind::FavoriteClipItem
            } else {
                ClipKind::TextClipItem
            },
            body_text: if is_folder {
                None
            } else {
                Some(name.into())
            },
            file_paths: Vec::new(),
            image_path: None,
            children: children.iter().map(|s| (*s).to_string()).collect(),
            create_time: chrono::Utc::now(),
            update_time: chrono::Utc::now(),
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
    fn favor_host_icon_and_summary_match_csharp_wrapper() {
        let folder = rec("home", None, true, "首页", &[]);
        let node = favor_folder_node(&folder, Vec::new(), &UserIcon::default());
        assert_eq!(node.icon, "fa:Light_Folder");
        assert_eq!(node.description, "【收藏夹】");
        assert_eq!(node.title, "首页");

        let text = rec("n", Some("home"), false, "hello", &[]);
        let leaf = favor_command_node(&text, &UserIcon::default());
        assert_eq!(leaf.icon, "");
        assert_eq!(leaf.description, "hello");
        assert_eq!(leaf.title, "hello");

        let mut file = rec("f", None, false, "", &[]);
        file.kind = ClipKind::FileClipItem;
        file.file_paths = vec![r"D:\a\b.txt".into()];
        assert_eq!(file.host_icon(), r"icon:D:\a\b.txt");
        assert_eq!(file.visual_title(), "b.txt");
        assert_eq!(file.summary(), r"D:\a\b.txt");

        let mut icons = UserIcon::default();
        icons.text = "fa:Solid_Font".into();
        icons.favorites = "fa:Solid_Star".into();
        icons.file = "fa:Solid_File".into();
        assert_eq!(favor_command_node(&text, &icons).icon, "fa:Solid_Font");
        assert_eq!(
            favor_folder_node(&folder, Vec::new(), &icons).icon,
            "fa:Solid_Star"
        );
        assert_eq!(file.host_icon_with(&icons), r"icon:D:\a\b.txt");

        let mut custom = rec("c", None, true, "x", &[]);
        custom.icon = "fa:Solid_Heart".into();
        assert_eq!(custom.host_icon_with(&icons), "fa:Solid_Heart");
    }

    #[test]
    fn favor_root_match_configs_and_save_as_match_csharp() {
        let mut menu_cfg = MenuGlobalSettings::default();
        menu_cfg.show_window_menu_item = false;
        menu_cfg.show_book_marks_menu = true;
        let favors = vec![
            rec("home", None, true, "首页", &["note"]),
            rec("note", Some("home"), false, "note", &[]),
            rec("work", None, true, "工作", &["task"]),
            rec("task", Some("work"), false, "task", &[]),
        ];
        let mut ctx = MenuBuildContext {
            menu_cfg: menu_cfg.clone(),
            favors: favors.clone(),
            bookmarks: None,
            ..MenuBuildContext::default()
        };
        let (items, _, _) = favor_root(&ctx);
        let roles: Vec<&str> = items.iter().filter_map(|n| n.role.as_deref()).collect();
        assert_eq!(roles[0], "favor-add");
        assert!(roles.contains(&"bookmark"));
        assert!(roles.contains(&"favor"));
        assert!(!roles.contains(&"favor-folder"));
        assert_eq!(
            items
                .iter()
                .find(|n| n.role.as_deref() == Some("favor"))
                .unwrap()
                .title,
            "note"
        );

        ctx.favor_menu = FavorMenuData {
            config_items: vec![
                FavorMenuConfigItem {
                    favorite_id1: "home".into(),
                    proc_names: "notepad".into(),
                    ..FavorMenuConfigItem::default()
                },
                FavorMenuConfigItem {
                    favorite_id1: "work".into(),
                    proc_names: "notepad".into(),
                    ..FavorMenuConfigItem::default()
                },
            ],
            ..FavorMenuData::default()
        };
        ctx.target = MenuTargetWindow {
            process_name: "notepad".into(),
            class_name: "Notepad".into(),
            title: "readme".into(),
            browser_url: None,
        };
        let (multi, _, _) = favor_root(&ctx);
        let multi_roles: Vec<&str> = multi.iter().filter_map(|n| n.role.as_deref()).collect();
        assert_eq!(
            multi
                .iter()
                .filter(|n| n.role.as_deref() == Some("favor-folder"))
                .map(|n| n.title.as_str())
                .collect::<Vec<_>>(),
            ["首页", "工作"]
        );
        assert!(!multi_roles.contains(&"favor"));

        ctx.target = MenuTargetWindow {
            class_name: crate::favor_menu::SAVE_AS_CLASS.into(),
            process_name: "notepad".into(),
            title: "另存为".into(),
            browser_url: None,
        };
        let (save_as, _, _) = favor_root(&ctx);
        let save_roles: Vec<&str> = save_as.iter().filter_map(|n| n.role.as_deref()).collect();
        assert_eq!(save_roles[0], "opened-folder");
        assert_eq!(save_roles[1], "recent-files");
        assert!(!save_roles.contains(&"favor-add"));
        assert_eq!(save_as[1].description, "最近使用的文件或者文件夹");

        let (bookmark_ctx, _) = context_for(
            Some("bookmark"),
            &json!({ "url": "https://example.com", "title": "ex" }),
            &[],
            0,
            None,
        );
        assert_eq!(bookmark_ctx[0].role.as_deref(), Some("bookmark-copy"));
        assert_eq!(bookmark_ctx[1].title, "复制标题+链接");
        assert_eq!(bookmark_ctx[1].description, "标题...\r\n链接...");
        assert_eq!(bookmark_ctx[1].role.as_deref(), Some("bookmark-copy-title"));
        assert_eq!(bookmark_ctx[2].role.as_deref(), Some("bookmark-delete"));
        assert_eq!(bookmark_ctx[2].icon, "fa:Light_Times:red");
        let (folder_ctx, _) = context_for(
            Some("bookmark"),
            &json!({ "url": "", "id": "0", "title": "浏览器书签" }),
            &[],
            0,
            None,
        );
        assert!(folder_ctx.is_empty());
        let (nested_folder, _) = context_for(Some("bookmark-folder"), &json!({ "id": "1" }), &[], 0, None);
        assert!(nested_folder.is_empty());
        let tree = parse_bookmarks_file(
            r#"{
              "roots": {
                "bookmark_bar": { "name": "书签栏", "guid": "bar", "children": [] }
              }
            }"#,
        )
        .unwrap();
        let root = bookmark_session_root(Some(&tree));
        assert_eq!(root.role.as_deref(), Some("bookmark"));
        assert!(!root.has_context_menu);
        assert!(root.icon.contains("99A6D79D46C1F1E67869507DFA7D7032F6EA63E4"));
    }

    #[test]
    fn window_group_payload_uses_last_focus_like_csharp() {
        let first = WindowInfo {
            hwnd: 11,
            title: "min".into(),
            process_name: "app".into(),
            minimized: true,
            ..WindowInfo::default()
        };
        let last = WindowInfo {
            hwnd: 22,
            title: "shown".into(),
            process_name: "app".into(),
            minimized: false,
            ..WindowInfo::default()
        };
        let node = window_group_node(&[first, last]);
        assert_eq!(node.payload["windowHandle"], 22);
        assert_eq!(node.role.as_deref(), Some("window-group"));
    }

    #[test]
    fn window_primary_actions_survive_registry_projection_and_group_expansion() {
        let first = WindowInfo {
            hwnd: 11,
            title: "minimized".into(),
            process_name: "app".into(),
            minimized: true,
            ..WindowInfo::default()
        };
        let last = WindowInfo {
            hwnd: 22,
            title: "shown".into(),
            process_name: "app".into(),
            ..WindowInfo::default()
        };
        let single = window_node(&last);
        let group = window_group_node(&[first.clone(), last.clone()]);
        let single_id = single.id.clone();
        let group_id = group.id.clone();
        let mut registry = MenuRegistry::default();
        let created = registry.create_fixture_session(
            &MenuBuildContext::default(), vec![single, group],
        );
        let session = created["sessionId"].as_str().unwrap();
        for (dto, id, role) in [
            (&created["root"]["items"][0], &single_id, "window"),
            (&created["root"]["items"][1], &group_id, "window-group"),
        ] {
            assert_eq!(dto["id"], id.as_str());
            assert_eq!(dto["role"], role);
            assert_eq!(dto["invokeMode"], "InvokeAndExpand");
            assert_eq!(dto["payload"]["windowHandle"], 22);
            assert_eq!(dto["hasChildren"], true);
            assert_eq!(dto["hasContextMenu"], true);
            assert_eq!(registry.node(session, id).unwrap().invoke_mode, "InvokeAndExpand");
        }

        // Inject the native query's result: expanding the group must retain
        // each child's activation action without querying desktop windows.
        let load = registry.prepare_nodes(session, &group_id, CollectionKind::Children, None)
            .unwrap().unwrap();
        let children = registry.commit_nodes(load.fixture_result(vec![
            window_node(&first), window_node(&last),
        ])).unwrap();
        let items = children["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        for (dto, hwnd) in items.iter().zip([11, 22]) {
            assert_eq!(dto["invokeMode"], "InvokeAndExpand");
            assert_eq!(dto["payload"]["windowHandle"], hwnd);
            assert_eq!(dto["hasChildren"], true);
            assert_eq!(dto["hasContextMenu"], true);
            let node = registry.node(session, dto["id"].as_str().unwrap()).unwrap();
            assert!(matches!(node.lazy, Some(LazyKind::WindowOps { hwnd: h }) if h == hwnd));
        }
    }

    #[test]
    fn window_root_context_prepends_global_setting_like_csharp() {
        let (root, _) = context_for(Some("window"), &json!({ "root": true }), &[], 0, None);
        assert_eq!(root[0].role.as_deref(), Some("setting"));
        assert_eq!(root[0].title, "全局菜单设置");
        assert_eq!(root[0].payload["page"], "menu_global");
        assert!(root.iter().any(|n| n.role.as_deref() == Some("setting")));

        let (empty, _) = context_for(Some("window"), &json!({}), &[], 0, None);
        assert_eq!(empty[0].role.as_deref(), Some("setting"));

        let (grouped, _) = context_for(Some("window-group"), &json!({}), &[], 0, None);
        assert!(grouped.is_empty());
    }

    #[test]
    fn text_and_file_root_context_match_csharp_wrappers() {
        let (leaf, _) = context_for(
            Some("text"),
            &json!({ "actionId": "0" }),
            &[],
            0,
            None,
        );
        assert_eq!(leaf[0].role.as_deref(), Some("text-edit"));
        assert_eq!(leaf[1].role.as_deref(), Some("text-delete"));

        let (group, _) = context_for(Some("text"), &json!({ "group": "格式" }), &[], 0, None);
        assert!(group.is_empty());

        let (root, _) = context_for(Some("text"), &json!({ "root": true }), &[], 0, None);
        assert_eq!(root[0].title, "添加");
        assert_eq!(root[0].role.as_deref(), Some("text-add"));
        assert_eq!(root[1].title, "文本操作设置");

        let (file_root_ctx, _) = context_for(Some("file"), &json!({ "root": true }), &[], 0, None);
        let roles: Vec<&str> = file_root_ctx.iter().filter_map(|n| n.role.as_deref()).collect();
        assert_eq!(roles, ["file-add-dir", "file-add-opened-root", "setting"]);
        assert_eq!(file_root_ctx.last().unwrap().payload["page"], "menu_file");
        assert!(!roles.contains(&"file-add-clipboard"));

        let (with_clip, _) = context_for(
            Some("file"),
            &json!({ "root": true }),
            &[],
            0,
            Some(r"C:\Windows"),
        );
        let clip_roles: Vec<&str> = with_clip.iter().filter_map(|n| n.role.as_deref()).collect();
        assert_eq!(
            clip_roles,
            ["file-add-dir", "file-add-opened-root", "file-add-clipboard", "setting"]
        );
        assert_eq!(with_clip[2].title, "粘贴路径添加");
        assert_eq!(with_clip[2].payload["path"], r"C:\Windows");
    }

    #[test]
    fn favor_action_template_expands_like_csharp_get_favor_action_menu_wrapper() {
        let folder = rec("home", None, true, "首页", &["win"]);
        let mut action = rec("win", Some("home"), false, "窗口", &[]);
        action.kind = ClipKind::ActionClipItem;
        action.body_text = Some(crate::quicker_action::MENU_WINDOW.into());
        let ctx = MenuBuildContext {
            favors: vec![folder.clone(), action.clone()],
            ..MenuBuildContext::default()
        };
        let (nodes, _) = favor_children_of(&ctx.favors, "home", &ctx);
        assert_eq!(nodes[0].role.as_deref(), Some("window"));
        assert_eq!(nodes[0].title, "窗口管理");
        assert_eq!(nodes[0].payload["root"], true);

        action.body_text = Some(crate::quicker_action::MENU_FILE.into());
        action.name = "文件".into();
        let ctx = MenuBuildContext {
            favors: vec![folder.clone(), action.clone()],
            ..MenuBuildContext::default()
        };
        let (nodes, _) = favor_children_of(&ctx.favors, "home", &ctx);
        assert_eq!(nodes[0].role.as_deref(), Some("file"));
        assert_eq!(nodes[0].title, "文件");
        assert_eq!(nodes[0].payload["root"], true);

        action.body_text = Some("custom-user-action".into());
        let ctx = MenuBuildContext {
            favors: vec![folder, action],
            ..MenuBuildContext::default()
        };
        let (nodes, _) = favor_children_of(&ctx.favors, "home", &ctx);
        assert_eq!(nodes[0].role.as_deref(), Some("favor"));
    }

    #[test]
    fn quicker_json_template_metadata_expands_but_parameters_keep_the_action() {
        let folder = rec("home", None, true, "首页", &["action"]);
        let mut action = rec("action", Some("home"), false, "模板收藏", &[]);
        action.kind = ClipKind::ActionClipItem;
        for key in ["TemplateId", "sharedActionId"] {
            for (template, role) in [
                (crate::quicker_action::MENU_WINDOW, "window"),
                (crate::quicker_action::MENU_BOOKMARKS, "bookmark"),
                (crate::quicker_action::MENU_FILE, "file"),
                (crate::quicker_action::MENU_REOPEN, "reopen-root"),
                (crate::quicker_action::MENU_TEXT, "text"),
                (crate::quicker_action::FAVOR_ADDER, "favor-add"),
            ] {
                action.body_text = Some(json!({"Id":"local-action-id", key:template}).to_string());
                for (parameter, expected) in [("", role), (" ", "favor")] {
                    action.execute_param = parameter.into();
                    let ctx = MenuBuildContext {favors:vec![folder.clone(), action.clone()], ..MenuBuildContext::default()};
                    let (nodes, _) = favor_children_of(&ctx.favors, "home", &ctx);
                    assert_eq!(nodes[0].role.as_deref(), Some(expected), "{key}: {template} / {parameter:?}");
                }
            }
        }
    }

    #[test]
    fn host_invoke_mode_matches_csharp_click_run() {
        let folder = rec("home", None, true, "首页", &[]);
        let node = favor_folder_node(&folder, Vec::new(), &UserIcon::default());
        assert_eq!(node.invoke_mode, "None");

        assert_eq!(window_manage_folder_node().invoke_mode, "None");
        assert_eq!(file_menu_root_folder_node(&folder).invoke_mode, "None");
        assert_eq!(text_menu_root_folder_node(&folder).invoke_mode, "None");

        let adder = folder_lazy(
            "添加选中",
            "favor-add",
            json!({}),
            "fa:Light_PlusCircle",
            LazyKind::FavorAdder,
        );
        assert_eq!(adder.invoke_mode, "None");

        let bookmark_folder = bookmark_node(&BookmarkNode {
            id: "1".into(),
            title: "folder".into(),
            url: String::new(),
            children: Vec::new(),
        });
        assert_eq!(bookmark_folder.role.as_deref(), Some("bookmark-folder"));
        assert_eq!(bookmark_folder.invoke_mode, "None");

        let reopen = reopen_root_node(&[r"D:\closed".into()]);
        assert_eq!(reopen.invoke_mode, "InvokeAndExpand");

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"x").unwrap();
        let file_dir = file_node_with_status(
            dir.path().to_str().unwrap(), &FileMenuSettings::default(),
            file_menu_extract::MenuPathStatus::Present,
        );
        assert_eq!(file_dir.invoke_mode, "InvokeAndExpand");
        assert_eq!(file_dir.role.as_deref(), Some("file"));
    }
}
