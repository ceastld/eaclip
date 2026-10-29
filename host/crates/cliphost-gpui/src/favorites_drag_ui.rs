//! Favorites drag presentation. Sources identify one relationship, never a
//! guessed ParentId. The core validates and commits the complete batch.
use super::*;

#[derive(Default)]
pub(super) struct FavoriteDragUi {
    selected: HashSet<OccurrencePath>,
    anchor: Option<OccurrencePath>,
    source: Option<ContentDrag>,
    target: Option<(OccurrencePath, DropTarget)>,
    candidate: Option<(OccurrencePath, DropTarget)>,
    hover: Option<OccurrencePath>,
    hover_task: Option<Task<()>>,
    mutation_task: Option<Task<()>>,
    last_scroll: Option<Instant>,
    scroll_edge: Option<ScrollEdge>,
    scroll_task: Option<Task<()>>,
    undo: Option<Value>,
    suppress_release: bool,
    hint: Option<&'static str>,
    offer: Option<ContentOffer>,
    mutation_sequence: u64,
    mutation: Option<MutationStamp>,
    deferred_refresh: bool,
    navigation: Option<DragNavigation>,
    /// Items whose last drag write changed placement only; their change event
    /// must not discard decoded previews.
    placement_only: HashSet<String>,
}

struct DragNavigation {
    source: ContentDrag,
    folder: Option<String>,
    memory: FavoritesViewState,
    selected: String,
    selected_path: OccurrencePath,
    selected_batch: HashSet<OccurrencePath>,
    selection_anchor: Option<OccurrencePath>,
    scroll_anchor: Option<ScrollAnchor>,
    loaded: usize,
}

fn drag_navigation_finished(source: &ContentDrag) -> Option<bool> {
    (!source.active()).then(|| !source.is_claimed())
}

#[derive(Clone)]
struct ContentOffer {
    operation_id: String,
    generation: u64,
    session: u64,
    /// Occurrence of the folder that receives the drop.
    destination: OccurrencePath,
}

impl ContentOffer {
    fn is_current(&self, operation: &str, generation: u64, session: u64) -> bool {
        self.operation_id == operation && self.generation == generation && self.session == session
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct MutationStamp {
    sequence: u64,
    generation: u64,
    session: u64,
}

#[derive(Clone)]
struct ScrollEdge {
    sidebar: bool,
    bounds: gpui::Bounds<gpui::Pixels>,
    event: gpui::MouseMoveEvent,
    drag: ContentDrag,
}

impl FavoriteDragUi {
    pub(super) fn active(&self) -> bool {
        self.source.as_ref().is_some_and(ContentDrag::active)
            || self
                .navigation
                .as_ref()
                .is_some_and(|navigation| navigation.source.active())
    }
}

fn parent(path: &[String]) -> Option<String> {
    path.iter().rev().nth(1).cloned()
}

fn same_parent(a: &[String], b: &[String]) -> bool {
    a.len() == b.len() && a.get(..a.len().saturating_sub(1)) == b.get(..b.len().saturating_sub(1))
}

fn drag_scroll_delta(y: f32, top: f32, bottom: f32, sidebar: bool) -> Option<f32> {
    if !y.is_finite() || !top.is_finite() || !bottom.is_finite() || bottom <= top {
        return None;
    }
    let offset = if y < top + 24.0 {
        16.0
    } else if y > bottom - 24.0 {
        -16.0
    } else {
        return None;
    };
    // ScrollHandle stores a negative content translation; ListState stores a
    // positive distance into the content. Their directions are opposite.
    Some(if sidebar { offset } else { -offset })
}

fn row_target(
    path: &[String],
    folder: bool,
    offset: f32,
    height: f32,
    ordered: bool,
    first_child: Option<&str>,
) -> Option<DropTarget> {
    let id = path.last()?;
    let placement = favorite_drag::hit_placement(folder, offset, height, ordered)?;
    let target = DropTarget {
        parent_id: if placement == Placement::Into {
            path.last().cloned()
        } else {
            parent(path)
        },
        anchor_id: if placement == Placement::Into {
            None
        } else {
            path.last().cloned()
        },
        placement,
    };
    Some(favorite_drag::retarget_expanded_edge(target, id, first_child))
}

impl NativeFavoritesView {
    pub(super) fn content_pointer_events(&self, cx: &mut Context<Self>) -> AnyElement {
        let listener = cx.listener(|this, event: &gpui::MouseDownEvent, _, _| {
            if event.button == MouseButton::Left {
                this.begin_pointer_sequence();
            }
        });
        gpui::canvas(
            |_, _, _| (),
            move |_, _, window, _| {
                window.on_mouse_event(move |event: &gpui::MouseDownEvent, phase, window, cx| {
                    if phase == gpui::DispatchPhase::Capture {
                        listener(event, window, cx);
                    }
                });
            },
        )
        .absolute()
        .size_full()
        .into_any_element()
    }

    pub(crate) fn cancel_content_drag(&mut self, cx: &mut Context<Self>) {
        let owner = self.window;
        cx.defer(move |cx| {
            let _ = owner.update(cx, |_, window, _| favorite_drag::clear_target(window));
        });
        if let Some(source) = self.drag_ui.source.take() {
            source.cancel();
        }
        if let Some(navigation) = &self.drag_ui.navigation {
            navigation.source.cancel();
        }
        self.settle_drag_navigation(cx);
        self.drag_ui.target = None;
        self.drag_ui.candidate = None;
        self.drag_ui.hint = None;
        self.drag_ui.offer = None;
        self.drag_ui.hover = None;
        self.drag_ui.hover_task = None;
        self.drag_ui.scroll_edge = None;
        self.drag_ui.scroll_task = None;
        self.native_suppress_click = Some(Instant::now() + Duration::from_millis(200));
        cx.notify();
    }

    pub(super) fn content_mutation_pending(&self) -> bool {
        self.drag_ui.mutation.is_some()
    }

    /// Drop the moved items from a change event's invalidation once: a move
    /// keeps content, and the reload still compares every preview key.
    pub(super) fn take_placement_only_changes(&mut self, ids: &HashSet<String>) -> HashSet<String> {
        let kept = ids.difference(&self.drag_ui.placement_only).cloned().collect();
        self.drag_ui.placement_only.clear();
        kept
    }

    pub(super) fn begin_pointer_sequence(&mut self) {
        self.drag_ui.suppress_release = false;
    }

    pub(super) fn content_drag_blocks_click(&self) -> bool {
        self.drag_ui.suppress_release || self.drag_ui.active()
    }

    pub(super) fn maintain_content_drag(&mut self, cx: &mut Context<Self>) {
        self.settle_drag_navigation(cx);
        if self
            .drag_ui
            .source
            .as_ref()
            .is_some_and(|source| !source.active())
        {
            self.drag_ui.source = None;
            self.drag_ui.target = None;
            self.drag_ui.candidate = None;
            self.drag_ui.hint = None;
            self.drag_ui.hover = None;
            self.drag_ui.hover_task = None;
            self.native_suppress_click = Some(Instant::now() + Duration::from_millis(200));
            cx.notify();
        }
        if self.drag_ui.deferred_refresh && !self.busy && !favorite_drag::active(cx) {
            self.drag_ui.deferred_refresh = false;
            self.refresh(cx);
        }
    }

    pub(super) fn reset_content_target(&mut self, cx: &mut Context<Self>) {
        self.drag_ui.target = None;
        self.drag_ui.candidate = None;
        self.drag_ui.hint = None;
        self.drag_ui.offer = None;
        self.drag_ui.scroll_edge = None;
        let owner = cx.weak_entity();
        cx.defer(move |cx| {
            let _ = owner.update(cx, |this, cx| {
                if this.drag_ui.hover.as_ref().is_some_and(|path| {
                    !this
                        .drag_ui
                        .candidate
                        .as_ref()
                        .is_some_and(|(p, t)| p == path && t.placement == Placement::Into)
                }) {
                    this.drag_ui.hover = None;
                    this.drag_ui.hover_task = None;
                }
                cx.notify();
            });
        });
    }

    pub(super) fn hover_content_background(
        &mut self,
        drag: &ContentDrag,
        path: OccurrencePath,
        copy: bool,
    ) {
        let target = DropTarget {
            parent_id: path.last().cloned(),
            anchor_id: None,
            placement: Placement::Into,
        };
        self.drag_ui.candidate = Some((path.clone(), target.clone()));
        if self.accepts_content_target(drag, &target, copy) {
            self.drag_ui.target = Some((path, target));
        }
    }

    pub(super) fn offer_content_target(
        &mut self,
        drag: &ContentDrag,
        bounds: gpui::Bounds<gpui::Pixels>,
        window: &Window,
    ) {
        // A rejected row must remove a containing blank-area offer; otherwise
        // native OLE could append into the parent beneath an invalid row.
        favorite_drag::clear_target(window);
        self.drag_ui.offer = None;
        if let Some((path, target)) = &self.drag_ui.target {
            favorite_drag::offer_target(drag, target, bounds, window, self.host.clone());
            self.drag_ui.offer = Some(ContentOffer {
                operation_id: drag.operation_id.clone(),
                generation: self.generation,
                session: self.native_session,
                destination: favorite_drag::destination_path(path, target),
            });
        }
    }

    pub(super) fn content_saved(
        &mut self,
        operation: String,
        result: Result<Value, String>,
        cx: &mut Context<Self>,
    ) {
        if !self.active
            || !self.drag_ui.offer.as_ref().is_some_and(|offer| {
                offer.is_current(&operation, self.generation, self.native_session)
            })
        {
            self.refresh_after_stale_content(cx);
            return;
        }
        let offered_path = self
            .drag_ui
            .offer
            .as_ref()
            .map(|offer| offer.destination.clone())
            .unwrap_or_default();
        match result {
            Ok(response) => {
                if let Some(id) = response["selectedIds"]
                    .as_array()
                    .and_then(|ids| ids.first())
                    .and_then(Value::as_str)
                {
                    self.selected = id.to_owned();
                    self.pending_reveal = Some(id.to_owned());
                }
                self.drag_ui.selected.clear();
                self.drag_ui.anchor = None;
                if let Some(id) = response["selectedIds"]
                    .as_array()
                    .and_then(|ids| ids.first())
                    .and_then(Value::as_str)
                {
                    let mut path = offered_path;
                    path.push(id.into());
                    self.selected_path = path;
                }
                self.drag_ui.undo = Some(response);
                self.error = None;
            }
            Err(error) => self.error = Some(error),
        }
        self.cancel_content_drag(cx);
        self.refresh_after_write(cx);
        cx.notify();
    }

    fn refresh_after_stale_content(&mut self, cx: &mut Context<Self>) {
        if self.busy || favorite_drag::active(cx) || self.drag_ui.active() {
            self.drag_ui.deferred_refresh = true;
        } else {
            self.refresh(cx);
        }
    }

    fn begin_content_mutation(&mut self) -> MutationStamp {
        self.drag_ui.mutation_sequence = self.drag_ui.mutation_sequence.wrapping_add(1);
        let stamp = MutationStamp {
            sequence: self.drag_ui.mutation_sequence,
            generation: self.generation,
            session: self.native_session,
        };
        self.drag_ui.mutation = Some(stamp);
        stamp
    }

    fn finish_content_mutation(&mut self, stamp: MutationStamp) -> bool {
        if self.drag_ui.mutation != Some(stamp) {
            return false;
        }
        self.drag_ui.mutation = None;
        self.busy = false;
        self.active && stamp.generation == self.generation && stamp.session == self.native_session
    }

    pub(super) fn has_drag_undo(&self) -> bool {
        self.drag_ui
            .undo
            .as_ref()
            .and_then(|v| v["undoId"].as_str())
            .is_some()
    }

    pub(super) fn undo_content_drag(&mut self, cx: &mut Context<Self>) {
        if self.busy || !self.active {
            return;
        }
        let Some(undo_id) = self
            .drag_ui
            .undo
            .as_ref()
            .and_then(|v| v["undoId"].as_str())
            .map(str::to_owned)
        else {
            return;
        };
        let accepted = match crate::native_interactions::accept_content_write() {
            Ok(accepted) => accepted,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        self.busy = true;
        let stamp = self.begin_content_mutation();
        let host = self.host.clone();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        cx.background_executor()
            .spawn(async move {
                let result = host.favorite_drag_undo(&json!({"undoId":undo_id}));
                accepted.finish(&result);
                let _ = sender.send(result);
            })
            .detach();
        self.drag_ui.mutation_task = Some(cx.spawn(async move |this, cx| {
            let result = receiver.await.unwrap_or_else(|_| {
                Err(tr("撤销工作线程意外中止", "The undo worker was interrupted").into())
            });
            let _ = this.update(cx, |this, cx| {
                if !this.finish_content_mutation(stamp) {
                    this.refresh_after_stale_content(cx);
                    cx.notify();
                    return;
                }
                match result {
                    Ok(response) => {
                        this.drag_ui.undo = None;
                        if let Some(id) = response["selectedIds"]
                            .as_array()
                            .and_then(|ids| ids.first())
                            .and_then(Value::as_str)
                        {
                            this.selected = id.into();
                            this.pending_reveal = Some(id.into());
                        }
                        this.error = None;
                    }
                    Err(error) => this.error = Some(error),
                }
                this.refresh_after_write(cx);
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(super) fn drag_selected(&self, index: usize) -> bool {
        self.drag_ui.selected.contains(&self.row_path(index))
    }

    pub(super) fn drag_has_selection(&self) -> bool {
        self.drag_ui.anchor.is_some()
    }

    pub(super) fn drag_navigation_active(&self) -> bool {
        self.drag_ui.navigation.is_some()
    }

    fn navigate_during_drag(
        &mut self,
        path: OccurrencePath,
        drag: ContentDrag,
        cx: &mut Context<Self>,
    ) {
        if self.drag_ui.navigation.is_none() {
            self.remember_view_memory();
            self.drag_ui.navigation = Some(DragNavigation {
                source: drag,
                folder: self.folder.clone(),
                memory: self.memory.clone(),
                selected: self.selected.clone(),
                selected_path: self.selected_path.clone(),
                selected_batch: self.drag_ui.selected.clone(),
                selection_anchor: self.drag_ui.anchor.clone(),
                scroll_anchor: self.current_scroll_anchor(),
                loaded: self.leaves.len(),
            });
        }
        self.folder = path.last().cloned();
        self.memory.active_folder = path;
        self.replace_drag_browse_page(PAGE_SIZE);
        self.drag_ui.selected.clear();
        self.drag_ui.anchor = None;
        self.drag_ui.hover = None;
        self.drag_ui.target = None;
        self.drag_ui.candidate = None;
        self.drag_ui.offer = None;
        self.cancel_native_hover();
        self.host.retire_native_clipboard_list_input();
        let owner = self.window;
        cx.defer(move |cx| {
            let _ = owner.update(cx, |_, window, _| favorite_drag::clear_target(window));
        });
        cx.notify();
    }

    fn replace_drag_browse_page(&mut self, take: usize) {
        let page = self.tree.direct_page(self.folder.as_deref(), 0, take);
        self.leaves = page.items;
        self.visible = self.leaves.clone();
        self.tree_rows.clear();
        self.total = page.total;
        self.has_more = page.has_more;
        self.next_skip = page.next_skip;
        self.selected = self
            .visible
            .first()
            .map(|item| item.id.clone())
            .unwrap_or_default();
        self.selected_path = if self.visible.is_empty() {
            Vec::new()
        } else {
            self.row_path(0)
        };
        self.reconcile_rendered_rows();
    }

    fn settle_drag_navigation(&mut self, cx: &mut Context<Self>) {
        let Some(restore) = self
            .drag_ui
            .navigation
            .as_ref()
            .and_then(|navigation| drag_navigation_finished(&navigation.source))
        else {
            return;
        };
        let navigation = self
            .drag_ui
            .navigation
            .take()
            .expect("inactive drag navigation");
        if restore {
            self.folder = navigation.folder;
            self.memory = navigation.memory;
            self.replace_drag_browse_page(navigation.loaded.max(PAGE_SIZE));
            self.selected = navigation.selected;
            self.selected_path = navigation.selected_path;
            self.drag_ui.selected = navigation.selected_batch;
            self.drag_ui.anchor = navigation.selection_anchor;
            self.restore_scroll_anchor(navigation.scroll_anchor);
        }
        self.save_view_memory(cx);
        cx.notify();
    }

    pub(super) fn drag_hint(&self) -> Option<&'static str> {
        self.drag_ui.hint
    }

    pub(super) fn select_drag_occurrence(&mut self, index: usize, control: bool, shift: bool) {
        let path = self.row_path(index);
        if path.is_empty() {
            return;
        }
        if shift {
            if let Some(anchor) = self
                .drag_ui
                .anchor
                .as_ref()
                .filter(|a| same_parent(a, &path))
            {
                let start = (0..self.visible.len()).position(|i| self.row_path(i) == *anchor);
                if let Some(start) = start {
                    if !control {
                        self.drag_ui.selected.clear();
                    }
                    for i in start.min(index)..=start.max(index) {
                        let candidate = self.row_path(i);
                        if same_parent(&candidate, &path) {
                            self.drag_ui.selected.insert(candidate);
                        }
                    }
                }
            } else {
                self.drag_ui.selected.clear();
                self.drag_ui.selected.insert(path.clone());
                self.drag_ui.anchor = Some(path.clone());
            }
        } else if control {
            if self.drag_ui.selected.iter().any(|p| !same_parent(p, &path)) {
                self.drag_ui.selected.clear();
            }
            if !self.drag_ui.selected.remove(&path) {
                self.drag_ui.selected.insert(path.clone());
            }
            self.drag_ui.anchor = Some(path.clone());
        } else {
            // Retain a selected batch on mouse-down so the ensuing drag moves
            // the batch. Ordinary click collapses it after GPUI rules out drag.
            if !self.drag_ui.selected.contains(&path) {
                self.drag_ui.selected.clear();
                self.drag_ui.selected.insert(path.clone());
            }
            self.drag_ui.anchor = Some(path.clone());
        }
        self.select_occurrence(index);
    }

    pub(super) fn collapse_drag_selection(&mut self, index: usize) {
        let path = self.row_path(index);
        self.drag_ui.selected.clear();
        if !path.is_empty() {
            self.drag_ui.selected.insert(path.clone());
        }
        self.drag_ui.anchor = Some(path);
    }

    pub(super) fn content_drag(&self, path: OccurrencePath, sidebar: bool) -> ContentDrag {
        let paths: Vec<_> = if !sidebar && self.drag_ui.selected.contains(&path) {
            (0..self.visible.len())
                .map(|i| self.row_path(i))
                .filter(|p| same_parent(p, &path) && self.drag_ui.selected.contains(p))
                .collect()
        } else {
            vec![path.clone()]
        };
        let ids: Vec<_> = paths.iter().filter_map(|p| p.last().cloned()).collect();
        let caption = if ids.len() > 1 {
            format!("{} {}", ids.len(), tr("个收藏项", "favorites"))
        } else {
            path.last()
                .map(|id| self.tree.title_of(id))
                .unwrap_or_default()
        };
        ContentDrag::favorite(
            parent(&path),
            ids,
            paths,
            self.tree.drag_revision.clone(),
            self.generation,
            self.native_session,
            caption,
        )
    }

    pub(super) fn begin_content_drag(&mut self, drag: &ContentDrag, cx: &mut Context<Self>) {
        if !self.active
            || self.busy
            || self.loading
            || self.dialog.is_some()
            || drag.generation != self.generation
            || self.tree.drag_revision.is_empty()
        {
            drag.cancel();
            return;
        }
        self.cancel_native_hover();
        self.drag_ui.source = Some(drag.clone());
        self.drag_ui.offer = None;
        self.drag_ui.suppress_release = true;
        self.drag_ui.target = None;
        self.native_suppress_click = Some(Instant::now() + Duration::from_millis(200));
        cx.notify();
    }

    pub(super) fn drag_target_for_row(
        &self,
        path: &[String],
        folder: bool,
        offset: f32,
        height: f32,
        first_child: Option<&str>,
    ) -> Option<DropTarget> {
        row_target(
            path,
            folder,
            offset,
            height,
            self.query.is_empty() && self.category == Category::All && !self.sort_title,
            first_child,
        )
    }

    fn accepts_content_target(&self, drag: &ContentDrag, target: &DropTarget, copy: bool) -> bool {
        if !drag.active() || !self.active || self.busy || self.loading || self.dialog.is_some() {
            return false;
        }
        if target
            .parent_id
            .as_ref()
            .is_some_and(|id| !self.tree.items.get(id).is_some_and(|item| item.is_folder))
        {
            return false;
        }
        if self.root_order_unsupported(drag, target) {
            return false;
        }
        if let DragSource::Favorite { revision, .. } = &drag.source {
            if revision != &self.tree.drag_revision {
                return false;
            }
            if !copy
                && target
                    .anchor_id
                    .as_ref()
                    .is_some_and(|id| drag.item_ids.contains(id))
            {
                return false;
            }
            // Root folders have persisted membership; root ordinary entries
            // remain visible but do not currently have an editable root order.
            if let Some(target_id) = &target.parent_id {
                if drag
                    .item_ids
                    .iter()
                    .any(|id| id == target_id || self.tree.descendant_ids(id).contains(target_id))
                {
                    return false;
                }
            }
        }
        true
    }

    fn root_order_unsupported(&self, drag: &ContentDrag, target: &DropTarget) -> bool {
        target.parent_id.is_none()
            && (matches!(drag.source, DragSource::History { .. })
                || (drag
                    .item_ids
                    .iter()
                    .any(|id| self.tree.items.get(id).is_some_and(|item| !item.is_folder))
                    || target.placement != Placement::Into
                        && target.anchor_id.as_ref().is_some_and(|id| {
                            self.tree.items.get(id).is_some_and(|item| !item.is_folder)
                        })))
    }

    pub(super) fn hover_content_target(
        &mut self,
        drag: &ContentDrag,
        path: OccurrencePath,
        target: Option<DropTarget>,
        sidebar: bool,
        copy: bool,
        cx: &mut Context<Self>,
    ) {
        self.drag_ui.candidate = target.clone().map(|t| (path.clone(), t));
        let navigate = !sidebar
            && !self.memory.layout.is_tree()
            && self.query.is_empty()
            && self.category == Category::All
            && !self.sort_title
            && path != self.scope_path()
            && target
                .as_ref()
                .is_some_and(|target| target.placement == Placement::Into)
            && path
                .last()
                .is_none_or(|id| self.tree.items.get(id).is_some_and(|item| item.is_folder))
            && drag.active()
            && !self.busy
            && !self.loading
            && path.last().is_none_or(|id| {
                !matches!(drag.source, DragSource::Favorite { .. })
                    || !drag
                        .item_ids
                        .iter()
                        .any(|source| self.tree.descendant_ids(source).contains(id))
            });
        self.drag_ui.hint = target
            .as_ref()
            .filter(|target| self.root_order_unsupported(drag, target))
            .map(|_| {
                tr(
                    "顶层只接收收藏夹；普通条目请放入具体收藏夹",
                    "The root accepts folders only; drop items into a folder",
                )
            });
        let target = target.filter(|t| self.accepts_content_target(drag, t, copy));
        let next = target.clone().map(|t| (path.clone(), t));
        if self.drag_ui.target != next {
            self.drag_ui.target = next;
            cx.notify();
        }
        let expand = navigate
            || target
                .as_ref()
                .is_some_and(|t| t.placement == Placement::Into)
                && !path.is_empty()
                && (sidebar || self.memory.layout.is_tree());
        if !expand {
            self.drag_ui.hover = None;
            self.drag_ui.hover_task = None;
        } else if self.drag_ui.hover.as_ref() != Some(&path) {
            self.drag_ui.hover = Some(path.clone());
            let source = drag.clone();
            self.drag_ui.hover_task = Some(cx.spawn(async move |this, cx| {
                Timer::after(Duration::from_millis(if navigate { 650 } else { 450 })).await;
                let _ = this.update(cx, |this, cx| {
                    if !source.active() || this.drag_ui.hover.as_ref() != Some(&path) {
                        return;
                    }
                    if navigate {
                        this.navigate_during_drag(path.clone(), source.clone(), cx);
                    } else if sidebar {
                        this.expanded.insert(path.clone());
                    } else {
                        let anchor = this.current_scroll_anchor();
                        let scope = this.scope_path();
                        this.memory
                            .context_mut(this.memory.layout, &scope)
                            .expanded
                            .insert(path.clone());
                        // Projection updates preserve the drag's original data
                        // revision and relationship source; no write occurs.
                        this.rebuild_tree_visible();
                        this.restore_scroll_anchor(anchor);
                    }
                    this.save_view_memory(cx);
                    cx.notify();
                });
            }));
        }
    }

    /// Into marks the hit folder; Before/After mark the anchor occurrence,
    /// which is the first child when an expanded folder's lower edge is hit.
    pub(super) fn drop_decoration(&self, path: &[String]) -> Option<Placement> {
        self.drag_ui
            .target
            .as_ref()
            .filter(|(hit, target)| favorite_drag::indicator_path(hit, target) == path)
            .map(|(_, t)| t.placement)
    }

    pub(super) fn apply_content_drop(
        &mut self,
        drag: &ContentDrag,
        path: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((_, target)) = self.drag_ui.candidate.clone().filter(|(p, _)| p == path) else {
            return;
        };
        if !self.accepts_content_target(drag, &target, window.modifiers().control) || !drag.claim()
        {
            return;
        }
        let accepted = match crate::native_interactions::accept_content_write() {
            Ok(accepted) => accepted,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        let copy = window.modifiers().control || matches!(drag.source, DragSource::History { .. });
        let request = drag.request(&target, window.modifiers().control);
        self.drag_ui.placement_only = if copy {
            HashSet::new()
        } else {
            drag.item_ids.iter().cloned().collect()
        };
        self.drag_ui.source = None;
        self.drag_ui.suppress_release = true;
        self.drag_ui.target = None;
        self.drag_ui.candidate = None;
        self.drag_ui.hint = None;
        self.drag_ui.hover = None;
        self.drag_ui.hover_task = None;
        self.native_suppress_click = Some(Instant::now() + Duration::from_millis(250));
        self.cancel_native_hover();
        self.busy = true;
        let stamp = self.begin_content_mutation();
        let host = self.host.clone();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        cx.background_executor()
            .spawn(async move {
                let result = host.favorite_drag_apply(&request);
                accepted.finish(&result);
                let _ = sender.send(result);
            })
            .detach();
        self.drag_ui.mutation_task = Some(cx.spawn(async move |this, cx| {
            let result = receiver.await.unwrap_or_else(|_| {
                Err(tr("拖拽工作线程意外中止", "The drag worker was interrupted").into())
            });
            let _ = this.update(cx, |this, cx| {
                if !this.finish_content_mutation(stamp) {
                    this.refresh_after_stale_content(cx);
                    cx.notify();
                    return;
                }
                match result {
                    Ok(response) => {
                        let ids: Vec<String> = response["selectedIds"]
                            .as_array()
                            .or_else(|| response["createdIds"].as_array())
                            .or_else(|| response["itemIds"].as_array())
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect();
                        this.drag_ui.selected.clear();
                        for id in &ids {
                            let mut path = target
                                .parent_id
                                .as_ref()
                                .map(|p| this.canonical_path(p))
                                .unwrap_or_default();
                            path.push(id.clone());
                            this.drag_ui.selected.insert(path);
                        }
                        if let Some(id) = ids.first() {
                            this.selected = id.clone();
                            this.pending_reveal = Some(id.clone());
                        }
                        this.drag_ui.undo = Some(response);
                        this.error = None;
                    }
                    Err(error) => {
                        this.drag_ui.placement_only.clear();
                        this.error = Some(error);
                    }
                }
                // Reload even on rejection: another window may have changed
                // the graph, so an optimistic rollback would be stale.
                this.refresh_after_write(cx);
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(super) fn content_drag_pointer(
        &mut self,
        event: &gpui::MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = self.drag_ui.source.clone() else {
            return;
        };
        if !drag.active() || event.pressed_button != Some(MouseButton::Left) {
            self.cancel_content_drag(cx);
            return;
        }
        let size = window.viewport_size();
        let outside = event.position.x < px(0.)
            || event.position.y < px(0.)
            || event.position.x >= size.width
            || event.position.y >= size.height;
        if outside {
            self.drag_ui.target = None;
            #[cfg(windows)]
            {
                if !self.native_dragging {
                    self.cancel_native_hover();
                    let result = self.interactions.as_ref().and_then(|shared| {
                        shared
                            .borrow_mut()
                            .as_mut()
                            .map(|native| native.start_content_drag(drag.clone()))
                    });
                    if let Some(result) = result {
                        self.native_dragging = result.is_ok();
                        if let Err(error) = result {
                            self.error = Some(error);
                        }
                    }
                }
            }
            cx.notify();
        }
    }

    pub(super) fn drag_auto_scroll(
        &mut self,
        drag: &ContentDrag,
        event: &gpui::MouseMoveEvent,
        bounds: gpui::Bounds<gpui::Pixels>,
        sidebar: bool,
        cx: &mut Context<Self>,
    ) {
        let y = f32::from(event.position.y);
        let top = f32::from(bounds.top());
        let bottom = f32::from(bounds.bottom());
        if y >= top + 24.0 && y <= bottom - 24.0 {
            return;
        }
        self.drag_ui.scroll_edge = Some(ScrollEdge {
            sidebar,
            bounds,
            event: event.clone(),
            drag: drag.clone(),
        });
        self.scroll_drag_edge(y, bounds, sidebar, cx);
        if self.drag_ui.scroll_task.is_none() {
            self.drag_ui.scroll_task = Some(cx.spawn(async move |this, cx| {
                loop {
                    Timer::after(Duration::from_millis(45)).await;
                    let next = this
                        .update(cx, |this, cx| {
                            let edge = this.drag_ui.scroll_edge.clone()?;
                            if !edge.drag.active() || !this.active {
                                this.drag_ui.scroll_edge = None;
                                return None;
                            }
                            this.scroll_drag_edge(
                                f32::from(edge.event.position.y),
                                edge.bounds,
                                edge.sidebar,
                                cx,
                            );
                            Some((this.window, edge.event))
                        })
                        .ok()
                        .flatten();
                    let Some((owner, event)) = next else {
                        break;
                    };
                    // Re-hit-test after the rows move beneath a stationary
                    // pointer; otherwise release could use a stale anchor ID.
                    let _ = owner.update(cx, |_, window, cx| {
                        window.dispatch_input(gpui::PlatformInput::MouseMove(event), cx);
                    });
                }
                let _ = this.update(cx, |this, _| this.drag_ui.scroll_task = None);
            }));
        }
    }

    fn scroll_drag_edge(
        &mut self,
        y: f32,
        bounds: gpui::Bounds<gpui::Pixels>,
        sidebar: bool,
        cx: &mut Context<Self>,
    ) {
        if self
            .drag_ui
            .last_scroll
            .is_some_and(|at| at.elapsed() < Duration::from_millis(35))
        {
            return;
        }
        let top = f32::from(bounds.top());
        let bottom = f32::from(bounds.bottom());
        let Some(delta) = drag_scroll_delta(y, top, bottom, sidebar) else {
            return;
        };
        self.drag_ui.last_scroll = Some(Instant::now());
        if sidebar {
            let offset = self.sidebar_scroll.offset();
            self.sidebar_scroll
                .set_offset(gpui::point(offset.x, offset.y + px(delta)));
        } else {
            self.list.scroll_by(px(delta));
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn path(ids: &[&str]) -> OccurrencePath {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn temporary_drag_navigation_restores_on_cancel_and_keeps_accepted_destination() {
        let source = ContentDrag::favorite(
            Some("a".into()),
            vec!["item".into()],
            vec![path(&["a", "item"])],
            "revision".into(),
            1,
            2,
            "Item".into(),
        );
        assert_eq!(drag_navigation_finished(&source), None);
        source.cancel();
        assert_eq!(drag_navigation_finished(&source), Some(true));
        let accepted = ContentDrag::favorite(
            Some("a".into()),
            vec!["item".into()],
            vec![path(&["a", "item"])],
            "revision".into(),
            1,
            2,
            "Item".into(),
        );
        assert!(accepted.claim());
        assert_eq!(drag_navigation_finished(&accepted), Some(false));
    }

    #[test]
    fn favorites_drag_edge_scroll_respects_list_and_translation_directions() {
        assert_eq!(drag_scroll_delta(101.0, 100.0, 400.0, false), Some(-16.0));
        assert_eq!(drag_scroll_delta(399.0, 100.0, 400.0, false), Some(16.0));
        assert_eq!(drag_scroll_delta(101.0, 100.0, 400.0, true), Some(16.0));
        assert_eq!(drag_scroll_delta(399.0, 100.0, 400.0, true), Some(-16.0));
        assert_eq!(drag_scroll_delta(250.0, 100.0, 400.0, false), None);
        assert_eq!(drag_scroll_delta(f32::NAN, 100.0, 400.0, false), None);
    }

    #[test]
    fn favorites_native_content_offer_rejects_old_operations_and_presentations() {
        let offer = ContentOffer {
            operation_id: "old-gesture".into(),
            generation: 7,
            session: 9,
            destination: path(&["target"]),
        };
        assert!(offer.is_current("old-gesture", 7, 9));
        assert!(!offer.is_current("new-gesture", 7, 9));
        assert!(!offer.is_current("old-gesture", 8, 9));
        assert!(!offer.is_current("old-gesture", 7, 10));
        let first = MutationStamp {
            sequence: 1,
            generation: 7,
            session: 9,
        };
        assert!(
            first
                != MutationStamp {
                    sequence: 2,
                    ..first
                }
        );
        assert!(
            first
                != MutationStamp {
                    generation: 8,
                    ..first
                }
        );
    }

    #[test]
    fn content_drag_targets_keep_exact_parent_and_folder_middle() {
        let p = path(&["a", "shared", "child"]);
        let before = row_target(&p, true, 3.0, 30.0, true, None).unwrap();
        assert_eq!(before.parent_id.as_deref(), Some("shared"));
        assert_eq!(before.anchor_id.as_deref(), Some("child"));
        assert_eq!(before.placement, Placement::Before);
        let into = row_target(&p, true, 15.0, 30.0, true, None).unwrap();
        assert_eq!(into.parent_id.as_deref(), Some("child"));
        assert_eq!(into.placement, Placement::Into);
        assert_eq!(
            row_target(&p, true, 27.0, 30.0, true, None).unwrap().placement,
            Placement::After
        );
        assert_eq!(
            row_target(&path(&["root"]), false, 3.0, 30.0, true, None)
                .unwrap()
                .parent_id,
            None
        );
    }

    #[test]
    fn favorites_expanded_folder_lower_edge_targets_first_child_at_child_level() {
        let folder = path(&["a", "work"]);
        let edge = row_target(&folder, true, 27.0, 30.0, true, Some("report")).unwrap();
        assert_eq!(edge.parent_id.as_deref(), Some("work"));
        assert_eq!(edge.anchor_id.as_deref(), Some("report"));
        assert_eq!(edge.placement, Placement::Before);
        // Upper edge and centre keep their sibling / Into meaning.
        let before = row_target(&folder, true, 3.0, 30.0, true, Some("report")).unwrap();
        assert_eq!(before.parent_id.as_deref(), Some("a"));
        assert_eq!(before.placement, Placement::Before);
        let into = row_target(&folder, true, 15.0, 30.0, true, Some("report")).unwrap();
        assert_eq!(into.placement, Placement::Into);
        // Collapsed or empty folders still insert after themselves.
        let after = row_target(&folder, true, 27.0, 30.0, true, None).unwrap();
        assert_eq!(after.parent_id.as_deref(), Some("a"));
        assert_eq!(after.placement, Placement::After);
    }

    #[test]
    fn filtered_or_named_sorted_rows_offer_only_folder_insertion() {
        let p = path(&["a", "b"]);
        assert!(row_target(&p, false, 3.0, 30.0, false, None).is_none());
        assert_eq!(
            row_target(&p, true, 3.0, 30.0, false, None).unwrap().placement,
            Placement::Into
        );
        assert!(!same_parent(
            &path(&["a", "shared"]),
            &path(&["b", "shared"])
        ));
        assert!(same_parent(&path(&["a", "x"]), &path(&["a", "y"])));
    }
}
