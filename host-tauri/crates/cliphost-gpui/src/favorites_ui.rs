//! Favorites presentation memory and occurrence navigation. Business commands
//! continue using canonical IDs; only presentation uses complete paths.
use gpui_rsx::rsx;
use super::*;

impl FavoriteTree {
    pub(super) fn graph(&self) -> TreeGraph<'_> {
        TreeGraph {
            items: &self.items,
            roots: self
                .ordered_root_items
                .as_ref()
                .unwrap_or(&self._root_items),
            children_by_id: &self.children_by_id,
        }
    }

    pub(super) fn direct_page(&self, parent: Option<&str>, skip: usize, take: usize) -> FavoritePage {
        let ids = favorites_tree::direct_members(self.graph(), parent);
        let total = ids.len();
        let end = skip.saturating_add(take).min(total);
        FavoritePage {
            items: ids.into_iter().skip(skip).take(take)
                .filter_map(|id| self.items.get(&id).cloned()).collect(),
            total,
            has_more: end < total,
            next_skip: end,
        }
    }
}

pub(super) struct SidebarRow {
    pub(super) id: String,
    pub(super) title: String,
    pub(super) path: OccurrencePath,
    pub(super) depth: usize,
    pub(super) expanded: bool,
    pub(super) has_folders: bool,
}

impl NativeFavoritesView {
    pub(super) fn favorite_scope(&self) -> FavoriteScope {
        FavoriteScope { layout: self.memory.layout, folder: self.scope_path(), query: self.query.clone(),
            category: self.category, sort_title: self.sort_title }
    }

    pub(super) fn reconcile_rendered_rows(&mut self) {
        let rows: Vec<_> = self.visible.iter().enumerate().map(|(index, item)|
            rendered_row_identity(item, self.row_path(index), self.tree_rows.get(index)
                .filter(|_| self.memory.layout.is_tree()))).collect();
        self.previous_rendered_paths = self.rendered_rows.iter().map(|row| row.path.clone()).collect();
        crate::list_reconcile::reconcile(&self.list, &self.rendered_rows, &rows);
        self.rendered_rows = rows;
        self.rendered_scope = Some(self.favorite_scope());
    }

    pub(super) fn resource_occurrences(&self) -> HashSet<(String, OccurrencePath, String)> {
        self.visible.iter().enumerate().map(|(index, item)| (
            item.id.clone(), self.rendered_rows.get(index).map(|row| row.path.clone())
                .unwrap_or_else(|| self.row_path(index)), preview_key(item),
        )).collect()
    }

    pub(super) fn retain_refreshed_content_resources(&mut self,
        previous: &HashSet<(String, OccurrencePath, String)>, cx: &mut Context<Self>) {
        // Resources are keyed by item and content, not occurrence: a moved row
        // with an unchanged preview key keeps its decoded images.
        let previous: HashSet<_> = previous.iter().map(|(id, _, key)| (id, key)).collect();
        let retained_ids: HashSet<_> = self.resource_occurrences().into_iter()
            .filter(|(id, _, key)| previous.contains(&(id, key))).map(|(id, _, _)| id).collect();
        let projections: BTreeMap<_, _> = self.visible.iter().filter(|item| retained_ids.contains(&item.id))
            .filter_map(|item| favorite_resource_projection(item).map(|projection| (item.id.clone(), projection))).collect();
        let foreground = self.tokens.theme(self.dark).text.clone();
        self.content_cache.advance_retaining(self.generation, |key| key.mode == ContentMode::Standard
            && key.foreground == foreground && projections.get(&key.item_id) == Some(&key.projection));
        self.content_cache.release_evicted(cx);
    }

    pub(crate) fn invalidate_content_resources(&mut self, ids: Option<&HashSet<String>>, cx: &mut Context<Self>) {
        let ids = ids.map(|ids| self.take_placement_only_changes(ids));
        self.content_cache.advance_retaining(self.generation, |key|
            ids.as_ref().is_some_and(|ids| !ids.contains(&key.item_id)));
        self.content_cache.release_evicted(cx);
    }

    pub(super) fn load_view_memory(&mut self, cx: &mut Context<Self>) {
        self.loading = true;
        let root = self.host.data_root();
        self.memory_load_task = Some(cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move { FavoritesViewStore::load(&root) })
                .await;
            let _ = this.update(cx, |this, cx| {
                match loaded {
                    Ok(store) => {
                        this.memory = store.snapshot();
                        this.memory_store = store;
                    }
                    Err(error) => {
                        this.memory_load_error = Some(error.clone());
                        this.error = Some(error);
                    }
                }
                if !this.explicit_folder_route && this.memory.layout.has_sidebar() {
                    this.folder = this.memory.active_folder.last().cloned();
                }
                if !this.memory.layout.has_sidebar() {
                    this.folder = None;
                }
                this.expanded = this.memory.sidebar_expanded.iter().cloned().collect();
                this.memory_ready = true;
                this.restore_context_memory();
                this.refresh(cx);
                cx.notify();
            });
        }));
    }

    pub(super) fn canonical_path(&self, id: &str) -> OccurrencePath {
        if id.is_empty() {
            return Vec::new();
        }
        self.canonical_paths
            .get(id)
            .cloned()
            .unwrap_or_else(|| vec![id.to_owned()])
    }

    pub(super) fn scope_path(&self) -> OccurrencePath {
        if self.memory.layout == LayoutMode::Tree {
            return Vec::new();
        }
        self.folder
            .as_deref()
            .map(|id| {
                if self
                    .memory
                    .active_folder
                    .last()
                    .is_some_and(|last| last == id)
                {
                    self.memory.active_folder.clone()
                } else {
                    self.canonical_path(id)
                }
            })
            .unwrap_or_default()
    }

    pub(super) fn row_path(&self, index: usize) -> OccurrencePath {
        if self.memory.layout.is_tree() {
            self.tree_rows
                .get(index)
                .map(|row| row.path.clone())
                .unwrap_or_default()
        } else {
            self.visible
                .get(index)
                .map(|item| {
                    if self.query.is_empty() {
                        let mut path = self.scope_path();
                        path.push(item.id.clone());
                        path
                    } else {
                        self.canonical_path(&item.id)
                    }
                })
                .unwrap_or_default()
        }
    }

    pub(super) fn current_scroll_anchor(&self) -> Option<ScrollAnchor> {
        let offset = self.list.logical_scroll_top();
        let path = self.rendered_rows.get(offset.item_ix).map(|row| row.path.clone())
            .unwrap_or_else(|| self.row_path(offset.item_ix));
        (!path.is_empty()).then(|| ScrollAnchor {
            path,
            offset: f32::from(offset.offset_in_item),
        })
    }

    pub(super) fn remember_view_memory(&mut self) {
        if !self.memory_ready || self.drag_navigation_active() {
            return;
        }
        self.memory.sidebar_expanded = self.expanded.iter().cloned().collect();
        if !self.filter_active && self.query.is_empty() && self.category == Category::All {
            self.capture_context_memory();
        }
        self.memory_store.remember(self.memory.clone());
    }

    fn capture_context_memory(&mut self) {
        if !self.memory_ready || self.pending_context_restore {
            return;
        }
        let scope = self.scope_path();
        let selected = (!self.selected_path.is_empty()).then(|| self.selected_path.clone());
        let anchor = self.current_scroll_anchor();
        let context = self.memory.context_mut(self.memory.layout, &scope);
        context.selected = selected;
        context.scroll_anchor = anchor;
    }

    /// Query/category fields may already contain the new filter while the list
    /// still contains normal rows. Capture those rows once on entry, and keep
    /// temporary filtering selection/scroll out of the normal context on exit.
    pub(super) fn prepare_filter_transition(&mut self) {
        let active = !self.query.is_empty() || self.category != Category::All;
        if active == self.filter_active {
            return;
        }
        if active {
            self.capture_context_memory();
            self.filter_active = true;
        } else {
            self.filter_active = false;
            self.restore_context_memory();
        }
    }

    pub(crate) fn flush_for_exit(&mut self) -> FavoritesViewStore {
        self.remember_view_memory();
        self.memory_store.clone()
    }

    pub(super) fn save_view_memory(&mut self, cx: &mut Context<Self>) {
        self.remember_view_memory();
        if !self.memory_store.has_pending() {
            return;
        }
        let store = self.memory_store.clone();
        self.memory_save_task = Some(cx.spawn(async move |this, cx| {
            Timer::after(Duration::from_millis(100)).await;
            let result = cx
                .background_executor()
                .spawn(async move { store.flush() })
                .await;
            if let Err(error) = result {
                let _ = this.update(cx, |this, cx| {
                    this.error = Some(error);
                    cx.notify();
                });
            }
        }));
    }

    pub(super) fn restore_context_memory(&mut self) {
        let context = self
            .memory
            .context(self.memory.layout, &self.scope_path())
            .cloned()
            .unwrap_or_default();
        self.selected_path = context.selected.unwrap_or_default();
        self.selected = self.selected_path.last().cloned().unwrap_or_default();
        self.pending_scroll_anchor = context.scroll_anchor;
        self.pending_context_restore = true;
    }

    pub(super) fn restore_scroll_anchor(&mut self, previous: Option<ScrollAnchor>) {
        let anchor = if self.pending_context_restore {
            self.pending_context_restore = false;
            self.pending_scroll_anchor.take()
        } else {
            previous
        };
        if let Some(anchor) = anchor {
            let paths: Vec<_> = self.rendered_rows.iter().map(|row| row.path.clone()).collect();
            if let Some((index, exact)) = surviving_anchor_index(&self.previous_rendered_paths, &paths, &anchor.path) {
                self.list.scroll_to(gpui::ListOffset {
                    item_ix: index,
                    offset_in_item: px(if exact { anchor.offset } else { 0.0 }),
                });
            }
        }
    }

    pub(super) fn reconcile_view_memory(&mut self) {
        self.canonical_paths = favorites_tree::first_occurrence_paths(self.tree.graph());
        if let Some(folder) = &self.folder {
            if self.memory.active_folder.last() != Some(folder) {
                self.memory.active_folder = self.canonical_path(folder);
            }
        }
        let mut paths = Vec::new();
        paths.push(self.memory.active_folder.clone());
        paths.extend(self.memory.sidebar_expanded.iter().cloned());
        for context in &self.memory.contexts {
            paths.push(context.folder.clone());
            paths.extend(context.expanded.iter().cloned());
            paths.extend(context.selected.iter().cloned());
            paths.extend(
                context
                    .scroll_anchor
                    .iter()
                    .map(|anchor| anchor.path.clone()),
            );
        }
        let valid = favorites_tree::valid_remembered_paths(self.tree.graph(), paths.iter());
        self.memory.prune(&valid);
        if self.memory.layout.has_sidebar() {
            self.folder = self
                .memory
                .active_folder
                .last()
                .cloned()
                .filter(|id| self.tree.items.get(id).is_some_and(|item| item.is_folder));
        }
        self.expanded = self.memory.sidebar_expanded.iter().cloned().collect();
        if self.pending_context_restore {
            self.restore_context_memory();
        }
        if let Some(id) = self.pending_reveal.take() {
            if self.memory.layout == LayoutMode::Tree {
                if let Some(path) = self.canonical_paths.get(&id).cloned() {
                    let context = self.memory.context_mut(LayoutMode::Tree, &[]);
                    for length in 1..path.len() {
                        context.expanded.insert(path[..length].to_vec());
                    }
                    context.selected = Some(path.clone());
                    let anchor = ScrollAnchor {
                        path: path.clone(),
                        offset: 0.0,
                    };
                    context.scroll_anchor = Some(anchor.clone());
                    self.selected_path = path;
                    self.selected = id;
                    self.pending_scroll_anchor = Some(anchor);
                    self.pending_context_restore = true;
                }
            }
        }
    }

    pub(super) fn sidebar_rows(&self) -> Vec<SidebarRow> {
        // A folder beneath ordinary content remains navigable. Traverse such
        // intermediate nodes automatically, without persisting their expansion
        // in the sidebar or transferring paths to another parent occurrence.
        let nonfolder = |item: &FavorItemDto| !item.is_folder;
        let mut options = ProjectionOptions::new(&self.expanded);
        options.auto_expand = Some(&nonfolder);
        let projection = favorites_tree::project_tree(self.tree.graph(), options);

        // Compute which children can lead to a real folder once. A folder with
        // only text descendants must not receive an empty expansion control.
        let mut children = self.tree.children_by_id.clone();
        for (id, item) in &self.tree.items {
            if !item.parent_id.is_empty() && !self.tree.children_by_id.contains_key(&item.parent_id)
            {
                children
                    .entry(item.parent_id.clone())
                    .or_default()
                    .push(id.clone());
            }
        }
        let mut parents: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for (parent, ids) in &children {
            for id in ids {
                parents.entry(id).or_default().push(parent);
            }
        }
        let mut leads_to_folder: HashSet<&str> = HashSet::new();
        let mut pending: Vec<&str> = self
            .tree
            .items
            .values()
            .filter(|item| item.is_folder)
            .map(|item| item.id.as_str())
            .collect();
        while let Some(id) = pending.pop() {
            if leads_to_folder.insert(id) {
                pending.extend(parents.get(id).into_iter().flatten().copied());
            }
        }
        projection
            .rows
            .into_iter()
            .filter_map(|row| {
                if row.is_cycle {
                    return None;
                }
                let item = self
                    .tree
                    .items
                    .get(&row.item_id)
                    .filter(|item| item.is_folder)?;
                let depth = row
                    .path
                    .iter()
                    .take(row.path.len().saturating_sub(1))
                    .filter(|id| self.tree.items.get(*id).is_some_and(|item| item.is_folder))
                    .count();
                let has_folders = row.has_children
                    && children.get(&row.item_id).is_some_and(|ids| {
                        ids.iter().any(|id| leads_to_folder.contains(id.as_str()))
                    });
                Some(SidebarRow {
                    id: row.item_id,
                    title: title(item),
                    depth,
                    expanded: row.expanded,
                    has_folders,
                    path: row.path,
                })
            })
            .collect()
    }

    pub(super) fn select_folder_path(
        &mut self,
        path: OccurrencePath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.remember_view_memory();
        if self.scope_path() != path {
            self.host.retire_native_clipboard_list_input();
        }
        self.explicit_folder_route = false;
        self.folder = path.last().cloned();
        self.memory.active_folder = path;
        self.restore_context_memory();
        self.focus.focus(window);
        self.restore_browsing(window);
        self.refresh(cx);
        self.save_view_memory(cx);
    }

    pub(super) fn rebuild_tree_visible(&mut self) {
        let scope = self.scope_path();
        let expanded = self
            .memory
            .context(self.memory.layout, &scope)
            .map(|context| context.expanded.iter().cloned().collect())
            .unwrap_or_default();
        let normalized_query = cliphost_core::clip_fuzzy::normalize_for_search(&self.query);
        let matches = |item: &FavorItemDto| {
            (self.category == Category::All || category(item) == self.category)
                && (normalized_query.is_empty()
                    || cliphost_core::clip_fuzzy::matches(
                        &format!("{} {}", title(item), item.summary),
                        &normalized_query,
                    ))
        };
        let compare = |a: &FavorItemDto, b: &FavorItemDto| title(a).cmp(&title(b));
        let mut options = ProjectionOptions::new(&expanded);
        options.scope = (!scope.is_empty()).then_some(scope.as_slice());
        if !self.query.is_empty() || self.category != Category::All {
            options.matches = Some(&matches);
        }
        if self.sort_title {
            options.compare = Some(&compare);
        }
        let mut projection = favorites_tree::project_tree(self.tree.graph(), options);
        // Filtering auto-expands matching paths; folders folded by hand during
        // this filter keep their row and hide their descendants.
        let filtering = !self.query.is_empty() || self.category != Category::All;
        let key = filtering.then(|| (self.query.clone(), self.category));
        if self.filter_collapse_key != key {
            self.filter_collapse_key = key;
            self.filter_collapsed.clear();
        }
        if filtering && !self.filter_collapsed.is_empty() {
            let collapsed = &self.filter_collapsed;
            projection.rows.retain(|row| {
                !collapsed
                    .iter()
                    .any(|path| row.path.len() > path.len() && row.path.starts_with(path))
            });
            for row in &mut projection.rows {
                if collapsed.contains(&row.path) {
                    row.expanded = false;
                }
            }
        }
        if projection.truncated {
            self.error = Some(
                tr(
                    "收藏树过大，请缩小目录或搜索范围",
                    "This tree is too large; narrow the folder or search scope",
                )
                .into(),
            );
        }
        self.tree_rows = projection.rows;
        self.visible = self
            .tree_rows
            .iter()
            .map(|row| {
                self.tree
                    .items
                    .get(&row.item_id)
                    .cloned()
                    .unwrap_or_else(|| missing_reference(&row.item_id))
            })
            .collect();
        if !self
            .tree_rows
            .iter()
            .any(|row| row.path == self.selected_path)
        {
            let previous_paths: Vec<_> = self.rendered_rows.iter().map(|row| row.path.clone()).collect();
            let paths: Vec<_> = self.tree_rows.iter().map(|row| row.path.clone()).collect();
            let neighbor = surviving_anchor_index(&previous_paths, &paths, &self.selected_path)
                .map(|(index, _)| index).unwrap_or(0);
            self.selected_path = self
                .tree_rows
                .get(neighbor)
                .map(|row| row.path.clone())
                .unwrap_or_default();
        }
        self.selected = self.selected_path.last().cloned().unwrap_or_default();
        self.has_more = false;
        self.total = self.visible.len();
        self.reconcile_rendered_rows();
    }

    pub(super) fn choose_layout(&mut self, layout: LayoutMode, cx: &mut Context<Self>) {
        if !self.memory_ready || self.memory.layout == layout {
            return;
        }
        self.remember_view_memory();
        self.memory.layout = layout;
        // Only pure tree starts at the root; pure list keeps browsing in place.
        self.folder = if layout == LayoutMode::Tree {
            None
        } else {
            self.memory.active_folder.last().cloned()
        };
        self.restore_context_memory();
        self.refresh(cx);
        self.save_view_memory(cx);
        cx.notify();
    }

    pub(super) fn toggle_tree_path(&mut self, path: OccurrencePath, cx: &mut Context<Self>) {
        if !self.memory.layout.is_tree() {
            return;
        }
        if !self.query.is_empty() || self.category != Category::All {
            // Temporary fold for this filter only; browsing memory is untouched.
            if !self.filter_collapsed.remove(&path) {
                if self.selected_path.starts_with(&path) {
                    self.selected_path = path.clone();
                    self.selected = path.last().cloned().unwrap_or_default();
                }
                self.filter_collapsed.insert(path);
            }
            self.rebuild_visible();
            cx.notify();
            return;
        }
        self.remember_view_memory();
        let scope = self.scope_path();
        let context = self.memory.context_mut(self.memory.layout, &scope);
        if context.expanded.remove(&path) {
            if self.selected_path.starts_with(&path) {
                self.selected_path = path.clone();
                self.selected = path.last().cloned().unwrap_or_default();
            }
        } else {
            context.expanded.insert(path);
        }
        self.rebuild_visible();
        self.save_view_memory(cx);
        cx.notify();
    }

    pub(super) fn tree_horizontal(&mut self, direction: i32, cx: &mut Context<Self>) {
        if !self.memory.layout.is_tree() {
            self.cycle_category(direction, cx);
            return;
        }
        let Some(index) = self
            .tree_rows
            .iter()
            .position(|row| row.path == self.selected_path)
        else {
            return;
        };
        let row = self.tree_rows[index].clone();
        if direction > 0 {
            if row.has_children && !row.expanded {
                self.toggle_tree_path(row.path, cx);
            } else if row.expanded
                && self
                    .tree_rows
                    .get(index + 1)
                    .is_some_and(|child| child.parent_path.as_ref() == Some(&row.path))
            {
                self.navigate(1, false, cx);
            }
        } else if row.has_children && row.expanded {
            self.toggle_tree_path(row.path, cx);
        } else if let Some(parent) = row.parent_path {
            if let Some(index) = self
                .tree_rows
                .iter()
                .position(|candidate| candidate.path == parent)
            {
                self.selected_path = parent;
                self.selected = self.tree_rows[index].item_id.clone();
                self.list.scroll_to_reveal_item(index);
                self.save_view_memory(cx);
                cx.notify();
            }
        }
    }

    pub(super) fn select_occurrence(&mut self, index: usize) {
        self.selected_path = self.row_path(index);
        self.selected = self
            .visible
            .get(index)
            .map(|item| item.id.clone())
            .unwrap_or_default();
    }

    pub(super) fn occurrence_key(&self, index: usize) -> String {
        let path = self.row_path(index);
        blake3::hash(&serde_json::to_vec(&path).unwrap_or_default())
            .to_hex()
            .to_string()
    }

    pub(super) fn tree_row_metadata(
        &self,
        index: usize,
        metadata: AnyElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(row) = self
            .tree_rows
            .get(index)
            .filter(|_| self.memory.layout.is_tree())
        else {
            return metadata;
        };
        let Some(item) = self.visible.get(index) else {
            return metadata;
        };
        if !item.is_folder && !row.has_children {
            return metadata;
        }
        // Filtering expands matches automatically but can still be folded by hand.
        let automatic = false;
        let path = row.path.clone();
        let icon = if item.is_folder {
            if row.expanded {
                "folder-open"
            } else {
                "folder-closed"
            }
        } else if row.expanded {
            "chevron-down"
        } else {
            "chevron-right"
        };
        let tooltip = if automatic {
            tr(
                "筛选时自动展开匹配路径",
                "Matching paths expand while filtering",
            )
        } else if row.expanded {
            tr("折叠", "Collapse")
        } else {
            tr("展开", "Expand")
        };
        let button = rsx! {
            <div id={("favorite-tree-expand", index)} size={px(18.0)} flex_none flex items_center
                justify_center />
        }
            .child(
                svg()
                    .path(
                        self.tokens
                            .icons
                            .get(icon)
                            .or_else(|| self.tokens.icons.get("folder-closed"))
                            .cloned()
                            .unwrap_or_default(),
                    )
                    .size(px(16.0))
                    .text_color(color(&self.tokens.theme(self.dark).text_secondary)),
            )
            .when(row.has_children && !automatic, |button| {
                button
                    .cursor_pointer()
                    .tooltip(favorite_tooltip(
                        tooltip.into(),
                        self.tokens.clone(),
                        self.dark,
                    ))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|_, _, _, cx| cx.stop_propagation()),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.toggle_tree_path(path.clone(), cx);
                    }))
            });
        rsx! {
            <div flex items_center gap_1 min_w_0>
                {button}
                {metadata}
            </div>
        }
            .into_any_element()
    }
}

fn missing_reference(id: &str) -> FavorItemDto {
    FavorItemDto {
        id: id.to_owned(),
        parent_id: String::new(),
        title: tr("失效的收藏引用", "Missing favorite reference").into(),
        summary: String::new(),
        kind: "MissingFavoriteReference".into(),
        is_folder: false,
        children_count: 0,
        create_time: chrono::DateTime::UNIX_EPOCH,
        update_time: chrono::DateTime::UNIX_EPOCH,
        source_process_name: String::new(),
        file_count: 0,
        file_paths: Vec::new(),
        text_length: None,
        im_rich_origin: None,
        im_rich_segments: Vec::new(),
    }
}

pub(super) fn surviving_anchor_index(old: &[OccurrencePath], new: &[OccurrencePath], anchor: &[String]) -> Option<(usize, bool)> {
    if let Some(index) = new.iter().position(|path| path == anchor) { return Some((index, true)); }
    let old_index = old.iter().position(|path| path == anchor)?;
    // Prefer the next surviving occurrence, then the previous one. A shared ID
    // in another path is never a replacement for the deleted occurrence.
    old.iter().skip(old_index + 1).chain(old[..old_index].iter().rev())
        .find_map(|path| new.iter().position(|current| current == path).map(|index| (index, false)))
        .or_else(|| (!new.is_empty()).then(|| (old_index.min(new.len() - 1), false)))
}

fn rendered_row_identity(item: &FavorItemDto, path: OccurrencePath, tree_row: Option<&TreeRow>) -> FavoriteRowIdentity {
    FavoriteRowIdentity {
        path,
        fingerprint: format!("{}:{tree_row:?}", preview_key(item)),
    }
}

#[cfg(test)]
mod refresh_tests {
    use super::*;

    fn path(ids: &[&str]) -> OccurrencePath {
        ids.iter().map(|id| (*id).to_owned()).collect()
    }

    #[test]
    fn favorites_refresh_deleted_anchor_uses_surviving_occurrence_neighbor() {
        let old = vec![path(&["a", "first"]), path(&["a", "shared"]),
            path(&["a", "next"]), path(&["b", "shared"])];
        let new = vec![old[2].clone(), old[3].clone()];
        assert_eq!(surviving_anchor_index(&old, &new, &old[1]), Some((0, false)));
        assert_eq!(surviving_anchor_index(&old, &new, &old[3]), Some((1, true)));
        assert_eq!(surviving_anchor_index(&old, &new[..1], &old[3]), Some((0, false)));
        assert_eq!(surviving_anchor_index(&old, &[], &old[1]), None);
    }

    #[test]
    fn favorites_refresh_row_identity_preserves_only_same_occurrence_and_presentation() {
        let mut item = missing_reference("shared");
        let occurrence = path(&["a", "shared"]);
        let original = rendered_row_identity(&item, occurrence.clone(), None);
        assert_eq!(rendered_row_identity(&item, occurrence.clone(), None), original);
        assert_ne!(rendered_row_identity(&item, path(&["b", "shared"]), None), original);
        // A DTO may change without advancing its timestamp.
        item.summary = "new image metadata".into();
        assert_ne!(rendered_row_identity(&item, occurrence.clone(), None), original);
        let mut row = TreeRow { item_id: item.id.clone(), path: occurrence.clone(), parent_path: Some(path(&["a"])),
            depth: 1, has_children: true, expanded: false, is_cycle: false,
            is_missing: false, context_only: false, depth_limited: false };
        let collapsed = rendered_row_identity(&item, occurrence.clone(), Some(&row));
        row.expanded = true;
        assert_ne!(rendered_row_identity(&item, occurrence, Some(&row)), collapsed);
    }
}
