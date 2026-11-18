//! The clipboard window owns the secondary list and its shared text editor.
//! All records, ordering, copy/paste tickets and writes still belong to HostState.
use super::*;
use crate::favorites::phrase_snapshot::PhraseSnapshot;
use crate::multiline::{MultilineEvent, MultilineInput};
use gpui::AnyElement;
use gpui_rsx::rsx;

#[path = "phrase_editor.rs"]
mod editor;

#[path = "phrase_drop.rs"]
mod drop_zone;

pub(super) struct PhraseState {
    pub(super) tree: Arc<PhraseSnapshot>,
    content_cache: ContentResourceCache,
    content_generation: u64,
    pub(super) hover_tickets: hover_binding::HoverBindings,
    image_height: u32,
    appearance: Option<AppearanceSettings>,
    ready: bool,
    dirty: bool,
    loading: bool,
    error: Option<String>,
    source: String,
    group: String,
    query: String,
    hits: std::collections::HashSet<String>,
    search_task: Option<Task<()>>,
    search_error: Option<String>,
    #[cfg(debug_assertions)]
    fixture_host: Option<Arc<HostState>>,
    tabs: Vec<(String, String)>,
    pub(super) rows: Arc<Vec<String>>,
    pub(super) selected: String,
    reveal: Option<String>,
    pub(super) list: ListState,
    pub(super) scrollbar: ScrollbarState,
    picking_source: bool,
    pub(super) draft: Option<PhraseDraft>,
}

impl PhraseState {
    pub(super) fn reset_picker(&mut self) {
        self.picking_source = false;
    }
}

impl Default for PhraseState {
    fn default() -> Self {
        Self {
            tree: Arc::default(),
            content_cache: ContentResourceCache::default(),
            content_generation: 0,
            hover_tickets: hover_binding::HoverBindings::default(),
            image_height: 0,
            appearance: None,
            dirty: true,
            ready: false,
            loading: false,
            error: None,
            source: String::new(),
            group: String::new(),
            query: String::new(),
            hits: Default::default(),
            search_task: None,
            search_error: None,
            #[cfg(debug_assertions)]
            fixture_host: None,
            tabs: Vec::new(),
            rows: Arc::default(),
            selected: String::new(),
            reveal: None,
            list: ListState::new(0, ListAlignment::Top, px(100.)),
            scrollbar: ScrollbarState::default(),
            picking_source: false,
            draft: None,
        }
    }
}

pub(super) struct PhraseDraft {
    title: Entity<SearchInput>,
    body: Entity<MultilineInput>,
    target: String,
    scope: Option<String>,
    picking: bool,
    saving: bool,
    exit_after_save: bool,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl ClipboardPreview {
    pub(super) fn toggle_phrase_column(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let enable = !self.phrase_enabled();
        self.cancel_native_hover();
        if !enable && self.sidebar_has_focus(window, cx) {
            self.focus.focus(window);
        }
        self.phrases.picking_source = false;
        if enable && window.viewport_size().width < px(480.) {
            window.resize(gpui::size(px(560.), window.viewport_size().height));
        }
        self.save_appearance(json!({"clipListPhraseColumn":enable}), cx);
    }

    pub(super) fn phrase_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let tokens = self.clipboard_tokens();
        let t = tokens.theme(self.dark);
        let enabled = self.phrase_enabled();
        let switch = div()
            .w(px(30.))
            .h(px(18.))
            .p(px(2.))
            .flex()
            .items_center()
            .rounded_full()
            .bg(color(if enabled {
                &t.accent
            } else {
                &t.border_secondary
            }))
            .when(enabled, |e| e.justify_end())
            .child(div().size(px(14.)).rounded_full().bg(gpui::white()));
        div()
            .flex_none()
            .flex()
            .flex_col()
            .gap_1()
            .pb_2()
            .child(
                div()
                    .id("phrase-enable")
                    .h(px(29.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .cursor_pointer()
                    .child(tr("双列显示收藏", "Favorites sidebar"))
                    .child(switch)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.toggle_phrase_column(window, cx);
                    })),
            )
            .when(enabled, |row| {
                row.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(tr("分类来源收藏夹", "Source folder"))
                        .child(self.phrase_folder_button(false, t, cx)),
                )
                .when(self.phrases.picking_source, |e| {
                    e.child(self.phrase_folder_choices(false, t, cx))
                })
            })
            .into_any_element()
    }

    fn phrase_folder_button(
        &self,
        editing: bool,
        t: &ThemeTokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = self
            .phrases
            .draft
            .as_ref()
            .filter(|_| editing)
            .map(|d| d.target.as_str())
            .unwrap_or(&self.phrases.source);
        let label = self.phrases.tree.path_label(id).to_owned();
        let hover = color(&t.hover);
        div()
            .id(if editing {
                "phrase-destination"
            } else {
                "phrase-source"
            })
            .min_w_0()
            .flex_1()
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .h(px(30.))
            .rounded_md()
            .border_1()
            .border_color(color(&t.border_secondary))
            .cursor_pointer()
            .hover(move |e| e.bg(hover))
            .child(self.icon("folder-closed", 15., color(&t.text_muted)))
            .child(div().flex_1().min_w_0().line_clamp(1).child(label))
            .child(self.icon("collapse", 12., color(&t.text_muted)))
            .on_click(cx.listener(move |this, _, _, cx| {
                if editing {
                    if let Some(draft) = this.phrases.draft.as_mut().filter(|d| !d.saving) {
                        draft.picking = !draft.picking;
                    }
                } else {
                    this.phrases.picking_source = !this.phrases.picking_source;
                }
                cx.notify();
            }))
            .into_any_element()
    }

    fn phrase_folder_choices(
        &self,
        editing: bool,
        t: &ThemeTokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let draft = self.phrases.draft.as_ref().filter(|_| editing);
        let allowed = draft
            .and_then(|d| d.scope.as_ref())
            .map(|id| self.phrases.tree.contents(id).0);
        let selected = draft.map(|d| &d.target).unwrap_or(&self.phrases.source);
        let root = (String::new(), tr("全部收藏", "All favorites").to_owned());
        let choices: Vec<_> = std::iter::once(root)
            .chain(self.phrases.tree.folders.iter().cloned())
            .filter(|(id, _)| allowed.as_ref().is_none_or(|allowed| allowed.contains(id)))
            .collect();
        let height = (choices.len() as f32 * 26.).min(114.);
        let selected = selected.clone();
        let neutral = color(&t.neutral_active);
        let weak = cx.weak_entity();
        gpui::uniform_list(
            "phrase-folder-choices",
            choices.len(),
            move |range, _, _| {
                range
                    .map(|index| {
                        let (id, label) = &choices[index];
                        let owner = weak.clone();
                        let id = id.clone();
                        div()
                            .id(("phrase-folder-choice", index))
                            .w_full()
                            .h(px(26.))
                            .px_2()
                            .flex()
                            .items_center()
                            .rounded_sm()
                            .cursor_pointer()
                            .when(id == selected, |e| e.bg(neutral))
                            .child(div().line_clamp(1).child(label.clone()))
                            .on_click(move |_, _, cx| {
                                let _ = owner.update(cx, |this, cx| {
                                    if editing {
                                        if let Some(draft) =
                                            this.phrases.draft.as_mut().filter(|d| !d.saving)
                                        {
                                            draft.target = id.clone();
                                            draft.picking = false;
                                        }
                                    } else {
                                        this.phrases.picking_source = false;
                                        this.save_appearance(
                                            json!({"clipListPhraseFolderId":id}),
                                            cx,
                                        );
                                    }
                                    cx.notify();
                                });
                            })
                    })
                    .collect()
            },
        )
        .h(px(height))
        .w_full()
        .flex_none()
        .bg(color(&t.secondary))
        .rounded_md()
        .into_any_element()
    }

    fn open_phrase_folder(
        &mut self,
        id: &str,
        item: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        query
            .append_pair("folderId", id)
            .append_pair("query", "")
            .append_pair("type", "all");
        if let Some(item) = item {
            query.append_pair("itemId", item);
        }
        self.set_clipboard_route(&format!("/favorites?{}", query.finish()), window, cx);
    }

    #[cfg(debug_assertions)]
    pub fn phrase_scroll_visual(&self) -> Value {
        let offset = |list: &ListState| {
            let top = list.logical_scroll_top();
            json!([top.item_ix, f32::from(top.offset_in_item)])
        };
        let bounds = self.phrases.list.viewport_bounds();
        json!({"history":offset(&self.list), "phrases":offset(&self.phrases.list), "count":self.phrases.rows.len(),
            "x":f32::from(bounds.left()), "y":f32::from(bounds.top()),
            "width":f32::from(bounds.size.width), "height":f32::from(bounds.size.height)})
    }

    #[cfg(debug_assertions)]
    pub fn seed_phrase_visual(
        &mut self,
        host: Arc<HostState>,
        folder: String,
        editor: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        assert!(matches!(self.model, UiModel::Fixture(_)) && cliphost_core::is_headless());
        self.configuration["clipListPhraseColumn"] = json!(true);
        self.configuration["clipListPhraseFolderId"] = json!(folder);
        self.phrases.tree = PhraseSnapshot::parse(host.favor_tree().expect("disposable favorites"))
            .expect("valid disposable favorites tree").into();
        self.phrases.fixture_host = Some(host);
        self.phrases.ready = true;
        self.phrases.dirty = false;
        self.sync_phrases(cx);
        if editor {
            self.open_phrase_draft(folder, true, window, cx);
        }
        cx.notify();
    }

    pub(super) fn phrase_enabled(&self) -> bool {
        self.configuration["clipListPhraseColumn"]
            .as_bool()
            .unwrap_or(false)
    }

    pub(super) fn history_width(&self, window: &Window) -> f32 {
        let width = f32::from(window.viewport_size().width);
        if self.phrase_enabled() && !self.favorites_active {
            width / 2.
        } else {
            width
        }
    }

    pub(super) fn invalidate_phrases(&mut self, cx: &mut Context<Self>) {
        self.phrases.dirty = true;
        if self.phrase_enabled() || self.phrases.draft.is_some() {
            cx.notify();
        }
    }

    pub(super) fn sync_phrases(&mut self, cx: &mut Context<Self>) {
        // Resolve the initial name only once. Renaming the chosen folder must
        // not silently change the right column on the next favorites refresh.
        if self.phrase_enabled()
            && self.configuration["clipListPhraseFolderId"].is_null()
            && self.phrases.ready
            && !self.phrases.loading
            && !self.phrases.dirty
            && self.model.host().is_some()
        {
            self.save_appearance(
                json!({"clipListPhraseFolderId": self.phrases.tree.default_source()}),
                cx,
            );
        }
        let source = self.configuration["clipListPhraseFolderId"]
            .as_str()
            .unwrap_or_else(|| self.phrases.tree.default_source());
        if source != self.phrases.source {
            self.phrases.source = source.to_owned();
            self.phrases.group.clear();
            self.rebuild_phrases();
        }
        self.sync_phrase_search(false, cx);
        if !self.phrases.dirty || self.phrases.loading {
            return;
        }
        let Some(host) = self.model.host() else {
            return;
        };
        self.phrases.dirty = false;
        self.phrases.loading = true;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { PhraseSnapshot::parse(host.favor_tree()?) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.phrases.loading = false;
                match result {
                    Ok(tree) => {
                        this.phrases.content_generation += 1;
                        this.phrases
                            .content_cache
                            .advance(this.phrases.content_generation);
                        this.phrases.content_cache.release_evicted(cx);
                        this.phrases.tree = Arc::new(tree);
                        this.phrases.ready = true;
                        this.phrases.error = None;
                        this.rebuild_phrases();
                        this.sync_phrase_search(true, cx);
                    }
                    Err(error) => this.phrases.error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn sync_phrase_search(&mut self, tree_changed: bool, cx: &mut Context<Self>) {
        let query = self.query.trim();
        if !self.phrase_enabled()
            || !self.phrases.ready
            || (query == self.phrases.query && (!tree_changed || query.is_empty()))
        {
            return;
        }
        self.phrases.search_task = None;
        self.phrases.query = query.to_owned();
        self.phrases.hits.clear();
        self.phrases.search_error = None;
        let query = self.phrases.query.clone();
        self.rebuild_phrases();
        if query.is_empty() {
            return;
        }
        let host = self.model.host();
        #[cfg(debug_assertions)]
        let host = host.or_else(|| self.phrases.fixture_host.clone());
        let Some(host) = host else { return };
        self.phrases.search_task = Some(cx.spawn(async move |this, cx| {
            Timer::after(CLIPBOARD_SEARCH_DELAY).await;
            let needle = query.clone();
            let result = cx
                .background_executor()
                .spawn(async move { host.matching_favorite_ids(&needle) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.phrases.query != query {
                    return;
                }
                this.phrases.search_task = None;
                match result {
                    Ok(ids) => this.phrases.hits = ids,
                    Err(error) => this.phrases.search_error = Some(error),
                }
                this.rebuild_phrases();
                cx.notify();
            });
        }));
    }

    fn rebuild_phrases(&mut self) {
        self.cancel_native_hover();
        let pane = &mut self.phrases;
        pane.tabs = pane.tree.tabs(&pane.source);
        if !pane.tabs.iter().any(|(id, _)| id == &pane.group) {
            pane.group.clear();
        }
        let mut rows = if pane.group.is_empty() {
            pane.tree.all.clone()
        } else {
            pane.tree.contents(&pane.group).1
        };
        if !pane.query.is_empty() {
            rows.retain(|id| pane.hits.contains(id));
        }
        pane.rows = Arc::new(rows);
        pane.scrollbar.cancel();
        pane.list.reset(pane.rows.len());
        if let Some(index) = pane
            .reveal
            .as_ref()
            .and_then(|id| pane.rows.iter().position(|row| row == id))
        {
            pane.selected = pane.reveal.take().unwrap();
            pane.list.scroll_to_reveal_item(index);
        } else if !pane.rows.contains(&pane.selected) {
            pane.selected = pane.rows.first().cloned().unwrap_or_default();
        }
    }

    fn choose_phrase_group(&mut self, id: String, cx: &mut Context<Self>) {
        self.phrases.group = id;
        self.rebuild_phrases();
        if !self.phrases.rows.is_empty() {
            self.phrases.list.scroll_to_reveal_item(0);
        }
        cx.notify();
    }

    fn prepare_favorite_resources(
        &mut self,
        item: &cliphost_core::dto::FavorItemDto,
        projection: &ContentProjection,
        foreground: &str,
        cx: &mut Context<Self>,
    ) {
        let host = self.model.host();
        #[cfg(debug_assertions)]
        let host = host.or_else(|| self.phrases.fixture_host.clone());
        let Some(host) = host else { return; };
        let generation = self.phrases.content_generation;
        let Some(ticket) = self.phrases.content_cache.reserve(
            generation, &item.id, projection, ContentMode::Standard, foreground,
        ) else { return; };
        let item = item.clone();
        let foreground = foreground.to_owned();
        let image_height = self.phrases.image_height;
        cx.spawn(async move |this, cx| {
            let id = item.id.clone();
            let resources = cx.background_executor().spawn(async move {
                crate::favorites::load_favorite_resources(&host, &item, &foreground, image_height)
            }).await;
            let _ = this.update(cx, |this, cx| {
                let current = this.phrases.content_generation == generation;
                if this.phrases.content_cache.finish(ticket, resources, current) {
                    if let Some(index) = this.phrases.rows.iter().position(|row| row == &id) {
                        let anchor = this.phrases.list.logical_scroll_top();
                        this.phrases.list.splice(index..index + 1, 1);
                        this.phrases.list.scroll_to(anchor);
                        this.phrases.scrollbar.invalidate_measurements();
                    }
                }
                this.phrases.content_cache.release_evicted(cx);
                cx.notify();
            });
        }).detach();
    }

    pub(super) fn sync_sidebar_actions(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.favorites_active {
            return None;
        }
        let settings = AppearanceSettings::from_config(&self.configuration);
        if self.phrases.appearance.as_ref() != Some(&settings) {
            let height = settings.image_max_height_px as u32;
            if self.phrases.image_height != height {
                self.phrases.image_height = height;
                self.phrases.content_generation += 1;
                self.phrases.content_cache.advance(self.phrases.content_generation);
                self.phrases.content_cache.release_evicted(cx);
            }
            crate::history_scroll::remeasure(&self.phrases.list);
            self.phrases.scrollbar.invalidate_measurements();
            self.phrases.appearance = Some(settings);
        }
        let host = self.model.host();
        #[cfg(debug_assertions)]
        let host = host.or_else(|| self.phrases.fixture_host.clone());
        let host = host.filter(|host| {
            let state = host.hide_snapshot();
            let visible = state.visible && !state.is_collapsed;
            #[cfg(debug_assertions)]
            let visible = visible || self.phrases.fixture_host.is_some();
            self.phrase_enabled() && !self.exit_pending && visible
        });
        if let Some(host) = host {
            self.ensure_favorites_view(host, window, cx);
            return self.favorites.as_ref()?.update(cx, |view, cx| {
                view.set_sidebar(self.phrases.tree.clone(), self.phrases.rows.clone(), cx);
                view.sidebar_dialog(cx)
            });
        }
        if let Some(view) = &self.favorites {
            view.update(cx, |view, cx| view.leave_sidebar(cx));
        }
        self.phrases.hover_tickets.clear();
        self.sidebar_favorite_dialog = false;
        None
    }

    pub(super) fn sidebar_has_focus(&self, window: &Window, cx: &gpui::App) -> bool {
        self.phrase_enabled()
            && !self.favorites_active
            && self
                .favorites
                .as_ref()
                .is_some_and(|view| view.read(cx).sidebar_focus().is_focused(window))
    }

    pub(super) fn sidebar_list_key(
        &mut self,
        key: &str,
        task: Option<ChromeTask>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.list_overlay_open() {
            return;
        }
        let pane = &mut self.phrases;
        let Some(index) = pane.rows.iter().position(|id| id == &pane.selected) else {
            return;
        };
        let last = pane.rows.len() - 1;
        let next = match key {
            "Up" => index.saturating_sub(1),
            "Down" => (index + 1).min(last),
            "Home" => 0,
            "End" => last,
            _ => {
                if let Some(view) = &self.favorites {
                    view.update(cx, |view, cx| {
                        view.sidebar_key(key, pane.selected.clone(), task, window, cx)
                    });
                }
                return;
            }
        };
        pane.selected = pane.rows[next].clone();
        pane.list.scroll_to_reveal_item(next);
        cx.notify();
    }

    fn phrase_row(&mut self, index: usize, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(id) = self.phrases.rows.get(index).cloned() else {
            return div().into_any_element();
        };
        let Some(item) = self.phrases.tree.item(&id).cloned() else {
            return div().into_any_element();
        };
        let tokens = self.clipboard_tokens();
        let t = tokens.theme(self.dark);
        let selected = id == self.phrases.selected;
        let settings = AppearanceSettings::from_config(&self.configuration);
        let metrics = settings.row_metrics();
        let group = SharedString::from(format!("sidebar-favorite-{id}"));
        let width = self.history_width(window);
        let content_width = (width - 2.0 * (metrics.padding_x + metrics.border_width)).max(1.0);
        let projection = crate::favorites::favorite_resource_projection(&item);
        if let Some(projection) = &projection {
            self.prepare_favorite_resources(&item, projection, &t.text, cx);
        }
        let loaded = projection.as_ref().and_then(|projection| self.phrases.content_cache.get(
            self.phrases.content_generation, &id, projection, ContentMode::Standard, &t.text,
        ));
        let Some(view) = self.favorites.as_ref() else { return div().into_any_element(); };
        let session = self.native_session;
        let generation = self.phrases.content_generation;
        #[cfg(windows)]
        let binding = (self.model.host().is_some() && !cliphost_core::is_headless() && !item.is_folder)
            .then(|| {
                self.phrases.hover_tickets.row(
                    &id,
                    index,
                    generation,
                    session,
                    crate::favorites::favorite_hover_plan(&item),
                    self.configuration["clipListImageHoverPreviewEnabled"].as_bool() != Some(false),
                )
            });
        #[cfg(not(windows))]
        let binding = None;
        let (content, toolbar) = view.update(cx, |view, cx| (
            view.sidebar_content(&item, loaded, content_width, window, binding.clone(), cx),
            view.sidebar_toolbar(&id, group.clone(), width, cx),
        ));
        let menu_id = id.clone();
        let hover_id = id.clone();
        let owner = cx.weak_entity();
        let last = index + 1 == self.phrases.rows.len()
            || self.phrases.rows.get(index + 1) == Some(&self.phrases.selected);
        let row = appearance::row_chrome_with_wallpaper(
            SharedString::from(format!("phrase-row-{id}")), group, &settings, &tokens,
            self.dark, selected, false, false, last, self.snapshot, self.window_background.snapshot().is_some(),
        )
        .cursor_pointer()
        .text_size(px(tokens.typography.font_size))
        .line_height(relative(metrics.text_line_height))
        .child(content)
        .children(toolbar.map(|buttons| div().absolute().top(px(3.)).right(px(metrics.padding_x)).child(buttons)))
        .on_mouse_down(MouseButton::Right, cx.listener(move |this, _, window, cx| {
            cx.stop_propagation();
            if this.search.read(cx).is_composing() || this.list_overlay_open() { return; }
            this.cancel_native_hover();
            this.phrases.selected = menu_id.clone();
            if let Some(view) = &this.favorites {
                view.update(cx, |view, cx| {
                    view.focus_list(window);
                    view.sidebar_menu(menu_id.clone(), cx);
                });
            }
            cx.notify();
        }))
        .on_click(cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
            this.phrases.selected = id.clone();
            if let Some(view) = &this.favorites {
                view.update(cx, |view, cx| {
                    view.focus_list(window);
                    view.click_item(id.clone(), event.click_count(), cx);
                });
            }
            cx.stop_propagation();
            cx.notify();
        }))
        .into_any_element();
        hover_binding::wrap_hover_row(
            row,
            binding,
            Rc::new(move |event, window, cx| {
                let _ = owner.update(cx, |this, cx| {
                    #[cfg(windows)]
                    if !this.favorites_active
                        && this.phrase_enabled()
                        && this.native_session == session
                        && this.phrases.content_generation == generation
                        && this.phrases.rows.get(index) == Some(&hover_id)
                    {
                        this.show_list_hover(event, window, cx);
                        return;
                    }
                    event.reject();
                });
            }),
        )
    }

    pub(super) fn phrase_column(
        &mut self,
        scrollbar: AnyElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tokens = self.clipboard_tokens();
        let t = tokens.theme(self.dark);
        let l = &tokens.layout;
        let ink = color(&t.text_muted);
        let pane = &mut self.phrases;
        let first = pane.list.logical_scroll_top().item_ix.min(pane.rows.len());
        let last = scrollbar::visible_end(&pane.list)
            .max(first.saturating_add(12))
            .saturating_add(2)
            .min(pane.rows.len());
        pane.content_cache
            .set_visible_items(pane.rows[first..last].iter().map(String::as_str));
        pane.content_cache.release_evicted(cx);
        let pane = &self.phrases;
        let valid = pane.ready && pane.tree.folder_exists(&pane.group);
        let add_target = if pane.group.is_empty() {
            &pane.source
        } else {
            &pane.group
        };
        let can_add = pane.ready && pane.tree.folder_exists(add_target);
        let star = self
            .tool_button("phrase-favorites", "star", color(&t.pin), false, t)
            .when(pane.group.is_empty(), |e| e.bg(color(&t.neutral_active)))
            .tooltip(preference_tooltip(
                tr("全部收藏", "All favorites").into(),
                t.clone(),
            ))
            .on_click(cx.listener(|this, _, _, cx| this.choose_phrase_group(String::new(), cx)));
        let mut tabs = div()
            .id("phrase-tabs")
            .flex_1()
            .min_w_0()
            .flex()
            .items_center()
            .gap(px(l.chip_gap))
            .overflow_x_scroll();
        for (index, (id, label)) in pane.tabs.iter().cloned().enumerate() {
            tabs = tabs.child(
                div()
                    .id(("phrase-tab", index))
                    .flex_none()
                    .px(px(l.chip_padding_x))
                    .h(px(l.chip_height))
                    .flex()
                    .items_center()
                    .rounded(px(l.chip_radius))
                    .text_size(px(12.))
                    .cursor_pointer()
                    .when(id == pane.group, |e| e.bg(color(&t.neutral_active)))
                    .child(label)
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.choose_phrase_group(id.clone(), cx)),
                    ),
            );
        }
        let weak = cx.weak_entity();
        let rows = list(pane.list.clone(), move |index, window, cx| {
            weak.update(cx, |this, cx| this.phrase_row(index, window, cx))
                .unwrap_or_else(|_| div().into_any_element())
        })
        .flex_1()
        .min_h_0()
        .w_full();
        let add = self
            .tool_button("phrase-add", "plus", ink, false, t)
            .tooltip(preference_tooltip(
                tr("添加文本收藏", "Add text").into(),
                t.clone(),
            ))
            .when(can_add, |e| {
                let target = add_target.clone();
                e.on_click(cx.listener(move |this, _, window, cx| {
                    this.open_phrase_draft(target.clone(), true, window, cx)
                }))
            })
            .when(!can_add, |e| e.opacity(0.45));
        let status = self.favorites.as_ref().and_then(|view| view.read(cx).sidebar_error())
            .or_else(|| pane.error.clone())
            .or_else(|| pane.search_error.clone())
            .or_else(|| {
                if !pane.ready {
                    Some(tr("加载中…", "Loading…").into())
                } else if !valid {
                    Some(
                        tr(
                            "收藏夹已移除，请在列表设置中重新选择",
                            "Choose another folder in list settings",
                        )
                        .into(),
                    )
                } else if pane.search_task.is_some() {
                    Some(tr("搜索中…", "Searching…").into())
                } else if pane.rows.is_empty() && !pane.query.is_empty() {
                    Some(tr("没有匹配的收藏", "No favorites found").into())
                } else if pane.rows.is_empty() {
                    Some(tr("暂无收藏", "No favorites").into())
                } else {
                    None
                }
            });
        rsx! { <div flex_1 min_w_0 min_h_0 flex flex_col px={px(l.content_padding_x)} py={px(l.content_padding_y)}
            border_l_1 border_color={color(&t.border)} bg={color(&t.surface)}>
            <div flex_none h={px(l.toolbar_height)} flex items_center gap={px(l.chip_gap)}>{star}{tabs}{add}</div>
        </div> }.when_some(self.favorites.as_ref().map(|view| view.read(cx).sidebar_focus()), |pane, focus| pane.track_focus(&focus))
            .child(div().flex_1().min_h_0().relative().flex().flex_col()
                .children(status.map(|msg| div().p_2().text_color(ink).child(msg)))
                .child(div().flex_1().min_h_0().relative().flex().flex_col().child(rows).child(scrollbar))
                .children(self.phrase_drop_zone(t, cx)))
            .child(div().flex_none().h(px(l.footer_height)).flex().items_center().border_t_1().border_color(color(&t.border))
                .text_size(px(tokens.typography.small_font_size)).text_color(ink).child(format!("{} {}", pane.rows.len(), tr("条收藏", "favorites")))).into_any_element()
    }
}
