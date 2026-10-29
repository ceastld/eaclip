//! Native favorites page. Data, output formats, folder mutations and revision
//! checks stay in HostState. This view lives in the clipboard root so the same
//! stamped input/paste lifetime applies to history and favorites.

use gpui_rsx::rsx;
use std::{
    collections::{BTreeMap, HashSet},
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use cliphost_core::{
    dto::FavorItemDto,
    host::{ChromeTask, HostState},
    i18n::tr,
    ChromeCommand,
};
use gpui::{
    div, img, list, prelude::*, px, relative, AnyElement, AnyWindowHandle, Context, Entity,
    EventEmitter, FocusHandle, KeyDownEvent, ListAlignment, ListState,
    MouseButton, ScrollHandle, SharedString, Subscription, Task, Timer, Window, WindowHandle,
};
use crate::crisp_svg::svg;
use serde::Deserialize;
use serde_json::{json, Value};

#[path = "favorites_ui.rs"]
mod view_memory;
#[path = "favorites_drag_ui.rs"]
mod drag_ui;
use crate::smooth_wheel::SmoothWheelExt;
use crate::favorite_drag::{self, ContentDrag, DragSource, DropTarget, Placement};
use drag_ui::FavoriteDragUi;
use crate::favorites_tree::{self, OccurrencePath, ProjectionOptions, TreeGraph, TreeRow};
use crate::favorites_view_state::{
    FavoriteDisplay, FavoritesViewState, FavoritesViewStore, LayoutMode, ScrollAnchor,
};

use crate::{
    appearance::{
        self, AppearanceSettings, AppearanceSlot, HoverAction, HoverCommand, HoverHandler,
        RowLayout, RowMetadata,
    },
    input::{InputColors, SearchEvent, SearchInput},
    menu::{ContextMenuPopup, MenuAction, MenuEntry, SelectHandler},
    multiline::{MultilineEvent, MultilineInput},
    native_content::{
        self,
        links::OpenTarget,
        model::{ContentMode, ContentProjection, MixedPart, TextProjection},
        ContentImage, ContentMetrics, ContentOpenRequest, ContentResourceCache, ContentResources,
        OpenHandler,
    },
    native_host::NativeWindow,
    platform,
    preview::DesignTokens,
    scrollbar::{ScrollbarChanged, ScrollbarState, ScrollbarStyle, ScrollbarTarget},
    view::color,
};

use crate::view::{hover_binding, next_native_session};
#[cfg(windows)]
use crate::{
    hover::{row_hover_content, RowHoverInput, RowHoverSource},
    native_interactions::{DropDestination, NativeInteractionEvent, NativeInteractions},
    view::SharedInteractions,
};
const PAGE_SIZE: usize = 100;
/// Coalesces bursts of change events into one reload.
const REFRESH_DEBOUNCE: Duration = Duration::from_millis(70);
const PRIMARY_IMAGE_RESOURCE: &str = "favorite-primary-image";

#[derive(Debug, Clone)]
pub enum FavoritesEvent {
    Edit(String),
    /// Empty means the existing home panel; a folder id navigates that panel.
    OpenPanel(String),
    /// The root opens the same shared list appearance/settings UI as history.
    OpenListSettings,
    OpenSettings,
    SwitchPrimaryTab,
}

/// F2 follows the existing WebView AddText -> FavoriteById path, leaving the
/// original history row and SourceClipId intact. Batch text has its own existing
/// folder-aware core operation and must not accidentally take this path.
fn add_text_to_history_and_favorites(host: &HostState, text: &str) -> Result<Value, String> {
    let added = host.add_text(&json!({"text":text,"lineByLine":false}))?;
    let id = added["createdIds"]
        .as_array()
        .and_then(|ids| ids.first())
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            tr("创建剪贴板条目失败", "Could not create a clipboard item.").to_string()
        })?;
    host.favorite_from_clip(&json!({"id":id}))
}

fn sort_favorite_rows(items: &mut [FavorItemDto], by_title: bool) {
    if by_title {
        items.sort_by(|a, b| title(a).cmp(&title(b)).then(a.id.cmp(&b.id)));
    }
}

fn normalized_text(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn meaningful_summary(item: &FavorItemDto) -> String {
    let mut summary_item = item.clone();
    summary_item.title = item.summary.clone();
    let summary = title(&summary_item);
    if matches!(
        summary.as_str(),
        "【收藏夹】" | "【图片】" | "【文件】" | "【文本】" | "【链接】"
    ) || (item.is_folder && summary.trim_matches(['【', '】']).trim() == "收藏夹")
    {
        String::new()
    } else {
        summary
    }
}

fn file_paths(item: &FavorItemDto) -> Vec<String> {
    let paths: Vec<_> = item
        .file_paths
        .iter()
        .map(|path| path.trim())
        .filter(|path| !path.is_empty())
        .map(str::to_string)
        .collect();
    if !paths.is_empty() || category(item) != Category::File {
        return paths;
    }
    meaningful_summary(item)
        .split(',')
        .map(str::trim)
        .filter(|path| !path.is_empty() && *path != "文件")
        .map(str::to_string)
        .collect()
}

/// Keep original DTO positions for the shared file projection and hover plan.
/// The established summary fallback is used only when no DTO path is present.
fn favorite_file_source_paths(item: &FavorItemDto) -> Vec<String> {
    if item.file_paths.iter().any(|path| !path.trim().is_empty()) {
        item.file_paths.clone()
    } else {
        file_paths(item)
    }
}

fn favorite_file_projection(item: &FavorItemDto) -> native_content::files::FileListProjection {
    native_content::files::project_file_paths(
        &favorite_file_source_paths(item),
        ContentMode::Standard,
        &favorite_display_text(item),
        &[],
    )
}

fn preview_key(item: &FavorItemDto) -> String {
    // Include all content list fields: a same-timestamp title/body projection
    // change also invalidates a thumbnail, and a late reply cannot replace a
    // new row. Placement (parent, child count) never changes a preview, so a
    // moved row keeps its decoded resources.
    let mut item = item.clone();
    item.parent_id.clear();
    item.children_count = 0;
    blake3::hash(&serde_json::to_vec(&item).unwrap_or_default())
        .to_hex()
        .to_string()
}

/// FavoriteListItemRow consumes only its list DTO. In particular, HTML and
/// Quicker steps are ordinary linked display text here, without detail fetches.
fn favorite_text_projection(item: &FavorItemDto) -> ContentProjection {
    if !item.is_folder && !item.im_rich_segments.is_empty() {
        ContentProjection::Mixed(native_content::model::project_mixed(&item.im_rich_segments))
    } else {
        let display = favorite_display_text(item);
        let display = if item.is_folder && display.is_empty() {
            tr("收藏夹", "Folder").to_owned()
        } else {
            display
        };
        ContentProjection::Text(TextProjection::new(&display, &[]))
    }
}

fn favorite_display_text(item: &FavorItemDto) -> String {
    let title = title(item);
    let summary = meaningful_summary(item);
    normalized_text(if !title.is_empty() {
        &title
    } else if !summary.is_empty() {
        &summary
    } else {
        &item.summary
    })
}

fn favorite_secondary_projection(item: &FavorItemDto) -> Option<TextProjection> {
    if item.is_folder || matches!(category(item), Category::Image | Category::File) {
        return None;
    }
    let summary = normalized_text(&meaningful_summary(item));
    (!summary.is_empty() && summary != favorite_display_text(item))
        .then(|| TextProjection::new(&summary, &[]))
}

/// A private cache key includes the whole immutable DTO fingerprint as well as
/// the shared mixed projection. The fingerprint is never rendered or emitted.
fn favorite_resource_projection(item: &FavorItemDto) -> Option<ContentProjection> {
    if item.is_folder
        || (!matches!(category(item), Category::Image | Category::File)
            && item.im_rich_segments.is_empty())
    {
        return None;
    }
    let mut parts = native_content::model::project_mixed(&item.im_rich_segments);
    parts.push(MixedPart::Placeholder(preview_key(item)));
    Some(ContentProjection::Mixed(parts))
}

fn load_favorite_resources(
    host: &HostState,
    item: &FavorItemDto,
    foreground: &str,
    image_height: u32,
) -> ContentResources {
    let mut resources = ContentResources::default();
    if category(item) == Category::Image {
        // Decode and resize on the worker straight into the display raster;
        // rows never decode the original or an intermediate PNG.
        resources.images.insert(
            PRIMARY_IMAGE_RESOURCE.into(),
            host.favor_image_bytes(&item.id)
                .ok()
                .and_then(|(bytes, _)| native_content::display_bitmap(&bytes, image_height)),
        );
    } else if category(item) == Category::File {
        resources = native_content::load_file_list_resources(host, &favorite_file_projection(item));
    } else {
        resources = native_content::load_content_resources(
            host,
            &favorite_text_projection(item),
            foreground,
        );
    }
    resources.limit_retained_bytes(native_content::CONTENT_RESOURCE_BUDGET_BYTES);
    resources
}

fn favorite_row_settings(settings: &AppearanceSettings) -> AppearanceSettings {
    // DESIGN 2026-10-14: favorites always use the quiet hairline rows; only
    // density follows the shared list setting. Their own display options
    // (FavoriteDisplay) decide lines, trailing details and hover keys.
    let mut custom = settings.resolved();
    custom.chrome = appearance::ItemChrome::Hairline;
    custom.layout = RowLayout::Minimal;
    custom.show_process = false;
    custom.show_time = false;
    AppearanceSettings {
        slot: AppearanceSlot::Custom,
        custom,
        ..settings.clone()
    }
}

fn favorite_hover_settings(
    settings: &AppearanceSettings,
    folder: bool,
    enabled: &[String],
) -> AppearanceSettings {
    // Favorites offer primary/edit/delete only, filtered by the user's choice;
    // the history pin/favorite keys never appear here.
    let mut custom = favorite_row_settings(settings).resolved();
    custom.hover_actions = [HoverAction::Primary, HoverAction::Edit, HoverAction::Delete]
        .into_iter()
        .filter(|action| !(folder && *action == HoverAction::Primary))
        .filter(|action| enabled.iter().any(|key| key == action.as_str()))
        .collect();
    AppearanceSettings {
        slot: AppearanceSlot::Custom,
        custom,
        ..settings.clone()
    }
}

struct FavoriteTooltip {
    text: String,
    tokens: DesignTokens,
    dark: bool,
}
impl Render for FavoriteTooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let theme = self.tokens.theme(self.dark);
        rsx! {
            <div max_w={px(400.0)} px_2 py_1 rounded={px(4.0)} border_1
                border_color={color(&theme.border)} bg={color(&theme.surface)} text_size={px(11.0)}
                text_color={color(&theme.text)}>
                {self.text.clone()}
            </div>
        }
    }
}

fn favorite_tooltip(
    text: String,
    tokens: DesignTokens,
    dark: bool,
) -> impl Fn(&mut Window, &mut gpui::App) -> gpui::AnyView + 'static {
    move |_, cx| {
        cx.new(|_| FavoriteTooltip {
            text: text.clone(),
            tokens: tokens.clone(),
            dark,
        })
        .into()
    }
}

fn bitmap_element(
    bitmap: &ContentImage,
    width: f32,
    max_height: f32,
    raster_scale: f32,
) -> AnyElement {
    // Explicit display dimensions preserve aspect ratio without asking the UI
    // thread to decode the original image to measure a virtual row.
    let layout = crate::raster_layout::natural_image_layout(
        bitmap.width,
        bitmap.height,
        width,
        max_height,
        raster_scale,
    );
    native_content::render_content_image(
        bitmap,
        img(bitmap.source())
            .w(px(layout.width_dip))
            .h(px(layout.height_dip))
            .max_w_full()
            .object_fit(gpui::ObjectFit::Contain)
            .rounded(px(
                4.0 * crate::raster_layout::image_dpi_compensation(raster_scale)
            ))
            .with_fallback(|| {
                div()
                    .child(tr("图片加载失败", "Image failed to load"))
                    .into_any_element()
            })
            .into_any_element(),
    )
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FavoriteTree {
    #[serde(rename = "rootItems")]
    _root_items: Vec<String>,
    #[serde(default)]
    ordered_root_items: Option<Vec<String>>,
    #[serde(default)]
    children_by_id: BTreeMap<String, Vec<String>>,
    items: BTreeMap<String, FavorItemDto>,
    #[serde(default)]
    drag_revision: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FavoritePage {
    items: Vec<FavorItemDto>,
    total: usize,
    has_more: bool,
    next_skip: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FavoriteScope {
    layout: LayoutMode,
    folder: OccurrencePath,
    query: String,
    category: Category,
    sort_title: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct FavoriteRowIdentity {
    path: OccurrencePath,
    fingerprint: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Category {
    All,
    Text,
    Image,
    File,
    Link,
    Folder,
    Other,
}

impl Category {
    const FILTERS: [Self; 5] = [Self::All, Self::Text, Self::Image, Self::File, Self::Link];
    fn label(self) -> &'static str {
        match self {
            Self::All => tr("全部", "All"),
            Self::Text => tr("文本", "Text"),
            Self::Image => tr("图片", "Images"),
            Self::File => tr("文件", "Files"),
            Self::Link => tr("链接", "Links"),
            Self::Folder => tr("收藏夹", "Folders"),
            Self::Other => tr("其他", "Other"),
        }
    }
    fn icon(self) -> &'static str {
        match self {
            Self::All => "grid",
            Self::Text => "text",
            Self::Image => "image",
            Self::File => "file",
            Self::Link => "link",
            Self::Folder => "folder",
            Self::Other => "clipboard",
        }
    }
}

fn category(item: &FavorItemDto) -> Category {
    if item.is_folder {
        return Category::Folder;
    }
    let kind = item.kind.to_ascii_lowercase();
    if kind.contains("text") || kind.contains("html") {
        Category::Text
    } else if kind.contains("image") {
        Category::Image
    } else if kind.contains("file") || kind.contains("directory") {
        Category::File
    } else if kind.contains("link") {
        Category::Link
    } else {
        Category::Other
    }
}

fn title(item: &FavorItemDto) -> String {
    let value = item.title.trim();
    // Existing SPA strips its old internal add marker for display and rename.
    if let Some(rest) = value.strip_prefix("[favorites-add:") {
        if let Some((digits, text)) = rest.split_once(']') {
            if !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()) {
                return text.trim_start().to_string();
            }
        }
    }
    value.to_string()
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FolderRow {
    id: String,
    title: String,
    depth: usize,
    expanded: bool,
    has_folders: bool,
}

impl FavoriteTree {
    /// Children recorded in the tree. Tree rows carry DTOs whose
    /// `children_count` is only filled by the paged list query.
    fn child_count(&self, id: &str) -> usize {
        self.children_by_id.get(id).map_or(0, Vec::len)
    }

    fn title_of(&self, id: &str) -> String {
        self.items.get(id).map(title).unwrap_or_default()
    }

    fn folder_rows(&self, expanded: &HashSet<String>) -> Vec<FolderRow> {
        let mut result = Vec::new();
        let mut visited = HashSet::new();
        let mut roots: Vec<_> = self
            .items
            .values()
            .filter(|item| {
                item.is_folder
                    && (item.parent_id.is_empty()
                        || !self.items.get(&item.parent_id).is_some_and(|p| p.is_folder))
            })
            .map(|item| item.id.clone())
            .collect();
        roots.sort_by(|a, b| {
            title(&self.items[a])
                .cmp(&title(&self.items[b]))
                .then(a.cmp(b))
        });
        let mut pending: Vec<_> = roots.into_iter().rev().map(|id| (id, 0usize)).collect();
        while let Some((id, depth)) = pending.pop() {
            if !visited.insert(id.clone()) {
                continue;
            }
            let Some(item) = self.items.get(&id) else {
                continue;
            };
            let mut children: Vec<_> = self
                .items
                .values()
                .filter(|child| child.is_folder && child.parent_id == id)
                .map(|child| child.id.clone())
                .collect();
            children.sort_by(|a, b| {
                title(&self.items[a])
                    .cmp(&title(&self.items[b]))
                    .then(a.cmp(b))
            });
            result.push(FolderRow {
                id: id.clone(),
                title: title(item),
                depth,
                expanded: expanded.contains(&id),
                has_folders: !children.is_empty(),
            });
            if expanded.contains(&id) {
                pending.extend(children.into_iter().rev().map(|id| (id, depth + 1)));
            } else {
                visited.extend(self.descendant_ids(&id));
            }
        }
        // Corrupt/cyclic old trees stay inspectable; do not recurse forever.
        for item in self.items.values().filter(|item| item.is_folder) {
            if visited.insert(item.id.clone()) {
                result.push(FolderRow {
                    id: item.id.clone(),
                    title: title(item),
                    depth: 0,
                    expanded: false,
                    has_folders: false,
                });
            }
        }
        result
    }

    fn descendant_ids(&self, id: &str) -> HashSet<String> {
        favorites_tree::descendant_ids(self.graph(), id)
    }

    fn move_choices(&self, id: &str) -> Vec<FolderRow> {
        let excluded = self.descendant_ids(id);
        let expanded = self.items.keys().cloned().collect();
        self.folder_rows(&expanded)
            .into_iter()
            .filter(|row| !excluded.contains(&row.id))
            .collect()
    }

    fn matching_folders(&self, folder: Option<&str>, query: &str) -> Vec<FavorItemDto> {
        let query = cliphost_core::clip_fuzzy::normalize_for_search(query);
        self.items
            .values()
            .filter(|item| item.is_folder)
            .filter(|item| folder.is_none_or(|id| item.parent_id == id))
            .filter(|item| {
                query.is_empty()
                    || cliphost_core::clip_fuzzy::matches(
                        &format!("{} {}", title(item), item.summary),
                        &query,
                    )
            })
            .cloned()
            .collect()
    }
}

enum Dialog {
    CreateFolder,
    Rename {
        id: String,
        revision: String,
    },
    AddText {
        line_by_line: bool,
    },
    Collect {
        draft_id: String,
        parent_id: String,
        folders: Vec<(String, String)>,
    },
    Delete {
        id: String,
        name: String,
        descendants: usize,
    },
    Move {
        id: String,
    },
}

pub struct NativeFavoritesView {
    window: AnyWindowHandle,
    #[cfg(windows)]
    interactions: Option<SharedInteractions>,
    native_session: u64,
    native_owner_enabled: bool,
    image_hover_disabled: bool,
    #[cfg(windows)]
    native_stamp: Option<(isize, u64, u64, bool)>,
    #[cfg(windows)]
    native_hover_tickets: hover_binding::HoverBindings,
    native_dragging: bool,
    native_drag_over: Option<u64>,
    native_suppress_click: Option<Instant>,
    host: Arc<HostState>,
    tokens: DesignTokens,
    dark: bool,
    window_background_enabled: bool,
    window_background_tree_colors: Option<(gpui::Hsla, gpui::Hsla)>,
    appearance: AppearanceSettings,
    execute_mode: i32,
    row_width: f32,
    appearance_generation: u64,
    appearance_task: Option<Task<()>>,
    content_cache: ContentResourceCache,
    /// Display height the cached rasters were prepared for.
    content_image_height: u32,
    /// Natural sizes of loaded images, kept past cache eviction so a reload
    /// placeholder reserves the final row height instead of jumping.
    image_sizes: BTreeMap<String, (f32, f32)>,
    focus: FocusHandle,
    search: Entity<SearchInput>,
    name: Entity<SearchInput>,
    body: Entity<MultilineInput>,
    _subscriptions: Vec<Subscription>,
    tree: FavoriteTree,
    tree_rows: Vec<TreeRow>,
    canonical_paths: BTreeMap<String, OccurrencePath>,
    selected_path: OccurrencePath,
    drag_ui: FavoriteDragUi,
    pending_scroll_anchor: Option<ScrollAnchor>,
    pending_context_restore: bool,
    filter_active: bool,
    pending_reveal: Option<String>,
    memory: FavoritesViewState,
    memory_store: FavoritesViewStore,
    memory_ready: bool,
    memory_load_error: Option<String>,
    memory_load_task: Option<Task<()>>,
    memory_save_task: Option<Task<()>>,
    explicit_folder_route: bool,
    expanded: HashSet<OccurrencePath>,
    /// Folders folded by hand while a search or type filter auto-expands the
    /// tree; valid only for that filter (query, category).
    filter_collapsed: HashSet<OccurrencePath>,
    filter_collapse_key: Option<(String, Category)>,
    folder: Option<String>,
    query: String,
    category: Category,
    sort_title: bool,
    leaves: Vec<FavorItemDto>,
    visible: Vec<FavorItemDto>,
    rendered_scope: Option<FavoriteScope>,
    rendered_rows: Vec<FavoriteRowIdentity>,
    previous_rendered_paths: Vec<OccurrencePath>,
    total: usize,
    has_more: bool,
    next_skip: usize,
    selected: String,
    list: ListState,
    list_scrollbar: ScrollbarState,
    sidebar_scroll: ScrollHandle,
    sidebar_scrollbar: ScrollbarState,
    loading: bool,
    background_refresh: bool,
    refresh_request_scope: Option<FavoriteScope>,
    refresh_requeued: bool,
    /// Generation of a reload started right after this view's own write; its
    /// snapshot already contains the write, so it is shown even if requeued.
    post_write_generation: Option<u64>,
    busy: bool,
    active: bool,
    error: Option<String>,
    dialog: Option<Dialog>,
    /// 「列表显示设置」 popover above the footer.
    display_panel: bool,
    generation: u64,
    dialog_generation: u64,
    pending: Option<Task<()>>,
    more: Option<Task<()>>,
    menu: Option<WindowHandle<ContextMenuPopup>>,
    menu_lease: Option<(u64, String)>,
}

impl EventEmitter<FavoritesEvent> for NativeFavoritesView {}

impl NativeFavoritesView {
    pub(crate) fn scrollbar_drag_active(&self) -> bool {
        self.active && (self.list_scrollbar.is_dragging() || self.sidebar_scrollbar.is_dragging()
            || self.drag_ui.active())
    }

    #[cfg(windows)]
    pub(crate) fn bind_native_interactions(
        &mut self,
        shared: SharedInteractions,
        cx: &mut Context<Self>,
    ) {
        self.interactions = Some(shared);
        self.retire_native_interactions();
        self.maintain_native_interactions(true, cx);
    }

    fn cancel_native_hover(&mut self) {
        #[cfg(windows)]
        {
            if let Some(shared) = &self.interactions {
                if let Some(native) = shared.borrow().as_ref() {
                    native.cancel_hover();
                }
            }
            self.native_hover_tickets.clear();
        }
    }

    fn retire_native_interactions(&mut self) {
        self.cancel_scrollbar_drags();
        self.cancel_native_hover();
        self.native_session = next_native_session();
        self.native_drag_over = None;
        self.native_dragging = false;
        self.apply_native_binding();
    }

    fn cancel_scrollbar_drags(&mut self) {
        self.list_scrollbar.cancel();
        self.sidebar_scrollbar.cancel();
    }

    fn apply_native_binding(&mut self) {
        #[cfg(windows)]
        {
            let Some(shared) = &self.interactions else {
                return;
            };
            let stamp = self.host.native_clipboard_presentation_stamp();
            if self.native_stamp != Some(stamp) {
                self.native_stamp = Some(stamp);
                self.native_session = next_native_session();
                self.native_hover_tickets.clear();
                self.native_drag_over = None;
            }
            let state = self.host.hide_snapshot();
            let accepting = self.native_owner_enabled
                && self.active
                && state.visible
                && !state.is_collapsed
                && !self.busy
                && self.dialog.is_none()
                && self.menu_lease.is_none();
            if let Some(native) = shared.borrow_mut().as_mut() {
                if let Err(error) = native.sync_session(
                    self.native_session,
                    accepting,
                    DropDestination {
                        label: "favorites",
                        parent_id: self.folder.clone(),
                        insert_index: -1,
                    },
                ) {
                    self.error = Some(error);
                }
                let _ = native.set_menu_open(self.menu_lease.is_some());
            }
        }
    }

    pub(crate) fn maintain_native_interactions(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.maintain_content_drag(cx);
        self.native_owner_enabled = enabled;
        let state = self.host.hide_snapshot();
        if !enabled
            || !self.active
            || !state.visible
            || state.is_collapsed
            || self.busy
            || self.dialog.is_some()
            || self.menu_lease.is_some()
        {
            self.cancel_scrollbar_drags();
            if self.drag_ui.active() { self.cancel_content_drag(cx); }
        }
        let before = self.native_session;
        self.apply_native_binding();
        if before != self.native_session {
            self.cancel_scrollbar_drags();
            self.native_dragging = false;
            cx.notify();
        }
        #[cfg(windows)]
        if self
            .menu_lease
            .as_ref()
            .is_some_and(|(lease, _)| !self.host.native_context_menu_is_current(*lease))
        {
            self.close_menu(cx);
        }
        self.maintain_content_resources(cx);
    }

    #[cfg(windows)]
    pub(crate) fn native_interaction_event(
        &mut self,
        event: NativeInteractionEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            NativeInteractionEvent::ContentSaved { operation_id, result } => {
                self.content_saved(operation_id, result, cx);
            }
            NativeInteractionEvent::DropSaved { request, result } => {
                if !self.active
                    || request.page_session() != self.native_session
                    || !self.host.native_drop_request_is_current(&request)
                {
                    return;
                }
                self.native_drag_over = None;
                match result {
                    Ok(_) => self.refresh(cx),
                    Err(error) => self.error = Some(error),
                }
            }
            NativeInteractionEvent::DragOver { ticket, over } => {
                if over && ticket.session() == self.native_session && ticket.is_current() {
                    self.native_drag_over = Some(ticket.id());
                } else if !over && self.native_drag_over == Some(ticket.id()) {
                    self.native_drag_over = None;
                }
            }
            NativeInteractionEvent::DragFinished { session, result } => {
                if session != self.native_session {
                    return;
                }
                self.native_dragging = false;
                self.native_suppress_click = Some(Instant::now() + Duration::from_millis(200));
                if let Err(error) = result {
                    self.error = Some(error);
                }
            }
            NativeInteractionEvent::Error(error) => self.error = Some(error),
        }
        cx.notify();
    }

    #[cfg(windows)]
    fn native_item_hover_binding(
        &self,
        index: usize,
        id: &str,
        generation: u64,
        session: u64,
    ) -> Option<hover_binding::HoverBinding> {
        if cliphost_core::is_headless() {
            return None;
        }
        let item = self
            .visible
            .get(index)
            .filter(|item| item.id == id && !item.is_folder)?;
        let paths = favorite_file_source_paths(item);
        let display = favorite_display_text(item);
        let plan = row_hover_content(
            RowHoverSource::Favorite,
            RowHoverInput {
                id,
                kind: &item.kind,
                title: &item.title,
                summary: &item.summary,
                display_text: &display,
                file_paths: &paths,
                im_rich_segments: &item.im_rich_segments,
            },
        );
        Some(self.native_hover_tickets.row(
            id,
            index,
            generation,
            session,
            plan,
            !self.image_hover_disabled,
        ))
    }

    fn hover_native_item(
        &mut self,
        index: usize,
        id: &str,
        generation: u64,
        session: u64,
        event: hover_binding::HoverEvent,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(windows)]
        {
            if !self.active
                || !self.native_owner_enabled
                || self.busy
                || self.dialog.is_some()
                || self.menu_lease.is_some()
                || self.loading
                || self.generation != generation
                || self.native_session != session
                || self.native_dragging
                || !self
                    .visible
                    .get(index)
                    .is_some_and(|item| item.id == id && !item.is_folder)
            {
                event.reject();
                return;
            }
            let owner = self.interactions.as_ref().and_then(|shared| {
                shared
                    .borrow()
                    .as_ref()
                    .and_then(NativeInteractions::hover_owner)
            });
            let Some(owner) = owner else {
                event.reject();
                return;
            };
            if let Err(error) = event.apply(owner, f64::from(window.scale_factor()), self.dark) {
                tracing::debug!(%error, "Hover preview request unavailable");
                cx.notify();
            }
        }
        #[cfg(not(windows))]
        let _ = (index, id, generation, session, event, window, cx);
    }
    pub fn new(
        host: Arc<HostState>,
        tokens: DesignTokens,
        dark: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let colors = input_colors(&tokens, dark);
        let config = host.config_get();
        let search = cx.new(|cx| {
            let mut input = SearchInput::new(window, cx);
            // Match the clipboard titlebar search: the field draws its own
            // fill and shows no placeholder text.
            input.set_colors(InputColors {
                background: gpui::transparent_black(),
                border: gpui::transparent_black(),
                placeholder: colors.placeholder.opacity(0.6),
                ..colors
            }, cx);
            input.set_metrics(
                tokens.layout.search_height - 2.0,
                tokens.typography.category_font_size,
                0.0,
                cx,
            );
            input.set_placeholder("", cx);
            input
        });
        let name = cx.new(|cx| {
            let mut input = SearchInput::new(window, cx);
            input.set_colors(colors, cx);
            input.set_metrics(
                tokens.settings.control_height,
                tokens.typography.font_size,
                8.0,
                cx,
            );
            input.set_double_click_clears(false);
            input.set_placeholder(tr("名称", "Name"), cx);
            input
        });
        let body = cx.new(|cx| {
            let mut input = MultilineInput::new(window, cx);
            input.set_colors(colors, cx);
            input.set_metrics(160.0, tokens.typography.font_size, 8.0, cx);
            input
        });
        let search_subscription =
            cx.subscribe_in(&search, window, |this, _, event, window, cx| match event {
                SearchEvent::Changed(query) => {
                    this.query = query.clone();
                    this.refresh(cx);
                }
                SearchEvent::Submit => {
                    window.blur();
                    this.restore_browsing(window);
                    this.execute_selected(None, cx);
                }
                SearchEvent::Escape => {
                    window.blur();
                    this.restore_browsing(window);
                }
            });
        let name_subscription =
            cx.subscribe_in(&name, window, |this, _, event, window, cx| match event {
                SearchEvent::Submit => this.submit_dialog(window, cx),
                SearchEvent::Escape => this.close_dialog(window, cx),
                SearchEvent::Changed(_) => {}
            });
        let body_subscription =
            cx.subscribe_in(&body, window, |this, _, event, window, cx| match event {
                MultilineEvent::Save => this.submit_dialog(window, cx),
                MultilineEvent::Escape => this.close_dialog(window, cx),
                MultilineEvent::Changed(_)
                | MultilineEvent::Submit
                | MultilineEvent::PasteRequested(_) => {}
            });
        let mut view = Self {
            window: window.window_handle(),
            #[cfg(windows)]
            interactions: None,
            native_session: next_native_session(),
            native_owner_enabled: false,
            image_hover_disabled: config["clipListImageHoverPreviewEnabled"].as_bool()
                == Some(false),
            #[cfg(windows)]
            native_stamp: None,
            #[cfg(windows)]
            native_hover_tickets: hover_binding::HoverBindings::default(),
            native_dragging: false,
            native_drag_over: None,
            native_suppress_click: None,
            host,
            tokens,
            dark,
            window_background_enabled: false,
            window_background_tree_colors: None,
            appearance: AppearanceSettings::from_config(&config),
            execute_mode: config["clipListExecuteMode"].as_i64().unwrap_or(1) as i32,
            row_width: 400.0,
            appearance_generation: 0,
            appearance_task: None,
            content_cache: ContentResourceCache::default(),
            content_image_height: 0,
            image_sizes: BTreeMap::new(),
            focus: cx.focus_handle(),
            search,
            name,
            body,
            _subscriptions: vec![search_subscription, name_subscription, body_subscription],
            tree: FavoriteTree::default(),
            tree_rows: Vec::new(),
            canonical_paths: BTreeMap::new(),
            selected_path: Vec::new(),
            drag_ui: FavoriteDragUi::default(),
            pending_scroll_anchor: None,
            pending_context_restore: true,
            filter_active: false,
            pending_reveal: None,
            memory: FavoritesViewState::default(),
            memory_store: FavoritesViewStore::in_memory(),
            memory_ready: false,
            memory_load_error: None,
            memory_load_task: None,
            memory_save_task: None,
            explicit_folder_route: false,
            expanded: HashSet::new(),
            folder: None,
            query: String::new(),
            category: Category::All,
            sort_title: false,
            leaves: Vec::new(),
            visible: Vec::new(),
            rendered_scope: None,
            rendered_rows: Vec::new(),
            previous_rendered_paths: Vec::new(),
            total: 0,
            has_more: false,
            next_skip: 0,
            selected: String::new(),
            list: ListState::new(0, ListAlignment::Top, px(100.0)),
            list_scrollbar: ScrollbarState::default(),
            sidebar_scroll: ScrollHandle::new(),
            sidebar_scrollbar: ScrollbarState::default(),
            loading: false,
            background_refresh: false,
            refresh_request_scope: None,
            refresh_requeued: false,
            post_write_generation: None,
            busy: false,
            active: true,
            error: None,
            dialog: None,
            filter_collapsed: HashSet::new(),
            filter_collapse_key: None,
            display_panel: false,
            generation: 0,
            dialog_generation: 0,
            pending: None,
            more: None,
            menu: None,
            menu_lease: None,
        };
        view._subscriptions.push(cx.on_app_quit(|this, cx| {
            this.remember_view_memory();
            let store = this.memory_store.clone();
            cx.background_executor().spawn(async move {
                if let Err(error) = store.flush() {
                    tracing::warn!(%error, "could not save favorites view state on exit");
                }
            })
        }));
        let weak = cx.weak_entity();
        view.list.set_scroll_handler(move |_, _, cx| {
            let weak = weak.clone();
            cx.defer(move |cx| {
                let _ = weak.update(cx, |this, cx| this.save_view_memory(cx));
            });
        });
        view.load_view_memory(cx);
        view
    }

    /// Forward favoritesChanged/nativeRefreshRequired here from the one service
    /// subscription owned by the root, instead of consuming core's event queue.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.refresh_after(REFRESH_DEBOUNCE, cx);
    }

    /// Reload after this view's own committed write: no change-event debounce,
    /// and the result is shown even when another change requeues a reload.
    pub(super) fn refresh_after_write(&mut self, cx: &mut Context<Self>) {
        let previous = self.generation;
        self.refresh_after(Duration::ZERO, cx);
        // A requeue keeps the older, pre-write request; only a load started
        // here is known to contain the write.
        if self.loading && self.generation != previous {
            self.post_write_generation = Some(self.generation);
        }
    }

    fn refresh_after(&mut self, debounce: Duration, cx: &mut Context<Self>) {
        if !self.active || !self.memory_ready {
            return;
        }
        // The change event of an in-flight drag write arrives before its
        // result; the completion reloads a snapshot that already contains it.
        if self.content_mutation_pending() {
            return;
        }
        self.prepare_filter_transition();
        self.cancel_content_drag(cx);
        self.retire_native_interactions();
        let scope = self.favorite_scope();
        if self.loading && self.refresh_request_scope.as_ref() == Some(&scope) {
            self.refresh_requeued = true;
            return;
        }
        self.refresh_requeued = false;
        self.post_write_generation = None;
        self.background_refresh = self.rendered_scope.as_ref() == Some(&scope);
        self.refresh_request_scope = Some(scope.clone());
        let same_scope = self.background_refresh;
        let preferred_loaded = if same_scope { self.leaves.len().max(PAGE_SIZE) } else { PAGE_SIZE };
        self.generation += 1;
        let generation = self.generation;
        self.appearance_generation += 1;
        let appearance_generation = self.appearance_generation;
        self.appearance_task = None;
        let host = self.host.clone();
        let (folder, query) = (self.folder.clone(), self.query.clone());
        let restore_targets = if !self.memory.layout.is_tree()
            && self.query.is_empty()
            && self.category == Category::All
        {
            let mut targets: HashSet<String> = self
                .pending_scroll_anchor
                .clone()
                .or_else(|| self.current_scroll_anchor())
                .and_then(|anchor| anchor.path.last().cloned())
                .into_iter()
                .collect();
            targets.extend(self.selected_path.last().cloned());
            targets
        } else {
            HashSet::new()
        };
        self.more = None;
        if same_scope {
            // Keep completed images visible during the request. Old pending
            // workers still retire with the data/input generation.
            self.content_cache.advance_retaining(generation, |_| true);
        } else {
            self.content_cache.advance(generation);
        }
        self.content_cache.release_evicted(cx);
        self.loading = true;
        self.pending = Some(cx.spawn(async move |this, cx| {
            if !debounce.is_zero() {
                Timer::after(debounce).await;
            }
            let result = cx
                .background_executor()
                .spawn(async move {
                    let config = host.config_get();
                    let tree: FavoriteTree = serde_json::from_value(host.favor_tree()?)
                        .map_err(|error| error.to_string())?;
                    let folder =
                        folder.filter(|id| tree.items.get(id).is_some_and(|item| item.is_folder));
                    let mut page: FavoritePage = if query.is_empty() {
                        tree.direct_page(folder.as_deref(), 0, preferred_loaded)
                    } else {
                        serde_json::from_value(host.favor_search(&json!({
                            "parentId": folder, "query": query, "skip": 0, "take": preferred_loaded.min(500)
                        }))?).map_err(|error| error.to_string())?
                    };
                    // Read additional pages only when a remembered position
                    // actually belongs to this scope and lies past page one.
                    let members: HashSet<_> = favorites_tree::direct_members(tree.graph(), folder.as_deref()).into_iter().collect();
                    let restore_targets: HashSet<_> = restore_targets.into_iter().filter(|id| members.contains(id)).collect();
                    if !restore_targets.is_empty() || preferred_loaded > page.items.len() {
                        while page.has_more && page.items.len() < 20_000
                            && (page.items.len() < preferred_loaded || restore_targets.iter().any(|target| !page.items.iter().any(|item| &item.id == target))) {
                            let take = if restore_targets.iter().any(|target| !page.items.iter().any(|item| &item.id == target)) { 500 }
                                else { preferred_loaded.saturating_sub(page.items.len()).min(500).max(1) };
                            let next: FavoritePage = if query.is_empty() {
                                tree.direct_page(folder.as_deref(), page.next_skip, take)
                            } else {
                                serde_json::from_value(host.favor_search(&json!({
                                    "parentId": folder, "query": query, "skip": page.next_skip, "take": take
                                }))?).map_err(|error| error.to_string())?
                            };
                            if next.next_skip <= page.next_skip || next.items.is_empty() { break; }
                            page.items.extend(next.items);
                            page.total = next.total;
                            page.has_more = next.has_more;
                            page.next_skip = next.next_skip;
                        }
                    }
                    Ok::<_, String>((tree, folder, page, config))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if generation != this.generation || !this.active {
                    return;
                }
                this.loading = false;
                this.refresh_request_scope = None;
                let post_write = this.post_write_generation.take() == Some(generation);
                let requeue_after_commit = this.refresh_requeued && post_write;
                if requeue_after_commit {
                    this.refresh_requeued = false;
                }
                if this.refresh_requeued {
                    // A change may have arrived after this worker read its
                    // snapshot. Keep the displayed rows until the replacement
                    // request returns instead of briefly committing stale data.
                    this.refresh_requeued = false;
                    this.refresh(cx);
                    return;
                }
                match result {
                    Ok((tree, folder, page, config)) => {
                        let old_resources = this.resource_occurrences();
                        let old_selection = this.selected_path.clone();
                        let explicit_reveal = this.pending_reveal.is_some();
                        if this.appearance_generation == appearance_generation {
                            this.apply_appearance(&config);
                        }
                        this.remember_view_memory();
                        if this.folder != folder {
                            this.host.retire_native_clipboard_list_input();
                        }
                        this.folder = folder.clone();
                        this.tree = tree;
                        this.reconcile_view_memory();
                        if this.folder != folder {
                            this.refresh(cx);
                            return;
                        }
                        if same_scope && !explicit_reveal {
                            // Let the row projection choose the deleted row's
                            // neighbor instead of pruning selection to a parent.
                            this.selected_path = old_selection;
                            this.selected = this.selected_path.last().cloned().unwrap_or_default();
                        }
                        this.leaves = page.items;
                        this.total = page.total;
                        this.has_more = page.has_more;
                        this.next_skip = page.next_skip;
                        this.error = this.memory_load_error.clone();
                        this.rebuild_visible();
                        if same_scope { this.retain_refreshed_content_resources(&old_resources, cx); }
                    }
                    Err(error) => this.error = Some(error),
                }
                if requeue_after_commit {
                    this.refresh(cx);
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub fn resume(&mut self, cx: &mut Context<Self>) {
        self.active = true;
        self.refresh(cx);
    }

    pub fn set_theme(&mut self, dark: bool, cx: &mut Context<Self>) {
        self.set_design_tokens(self.tokens.clone(), dark, cx);
    }

    pub fn set_design_tokens(&mut self, tokens: DesignTokens, dark: bool, cx: &mut Context<Self>) {
        if self.dark == dark && self.tokens.theme(dark) == tokens.theme(dark) {
            return;
        }
        self.cancel_scrollbar_drags();
        self.cancel_native_hover();
        self.tokens = tokens;
        self.dark = dark;
        let colors = input_colors(&self.tokens, dark);
        self.search
            .update(cx, |input, cx| input.set_colors(InputColors {
                placeholder: colors.placeholder.opacity(0.6),
                ..colors
            }, cx));
        self.name
            .update(cx, |input, cx| input.set_colors(colors, cx));
        self.body
            .update(cx, |input, cx| input.set_colors(colors, cx));
        cx.notify();
    }

    /// Root passes wallpaper tokens only after the current image is loaded.
    /// Keep existing input entities, selection, dialog drafts and composition.
    pub fn set_window_background(
        &mut self,
        enabled: bool,
        tokens: DesignTokens,
        dark: bool,
        cx: &mut Context<Self>,
    ) {
        let changed = self.window_background_enabled != enabled;
        self.window_background_enabled = enabled;
        self.set_design_tokens(tokens, dark, cx);
        if changed {
            cx.notify();
        }
    }

    /// These colors must be derived from the original theme, before its
    /// secondary surface receives the wallpaper opacity multiplier.
    pub fn set_window_background_tree_colors(
        &mut self,
        hover: gpui::Hsla,
        active: gpui::Hsla,
        cx: &mut Context<Self>,
    ) {
        let colors = Some((hover, active));
        if self.window_background_tree_colors != colors {
            self.window_background_tree_colors = colors;
            if self.window_background_enabled {
                cx.notify();
            }
        }
    }

    /// Root may apply its current config snapshot on configChanged/maintain.
    /// No search, selected item, dialog draft, or composition is rebuilt.
    pub fn set_config(&mut self, config: &Value, cx: &mut Context<Self>) {
        let image_hover_disabled =
            config["clipListImageHoverPreviewEnabled"].as_bool() == Some(false);
        if self.image_hover_disabled != image_hover_disabled {
            self.image_hover_disabled = image_hover_disabled;
            self.cancel_native_hover();
            cx.notify();
        }
        self.appearance_generation += 1;
        self.appearance_task = None;
        if self.apply_appearance(config) {
            cx.notify();
        }
    }

    pub fn refresh_appearance(&mut self, cx: &mut Context<Self>) {
        if !self.active {
            return;
        }
        self.appearance_generation += 1;
        let generation = self.appearance_generation;
        let host = self.host.clone();
        self.appearance_task = Some(cx.spawn(async move |this, cx| {
            let config = cx
                .background_executor()
                .spawn(async move { host.config_get() })
                .await;
            let _ = this.update(cx, |this, cx| {
                if !this.active || this.appearance_generation != generation {
                    return;
                }
                this.appearance_task = None;
                if this.apply_appearance(&config) {
                    cx.notify();
                }
            });
        }));
    }

    fn apply_appearance(&mut self, config: &Value) -> bool {
        let settings = AppearanceSettings::from_config(config);
        let mode = config["clipListExecuteMode"].as_i64().unwrap_or(1) as i32;
        let changed = self.execute_mode != mode || self.appearance != settings;
        self.execute_mode = mode;
        if self.appearance != settings {
            self.appearance = settings;
            self.list_scrollbar.cancel();
            crate::history_scroll::remeasure(&self.list);
        }
        changed
    }

    /// The root's existing maintenance tick prepares only visible rows. Render
    /// is a pure cache consumer, and retiring a page keeps physical reservations
    /// occupied until its detached workers finish (four across page changes).
    fn maintain_content_resources(&mut self, cx: &mut Context<Self>) {
        let image_height = self.appearance.image_max_height_px.clamp(1, 640) as u32;
        if self.content_image_height != image_height {
            // Rasters are sized for the old setting; reload them at the new one.
            self.content_image_height = image_height;
            self.content_cache.retain(|_| false);
            self.content_cache.release_evicted(cx);
        }
        let state = self.host.hide_snapshot();
        if self.loading
            || !self.active
            || !self.native_owner_enabled
            || !state.visible
            || state.is_collapsed
        {
            self.content_cache.set_visible_items(std::iter::empty::<&str>());
            if (!self.active || !state.visible || state.is_collapsed)
                && self.content_cache.reclaim_idle()
            {
                self.content_cache.release_evicted(cx);
            }
            return;
        }
        let generation = self.generation;
        let session = self.native_session;
        let foreground = self.tokens.theme(self.dark).text.clone();
        let viewport = self.list.viewport_bounds();
        // Walk from the scroll anchor, as history does: rows above it have no
        // bounds and must not count as visible (they pinned the first rows).
        let top = self.list.logical_scroll_top().item_ix;
        let mut on_screen: Vec<usize> = (top..self.visible.len())
            .map_while(|index| self.list.bounds_for_item(index)
                .filter(|bounds| bounds.top() < viewport.bottom())
                .map(|bounds| (index, bounds)))
            .filter(|(_, bounds)| bounds.bottom() > viewport.top())
            .map(|(index, _)| index)
            .take(32)
            .collect();
        if on_screen.is_empty() {
            on_screen = (top..self.visible.len()).take(12).collect();
        }
        // On-screen rows first (they reserve worker slots first), then a
        // bounded prefetch below/above so short scrolls find ready rasters.
        let rows: Vec<_> = native_content::cache::prefetch_order(&on_screen, self.visible.len())
            .into_iter()
            .filter_map(|index| {
                let item = &self.visible[index];
                favorite_resource_projection(item).map(|projection| (item.clone(), projection))
            })
            .take(32)
            .collect();
        self.content_cache.set_visible_items(rows.iter().map(|(item, _)| item.id.as_str()));
        for (item, projection) in rows {
            let Some(ticket) = self.content_cache.reserve(
                generation,
                &item.id,
                &projection,
                ContentMode::Standard,
                &foreground,
            ) else {
                continue;
            };
            let host = self.host.clone();
            let worker_item = item.clone();
            let worker_foreground = foreground.clone();
            let reply_foreground = foreground.clone();
            cx.spawn(async move |this, cx| {
                let resources = cx
                    .background_executor()
                    .spawn(async move {
                        load_favorite_resources(&host, &worker_item, &worker_foreground, image_height)
                    })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    let current = this.active
                        && this.generation == generation
                        && this.native_session == session
                        && this.tokens.theme(this.dark).text == reply_foreground
                        && this
                            .visible
                            .iter()
                            .find(|current| current.id == item.id)
                            .and_then(favorite_resource_projection)
                            .as_ref()
                            == Some(&projection);
                    let size = resources.images.values().flatten().next()
                        .map(|image| (image.width, image.height));
                    // A placeholder that already reserved this exact size keeps
                    // the row height; only other resources need re-measuring.
                    let same_height = size.is_some()
                        && this.image_sizes.get(&item.id).copied() == size;
                    if let (true, Some(size)) = (current, size) {
                        if this.image_sizes.len() >= 4096 {
                            this.image_sizes.clear();
                        }
                        this.image_sizes.insert(item.id.clone(), size);
                    }
                    let accepted = this.content_cache.finish(ticket, resources, current);
                    this.content_cache.release_evicted(cx);
                    if accepted && !same_height {
                        let anchor = this.list.logical_scroll_top();
                        this.list_scrollbar.invalidate_measurements();
                        for (index, _) in this
                            .visible
                            .iter()
                            .enumerate()
                            .filter(|(_, current)| current.id == item.id)
                        {
                            this.list.splice(index..index + 1, 1);
                        }
                        this.list.scroll_to(anchor);
                    }
                    // Even rejected completions free a physical slot. Keep the
                    // next visible row moving without rebuilding the whole list.
                    this.maintain_content_resources(cx);
                    cx.notify();
                });
            })
            .detach();
        }
        self.content_cache.release_evicted(cx);
    }

    /// Existing host navigation uses /favorites?draftId=...&focusSearch=1.
    /// folderId/parentId select a real folder without manufacturing a new root.
    pub fn set_route(&mut self, route: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel_scrollbar_drags();
        self.active = true;
        let params = route
            .split_once('?')
            .map(|(_, query)| query)
            .unwrap_or_default()
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .map(|(key, value)| (key.to_string(), decode_query(value)))
            .collect::<BTreeMap<_, _>>();
        if let Some(folder) = params.get("folderId").or_else(|| params.get("parentId")) {
            self.explicit_folder_route = true;
            self.remember_view_memory();
            let folder = (!folder.is_empty()).then(|| folder.clone());
            self.pending_reveal = folder.clone();
            let folder = if self.memory.layout == LayoutMode::Tree {
                None
            } else {
                folder
            };
            if self.folder != folder {
                self.host.retire_native_clipboard_list_input();
                self.folder = folder;
                self.memory.active_folder = self
                    .folder
                    .as_deref()
                    .map(|id| self.canonical_path(id))
                    .unwrap_or_default();
                self.restore_context_memory();
            }
        }
        if let Some(query) = params.get("query") {
            self.query = query.clone();
        }
        self.sync_search(window, cx);
        if let Some(id) = params.get("itemId") {
            self.selected = id.clone();
            self.pending_reveal = Some(id.clone());
        }
        if let Some(id) = params.get("draftId").filter(|id| !id.is_empty()) {
            self.open_collect(id.clone(), window, cx);
        } else if params
            .get("focusSearch")
            .is_some_and(|value| value != "0" && value != "false")
        {
            self.focus_search(window, cx);
        } else if self.dialog.is_none() {
            // The owner blurred the departing page. Give normal navigation
            // to this page so the next Tab can switch back again.
            self.focus.focus(window);
            self.restore_browsing(window);
        }
    }

    /// Owner tick while the favorites tab is not shown: return the retained
    /// resources in stages (see `ListContentCache::reclaim_idle`).
    pub(crate) fn reclaim_idle_content(&mut self, cx: &mut Context<Self>) {
        if self.active {
            return;
        }
        self.content_cache.set_visible_items(std::iter::empty::<&str>());
        if self.content_cache.reclaim_idle() {
            self.content_cache.release_evicted(cx);
        }
    }

    pub(crate) fn next_idle_content_deadline(&self) -> Option<Instant> {
        self.content_cache.next_idle_deadline()
    }

    /// Called on owner hide, tab departure or window replacement. Results may
    /// finish on workers, but no old search/dialog/menu can reappear afterward.
    pub fn suspend(&mut self, cx: &mut Context<Self>) {
        self.cancel_content_drag(cx);
        self.save_view_memory(cx);
        self.active = false;
        self.retire_native_interactions();
        self.generation += 1;
        self.dialog_generation += 1;
        self.appearance_generation += 1;
        self.appearance_task = None;
        // Keep completed resources across a tab switch; pending workers retire
        // with the generation. Idle reclamation returns them if we stay away.
        self.content_cache.advance_retaining(self.generation, |_| true);
        self.content_cache.release_evicted(cx);
        self.pending = None;
        self.more = None;
        self.loading = false;
        self.background_refresh = false;
        self.refresh_request_scope = None;
        self.refresh_requeued = false;
        self.post_write_generation = None;
        self.dialog = None;
        self.host.apply_window_action("setSpaModalOpen", false);
        self.close_menu(cx);
    }

    fn rebuild_visible(&mut self) {
        self.prepare_filter_transition();
        let old_anchor = self.current_scroll_anchor();
        self.cancel_native_hover();
        // Local category/sort/folder rebuilds have no route change for core to
        // observe. Retire the original list-action tickets before replacing rows.
        self.host.retire_native_clipboard_list_input();
        self.retire_native_interactions();
        if self.memory.layout.is_tree() {
            self.rebuild_tree_visible();
            self.restore_scroll_anchor(old_anchor);
            return;
        }
        self.tree_rows.clear();
        let mut items = self.leaves.clone();
        if !self.query.is_empty() {
            items.extend(self.tree.matching_folders(self.folder.as_deref(), &self.query));
        }
        items.retain(|item| self.category == Category::All || category(item) == self.category);
        sort_favorite_rows(&mut items, self.sort_title);
        self.visible = items;
        if !self.visible.iter().any(|item| item.id == self.selected) {
            let previous_paths: Vec<_> = self.rendered_rows.iter().map(|row| row.path.clone()).collect();
            let paths: Vec<_> = (0..self.visible.len()).map(|index| self.row_path(index)).collect();
            let neighbor = view_memory::surviving_anchor_index(&previous_paths, &paths, &self.selected_path)
                .map(|(index, _)| index).unwrap_or(0);
            self.selected = self.visible
                .get(neighbor)
                .map(|item| item.id.clone())
                .unwrap_or_default();
        }
        self.selected_path = self.visible.iter().position(|item| item.id == self.selected)
            .map(|index| self.row_path(index)).unwrap_or_default();
        self.reconcile_rendered_rows();
        self.restore_scroll_anchor(old_anchor);
    }

    fn load_more(&mut self, cx: &mut Context<Self>) {
        if self.memory.layout.is_tree() {
            return;
        }
        if self.loading || self.more.is_some() || !self.has_more {
            return;
        }
        if self.query.is_empty() {
            let page = self.tree.direct_page(self.folder.as_deref(), self.next_skip, PAGE_SIZE);
            self.leaves.extend(page.items);
            self.total = page.total;
            self.has_more = page.has_more;
            self.next_skip = page.next_skip;
            self.rebuild_visible();
            cx.notify();
            return;
        }
        let generation = self.generation;
        let (host, folder, query, skip) = (
            self.host.clone(),
            self.folder.clone(),
            self.query.clone(),
            self.next_skip,
        );
        self.more = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    serde_json::from_value::<FavoritePage>(host.favor_search(&json!({
                        "parentId": folder, "query": query, "skip": skip, "take": PAGE_SIZE
                    }))?)
                    .map_err(|error| error.to_string())
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation || !this.active {
                    return;
                }
                this.more = None;
                match result {
                    Ok(page) => {
                        let present: HashSet<_> =
                            this.leaves.iter().map(|item| item.id.clone()).collect();
                        this.leaves.extend(
                            page.items
                                .into_iter()
                                .filter(|item| !present.contains(&item.id)),
                        );
                        this.total = page.total;
                        this.has_more = page.has_more;
                        this.next_skip = page.next_skip;
                        this.rebuild_visible();
                    }
                    Err(error) => this.error = Some(error),
                }
                cx.notify();
            });
        }));
    }

    fn select_folder(
        &mut self,
        folder: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.remember_view_memory();
        if self.folder != folder {
            // Folder navigation is local to this page, so /favorites alone
            // cannot distinguish A -> B -> A. Retire core's stamped copy/paste
            // requests before changing the folder, including stale queue work.
            self.host.retire_native_clipboard_list_input();
        }
        self.folder = folder;
        if self.memory.active_folder.last() != self.folder.as_ref() {
            self.memory.active_folder = self
                .folder
                .as_deref()
                .map(|id| self.canonical_path(id))
                .unwrap_or_default();
        }
        self.selected_path.clear();
        self.restore_context_memory();
        self.focus.focus(window);
        self.restore_browsing(window);
        self.refresh(cx);
        self.save_view_memory(cx);
    }

    fn restore_browsing(&mut self, window: &Window) {
        if let Err(error) = NativeWindow::restore_browsing(window, &self.host) {
            self.error = Some(error);
        }
    }

    pub fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        cx.defer_in(window, |this, window, cx| {
            match NativeWindow::request_search_focus(window, &this.host) {
                Ok(()) => this.search.read(cx).focus_handle().focus(window),
                Err(error) => this.error = Some(error),
            }
            cx.notify();
        });
    }

    fn blur_search_from_pointer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dialog.is_some()
            || !self.search.read(cx).focus_handle().is_focused(window)
            || self.search.read(cx).is_composing()
        {
            return;
        }
        self.focus.focus(window);
        self.restore_browsing(window);
        cx.notify();
    }

    #[cfg(debug_assertions)]
    pub(crate) fn search_focus_report(&self, window: &Window, cx: &gpui::App) -> Value {
        json!({"list": self.focus.is_focused(window),
            "search": self.search.read(cx).focus_handle().is_focused(window),
            "query": self.query})
    }

    /// Keep the existing search entity and its input lifetime while the root
    /// places it alongside the page navigation in the native title bar.
    pub(crate) fn titlebar_search(
        &mut self,
        width: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tokens = &self.tokens;
        let t = tokens.theme(self.dark);
        let l = &tokens.layout;
        let search_focused = self.search.read(cx).focus_handle().is_focused(window);
        div()
            .id("favorite-titlebar-search")
            .on_mouse_down_out(cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                if event.button == MouseButton::Left {
                    this.blur_search_from_pointer(window, cx);
                }
            }))
            .relative()
            .w(px(width))
            .min_w_0()
            .flex_none()
            .h(px(l.search_height))
            .flex()
            .items_center()
            .gap(px(6.0))
            .pl(px(l.search_icon_inset))
            .pr(px(8.0))
            .rounded(px(l.search_radius))
            .bg(color(&t.secondary))
            .border_1()
            .border_color(if search_focused {
                color(&t.border_secondary)
            } else {
                gpui::transparent_black()
            })
            .child(
                gpui::canvas(
                    |bounds, window, _| platform::record_caption_exclusion(window, bounds),
                    |_, _, _, _| {},
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
            .child(rsx! {
                <div flex_1 min_w_0>
                    {self.search.clone()}
                </div>
            })
            // The input stops mouse bubbling for text selection, so native
            // focus handoff must run in capture. The page's old modal/backdrop
            // covered the search while these surfaces were open.
            .capture_any_mouse_down(cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                if event.button != MouseButton::Left {
                    return;
                }
                this.begin_pointer_sequence();
                this.cancel_native_hover();
                if this.dialog.is_some() {
                    cx.stop_propagation();
                } else if this.display_panel {
                    this.display_panel = false;
                    cx.stop_propagation();
                    cx.notify();
                } else {
                    this.focus_search(window, cx);
                }
            }))
            .on_key_down(cx.listener(|this, event, window, cx| {
                this.cancel_native_hover();
                this.key_down(event, window, cx)
            }))
            .into_any_element()
    }

    pub fn is_composing(&self, cx: &gpui::App) -> bool {
        self.search.read(cx).is_composing()
    }

    pub fn handle_chrome_input(
        &mut self,
        task: ChromeTask,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if let ChromeCommand::DismissContextMenu(lease) = task.command() {
            if self.menu_lease.as_ref().is_some_and(|(id, _)| *id == lease) {
                self.close_menu(cx);
                return true;
            }
            return false;
        }
        if !self.active || !self.host.chrome_task_input_allowed(task) {
            return false;
        }
        if self.dialog.is_some() {
            if task.command() == ChromeCommand::ClipboardHandleEscape {
                self.close_dialog(window, cx);
            }
            return true;
        }
        match task.command() {
            ChromeCommand::ClipboardKeyboardFocus | ChromeCommand::ClipboardActivateSearch => {
                self.focus_search(window, cx)
            }
            ChromeCommand::ClipboardAppendSearch(ch) => {
                self.query.push(ch);
                self.sync_search(window, cx);
            }
            ChromeCommand::RouteDigit { digit, .. } => {
                // Favorites web page always treats digits as search characters.
                self.query.push(char::from(b'0' + digit));
                self.sync_search(window, cx);
            }
            ChromeCommand::ClipboardSearchBackspace => {
                use unicode_segmentation::UnicodeSegmentation;
                if let Some((index, _)) = self.query.grapheme_indices(true).next_back() {
                    self.query.truncate(index);
                }
                self.sync_search(window, cx);
            }
            ChromeCommand::ClipboardNavKey { key, .. } => match key {
                "ArrowDown" | "Down" => self.navigate(1, false, cx),
                "ArrowUp" | "Up" => self.navigate(-1, false, cx),
                "Home" => self.navigate(-1, true, cx),
                "End" => self.navigate(1, true, cx),
                "ArrowLeft" | "Left" => self.tree_horizontal(-1, cx),
                "ArrowRight" | "Right" => self.tree_horizontal(1, cx),
                "Enter" => self.execute_selected(Some(task), cx),
                "Delete" => self.delete_selected(window, cx),
                _ => {}
            },
            ChromeCommand::ClipboardSwitchTab { reverse } => {
                self.cycle_category(if reverse { -1 } else { 1 }, cx)
            }
            ChromeCommand::ClipboardHandleEscape => {
                if !self.search.read(cx).is_composing() {
                    self.host.clipboard_escape_unhandled(
                        Some(&task.escape_session_token()),
                        task.escape_input_token().as_deref(),
                    );
                }
            }
            _ => return false,
        }
        true
    }

    fn sync_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search.update(cx, |input, cx| {
            input.set_text(self.query.clone(), window, cx)
        });
        self.refresh(cx);
    }

    fn cycle_category(&mut self, direction: i32, cx: &mut Context<Self>) {
        let index = Category::FILTERS
            .iter()
            .position(|item| *item == self.category)
            .unwrap_or(0) as i32;
        self.category = Category::FILTERS
            [(index + direction).rem_euclid(Category::FILTERS.len() as i32) as usize];
        self.rebuild_visible();
        cx.notify();
    }

    fn navigate(&mut self, direction: i32, edge: bool, cx: &mut Context<Self>) {
        if self.visible.is_empty() {
            return;
        }
        let current = self
            .visible
            .iter()
            .enumerate()
            .position(|(index, item)| {
                if self.memory.layout.is_tree() {
                    self.tree_rows
                        .get(index)
                        .is_some_and(|row| row.path == self.selected_path)
                } else {
                    item.id == self.selected
                }
            })
            .unwrap_or(0) as i32;
        let index = if edge {
            if direction < 0 {
                0
            } else {
                self.visible.len() - 1
            }
        } else {
            (current + direction).clamp(0, self.visible.len() as i32 - 1) as usize
        };
        self.select_occurrence(index);
        self.collapse_drag_selection(index);
        self.list.scroll_to_reveal_item(index);
        if index + 10 >= self.visible.len() {
            self.load_more(cx);
        }
        self.save_view_memory(cx);
        cx.notify();
    }

    fn execute_selected(&mut self, task: Option<ChromeTask>, cx: &mut Context<Self>) {
        self.execute_item(self.selected.clone(), task, cx);
    }

    fn execute_item(&mut self, id: String, task: Option<ChromeTask>, cx: &mut Context<Self>) {
        if self.busy || !self.active {
            return;
        }
        let Some(item) = self.visible.iter().find(|item| item.id == id).cloned() else {
            return;
        };
        if !self.tree.items.contains_key(&id) {
            return;
        }
        if item.is_folder {
            if self.memory.layout.is_tree() {
                if self
                    .tree_rows
                    .iter()
                    .any(|row| row.path == self.selected_path && row.has_children)
                {
                    self.toggle_tree_path(self.selected_path.clone(), cx);
                }
                return;
            }
            cx.emit(FavoritesEvent::OpenPanel(item.id));
            return;
        }
        let request = match self.host.capture_native_paste(task) {
            Ok(request) => request,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        self.run(false, cx, move |host| {
            host.favor_paste_native(&json!({"id":item.id}), &request)
        });
    }

    fn copy_item(&mut self, id: String, task: Option<ChromeTask>, cx: &mut Context<Self>) {
        if self.busy
            || !self.active
            || !self
                .visible
                .iter()
                .any(|item| item.id == id && !item.is_folder)
        {
            return;
        }
        // Capture the clipboard-root lifetime before any slow database/image
        // preparation. Copy works without an external input target; core checks
        // this ticket again on its actual clipboard-writing owner thread.
        let request = match self.host.capture_native_clipboard_action(task) {
            Ok(request) => request,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        self.run(false, cx, move |host| {
            host.favor_copy_favorites_native(&json!({"id":id}), &request)
        });
    }

    fn hover_action(
        &mut self,
        id: String,
        command: HoverCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.active || self.busy || self.dialog.is_some() {
            return;
        }
        if !self.visible.iter().any(|item| item.id == id) {
            return;
        }
        // The clicked row ID is the command target. In particular, hover Copy
        // never mutates selection before dispatching another row's action.
        match command {
            HoverCommand::Paste => self.execute_item(id, None, cx),
            HoverCommand::Copy => self.copy_item(id, None, cx),
            HoverCommand::Edit => cx.emit(FavoritesEvent::Edit(id)),
            HoverCommand::Delete => self.delete(id, window, cx),
            _ => {}
        }
    }

    fn run(
        &mut self,
        refresh: bool,
        cx: &mut Context<Self>,
        action: impl FnOnce(Arc<HostState>) -> Result<Value, String> + Send + 'static,
    ) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.error = None;
        let (host, generation) = (self.host.clone(), self.generation);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { action(host) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                if !this.active {
                    return;
                }
                match result {
                    Ok(_) if refresh => this.refresh(cx),
                    Ok(_) => {}
                    Err(error) if generation == this.generation => this.error = Some(error),
                    Err(_) => {}
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn open_dialog(
        &mut self,
        dialog: Dialog,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.active {
            return;
        }
        self.cancel_native_hover();
        self.close_menu(cx);
        self.dialog_generation += 1;
        self.set_dialog_read_only(false, cx);
        self.dialog = Some(dialog);
        self.error = None;
        self.host.apply_window_action("setSpaModalOpen", true);
        self.name
            .update(cx, |input, cx| input.set_text(text, window, cx));
        self.body
            .update(cx, |input, cx| input.set_text(String::new(), window, cx));
        let scope = (self.native_session, self.dialog_generation);
        cx.defer_in(window, move |this, window, cx| {
            if !this.active
                || this.dialog.is_none()
                || scope != (this.native_session, this.dialog_generation)
            {
                return;
            }
            match NativeWindow::request_search_focus(window, &this.host) {
                Ok(()) => {
                    if matches!(
                        this.dialog,
                        Some(Dialog::AddText { .. } | Dialog::Collect { .. })
                    ) {
                        this.body.read(cx).focus_handle().focus(window);
                    } else {
                        this.name.read(cx).focus_handle().focus(window);
                    }
                }
                Err(error) => this.error = Some(error),
            }
            cx.notify();
        });
        cx.notify();
    }

    fn close_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        if let Some(Dialog::Collect { draft_id, .. }) = &self.dialog {
            let host = self.host.clone();
            let id = draft_id.clone();
            cx.background_executor()
                .spawn(async move {
                    let _ = host.collect_discard(&json!({"draftId":id}));
                })
                .detach();
        }
        self.dialog = None;
        self.dialog_generation += 1;
        self.host.apply_window_action("setSpaModalOpen", false);
        self.focus.focus(window);
        self.restore_browsing(window);
        cx.notify();
    }

    fn rename(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let host = self.host.clone();
        self.dialog_generation += 1;
        let generation = self.dialog_generation;
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { host.favor_get(&json!({"id":id})) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                if !this.active || this.dialog_generation != generation {
                    return;
                }
                match result {
                    Ok(detail) => {
                        let id = detail["id"].as_str().unwrap_or_default().to_string();
                        let revision = detail["revision"].as_str().unwrap_or_default().to_string();
                        let text = detail["title"].as_str().unwrap_or_default().to_string();
                        this.open_dialog(Dialog::Rename { id, revision }, text, window, cx);
                    }
                    Err(error) => {
                        this.error = Some(error);
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    fn open_collect(&mut self, draft_id: String, window: &mut Window, cx: &mut Context<Self>) {
        self.dialog_generation += 1;
        let generation = self.dialog_generation;
        let host = self.host.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    host.collect_get(&json!({"draftId":draft_id}))
                        .map(|bundle| (draft_id, bundle))
                })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                if !this.active || this.dialog_generation != generation {
                    return;
                }
                match result {
                    Ok((draft_id, bundle)) => {
                        let folders: Vec<_> = bundle["folderChoices"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|row| {
                                Some((
                                    row["id"].as_str()?.to_string(),
                                    row["title"].as_str().unwrap_or_default().to_string(),
                                ))
                            })
                            .collect();
                        let detail = &bundle["detail"];
                        let initial = detail["parentId"].as_str().unwrap_or_default();
                        let parent_id = folders
                            .iter()
                            .find(|(id, _)| id == initial)
                            .or_else(|| folders.first())
                            .map(|(id, _)| id.clone())
                            .unwrap_or_default();
                        let text = detail["title"].as_str().unwrap_or_default().to_string();
                        let content = detail["bodyText"]
                            .as_str()
                            .or_else(|| detail["summary"].as_str())
                            .unwrap_or_default()
                            .to_string();
                        this.open_dialog(
                            Dialog::Collect {
                                draft_id,
                                parent_id,
                                folders,
                            },
                            text,
                            window,
                            cx,
                        );
                        this.body
                            .update(cx, |input, cx| input.set_text(content, window, cx));
                    }
                    Err(error) => {
                        this.error = Some(error);
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    fn delete_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.selected.clone();
        self.delete(id, window, cx);
    }

    fn delete(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.tree.items.get(&id) else {
            return;
        };
        let descendants = self.tree.descendant_ids(&id).len().saturating_sub(1);
        if item.is_folder && descendants > 0 {
            self.open_dialog(
                Dialog::Delete {
                    id,
                    name: title(item),
                    descendants,
                },
                String::new(),
                window,
                cx,
            );
        } else {
            self.run(true, cx, move |host| host.favor_delete(&json!({"id":id})));
        }
    }

    fn submit_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.name.read(cx).is_composing() || self.body.read(cx).is_composing() {
            return;
        }
        let parent = self.folder.clone();
        let name = self.name.read(cx).text().trim().to_string();
        let body = self.body.read(cx).text().trim().to_string();
        let action: Box<dyn FnOnce(Arc<HostState>) -> Result<Value, String> + Send> = match &self
            .dialog
        {
            Some(Dialog::CreateFolder) => {
                if name.is_empty() {
                    self.error = Some(tr("请输入收藏夹名称", "Enter a folder name").into());
                    cx.notify();
                    return;
                }
                Box::new(move |host| {
                    host.favor_folder_create(&json!({"parentId":parent,"title":name}))
                })
            }
            Some(Dialog::Rename { id, revision }) => {
                let (id, revision) = (id.clone(), revision.clone());
                Box::new(move |host| {
                    host.favor_update(&json!({"id":id,"title":name,"expectedRevision":revision}))
                })
            }
            Some(Dialog::AddText { line_by_line }) => {
                if body.is_empty() {
                    self.error =
                        Some(tr("请输入要收藏的文本", "Enter text to add to favorites").into());
                    cx.notify();
                    return;
                }
                let line_by_line = *line_by_line;
                Box::new(move |host| {
                    if line_by_line {
                        host.favor_add_text_items(
                            &json!({"parentId":parent,"text":body,"lineByLine":true}),
                        )
                    } else {
                        add_text_to_history_and_favorites(&host, &body)
                    }
                })
            }
            Some(Dialog::Collect {
                draft_id,
                parent_id,
                ..
            }) => {
                let (draft_id, parent_id, content) = (
                    draft_id.clone(),
                    parent_id.clone(),
                    self.body.read(cx).text().to_string(),
                );
                Box::new(move |host| {
                    host.collect_commit(&json!({"draftId":draft_id,"parentId":parent_id,"title":name,"content":content,"insertIndex":0}))
                })
            }
            Some(Dialog::Delete { id, .. }) => {
                let id = id.clone();
                Box::new(move |host| host.favor_delete(&json!({"id":id})))
            }
            _ => return,
        };
        let host = self.host.clone();
        let generation = self.dialog_generation;
        self.busy = true;
        self.set_dialog_read_only(true, cx);
        self.error = None;
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { action(host) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = false;
                if !this.active || this.dialog_generation != generation {
                    return;
                }
                this.set_dialog_read_only(false, cx);
                match result {
                    Ok(value) if value.get("ok").and_then(Value::as_bool) == Some(false) => {
                        this.error = Some(tr("未能添加文本", "Could not add text").into());
                    }
                    Ok(_) => {
                        this.close_dialog(window, cx);
                        this.refresh(cx);
                    }
                    Err(error) => this.error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn set_dialog_read_only(&mut self, read_only: bool, cx: &mut Context<Self>) {
        self.name
            .update(cx, |input, cx| input.set_read_only(read_only, cx));
        self.body
            .update(cx, |input, cx| input.set_read_only(read_only, cx));
    }

    fn move_to(&mut self, target: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Dialog::Move { id }) = &self.dialog else {
            return;
        };
        let (id, host, generation) = (id.clone(), self.host.clone(), self.dialog_generation);
        if self.busy {
            return;
        }
        self.busy = true;
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(
                    async move { host.panel_move(&json!({"itemId":id,"targetFolderId":target})) },
                )
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = false;
                if !this.active || this.dialog_generation != generation {
                    return;
                }
                match result {
                    Ok(_) => {
                        this.close_dialog(window, cx);
                        this.refresh(cx);
                    }
                    Err(error) => this.error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn close_menu(&mut self, cx: &mut Context<Self>) {
        #[cfg(windows)]
        if let Some(shared) = &self.interactions {
            if let Some(native) = shared.borrow().as_ref() {
                let _ = native.set_menu_open(false);
            }
        }
        if let Some((lease, _)) = self.menu_lease.take() {
            self.host.release_native_context_menu(lease);
        }
        if let Some(menu) = self.menu {
            ContextMenuPopup::hide(menu, cx);
        }
    }

    fn open_menu(&mut self, id: String, entries: Vec<MenuEntry>, cx: &mut Context<Self>) {
        if self.search.read(cx).is_composing() || self.dialog.is_some() {
            return;
        }
        let Some(point) = platform::cursor_position() else {
            return;
        };
        self.close_menu(cx);
        let lease = crate::menu::next_lease();
        if !self.host.acquire_native_context_menu(lease, "clipboard") {
            return;
        }
        self.cancel_native_hover();
        #[cfg(windows)]
        if let Some(shared) = &self.interactions {
            if let Some(native) = shared.borrow().as_ref() {
                let _ = native.set_menu_open(true);
            }
        }
        self.menu_lease = Some((lease, id));
        let view = cx.weak_entity();
        let on_select: SelectHandler = Rc::new(move |lease, key, cx| {
            let _ = view.update(cx, |this, cx| this.menu_selected(lease, key, cx));
        });
        let view = cx.weak_entity();
        let host = self.host.clone();
        let tokens = self.tokens.clone();
        let dark = self.dark;
        cx.defer(move |cx| {
            let _ = view.update(cx, |this, cx| {
                if this.menu_lease.as_ref().map(|(id, _)| *id) != Some(lease)
                    || !host.native_context_menu_is_current(lease)
                {
                    return;
                }
                if let Err(error) = ContextMenuPopup::show(
                    &mut this.menu,
                    &host,
                    host.host_hwnd(),
                    lease,
                    point,
                    entries,
                    tokens,
                    dark,
                    on_select,
                    cx,
                ) {
                    host.release_native_context_menu(lease);
                    this.menu_lease = None;
                    this.error = Some(error);
                    cx.notify();
                }
            });
        });
    }

    fn row_menu(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(item) = self.tree.items.get(&id) else {
            return;
        };
        if self.selected_path.last() != Some(&id) {
            self.selected_path = self.canonical_path(&id);
        }
        self.selected = id.clone();
        let mut entries = Vec::new();
        if !item.is_folder {
            entries.push(MenuAction::tile("paste", tr("粘贴", "Paste")));
            entries.push(MenuAction::tile("copy-only", tr("复制", "Copy")));
        }
        entries.extend([
            MenuAction::tile("edit", tr("编辑", "Edit")),
            MenuAction::danger_tile("delete", tr("移除", "Remove")),
        ]);
        entries.push(MenuAction::row("rename", tr("重命名", "Rename")));
        entries.push(MenuAction::row("move", tr("移动到…", "Move to…")));
        if item.is_folder {
            entries.push(MenuAction::row(
                "open-panel",
                tr("在超级面板中打开", "Open in panel"),
            ));
        }
        if !item.file_paths.is_empty() {
            entries.push(MenuAction::tile(
                "reveal-file",
                tr("定位文件", "Reveal file"),
            ));
            entries.push(MenuAction::tile(
                "reveal-folder",
                tr("定位文件夹", "Reveal folder"),
            ));
            entries.push(MenuAction::row("open-file", tr("打开文件", "Open file")));
        }
        if self.has_drag_undo() {
            entries.push(MenuAction::row("undo-drag", tr("撤销拖拽", "Undo drag")));
        }
        self.open_menu(id, entries, cx);
        cx.notify();
    }

    fn add_menu(&mut self, cx: &mut Context<Self>) {
        let mut entries =
            // Grouped by how often each is used; 添加动作 is rare and goes last.
            vec![
                MenuAction::row("add-clipboard", tr("从剪贴板收藏", "Add from clipboard")),
                // One entry; the dialog's 「每行一条」 switch covers batch adding.
                MenuAction::row("add-text", tr("添加文本", "Add text")),
                MenuEntry::Separator,
                MenuAction::row("add-files", tr("添加文件", "Add files")),
                MenuAction::row("add-directories", tr("添加文件夹", "Add directories")),
                MenuAction::row(
                    "add-link",
                    tr("添加当前打开网址", "Add current browser URL"),
                ),
                MenuEntry::Separator,
                MenuAction::row("new-folder", tr("新建收藏夹", "New favorite folder")),
                MenuEntry::Separator,
                MenuAction::row("add-action", tr("添加动作", "Add action")),
            ];
        if self.has_drag_undo() {
            entries.extend([MenuEntry::Separator, MenuAction::row("undo-drag", tr("撤销拖拽", "Undo drag"))]);
        }
        self.open_menu(String::new(), entries, cx);
    }

    fn menu_selected(&mut self, lease: u64, key: &'static str, cx: &mut Context<Self>) {
        let Some((_, id)) = self.menu_lease.take_if(|(current, _)| *current == lease) else {
            return;
        };
        if let Some(menu) = self.menu {
            ContextMenuPopup::hide(menu, cx);
        }
        if !self.host.release_native_context_menu(lease) || !self.active {
            return;
        }
        match key {
            "undo-drag" => self.undo_content_drag(cx),
            "layout-list" => self.choose_layout(LayoutMode::SidebarList, cx),
            "layout-tree" => self.choose_layout(LayoutMode::Tree, cx),
            "layout-sidebar-tree" => self.choose_layout(LayoutMode::SidebarTree, cx),
            "layout-pure-list" => self.choose_layout(LayoutMode::List, cx),
            "paste" => {
                self.execute_item(id, None, cx);
            }
            "copy-only" => self.copy_item(id, None, cx),
            "edit" => cx.emit(FavoritesEvent::Edit(id)),
            "open-panel" => cx.emit(FavoritesEvent::OpenPanel(id)),
            "add-action" => {
                self.error = Some(
                    tr(
                        "请从 Quicker 面板上拖拽添加动作，或在超级面板中管理",
                        "Drag an action from Quicker or manage it in the panel",
                    )
                    .into(),
                );
                cx.emit(FavoritesEvent::OpenPanel(String::new()));
                cx.notify();
            }
            "delete" | "rename" | "move" | "new-folder" | "add-text" | "add-batch-text" => {
                let view = cx.weak_entity();
                let owner = self.window;
                let scope = (self.native_session, self.generation, self.dialog_generation);
                cx.defer(move |cx| {
                    let _ = owner.update(cx, |_, window, cx| {
                        let _ = view.update(cx, |this, cx| {
                            if !this.active
                                || scope
                                    != (
                                        this.native_session,
                                        this.generation,
                                        this.dialog_generation,
                                    )
                            {
                                return;
                            }
                            match key {
                                "delete" => this.delete(id, window, cx),
                                "rename" => this.rename(id, window, cx),
                                "move" => {
                                    this.open_dialog(Dialog::Move { id }, String::new(), window, cx)
                                }
                                "new-folder" => this.open_dialog(
                                    Dialog::CreateFolder,
                                    String::new(),
                                    window,
                                    cx,
                                ),
                                _ => this.open_dialog(
                                    Dialog::AddText {
                                        line_by_line: key == "add-batch-text",
                                    },
                                    String::new(),
                                    window,
                                    cx,
                                ),
                            }
                        });
                    });
                });
            }
            "add-clipboard" => {
                let parent = self.folder.clone();
                self.run(true, cx, move |host| {
                    host.favor_from_clipboard(&json!({"parentId":parent}))
                });
            }
            "add-link" => {
                let parent = self.folder.clone();
                self.run(true, cx, move |host| {
                    host.favor_add_link(&json!({"parentId":parent}))
                });
            }
            "add-files" | "add-directories" => {
                if self.busy {
                    return;
                }
                self.host.apply_window_action("setSpaModalOpen", true);
                let parent = self.folder.clone();
                self.run(true, cx, move |host| {
                    let result = (|| {
                        let pick = crate::native_dialog::run_blocking(|| {
                            if key == "add-files" {
                                host.pick_files(&json!({}))
                            } else {
                                host.pick_directories(&json!({}))
                            }
                        })?;
                        if pick["cancelled"].as_bool() == Some(true) {
                            return Ok(json!({"ok":true}));
                        }
                        host.favor_add_paths(&json!({"parentId":parent,"paths":pick["paths"]}))
                    })();
                    host.apply_window_action("setSpaModalOpen", false);
                    result
                });
            }
            "reveal-file" | "reveal-folder" | "open-file" => {
                let Some(path) = self
                    .tree
                    .items
                    .get(&id)
                    .and_then(|item| item.file_paths.first())
                    .cloned()
                else {
                    return;
                };
                let path = if key == "reveal-folder" {
                    std::path::Path::new(&path)
                        .parent()
                        .map(|parent| parent.to_string_lossy().to_string())
                        .unwrap_or(path)
                } else {
                    path
                };
                self.run(false, cx, move |host| {
                    if key == "open-file" {
                        host.shell_open_path(&json!({"path":path}))
                    } else {
                        host.shell_reveal(&json!({"path":path}))
                    }
                });
            }
            _ => {}
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.key == "escape" && self.drag_ui.active() {
            favorite_drag::cancel_active(window, cx);
            self.cancel_content_drag(cx);
            cx.stop_propagation();
            return;
        }
        if self.dialog.is_some() {
            return;
        }
        if self.display_panel {
            if event.keystroke.key == "escape" && !event.is_held {
                self.display_panel = false;
                cx.stop_propagation();
                cx.notify();
            }
            return;
        }
        let search_focused = self.search.read(cx).focus_handle().is_focused(window);
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;
        if key == "tab"
            && !event.is_held
            && (self.focus.is_focused(window) || search_focused)
            && !self.search.read(cx).is_composing()
            && modifiers.number_of_modifiers() == u8::from(modifiers.shift)
        {
            cx.emit(FavoritesEvent::SwitchPrimaryTab);
            cx.stop_propagation();
            return;
        }
        if search_focused {
            return;
        }
        if event.keystroke.modifiers.control && key == "z" && self.has_drag_undo() {
            self.undo_content_drag(cx);
            cx.stop_propagation();
            return;
        }
        if event.is_held && matches!(event.keystroke.key.as_str(), "enter" | "delete") {
            return;
        }
        if event.keystroke.modifiers.control && key == "f" {
            self.focus_search(window, cx);
            cx.stop_propagation();
            return;
        }
        if event.keystroke.modifiers.control && key == "c" {
            self.copy_item(self.selected.clone(), None, cx);
            cx.stop_propagation();
            return;
        }
        if event.keystroke.modifiers.number_of_modifiers() != 0 {
            return;
        }
        match key {
            "down" => self.navigate(1, false, cx),
            "up" => self.navigate(-1, false, cx),
            "home" => self.navigate(-1, true, cx),
            "end" => self.navigate(1, true, cx),
            "left" => self.tree_horizontal(-1, cx),
            "right" => self.tree_horizontal(1, cx),
            "enter" => self.execute_selected(None, cx),
            "delete" => self.delete_selected(window, cx),
            "escape" => {
                self.host.window_hide();
            }
            _ => return,
        }
        cx.stop_propagation();
    }

    /// Row metrics for favorites: one content line in 简约, the shared
    /// multi-line clamp in 详细.
    fn favorite_metrics(&self) -> appearance::RowMetrics {
        let mut metrics = favorite_row_settings(&self.appearance).row_metrics();
        if !self.memory.display.detailed {
            metrics.text_max_lines = 1;
        }
        metrics
    }

    fn set_display(&mut self, edit: impl FnOnce(&mut FavoriteDisplay), cx: &mut Context<Self>) {
        edit(&mut self.memory.display);
        // Row heights change with the line count.
        crate::history_scroll::remeasure(&self.list);
        self.save_view_memory(cx);
        cx.notify();
    }

    /// 「列表显示设置」: style cards, trailing-detail pills and hover-key toggles.
    fn display_panel_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let tokens = &self.tokens;
        let t = tokens.theme(self.dark);
        let l = &tokens.layout;
        let display = self.memory.display.clone();
        let neutral = color(&t.neutral_active);
        let section = |text: &'static str| {
            rsx! {
                <div mt={px(8.0)} mb={px(6.0)} text_size={px(11.0)}
                    text_color={color(&t.text_muted)}>
                    {text}
                </div>
            }
        };
        let bar = |width: f32, alpha: f32| {
            rsx! {
                <div h={px(3.0)} w={relative(width)} rounded={px(2.0)}
                    bg={color(&t.text).opacity(alpha)} />
            }
        };
        let style_card = |id: &'static str, detailed: bool, label: &'static str| {
            let active = display.detailed == detailed;
            let hover = color(&t.hover);
            let sketch_row = || {
                rsx! {
                    <div flex items_center gap={px(4.0)}>
                        <div size={px(6.0)} flex_none rounded={px(2.0)}
                            bg={color(&t.text).opacity(0.35)} />
                    </div>
                }
                    .child(
                        rsx! {
                            <div flex_1 flex flex_col gap={px(2.0)}>
                                {bar(0.8, 0.45)}
                            </div>
                        }
                            .when(detailed, |lines| lines.child(bar(0.55, 0.2))),
                    )
            };
            rsx! {
                <div id={id} flex_1 flex flex_col gap={px(6.0)} p={px(6.0)} rounded={px(8.0)}
                    border_1 />
            }
                .border_color(color(if active { &t.text_secondary } else { &t.border }))
                .text_size(px(12.0))
                .text_color(color(if active { &t.text } else { &t.text_secondary }))
                .cursor_pointer()
                .hover(move |style| style.bg(hover))
                .child(
                    rsx! {
                        <div flex flex_col gap={px(4.0)} p={px(6.0)} rounded={px(5.0)}
                            bg={color(&t.secondary)}>
                            {sketch_row()}
                            {sketch_row()}
                        </div>
                    },
                )
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_display(|display| display.detailed = detailed, cx)
                }))
        };
        let pill = |id: &'static str, on: bool, label: &'static str| {
            rsx! {
                <div id={id} h={px(26.0)} px={px(10.0)} flex items_center rounded={px(13.0)}
                    text_size={px(12.0)} cursor_pointer />
            }
                .when(on, |pill| pill.bg(neutral).text_color(color(&t.text)))
                .when(!on, |pill| {
                    pill.border_1()
                        .border_color(color(&t.border))
                        .text_color(color(&t.text_secondary))
                })
                .child(label)
        };
        let hover_key = |action: HoverAction, icon: &'static str, label: &'static str| {
            let on = display.hover.iter().any(|key| key == action.as_str());
            rsx! {
                <div id={SharedString::from(format!("favorite-hover-{}", action.as_str()))}
                    w={px(32.0)} h={px(28.0)} flex items_center justify_center rounded={px(6.0)}
                    cursor_pointer />
            }
                .when(on, |key| key.bg(neutral))
                .when(!on, |key| key.border_1().border_color(color(&t.border)).opacity(0.7))
                .child(
                    svg()
                        .path(tokens.icons.get(icon).cloned().unwrap_or_default())
                        .size(px(l.icon_size))
                        .text_color(color(if on { &t.text } else { &t.text_muted })),
                )
                .tooltip(favorite_tooltip(
                    format!("{}（{}）", label, if on { tr("显示", "shown") } else { tr("隐藏", "hidden") }),
                    tokens.clone(),
                    self.dark,
                ))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_display(
                        |display| {
                            let key = action.as_str().to_string();
                            if on {
                                display.hover.retain(|item| *item != key);
                            } else {
                                display.hover.push(key);
                            }
                        },
                        cx,
                    )
                }))
        };
        let primary_icon = if matches!(self.execute_mode, 2 | 3) { "paste" } else { "copy" };
        let card = rsx! {
            <div id={"favorite-display-panel"} absolute right={px(l.content_padding_x + 2.0)}
                bottom={px(l.footer_height + 2.0)} w={px(220.0)} px={px(10.0)} pt={px(2.0)}
                pb={px(10.0)} flex flex_col bg={color(&t.surface)} border_1
                border_color={color(&t.border)} rounded={px(8.0)} shadow_lg occlude />
        }
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(section(tr("样式", "Style")))
            .child(
                rsx! {
                    <div flex gap={px(6.0)}>
                        {style_card("favorite-style-simple", false, tr("简约", "Simple"))}
                        {style_card("favorite-style-detailed", true, tr("详细", "Detailed"))}
                    </div>
                },
            )
            .child(section(tr("行尾信息", "Trailing details")))
            .child(
                div()
                    .flex()
                    .gap(px(6.0))
                    .child(pill("favorite-show-time", display.show_time, tr("更新时间", "Updated")).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.set_display(|display| display.show_time = !display.show_time, cx)
                        }),
                    ))
                    .child(pill("favorite-show-count", display.show_count, tr("子项数", "Item count")).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.set_display(|display| display.show_count = !display.show_count, cx)
                        }),
                    )),
            )
            .child(section(tr("悬停按键", "Hover keys")))
            .child(
                rsx! {
                    <div flex gap={px(6.0)}>
                        {hover_key(HoverAction::Primary, primary_icon, tr("复制/粘贴", "Copy/paste"))}
                        {hover_key(HoverAction::Edit, "edit", tr("编辑", "Edit"))}
                        {hover_key(HoverAction::Delete, "delete", tr("删除", "Delete"))}
                    </div>
                },
            );
        div()
            .id("favorite-display-backdrop")
            .absolute()
            .inset_0()
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.display_panel = false;
                    cx.notify();
                }),
            )
            .child(card)
            .into_any_element()
    }

    fn open_file_button(
        &self,
        item: &FavorItemDto,
        path: String,
        index: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = item.id.clone();
        let generation = self.generation;
        let session = self.native_session;
        let key = preview_key(item);
        let theme = self.tokens.theme(self.dark);
        rsx! {
            <div id={SharedString::from(format!(
                "favorite-file-open-{}-{index}",
                item.id
            ))}
                flex_none text_size={px(11.0)} text_color={color(&theme.accent)} cursor_pointer>
                {tr("打开", "Open")}
            </div>
        }
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.open_content(
                    ContentOpenRequest {
                        item_id: id.clone(),
                        target: OpenTarget::ShellPath(path.clone()),
                    },
                    generation,
                    session,
                    &key,
                    cx,
                );
            }))
            .into_any_element()
    }

    fn content_open_handler(&self, item: &FavorItemDto, cx: &mut Context<Self>) -> OpenHandler {
        let owner = cx.weak_entity();
        let generation = self.generation;
        let session = self.native_session;
        let key = preview_key(item);
        Rc::new(move |request, _, cx| {
            let _ = owner.update(cx, |this, cx| {
                this.open_content(request, generation, session, &key, cx);
            });
        })
    }

    fn open_content(
        &mut self,
        request: ContentOpenRequest,
        generation: u64,
        session: u64,
        key: &str,
        cx: &mut Context<Self>,
    ) {
        if !self.active
            || !self.native_owner_enabled
            || self.loading
            || self.busy
            || self.dialog.is_some()
            || self.menu_lease.is_some()
            || self.native_dragging
            || self.content_drag_blocks_click()
            || self
                .native_suppress_click
                .is_some_and(|until| Instant::now() < until)
            || self.generation != generation
            || self.native_session != session
            || !self
                .visible
                .iter()
                .any(|item| item.id == request.item_id && preview_key(item) == key)
        {
            return;
        }
        let ticket = match self.host.capture_native_clipboard_action(None) {
            Ok(ticket) => ticket,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        // The accepted original ticket travels to core's actual Shell boundary;
        // leaving/reopening cannot manufacture a fresh ticket on the worker.
        self.run(false, cx, move |host| match request.target {
            OpenTarget::Http(uri) => host.shell_open_uri_native(&json!({"uri":uri}), &ticket),
            OpenTarget::ShellPath(path) => {
                host.shell_open_path_native(&json!({"path":path}), &ticket)
            }
        });
    }

    /// Before a (re)load completes, reserve the remembered display size so the
    /// row does not jump when the raster arrives. Unknown sizes keep the
    /// compact text placeholder.
    fn image_placeholder(
        &self,
        item: &FavorItemDto,
        message: &str,
        completed: bool,
        width: f32,
        max_height: f32,
        raster_scale: f32,
    ) -> AnyElement {
        let muted = color(&self.tokens.theme(self.dark).text_muted);
        match self.image_sizes.get(&item.id).filter(|_| !completed) {
            Some(&(natural_width, natural_height)) => {
                let layout = crate::raster_layout::natural_image_layout(
                    natural_width, natural_height, width, max_height, raster_scale,
                );
                div().w(px(layout.width_dip)).h(px(layout.height_dip)).max_w_full()
                    .overflow_hidden().text_color(muted).child(message.to_owned())
                    .into_any_element()
            }
            None => div().min_h(px(48.0)).text_color(muted).child(message.to_owned()).into_any_element(),
        }
    }

    fn row_content(
        &self,
        item: &FavorItemDto,
        width: f32,
        raster_scale: f32,
        hover_binding: Option<hover_binding::HoverBinding>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = self.tokens.theme(self.dark);
        let metrics = self.favorite_metrics();
        let detailed = self.memory.display.detailed;
        let file_projection = (!item.is_folder && category(item) == Category::File)
            .then(|| favorite_file_projection(item));
        let display = favorite_display_text(item);
        let resource_projection = favorite_resource_projection(item);
        let loaded = resource_projection.as_ref().and_then(|projection| {
            self.content_cache.get(
                self.generation,
                &item.id,
                projection,
                ContentMode::Standard,
                &theme.text,
            )
        });
        let empty = ContentResources::default();
        let resources = loaded.unwrap_or(&empty);
        let single_image_entry = file_projection.as_ref().and_then(|projection| {
            (projection.total_entries == 1 && projection.entries.len() == 1)
                .then(|| &projection.entries[0])
                .filter(|entry| {
                    entry.is_image
                        && !resources
                            .paths
                            .get(&entry.path)
                            .is_some_and(|info| info.exists == Some(false))
                })
        });
        let is_image_file = single_image_entry.is_some();
        let text_metrics = ContentMetrics {
            width,
            font_size: self.tokens.typography.font_size,
            line_height: metrics.text_line_height,
            max_lines: metrics.text_max_lines,
        };
        let max_height = self.appearance.image_max_height_px as f32;
        let mut content = rsx! {
            <div relative min_w_0 min_h={px(20.0)} flex flex_col gap={px(metrics.content_gap)} />
        };
        if item.is_folder {
            content = content.child(native_content::render_content(
                &item.id,
                &favorite_text_projection(item),
                resources,
                text_metrics,
                &self.tokens,
                self.dark,
                self.content_open_handler(item, cx),
            ));
        } else if category(item) == Category::Image {
            content = if let Some(bitmap) = resources
                .images
                .get(PRIMARY_IMAGE_RESOURCE)
                .and_then(Option::as_ref)
            {
                content.child(hover_binding::wrap_hover_thumbnail(
                    bitmap_element(bitmap, width, max_height, raster_scale),
                    hover_binding.clone(),
                    crate::hover::HoverThumbnailTarget::ItemImage,
                    bitmap.source(),
                ))
            } else {
                let message = if loaded.is_some() {
                    tr("图片不可用", "Image unavailable")
                } else {
                    tr("图片加载中…", "Loading image…")
                };
                content.child(self.image_placeholder(item, message, loaded.is_some(), width, max_height, raster_scale))
            };
        } else if let Some(entry) = single_image_entry {
            content =
                if let Some(bitmap) = resources.images.get(&entry.path).and_then(Option::as_ref) {
                    content.child(hover_binding::wrap_hover_thumbnail(
                        bitmap_element(bitmap, width, max_height, raster_scale),
                        hover_binding.clone(),
                        crate::hover::HoverThumbnailTarget::FileImage(entry.original_index),
                        bitmap.source(),
                    ))
                } else {
                    let message = if loaded.is_some() {
                        tr("图片不可用", "Image unavailable")
                    } else {
                        tr("图片加载中…", "Loading image…")
                    };
                    content.child(self.image_placeholder(item, message, loaded.is_some(), width, max_height, raster_scale))
                };
            content = content.child(
                rsx! {
                    <div flex items_center gap_2>
                        <div flex_1 min_w_0 text_size={px(11.0)} text_ellipsis>
                            {entry.name.clone()}
                        </div>
                        {self.open_file_button(
                        item,
                        entry.path.clone(),
                        entry.original_index,
                        cx,
                    )}
                    </div>
                },
            );
        } else if let Some(projection) = file_projection.as_ref() {
            let file_wrapper: Option<native_content::HistoryThumbnailWrapper> =
                hover_binding.as_ref().map(|binding| {
                    let binding = binding.clone();
                    Rc::new(move |target, _, child| {
                        let target = match target {
                            native_content::HistoryThumbnailTarget::FileImage(index) =>
                                crate::hover::HoverThumbnailTarget::FileImage(index),
                            native_content::HistoryThumbnailTarget::FileMedia(index) =>
                                crate::hover::HoverThumbnailTarget::FileMedia(index),
                            native_content::HistoryThumbnailTarget::TextImage =>
                                crate::hover::HoverThumbnailTarget::TextImage,
                        };
                        hover_binding::wrap_hover_target(child, Some(binding.clone()), target)
                    }) as native_content::HistoryThumbnailWrapper
                });
            content = content.child(native_content::render_favorite_file_chips(
                &item.id,
                projection,
                resources,
                text_metrics,
                &self.tokens,
                self.dark,
                self.content_open_handler(item, cx),
                file_wrapper,
            ));
        } else {
            let source_indices =
                native_content::model::project_mixed_source_indices(&item.im_rich_segments);
            let thumbnail_wrapper: Option<native_content::ThumbnailWrapper> =
                hover_binding.as_ref().map(|binding| {
                    let binding = binding.clone();
                    Rc::new(move |original_index, image, child| {
                        hover_binding::wrap_hover_thumbnail(
                            child,
                            Some(binding.clone()),
                            crate::hover::HoverThumbnailTarget::ImImage(original_index),
                            gpui::ImageSource::Image(image),
                        )
                    }) as native_content::ThumbnailWrapper
                });
            content = content.child(native_content::render_content_with_bindings(
                &item.id,
                &favorite_text_projection(item),
                resources,
                text_metrics,
                &self.tokens,
                self.dark,
                self.content_open_handler(item, cx),
                None,
                &source_indices,
                thumbnail_wrapper,
            ));
        }
        if let Some(summary) = favorite_secondary_projection(item).filter(|_| detailed) {
            content = content.child(native_content::render_secondary_content(
                &item.id,
                &summary,
                width,
                self.appearance.density == crate::appearance::Density::Relaxed,
                &self.tokens,
                self.dark,
                self.content_open_handler(item, cx),
            ));
        }
        // Sizes are rarely read: only 详细 shows them; folder counts live in
        // the row's trailing details instead.
        let badge = if item.is_folder || !detailed {
            None
        } else if category(item) == Category::Text {
            Some(format!(
                "{} {}",
                item.text_length
                    .map(|length| length.max(0) as usize)
                    .unwrap_or_else(|| display.chars().count()),
                tr("字符", "chars")
            ))
        } else if category(item) == Category::File && !is_image_file {
            Some(format!(
                "{} {}",
                if item.file_count > 0 {
                    item.file_count as usize
                } else {
                    file_projection
                        .as_ref()
                        .map(|projection| projection.total_entries)
                        .unwrap_or_default()
                        .max(1)
                },
                tr("个文件", "files")
            ))
        } else {
            None
        };
        content
            .when_some(badge, |content, badge| {
                content.child(
                    rsx! {
                        <div absolute bottom_0 right_0 rounded={px(3.0)} px={px(3.0)}
                            text_size={px(10.0)} line_height={px(14.0)}
                            text_color={color(&theme.text_muted)}
                            bg={color(&theme.surface).opacity(0.9)}>
                            {badge}
                        </div>
                    },
                )
            })
            .into_any_element()
    }

    fn row(
        &mut self,
        index: usize,
        item: FavorItemDto,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tree_row = self
            .tree_rows
            .get(index)
            .filter(|_| self.memory.layout.is_tree())
            .cloned();
        let indent = tree_row
            .as_ref()
            .map(|row| row.depth as f32 * 16.0)
            .unwrap_or_default()
            .min((self.row_width - 80.0).max(0.0));
        if let Some(row) = tree_row.as_ref().filter(|row| row.is_missing) {
            // A broken Children edge: hovering offers removing just this edge
            // from its folder (the referenced item no longer exists).
            let theme = self.tokens.theme(self.dark);
            let group = SharedString::from(format!("favorite-missing-{}", self.occurrence_key(index)));
            let parent = row.path.iter().rev().nth(1).cloned();
            let child = row.item_id.clone();
            return div()
                .id(group.clone())
                .group(group.clone())
                .relative()
                .flex()
                .items_center()
                .pl(px(indent))
                .pr(px(8.0))
                .py_2()
                .text_color(color(&theme.text_muted))
                .child(rsx! {
                    <div flex_1 min_w_0>
                        {tr("失效的收藏引用", "Missing favorite reference")}
                    </div>
                })
                .when_some(parent, |row, parent| {
                    row.child(
                        rsx! {
                            <div id={SharedString::from(format!("{group}-remove"))} flex_none
                                px={px(6.0)} h={px(22.0)} flex items_center rounded={px(4.0)}
                                text_size={px(12.0)} text_color={color(&theme.text_secondary)}
                                cursor_pointer invisible />
                        }
                            .group_hover(group.clone(), |style| style.visible())
                            .hover(|style| style.bg(color(&theme.text).opacity(0.10)))
                            .child(tr("移除", "Remove"))
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                let params = json!({"parentId": parent, "childId": child});
                                this.run(true, cx, move |host| host.favor_remove_missing_reference(&params));
                            })),
                    )
                })
                .into_any_element();
        }
        let occurrence = self.occurrence_key(index);
        let occurrence_path = tree_row.as_ref().map(|row| row.path.clone());
        let click_occurrence_path = occurrence_path.clone();
        let (id, menu_id) = (item.id.clone(), item.id.clone());
        let is_folder = item.is_folder;
        let selected = if self.drag_has_selection() { self.drag_selected(index) } else {
            occurrence_path.as_ref().map_or(self.selected == id, |path| self.selected_path == *path)
        };
        let settings = favorite_row_settings(&self.appearance);
        let metrics = self.favorite_metrics();
        let metadata = RowMetadata::new(&item.source_process_name, &item.update_time.to_rfc3339());
        let group = SharedString::from(format!("favorite-group-{occurrence}"));
        let weak = cx.weak_entity();
        let generation = self.generation;
        let key = preview_key(&item);
        let handler: HoverHandler = Rc::new(move |invocation, window, cx| {
            let _ = weak.update(cx, |this, cx| {
                if this.generation != generation
                    || !this
                        .visible
                        .iter()
                        .any(|item| item.id == invocation.item_id && preview_key(item) == key)
                {
                    return;
                }
                this.hover_action(invocation.item_id, invocation.command, window, cx);
            });
        });
        let actions = appearance::render_hover_actions(
            &id,
            false,
            self.execute_mode,
            false,
            &favorite_hover_settings(&settings, is_folder, &self.memory.display.hover),
            &self.tokens,
            self.dark,
            group.clone(),
            self.row_width,
            false,
            false,
            handler,
        );
        let drag_path = self.row_path(index);
        let target_path = drag_path.clone();
        let drop_path = drag_path.clone();
        let decoration = self.drop_decoration(&drag_path);
        let first_child = tree_row
            .as_ref()
            .filter(|_| is_folder)
            .and_then(|row| {
                favorite_drag::first_child_below(
                    &row.path,
                    self.tree_rows.get(index + 1).map(|next| next.path.as_slice()),
                )
            });
        let drag = self.content_drag(drag_path, false);
        let drag_owner = cx.weak_entity();
        let hover_id = id.clone();
        let hover_owner = cx.weak_entity();
        let hover_session = self.native_session;
        #[cfg(windows)]
        let hover_binding = self.native_item_hover_binding(index, &id, generation, hover_session);
        #[cfg(not(windows))]
        let hover_binding: Option<hover_binding::HoverBinding> = None;
        let click_id = id.clone();
        // Lead icon (16) + gap (8) on the left, trailing details on the right.
        let content_width = (self.row_width - indent - 2.0 * metrics.padding_x - 24.0 - 48.0).max(1.0);
        let content = self.row_content(
            &item,
            content_width,
            window.scale_factor(),
            hover_binding.clone(),
            cx,
        );
        let theme = self.tokens.theme(self.dark);
        let display = &self.memory.display;
        let icon = |key: &str, size: f32| {
            svg()
                .path(self.tokens.icons.get(key).cloned().unwrap_or_default())
                .size(px(size))
                .flex_none()
                .text_color(color(&theme.text_muted))
        };
        let kind_icon = icon(
            if is_folder { "folder-closed" } else { category(&item).icon() },
            16.0,
        )
        .into_any_element();
        // Tree rows keep their expand control; folders show it as the folder icon.
        let lead = if tree_row.is_some() {
            self.tree_row_metadata(
                index,
                if is_folder { div().into_any_element() } else { kind_icon },
                cx,
            )
        } else {
            kind_icon
        };
        let trail = rsx! {
            <div flex_none flex items_center gap={px(6.0)} text_size={px(11.0)}
                text_color={color(&theme.text_muted)} />
        }
            .when(display.show_time, |trail| trail.child(metadata.natural_time.clone()))
            .when(is_folder && display.show_count, |trail| {
                let count = self.tree.child_count(&item.id).max(item.children_count.max(0) as usize);
                trail.child(count.to_string())
            })
            .when(is_folder && !self.memory.layout.is_tree(), |trail| {
                trail.child(icon("chevron-right", 12.0))
            });
        let row = appearance::row_chrome_with_wallpaper(
            SharedString::from(format!("favorite-{occurrence}")),
            group,
            &settings,
            &self.tokens,
            self.dark,
            selected,
            false,
            false,
            index + 1 == self.visible.len(),
            false,
            self.window_background_enabled,
        )
        .cursor_pointer()
        .text_size(px(self.tokens.typography.font_size))
        .line_height(relative(metrics.text_line_height))
        .child(
            rsx! {
                <div flex items_start gap={px(8.0)} pl={px(indent)}>
                    <div flex_none min_h={px(20.0)} flex items_center>
                        {lead}
                    </div>
                    <div flex_1 min_w_0>
                        {content}
                    </div>
                    <div min_h={px(20.0)} flex items_center>
                        {trail}
                    </div>
                </div>
            },
        )
        // The hover bar floats over the trailing details (DESIGN 2026-10-14).
        .child(rsx! {
            <div absolute top={px(3.0)} right={px(metrics.padding_x)}>
                {actions}
            </div>
        });
        // Lines start at this row's own indent, i.e. the level being inserted
        // into; Into tints the whole row instead (DESIGN 2026-10-15).
        let accent = color(&theme.accent);
        let row = row
            .when_some(decoration, |row, placement| {
                row.child(if placement == Placement::Into {
                    favorite_drag::into_highlight(metrics.radius, accent)
                } else {
                    favorite_drag::insertion_marker(placement, metrics.padding_x + indent - 3.0, metrics.padding_x, accent)
                })
            })
            .on_drag_move(cx.listener(move |this, event: &gpui::DragMoveEvent<ContentDrag>, window, cx| {
                if !event.bounds.contains(&event.event.position) { return; }
                let drag = event.dragged_item().downcast_ref::<ContentDrag>().expect("content drag");
                let offset = f32::from(event.event.position.y - event.bounds.top());
                let target = this.drag_target_for_row(&target_path, is_folder, offset, f32::from(event.bounds.size.height), first_child.as_deref());
                this.hover_content_target(drag, target_path.clone(), target, false, event.event.modifiers.control, cx);
                this.offer_content_target(drag, event.bounds, window);
            }))
            .on_drop(cx.listener(move |this, drag: &ContentDrag, window, cx| {
                cx.stop_propagation();
                this.apply_content_drop(drag, &drop_path, window, cx);
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                    if !this.active
                        || this.dialog.is_some()
                        || this.busy
                        || this.generation != generation
                        || !this.visible.get(index).is_some_and(|item| item.id == id)
                        || occurrence_path.as_ref().is_some_and(|path| {
                            !this
                                .tree_rows
                                .get(index)
                                .is_some_and(|row| &row.path == path)
                        })
                    {
                        return;
                    }
                    this.cancel_native_hover();
                    this.select_drag_occurrence(index, event.modifiers.control || event.modifiers.platform, event.modifiers.shift);
                    this.save_view_memory(cx);
                    this.focus.focus(window);
                    this.restore_browsing(window);
                    if index + 10 >= this.visible.len() {
                        this.load_more(cx);
                    }
                    cx.notify();
                }),
            )
            .on_click(cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                if this.native_dragging || this.content_drag_blocks_click()
                    || this
                        .native_suppress_click
                        .is_some_and(|until| Instant::now() < until)
                    || !this.active
                    || this.busy
                    || this.dialog.is_some()
                    || this.generation != generation
                    || click_occurrence_path.as_ref().is_some_and(|path| {
                        !this
                            .tree_rows
                            .get(index)
                            .is_some_and(|row| &row.path == path)
                    })
                {
                    return;
                }
                this.cancel_native_hover();
                let modifiers = event.modifiers();
                if modifiers.control || modifiers.platform || modifiers.shift { return; }
                this.collapse_drag_selection(index);
                cx.notify();
                if is_folder {
                    if this.memory.layout.is_tree() {
                        if let Some(row) = this.tree_rows.get(index).filter(|row| row.has_children)
                        {
                            this.toggle_tree_path(row.path.clone(), cx);
                        }
                        return;
                    }
                    // List layouts open the folder in place (the panel stays in
                    // the footer and the row menu).
                    this.select_folder(Some(click_id.clone()), window, cx);
                    return;
                }
                let count = event.click_count();
                if count == 1 && this.execute_mode == 2 {
                    this.copy_item(click_id.clone(), None, cx);
                } else if count == 1 && this.execute_mode == 1
                    || count >= 2 && matches!(this.execute_mode, 2 | 3)
                {
                    this.execute_item(click_id.clone(), None, cx);
                }
            }))
            .when(!self.tree.drag_revision.is_empty(), |row| {
                row.on_drag(drag, move |drag: &ContentDrag, _, window, cx| {
                    let _ = drag_owner.update(cx, |this, cx| this.begin_content_drag(drag, cx));
                    favorite_drag::activate_drag(drag, window, cx);
                    cx.new(|_| drag.clone())
                })
            })
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.select_occurrence(index);
                    this.row_menu(menu_id.clone(), cx);
                }),
            )
            .into_any_element();
        let row = hover_binding::wrap_hover_row(
            row,
            hover_binding,
            Rc::new(move |event, window, cx| {
                let _ = hover_owner.update(cx, |this, cx| {
                    this.hover_native_item(
                        index,
                        &hover_id,
                        generation,
                        hover_session,
                        event,
                        window,
                        cx,
                    )
                });
            }),
        );
        // The fill and divider span the full row; only the content is indented.
        div().w_full().child(row).into_any_element()
    }

    /// Footer (DESIGN 2026-10-14): sidebar toggle and count on the left; add,
    /// sort, list/tree, panel, list display and app settings on the right.
    fn footer(
        &self,
        tokens: &DesignTokens,
        t: &crate::preview::ThemeTokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let l = &tokens.layout;
        let layout = self.memory.layout;
        let button = |id: &'static str, icon: &str, on: bool, tip: &'static str| {
            let hover = color(&t.hover);
            rsx! {
                <div id={id} size={px(l.icon_button_size)} flex_none flex items_center
                    justify_center rounded={px(l.chip_radius)} cursor_pointer />
            }
                .hover(move |style| style.bg(hover))
                .child(
                    svg()
                        .path(tokens.icons.get(icon).cloned().unwrap_or_default())
                        .size(px(l.chrome_icon_size))
                        .flex_none()
                        .text_color(color(if on { &t.text } else { &t.text_muted })),
                )
                .tooltip(favorite_tooltip(tip.into(), tokens.clone(), self.dark))
        };
        let count = self.visible.len().max(
            self.total
                + if layout.is_tree() {
                    0
                } else {
                    self.tree.matching_folders(self.folder.as_deref(), &self.query).len()
                },
        );
        let sort_tip = if self.sort_title {
            tr("排序：名称（点击切换）", "Sort: name (click to switch)")
        } else if layout.is_tree() {
            tr("排序：原有顺序（点击切换）", "Sort: original order (click to switch)")
        } else {
            tr("排序：更新时间（点击切换）", "Sort: updated (click to switch)")
        };
        rsx! {
            <div h={px(l.footer_height)} flex_none flex items_center
                gap={px(l.chrome_button_gap)} px={px(l.content_padding_x)} border_t_1
                border_color={color(&t.border)} text_size={px(tokens.typography.small_font_size)}
                text_color={color(&t.text_muted)} />
        }
            .child(
                button(
                    "favorite-sidebar-toggle",
                    "layout-sidebar",
                    layout.has_sidebar(),
                    if layout.has_sidebar() {
                        tr("隐藏目录栏", "Hide folders")
                    } else {
                        tr("显示目录栏", "Show folders")
                    },
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.choose_layout(layout.with_sidebar(!layout.has_sidebar()), cx)
                })),
            )
            .child(
                rsx! {
                    <div pl={px(2.0)} flex_1 min_w_0 overflow_hidden whitespace_nowrap
                        text_ellipsis>
                        {cliphost_core::trf!("{} 项", "{} items", count)}
                    </div>
                }
                    .when(self.loading && !self.background_refresh, |e| e.child(tr(" · 更新中…", " · Refreshing…"))),
            )
            .child(
                button("favorite-add", "plus", false, tr("添加", "Add"))
                    .on_click(cx.listener(|this, _, _, cx| this.add_menu(cx))),
            )
            .child(button("favorite-sort", "sort", self.sort_title, sort_tip).on_click(
                cx.listener(|this, _, _, cx| {
                    this.sort_title = !this.sort_title;
                    this.rebuild_visible();
                    cx.notify();
                }),
            ))
            .child(
                button(
                    "favorite-layout",
                    if layout.is_tree() { "list" } else { "tree" },
                    false,
                    if layout.is_tree() {
                        tr("切换为列表", "Show as list")
                    } else {
                        tr("切换为树状", "Show as tree")
                    },
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.choose_layout(layout.with_tree(!layout.is_tree()), cx)
                })),
            )
            .child(
                button("favorite-open-panel", "open-panel", false, tr("打开面板", "Open panel"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        cx.emit(FavoritesEvent::OpenPanel(this.folder.clone().unwrap_or_default()))
                    })),
            )
            .child(
                button("favorite-list-settings", "sliders", self.display_panel, tr("列表显示设置", "List display"))
                    .when(self.display_panel, |button| button.bg(color(&t.neutral_active)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.display_panel = !this.display_panel;
                        cx.notify();
                    })),
            )
            .child(
                button("favorite-settings", "gear", false, tr("设置（含帮助与关于）", "Settings (help and about)"))
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(FavoritesEvent::OpenSettings))),
            )
            .into_any_element()
    }

    fn render_dialog(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let dialog = self.dialog.as_ref()?;
        let t = self.tokens.theme(self.dark).clone();
        let heading = match dialog {
            Dialog::CreateFolder => tr("新建收藏夹", "New favorite folder").to_string(),
            Dialog::Rename { .. } => tr("重命名", "Rename").to_string(),
            Dialog::AddText { .. } => tr("添加文本", "Add text").to_string(),
            Dialog::Collect { .. } => tr("收藏剪贴板内容", "Collect clipboard content").to_string(),
            Dialog::Delete { name, .. } => {
                format!("{}「{}」？", tr("删除收藏夹", "Delete folder"), name)
            }
            Dialog::Move { .. } => tr("移动到收藏夹", "Move to favorite folder").to_string(),
        };
        let mut card = rsx! {
            <div w={px(360.0)} max_w_full p_4 flex flex_col gap_3 bg={color(&t.surface)}
                rounded={px(self.tokens.settings.card_radius)} border_1
                border_color={color(&t.border)}>
                <div text_color={color(&t.text)}>
                    {heading}
                </div>
            </div>
        };
        match dialog {
            Dialog::CreateFolder | Dialog::Rename { .. } => card = card.child(self.name.clone()),
            Dialog::AddText { line_by_line } => {
                let on = *line_by_line;
                let hover = color(&t.hover);
                card = card.child(self.body.clone()).child(
                    rsx! {
                        <div id={"favorite-add-text-lines"} flex items_center gap={px(8.0)}
                            px={px(4.0)} py={px(3.0)} rounded={px(6.0)} cursor_pointer />
                    }
                        .hover(move |style| style.bg(hover))
                        .child(
                            rsx! {
                                <div size={px(14.0)} flex_none flex items_center justify_center
                                    rounded={px(3.0)} border_1 />
                            }
                                .border_color(color(if on { &t.text } else { &t.border_secondary }))
                                .when(on, |check| check.bg(color(&t.text)).child(
                                    svg()
                                        .path(self.tokens.icons.get("check").cloned().unwrap_or_default())
                                        .size(px(10.0))
                                        .text_color(color(&t.surface)),
                                )),
                        )
                        .child(
                            rsx! {
                                <div flex flex_col>
                                    <div text_size={px(12.0)} text_color={color(&t.text)}>
                                        {tr("每行一条", "One per line")}
                                    </div>
                                </div>
                            }
                                .child(
                                    rsx! {
                                        <div text_size={px(11.0)} text_color={color(&t.text_muted)}>
                                            {tr(
                                            "每个非空行各存为一条收藏",
                                            "Each nonempty line becomes its own favorite",
                                        )}
                                        </div>
                                    },
                                ),
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(Dialog::AddText { line_by_line }) = &mut this.dialog {
                                *line_by_line = !*line_by_line;
                                cx.notify();
                            }
                        })),
                );
            }
            Dialog::Collect {
                parent_id, folders, ..
            } => {
                card = card.child(self.name.clone()).child(self.body.clone());
                let mut picker = div().flex().flex_wrap().gap_1();
                for (index, (id, title)) in folders.iter().enumerate() {
                    let id = id.clone();
                    picker = picker.child(
                        rsx! {
                            <div id={("collect-folder", index)} px_2 py_1 cursor_pointer>
                                {title.clone()}
                            </div>
                        }
                            .when(*parent_id == id, |e| {
                                e.bg(color(&t.tab_active)).text_color(color(&t.accent))
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if this.busy {
                                    return;
                                }
                                if let Some(Dialog::Collect { parent_id, .. }) = &mut this.dialog {
                                    *parent_id = id.clone();
                                }
                                cx.notify();
                            })),
                    );
                }
                card = card.child(picker);
            }
            Dialog::Delete { descendants, .. } => {
                card = card.child(rsx! {
                    <div text_size={px(12.0)} text_color={color(&t.error)}>
                        {format!(
                        "{} {} {}",
                        tr("将删除共", "This deletes"),
                        descendants + 1,
                        tr(
                            "项（含全部内容），且无法撤销。",
                            "items including all contents. This cannot be undone."
                        )
                    )}
                    </div>
                })
            }
            Dialog::Move { id } => {
                let choices = self.tree.move_choices(id);
                if choices.is_empty() {
                    card = card.child(tr(
                        "没有可用的目标收藏夹",
                        "No destination folder is available",
                    ));
                }
                for (index, row) in choices.into_iter().enumerate() {
                    let id = row.id.clone();
                    let hover = color(&t.hover);
                    card = card.child(
                        rsx! {
                            <div id={("move-folder", index)}
                                pl={px(row.depth as f32 * 12.0 + 8.0)} py_1 cursor_pointer />
                        }
                            .hover(move |style| style.bg(hover))
                            .child(row.title)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.move_to(id.clone(), window, cx)
                            })),
                    );
                }
            }
        }
        let cancel_hover = color(&t.hover);
        let action_row = div()
            .flex()
            .justify_end()
            .gap_2()
            .child(
                rsx! {
                    <div id={"favorite-dialog-cancel"} px_3 py_1 cursor_pointer />
                }
                    .hover(move |style| style.bg(cancel_hover))
                    .child(tr("取消", "Cancel"))
                    .on_click(cx.listener(|this, _, window, cx| this.close_dialog(window, cx))),
            )
            .when(!matches!(dialog, Dialog::Move { .. }), |e| {
                e.child(
                    rsx! {
                        <div id={"favorite-dialog-save"} px_3 py_1 rounded_md
                            bg={color(&t.tab_active)} text_color={color(&t.accent)}
                            cursor_pointer />
                    }
                        .child(if self.busy {
                            tr("处理中…", "Working…")
                        } else if matches!(dialog, Dialog::Delete { .. }) {
                            tr("删除", "Delete")
                        } else {
                            tr("保存", "Save")
                        })
                        .on_click(
                            cx.listener(|this, _, window, cx| this.submit_dialog(window, cx)),
                        ),
                )
            });
        card = card
            .when(self.error.is_some(), |e| {
                e.child(
                    rsx! {
                        <div text_size={px(12.0)} text_color={color(&t.error)}>
                            {self.error.clone().unwrap_or_default()}
                        </div>
                    },
                )
            })
            .child(action_row);
        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui::rgba(0x00000066))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(card)
                .into_any_element(),
        )
    }
}

fn input_colors(tokens: &DesignTokens, dark: bool) -> InputColors {
    let t = tokens.theme(dark);
    InputColors {
        background: color(&t.search_background),
        foreground: color(&t.text),
        placeholder: color(&t.text_muted),
        border: color(&t.border_secondary),
        selection: color(&t.tab_active),
        caret: color(&t.accent),
    }
}

fn decode_query(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hi = (bytes[index + 1] as char).to_digit(16);
            let lo = (bytes[index + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                decoded.push((hi * 16 + lo) as u8);
                index += 3;
                continue;
            }
        }
        decoded.push(if bytes[index] == b'+' {
            b' '
        } else {
            bytes[index]
        });
        index += 1;
    }
    String::from_utf8(decoded).unwrap_or_else(|_| value.to_string())
}

impl Render for NativeFavoritesView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::content_cache_owners::register(cx, |this, cx| {
            let state = this.host.hide_snapshot();
            if !this.active || !this.native_owner_enabled || !state.visible || state.is_collapsed {
                this.content_cache.set_visible_items(std::iter::empty::<&str>());
            }
            let changed = this.content_cache.apply_budget_pressure();
            this.content_cache.release_evicted(cx);
            if changed { cx.notify(); }
        });
        favorite_drag::register_surface(window, self.host.clone(), "favorites", cx);
        let tokens = self.tokens.clone();
        let t = tokens.theme(self.dark).clone();
        let l = &tokens.layout;
        let viewport_width = f32::from(window.viewport_size().width);
        // Overlay scrollbars (DESIGN 2026-10-14) reserve no gutter.
        let scrollbar_width = 0.0;
        // Leave the main HWND's 3-DIP resize band outside the content scrollbar.
        let resize_inset = 3.0;
        let sidebar_width = if self.memory.layout.has_sidebar() {
            (viewport_width * 0.25).clamp(96.0, 120.0)
        } else {
            0.0
        };
        // Leave room for the folder icon and a label even in a deep tree.
        let max_folder_indent =
            (sidebar_width - scrollbar_width - l.icon_size - 4.0 - 5.0 - 28.0).max(4.0);
        self.row_width = (viewport_width
            - sidebar_width
            - if self.memory.layout.has_sidebar() {
                8.0
            } else {
                0.0
            }
            - 2.0 * l.content_padding_x
            - scrollbar_width
            - resize_inset)
            .max(80.0);
        let rows = self.sidebar_rows();
        let weak = cx.weak_entity();
        let virtual_list = list(self.list.clone(), move |index, window, cx| {
            weak.update(cx, |this, cx| {
                this.visible
                    .get(index)
                    .cloned()
                    .map(|item| this.row(index, item, window, cx))
                    .unwrap_or_else(|| div().into_any_element())
            })
            .unwrap_or_else(|_| div().into_any_element())
        })
        .flex_1()
        .min_h_0()
        .w_full();
        let scrollbar_style = ScrollbarStyle {
            ink: color(&t.text),
            metrics: tokens.scrollbar,
            always_visible: false,
        };
        let session = self.native_session;
        let list_owner = cx.weak_entity();
        let list_changed: ScrollbarChanged = Rc::new(move |_, cx| {
            let _ = list_owner.update(cx, |this, cx| {
                if !this.active || this.native_session != session {
                    return;
                }
                this.cancel_native_hover();
                this.save_view_memory(cx);
                cx.notify();
            });
        });
        let list_scrollbar = self.list_scrollbar.render(
            "favorite-list-scrollbar",
            ScrollbarTarget::List(self.list.clone()),
            session,
            scrollbar_style,
            list_changed,
        );
        let sidebar_owner = cx.weak_entity();
        let sidebar_changed: ScrollbarChanged = Rc::new(move |_, cx| {
            let _ = sidebar_owner.update(cx, |this, cx| {
                if this.active && this.native_session == session {
                    this.cancel_native_hover();
                    cx.notify();
                }
            });
        });
        let sidebar_scrollbar = self.sidebar_scrollbar.render(
            "favorite-folder-scrollbar",
            ScrollbarTarget::Scroll(self.sidebar_scroll.clone()),
            session,
            scrollbar_style,
            sidebar_changed,
        );
        let wallpaper = self.window_background_enabled;
        let (tree_hover, tree_active) = if wallpaper {
            self.window_background_tree_colors
                .unwrap_or((color(&t.hover), color(&t.tab_active)))
        } else {
            (color(&t.hover), color(&t.tab_active))
        };
        let all_selected = self.folder.is_none();
        let neutral = color(&t.neutral_active);
        // Sidebar rows: 28 px, neutral selection, folder icons show expansion
        // without a separate chevron column. One click selects and opens a folder;
        // clicking the selected folder again folds it.
        let icon = |key: &str, size: f32, tint: gpui::Hsla| {
            svg()
                .path(tokens.icons.get(key).cloned().unwrap_or_default())
                .size(px(size))
                .flex_none()
                .text_color(tint)
        };
        let side_row = |selected: bool| {
            rsx! {
                <div flex_none h={px(28.0)} flex items_center gap={px(4.0)} pr={px(6.0)} relative
                    rounded={px(l.chip_radius)} cursor_pointer text_size={px(12.0)} />
            }
                .text_color(color(if selected { &t.text } else { &t.text_secondary }))
                .when(selected, |row| row.bg(if wallpaper { tree_active } else { neutral }))
                .when(!selected, |row| row.hover(move |style| style.bg(tree_hover)))
        };
        let mut sidebar = div()
            .id("favorite-folders")
            .w_full()
            .h_full()
            .overflow_y_scroll()
            .overflow_x_hidden()
            .track_scroll(&self.sidebar_scroll)
            .smooth_wheel("favorite-folders", &self.sidebar_scroll, cx)
            .flex()
            .flex_col()
            .gap(px(1.0))
            .py(px(2.0))
            .pr(px(4.0))
            .child(
                side_row(all_selected)
                    .id("favorite-all")
                    .when(self.drop_decoration(&[]) == Some(Placement::Into), |row| row.child(favorite_drag::into_highlight(l.chip_radius, color(&t.accent))))
                    .on_drag_move(cx.listener(|this, event: &gpui::DragMoveEvent<ContentDrag>, window, cx| {
                        if !event.bounds.contains(&event.event.position) { return; }
                        let drag = event.dragged_item().downcast_ref::<ContentDrag>().expect("content drag");
                        this.hover_content_target(drag, Vec::new(), Some(DropTarget { parent_id: None, anchor_id: None, placement: Placement::Into }), true, event.event.modifiers.control, cx);
                        this.offer_content_target(drag, event.bounds, window);
                    }))
                    .on_drop(cx.listener(|this, drag: &ContentDrag, window, cx| {
                        cx.stop_propagation();
                        this.apply_content_drop(drag, &[], window, cx);
                    }))
                    .pl(px(2.0))
                    .child(icon(
                        "star",
                        l.icon_size,
                        color(if all_selected { &t.text } else { &t.text_muted }),
                    ))
                    .child(div().min_w_0().line_clamp(1).child(tr("全部", "All")))
                    .tooltip(favorite_tooltip(
                        tr("全部收藏", "All favorites").into(),
                        tokens.clone(),
                        self.dark,
                    ))
                    .on_click(
                        cx.listener(|this, _, window, cx| this.select_folder(None, window, cx)),
                    ),
            );
        let next_paths: Vec<OccurrencePath> = rows.iter().skip(1).map(|row| row.path.clone()).collect();
        for (index, row) in rows.into_iter().enumerate() {
            let first_child = if row.expanded {
                favorite_drag::first_child_below(&row.path, next_paths.get(index).map(Vec::as_slice))
            } else {
                None
            };
            let indent = (row.depth as f32 * 8.0 + 2.0).min(max_folder_indent);
            let (path, toggle_id, menu_id) = (row.path.clone(), row.path.clone(), row.id.clone());
            let selected = self.memory.active_folder == row.path;
            let has_folders = row.has_folders;
            let expanded = row.expanded;
            let open_id = row.path.clone();
            let target_path = row.path.clone();
            let drop_path = row.path.clone();
            let decoration = self.drop_decoration(&row.path);
            let drag = self.content_drag(row.path.clone(), true);
            let drag_owner = cx.weak_entity();
            sidebar = sidebar.child(
                side_row(selected)
                    .id(("favorite-folder", index))
                    // Dot sits at the folder icon column of the inserted level.
                    .when_some(decoration, |row, placement| {
                        row.child(if placement == Placement::Into {
                            favorite_drag::into_highlight(l.chip_radius, color(&t.accent))
                        } else {
                            favorite_drag::insertion_marker(placement, indent - 3.0, 4.0, color(&t.accent))
                        })
                    })
                    .on_drag(drag, move |drag: &ContentDrag, _, window, cx| {
                        let _ = drag_owner.update(cx, |this, cx| this.begin_content_drag(drag, cx));
                        favorite_drag::activate_drag(drag, window, cx);
                        cx.new(|_| drag.clone())
                    })
                    .on_drag_move(cx.listener(move |this, event: &gpui::DragMoveEvent<ContentDrag>, window, cx| {
                        if !event.bounds.contains(&event.event.position) { return; }
                        let drag = event.dragged_item().downcast_ref::<ContentDrag>().expect("content drag");
                        let offset = f32::from(event.event.position.y - event.bounds.top());
                        let target = this.drag_target_for_row(&target_path, true, offset, f32::from(event.bounds.size.height), first_child.as_deref());
                        this.hover_content_target(drag, target_path.clone(), target, true, event.event.modifiers.control, cx);
                        this.offer_content_target(drag, event.bounds, window);
                    }))
                    .on_drop(cx.listener(move |this, drag: &ContentDrag, window, cx| {
                        cx.stop_propagation();
                        this.apply_content_drop(drag, &drop_path, window, cx);
                    }))
                    .pl(px(indent))
                    .tooltip(favorite_tooltip(row.title.clone(), tokens.clone(), self.dark))
                    .child(
                        rsx! {
                            <div id={("favorite-expand", index)} size={px(l.icon_size)} flex_none
                                flex items_center justify_center />
                        }
                            .child(icon(
                                if has_folders && expanded { "folder-open" } else { "folder-closed" },
                                l.icon_size,
                                color(if selected { &t.text } else { &t.text_muted }),
                            ))
                            .when(has_folders, |button| {
                                button
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        if this.content_drag_blocks_click() || this.native_suppress_click.is_some_and(|until| Instant::now() < until) { return; }
                                        this.sidebar_scrollbar.cancel();
                                        if !this.expanded.remove(&toggle_id) {
                                            this.expanded.insert(toggle_id.clone());
                                        }
                                        this.save_view_memory(cx);
                                        cx.notify();
                                    }))
                            }),
                    )
                    .child(div().flex_1().min_w_0().line_clamp(1).child(row.title))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if this.content_drag_blocks_click() || this.native_suppress_click.is_some_and(|until| Instant::now() < until) { return; }
                        if has_folders {
                            // Re-clicking the open folder folds it; otherwise open it.
                            if selected && expanded {
                                this.expanded.remove(&open_id);
                            } else {
                                this.expanded.insert(open_id.clone());
                            }
                        }
                        this.select_folder_path(path.clone(), window, cx)
                    }))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.row_menu(menu_id.clone(), cx);
                        }),
                    ),
            );
        }
        let sidebar = rsx! {
            <div id={"favorite-folders-scroll-region"} relative w={px(sidebar_width)} flex_none
                h_full pr={px(scrollbar_width)} border_r_1 border_color={color(&t.border)} />
        }
            .when(wallpaper, |sidebar| sidebar.bg(color(&t.surface)))
            .on_drag_move(cx.listener(|this, event: &gpui::DragMoveEvent<ContentDrag>, _, cx| {
                if !event.bounds.contains(&event.event.position) { return; }
                let drag = event.dragged_item().downcast_ref::<ContentDrag>().expect("content drag");
                this.drag_auto_scroll(drag, &event.event, event.bounds, true, cx);
            }))
            .child(sidebar)
            .child(sidebar_scrollbar);
        // Search is in the title bar. Type chips use this whole row and fall
        // back to icons only when the window itself is too narrow.
        let chip_width = |label: &str| {
            crate::view::label_width(
                label,
                tokens.typography.category_font_size,
                &tokens.typography.font_family,
                window,
            ) + 2.0 * l.chip_padding_x
        };
        let text_strip = l.icon_button_size
            + Category::FILTERS[1..]
                .iter()
                .map(|category| chip_width(category.label()))
                .sum::<f32>()
            + (Category::FILTERS.len() - 1) as f32 * l.chip_gap;
        let compact_types = text_strip
            > viewport_width - 2.0 * (l.content_padding_x + 2.0);
        let neutral = color(&t.neutral_active);
        let category_bar = rsx! {
            <div flex_none min_w_0 overflow_hidden flex items_center gap={px(l.chip_gap)} />
        }
            .children(Category::FILTERS.into_iter().enumerate().map(|(index, category)| {
                let active = self.category == category;
                let icon_only = category == Category::All || compact_types;
                let tint = color(if active { &t.text } else { &t.text_secondary });
                let hover = color(&t.hover);
                rsx! {
                    <div id={("favorite-category", index)} h={px(l.chip_height)}
                        rounded={px(l.chip_radius)} flex flex_none items_center justify_center
                        whitespace_nowrap cursor_pointer
                        text_size={px(tokens.typography.category_font_size)} text_color={tint} />
                }
                    .when(icon_only, |chip| {
                        chip.w(px(l.icon_button_size)).child(
                            svg()
                                .path(tokens.icons.get(category.icon()).cloned().unwrap_or_default())
                                .size(px(l.icon_size))
                                .flex_none()
                                .text_color(tint),
                        )
                    })
                    .when(!icon_only, |chip| chip.px(px(l.chip_padding_x)).child(category.label()))
                    .when(active, |chip| chip.bg(neutral))
                    .when(!active, |chip| chip.hover(move |style| style.bg(hover)))
                    .tooltip(favorite_tooltip(category.label().into(), tokens.clone(), self.dark))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.category = category;
                        this.rebuild_visible();
                        cx.notify();
                    }))
            }));
        let toolbar = rsx! {
            <div flex_none h={px(l.toolbar_height)} px={px(l.content_padding_x + 2.0)} flex
                items_center>
                {category_bar}
            </div>
        };
        let up_row = (self.memory.layout == LayoutMode::List
            && !self.memory.active_folder.is_empty()
            && self.query.is_empty()
            && self.category == Category::All)
            .then(|| {
                let hover = color(&t.hover);
                let mut parent = self.memory.active_folder.clone();
                let current = parent.pop().unwrap_or_default();
                let name = self.tree.title_of(&current);
                let target_parent = parent.clone();
                let drop_parent = parent.clone();
                rsx! {
                    <div id={"favorite-up"} flex_none mx={px(l.row_inset_x)} h={px(34.0)}
                        px={px(l.minimal_row_padding_x)} flex items_center gap={px(8.0)}
                        rounded={px(l.row_radius)} cursor_pointer />
                }
                    .hover(move |style| style.bg(hover))
                    .on_drag_move(cx.listener(move |this, event: &gpui::DragMoveEvent<ContentDrag>, window, cx| {
                        if !event.bounds.contains(&event.event.position) { return; }
                        let drag = event.dragged_item().downcast_ref::<ContentDrag>().expect("content drag");
                        this.hover_content_target(drag, target_parent.clone(), Some(DropTarget { parent_id: target_parent.last().cloned(), anchor_id: None, placement: Placement::Into }), false, event.event.modifiers.control, cx);
                        this.offer_content_target(drag, event.bounds, window);
                    }))
                    .on_drop(cx.listener(move |this, drag: &ContentDrag, window, cx| {
                        cx.stop_propagation();
                        this.apply_content_drop(drag, &drop_parent, window, cx);
                    }))
                    .child(
                        svg()
                            .path(tokens.icons.get("level-up").cloned().unwrap_or_default())
                            .size(px(l.icon_size))
                            .flex_none()
                            .text_color(color(&t.text_muted)),
                    )
                    .child(
                        rsx! {
                            <div flex_1 text_size={px(13.0)} text_color={color(&t.text_secondary)}>
                                {tr("上级", "Up")}
                            </div>
                        },
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(color(&t.text_muted))
                            .line_clamp(1)
                            .child(name),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.select_folder_path(parent.clone(), window, cx)
                    }))
                    .into_any_element()
            });
        let dialog = self.render_dialog(cx);
        rsx! {
            <div w_full h_full min_h_0 flex flex_col relative />
        }
            .when(self.native_drag_over.is_some(), |root| {
                root.border_2().border_color(color(&t.accent))
            })
            .bg(color(&t.background))
            .text_color(color(&t.text))
            .track_focus(&self.focus)
            .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, _| this.begin_pointer_sequence()))
            .on_drag_move(cx.listener(|this, event: &gpui::DragMoveEvent<ContentDrag>, window, cx| {
                favorite_drag::clear_target(window);
                this.reset_content_target(cx);
                if favorite_drag::route_drag_move(event, window, cx) { return; }
                this.content_drag_pointer(&event.event, window, cx);
            }))
            .on_scroll_wheel(cx.listener(|this, _, _, _| this.cancel_native_hover()))
            .on_key_down(cx.listener(|this, event, window, cx| {
                this.cancel_native_hover();
                this.key_down(event, window, cx)
            }))
            .child(toolbar)
            .child(
                rsx! {
                    <div flex_1 min_h_0 flex gap={px(2.0)} pr={px(resize_inset)} />
                }
                    .when(self.memory.layout.has_sidebar(), |body| body.child(sidebar))
                    .child(
                        rsx! {
                            <div flex_1 min_w_0 min_h_0 flex flex_col />
                        }
                            .on_drag_move(cx.listener(|this, event: &gpui::DragMoveEvent<ContentDrag>, window, _cx| {
                                if !event.bounds.contains(&event.event.position) { return; }
                                let drag = event.dragged_item().downcast_ref::<ContentDrag>().expect("content drag");
                                let scope = this.scope_path();
                                if this.query.is_empty() || !scope.is_empty() { this.hover_content_background(drag, scope, event.event.modifiers.control); }
                                this.offer_content_target(drag, event.bounds, window);
                            }))
                            .on_drop(cx.listener(|this, drag: &ContentDrag, window, cx| {
                                let scope = this.scope_path();
                                this.apply_content_drop(drag, &scope, window, cx);
                            }))
                            .when(up_row.is_some(), |column| column.children(up_row))
                            .child(
                                rsx! {
                                    <div id={"favorite-list-scroll-region"} relative flex_1
                                        min_h_0 pr={px(scrollbar_width)} flex flex_col />
                                }
                                    .on_drag_move(cx.listener(|this, event: &gpui::DragMoveEvent<ContentDrag>, window, cx| {
                                        if !event.bounds.contains(&event.event.position) { return; }
                                        let drag = event.dragged_item().downcast_ref::<ContentDrag>().expect("content drag");
                                        let scope = this.scope_path();
                                        if this.query.is_empty() || !scope.is_empty() { this.hover_content_background(drag, scope, event.event.modifiers.control); }
                                        this.offer_content_target(drag, event.bounds, window);
                                        this.drag_auto_scroll(drag, &event.event, event.bounds, false, cx);
                                    }))
                                    .on_drop(cx.listener(|this, drag: &ContentDrag, window, cx| {
                                        let scope = this.scope_path();
                                        this.apply_content_drop(drag, &scope, window, cx);
                                    }))
                                    .child(virtual_list)
                                    .child(list_scrollbar),
                            )
                            .when(self.visible.is_empty(), |e| {
                                e.child(div().p_4().text_color(color(&t.text_muted)).child(
                                    if self.loading && !self.background_refresh {
                                        tr("加载中…", "Loading…")
                                    } else {
                                        tr("暂无收藏项", "No favorites")
                                    },
                                ))
                            })
                            .when(self.has_more, |e| {
                                e.child(
                                    rsx! {
                                        <div id={"favorite-more"} px_2 py_1
                                            text_color={color(&t.accent)} cursor_pointer />
                                    }
                                        .child(if self.more.is_some() {
                                            tr("加载中…", "Loading…")
                                        } else {
                                            tr("加载更多", "Load more")
                                        })
                                        .on_click(cx.listener(|this, _, _, cx| this.load_more(cx))),
                                )
                            }),
                    ),
            )
            .when(self.drag_hint().is_some(), |root| root.child(rsx! {
                <div px_2 py_1 text_size={px(12.0)} text_color={color(&t.text_muted)}>
                    {self.drag_hint().unwrap_or_default()}
                </div>
            }))
            .when(self.error.is_some() && self.dialog.is_none(), |e| {
                e.child(
                    rsx! {
                        <div px_2 py_1 text_size={px(12.0)} text_color={color(&t.error)}>
                            {self.error.clone().unwrap_or_default()}
                        </div>
                    },
                )
            })
            .child(self.footer(&tokens, &t, cx))
            .when(self.display_panel, |root| root.child(self.display_panel_view(cx)))
            .children(dialog)
            .child(favorite_drag::surface_events())
            .child(self.content_pointer_events(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn item(id: &str, parent: &str, folder: bool) -> FavorItemDto {
        serde_json::from_value(json!({"id":id,"parentId":parent,"title":id,"summary":"", "kind":if folder{"Favorite"}else{"Text"},
            "isFolder":folder,"childrenCount":0,"createTime":"2026-10-12T00:00:00Z","updateTime":"2026-10-12T00:00:00Z",
            "sourceProcessName":"","fileCount":0,"filePaths":[],"textLength":null,"imRichOrigin":null,"imRichSegments":[]})).unwrap()
    }
    fn tree(items: Vec<FavorItemDto>) -> FavoriteTree {
        FavoriteTree {
            drag_revision: String::new(),
            ordered_root_items: None,
            children_by_id: BTreeMap::new(),
            _root_items: Vec::new(),
            items: items
                .into_iter()
                .map(|item| (item.id.clone(), item))
                .collect(),
        }
    }
    #[test]
    fn favorites_moved_row_keeps_preview_key_but_content_change_does_not() {
        let original = item("a", "one", false);
        let mut moved = item("a", "two", false);
        moved.children_count = 3;
        assert_eq!(preview_key(&original), preview_key(&moved));
        let mut edited = original.clone();
        edited.summary = "changed".into();
        assert_ne!(preview_key(&original), preview_key(&edited));
        let mut touched = original.clone();
        touched.update_time += chrono::Duration::seconds(1);
        assert_ne!(preview_key(&original), preview_key(&touched));
    }

    #[test]
    fn folder_tree_handles_collapsed_children_orphans_and_cycles() {
        let tree = tree(vec![
            item("a", "", true),
            item("b", "a", true),
            item("orphan", "missing", true),
            item("cycle1", "cycle2", true),
            item("cycle2", "cycle1", true),
        ]);
        let rows = tree.folder_rows(&HashSet::new());
        assert!(rows.iter().any(|row| row.id == "a"));
        // Collapsed descendants must stay collapsed, while disconnected cycles
        // are surfaced separately so malformed persisted data remains visible.
        assert!(rows.len() <= tree.items.len());
        let expanded = tree.items.keys().cloned().collect();
        let rows = tree.folder_rows(&expanded);
        assert_eq!(rows.iter().filter(|row| row.id == "b").count(), 1);
        assert_eq!(rows.iter().find(|row| row.id == "b").unwrap().depth, 1);
    }
    #[test]
    fn moving_a_folder_excludes_itself_and_all_descendants() {
        let tree = tree(vec![
            item("a", "", true),
            item("b", "a", true),
            item("c", "b", true),
            item("target", "", true),
            item("leaf", "c", false),
        ]);
        assert_eq!(tree.descendant_ids("a").len(), 4);
        assert_eq!(
            tree.move_choices("a")
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            vec!["target"]
        );
    }
    #[test]
    fn folder_search_respects_scope_and_core_pinyin_matching() {
        let mut chinese = item("a", "", true);
        chinese.title = "剪贴板".into();
        let tree = tree(vec![chinese, item("child", "a", true)]);
        assert_eq!(tree.matching_folders(None, "jiantieban").len(), 1);
        assert_eq!(
            tree.matching_folders(Some("a"), "")
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            vec!["child"]
        );
        assert!(tree.matching_folders(Some("missing"), "").is_empty());
    }

    #[test]
    fn flat_favorites_browse_direct_children_and_keep_real_root_order() {
        let mut tree = tree(vec![item("folder", "", true), item("root-text", "", false),
            item("nested", "folder", true), item("child", "folder", false)]);
        tree.ordered_root_items = Some(vec!["root-text".into(), "folder".into()]);
        tree.children_by_id.insert("folder".into(), vec!["child".into(), "nested".into()]);
        tree.children_by_id.insert("nested".into(), Vec::new());
        let root = tree.direct_page(None, 0, 100);
        assert_eq!(root.items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), vec!["root-text", "folder"]);
        let folder = tree.direct_page(Some("folder"), 0, 100);
        assert_eq!(folder.items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), vec!["child", "nested"]);
        assert!(tree.direct_page(Some("nested"), 0, 100).items.is_empty());
        assert!(!root.has_more);
    }

    #[test]
    fn flat_favorites_page_append_retains_child_order_and_existing_anchor_indices() {
        let mut rows = vec![item("folder", "", true)];
        for index in 0..125 {
            let mut row = item(&format!("entry-{index:03}"), "folder", false);
            row.update_time = chrono::DateTime::from_timestamp(index, 0).unwrap();
            rows.push(row);
        }
        let mut tree = tree(rows);
        let order: Vec<String> = (0..125).rev().map(|i| format!("entry-{i:03}")).collect();
        tree.children_by_id.insert("folder".into(), order.clone());
        let mut first = tree.direct_page(Some("folder"), 0, PAGE_SIZE);
        let anchor = first.items[96].id.clone();
        let second = tree.direct_page(Some("folder"), first.next_skip, PAGE_SIZE);
        first.items.extend(second.items);
        // Original order is not recency order. A newly touched older entry
        // cannot cross the persisted insertion position or the page boundary.
        first.items[105].update_time = chrono::DateTime::from_timestamp(10_000, 0).unwrap();
        sort_favorite_rows(&mut first.items, false);
        assert_eq!(first.items.iter().map(|row| &row.id).collect::<Vec<_>>(), order.iter().collect::<Vec<_>>());
        assert_eq!(first.items.iter().position(|row| row.id == anchor), Some(96));
        assert_eq!(first.items[105].id, order[105]);
        assert!(!second.has_more);
        assert_eq!(second.next_skip, 125);
    }

    #[test]
    fn drag_cycle_checks_follow_children_even_when_parent_id_names_another_occurrence() {
        let mut tree = tree(vec![item("a", "", true), item("b", "", true),
            item("shared", "b", true), item("descendant", "shared", true)]);
        tree.children_by_id.insert("a".into(), vec!["shared".into()]);
        tree.children_by_id.insert("shared".into(), vec!["descendant".into()]);
        tree.children_by_id.insert("descendant".into(), vec!["a".into()]);
        let descendants = tree.descendant_ids("a");
        assert!(descendants.contains("shared"));
        assert!(descendants.contains("descendant"));
        assert!(!descendants.contains("b"));
        // Explicitly empty membership wins over stale compatibility ParentId.
        tree.children_by_id.insert("shared".into(), Vec::new());
        assert!(!tree.descendant_ids("a").contains("descendant"));
    }
    #[test]
    fn favorite_display_strips_only_the_historical_internal_marker() {
        let mut row = item("a", "", false);
        row.title = "[favorites-add:123]  名称".into();
        assert_eq!(title(&row), "名称");
        row.title = "[favorites-add:abc] 名称".into();
        assert_eq!(title(&row), "[favorites-add:abc] 名称");
        assert_eq!(category(&row), Category::Text);
        row.kind = "Html".into();
        assert_eq!(category(&row), Category::Text);
    }
    #[test]
    fn route_query_decodes_unicode_and_preserves_malformed_sequences() {
        assert_eq!(decode_query("%E4%B8%AD%E6%96%87+%F0%9F%98%80"), "中文 😀");
        assert_eq!(decode_query("%invalid"), "%invalid");
        assert_eq!(decode_query("%FF"), "%FF");
    }

    #[test]
    fn f2_service_path_keeps_history_and_real_source_clip_id_in_isolated_store() {
        cliphost_core::force_headless(true);
        let dir = tempfile::tempdir().unwrap();
        let paths = cliphost_core::AppPaths::from_data_root(dir.path().join("favorites-f2"));
        let host = HostState::open(paths.clone(), 28965).unwrap();
        let text = "第一行 😀\n第二行";
        assert_eq!(
            add_text_to_history_and_favorites(&host, text).unwrap()["ok"],
            true
        );
        // Reopen the actual SQLite AppStore, rather than only checking a mocked
        // request body or DTO response which cannot establish persisted linkage.
        let store = cliphost_core::AppStore::open(&paths.db).unwrap();
        let history = store.list_clips().unwrap();
        let favorites = store.list_favors().unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(favorites.len(), 1);
        assert_eq!(history[0].body_text.as_deref(), Some(text));
        assert_eq!(favorites[0].body_text.as_deref(), Some(text));
        assert_eq!(
            favorites[0].source_clip_id.as_deref(),
            Some(history[0].id.as_str())
        );
        assert_eq!(favorites[0].source_process_name, "manual");
        assert!(favorites[0].parent_id.is_none());
    }

    #[test]
    fn timestamp_ties_keep_core_sub_index_and_parent_child_order_not_id() {
        cliphost_core::force_headless(true);
        let dir = tempfile::tempdir().unwrap();
        let paths = cliphost_core::AppPaths::from_data_root(dir.path().join("favorites-order"));
        paths.ensure().unwrap();
        let store = cliphost_core::AppStore::open(&paths.db).unwrap();
        let now = chrono::Utc::now();
        let clip = cliphost_core::ClipRecord::new_text("fixture", "manual");
        let mut folder = cliphost_core::FavorRecord::from_clip(&clip, None);
        folder.id = "folder".into();
        folder.is_folder = true;
        folder.children.clear();
        folder.create_time = now;
        folder.update_time = now;
        store.insert_favor(&folder).unwrap();
        for (index, id) in ["zzzz", "mmmm", "aaaa"].iter().enumerate() {
            let mut favorite =
                cliphost_core::FavorRecord::from_clip(&clip, Some(folder.id.clone()));
            favorite.id = (*id).into();
            favorite.create_time = now;
            favorite.update_time = now;
            favorite.sub_index = index as i32;
            store.insert_favor_checked(&favorite).unwrap();
        }
        assert_eq!(
            store.get_favor("folder").unwrap().children,
            vec!["zzzz", "mmmm", "aaaa"]
        );
        drop(store);
        let host = HostState::open(paths, 28965).unwrap();
        let mut page: FavoritePage = serde_json::from_value(
            host.favor_search(&json!({"parentId":"folder","take":100}))
                .unwrap(),
        )
        .unwrap();
        sort_favorite_rows(&mut page.items, false);
        assert_eq!(
            page.items
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            vec!["zzzz", "mmmm", "aaaa"]
        );
    }

    #[test]
    fn favorite_rows_use_hairline_style_and_their_own_hover_keys() {
        let settings = AppearanceSettings::from_config(&json!({"clipListAppearanceSlot":"custom",
            "clipListLayoutDensity":"relaxed","clipListCustomAppearance":{"chrome":"flush","layout":"standard",
            "showProcess":true,"showTime":true,"hoverActions":["favorite","pin"]}}));
        // DESIGN 2026-10-14: quiet hairline rows; only density follows the clipboard list.
        let effective = favorite_row_settings(&settings);
        assert_eq!(effective.resolved().layout, RowLayout::Minimal);
        assert_eq!(effective.resolved().chrome, appearance::ItemChrome::Hairline);
        assert_eq!(effective.density, settings.density);
        assert!(!effective.resolved().show_process);
        assert!(!effective.resolved().show_time);
        let defaults = FavoriteDisplay::default().hover;
        assert_eq!(
            favorite_hover_settings(&settings, false, &defaults)
                .resolved()
                .rendered_actions(false),
            vec![HoverAction::Primary, HoverAction::Edit, HoverAction::Delete]
        );
        assert_eq!(
            favorite_hover_settings(&settings, true, &defaults)
                .resolved()
                .rendered_actions(false),
            vec![HoverAction::Edit, HoverAction::Delete]
        );
        // Unchecked keys disappear; history-only keys never appear.
        let only_edit = vec!["edit".to_string(), "pin".to_string()];
        assert_eq!(
            favorite_hover_settings(&settings, false, &only_edit)
                .resolved()
                .rendered_actions(false),
            vec![HoverAction::Edit]
        );
        assert_eq!(
            appearance::hover_command(HoverAction::Primary, 1),
            HoverCommand::Copy
        );
        assert_eq!(
            appearance::hover_command(HoverAction::Primary, 2),
            HoverCommand::Paste
        );
        assert_eq!(
            appearance::hover_command(HoverAction::Primary, 3),
            HoverCommand::Paste
        );
    }

    #[test]
    fn list_content_normalizes_placeholders_paths_and_invalidates_same_timestamp() {
        let mut row = item("a", "", false);
        row.title = "[favorites-add:123]  title\nnext".into();
        row.summary = "【文本】".into();
        assert_eq!(normalized_text(&title(&row)), "title next");
        assert!(meaningful_summary(&row).is_empty());
        let old_key = preview_key(&row);
        row.summary = "new contents".into();
        assert_ne!(preview_key(&row), old_key);
        row.kind = "File".into();
        row.summary = "C:\\one.txt, C:\\two.png".into();
        assert_eq!(file_paths(&row), vec!["C:\\one.txt", "C:\\two.png"]);
        row.file_paths = vec![" C:\\actual.png ".into(), " ".into()];
        assert_eq!(file_paths(&row), vec!["C:\\actual.png"]);
        assert!(favorite_file_projection(&row).entries[0].is_image);
        row.file_paths = vec![
            " ".into(),
            " C:\\actual.png ".into(),
            String::new(),
            "C:\\actual.png".into(),
        ];
        let before = serde_json::to_value(&row).unwrap();
        let files = favorite_file_projection(&row);
        assert_eq!(
            files
                .entries
                .iter()
                .map(|entry| entry.original_index)
                .collect::<Vec<_>>(),
            [1, 3]
        );
        assert_eq!(favorite_file_source_paths(&row), row.file_paths);
        assert_eq!(serde_json::to_value(&row).unwrap(), before);
    }

    #[test]
    fn favorites_use_linked_text_for_html_and_steps_without_detail_structures() {
        for kind in ["Html", "HtmlClipItem", "StepClipItem"] {
            let mut row = item("linked", "", false);
            row.kind = kind.into();
            row.title = "  查看 https://example.test/help。  ".into();
            row.summary = "来源 https://example.test/source".into();
            let before = serde_json::to_value(&row).unwrap();
            let ContentProjection::Text(primary) = favorite_text_projection(&row) else {
                panic!("Favorites must use its list display text for HTML/steps");
            };
            assert_eq!(primary.text, "查看 https://example.test/help。");
            assert_eq!(primary.links.len(), 1);
            let secondary = favorite_secondary_projection(&row).unwrap();
            assert_eq!(secondary.text, "来源 https://example.test/source");
            assert_eq!(secondary.links.len(), 1);
            assert!(favorite_resource_projection(&row).is_none());
            assert_eq!(serde_json::to_value(&row).unwrap(), before);
        }
    }

    #[test]
    fn favorites_mixed_projection_keeps_text_image_file_order_and_raw_dto() {
        let mut row = item("mixed", "", false);
        row.kind = "QQRichTextItem".into();
        row.title = "消息".into();
        row.summary = "另一段 https://example.test/summary".into();
        row.im_rich_segments = vec![
            json!({"kind":"text","text":" 左 "}),
            json!({"kind":"image","path":"C:\\picture.png"}),
            json!({"kind":"text","text":"右"}),
            json!({"kind":"file","path":"C:\\folder\\说明.txt"}),
            json!({"kind":"placeholder","text":"[表情]"}),
        ];
        let before = serde_json::to_value(&row).unwrap();
        let ContentProjection::Mixed(parts) = favorite_text_projection(&row) else {
            panic!("QQ list DTO must use the shared inline mixed renderer");
        };
        assert_eq!(
            parts,
            vec![
                MixedPart::Text("左".into()),
                MixedPart::Image {
                    path: Some("C:\\picture.png".into())
                },
                MixedPart::Text("右".into()),
                MixedPart::File {
                    name: "说明.txt".into(),
                    path: "C:\\folder\\说明.txt".into()
                },
                MixedPart::Placeholder("[表情]".into()),
            ]
        );
        assert_eq!(favorite_secondary_projection(&row).unwrap().links.len(), 1);
        assert_eq!(serde_json::to_value(&row).unwrap(), before);
    }

    #[test]
    fn favorite_resources_invalidate_whole_dto_and_keep_special_row_precedence() {
        let mut row = item("same", "", false);
        row.kind = "QQRichTextItem".into();
        row.im_rich_segments = vec![json!({"kind":"image","path":"C:\\same.png"})];
        let first = favorite_resource_projection(&row).unwrap();
        row.summary = "same timestamp, new summary".into();
        assert_ne!(first, favorite_resource_projection(&row).unwrap());
        row.is_folder = true;
        assert!(matches!(
            favorite_text_projection(&row),
            ContentProjection::Text(_)
        ));
        assert!(favorite_resource_projection(&row).is_none());
        assert!(favorite_secondary_projection(&row).is_none());
        row.is_folder = false;
        row.kind = "Image".into();
        assert!(favorite_secondary_projection(&row).is_none());
        row.kind = "File".into();
        assert!(favorite_secondary_projection(&row).is_none());
    }

    #[test]
    fn favorite_secondary_links_skip_equal_text_and_placeholder_summaries() {
        let mut row = item("same", "", false);
        row.title = "https://example.test".into();
        row.summary = " https://example.test \n".into();
        assert!(favorite_secondary_projection(&row).is_none());
        row.summary = "【文本】".into();
        assert!(favorite_secondary_projection(&row).is_none());
        row.summary = "C:\\folder\\file.txt".into();
        let summary = favorite_secondary_projection(&row).unwrap();
        assert_eq!(
            summary.links[0].target,
            OpenTarget::ShellPath("C:\\folder\\file.txt".into())
        );
    }
}
