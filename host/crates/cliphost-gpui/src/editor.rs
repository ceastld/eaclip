//! History/favorite editor lifecycle and native source-preserving controls.
//! Every write is serialized and carries the last committed revision. A save
//! response may advance that baseline without replacing input typed meanwhile.
//! The application must await `save_and_close` before calling `App::quit`.

use gpui_rsx::rsx;
use std::{
    cell::RefCell,
    collections::{BTreeSet, HashMap, HashSet},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc, Arc, Mutex, OnceLock,
    },
    time::Instant,
};

use cliphost_core::{i18n::tr, HostState};
use gpui::{
    div, img, prelude::*, px, size, AnyElement, App, Bounds, Context, Entity, FontWeight,
    KeyDownEvent, MouseButton, Pixels, Point, ScrollHandle, Subscription, Window, WindowBounds,
    WindowHandle, WindowOptions,
};
use serde_json::{json, Map, Value};

use crate::{
    editor_html::html_to_plain_text,
    input::{InputColors, SearchEvent, SearchInput},
    preview::DesignTokens,
    view::color,
    window_background::{PreparedBackground, WindowBackgroundOptions},
};

#[path = "editor_paint.rs"]
mod paint_metadata;
#[path = "editor_picker_lease.rs"]
mod picker_lease;
#[path = "editor_text_history.rs"]
mod text_history;
#[path = "editor_style.rs"]
mod style;
#[path = "editor_properties.rs"]
mod properties;
#[path = "editor_content.rs"]
mod content;

#[path = "rich_content.rs"]
pub mod rich_content;
#[path = "rich_editor.rs"]
pub mod rich_editor;

use rich_content::RichOrigin;
use crate::smooth_wheel::SmoothWheelExt;
use rich_editor::{EditorTextEvent, EditorTextInput, RichEditor, RichEditorEvent, RichEditorKind};

pub(crate) fn minimum_width_dip() -> f32 {
    style::metric("minWidth", 320.)
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum EditorTarget {
    History(String),
    Favorite(String),
}

impl EditorTarget {
    pub fn id(&self) -> &str {
        match self {
            Self::History(id) | Self::Favorite(id) => id,
        }
    }
    fn is_favorite(&self) -> bool {
        matches!(self, Self::Favorite(_))
    }
    fn load(&self, host: &HostState) -> Result<Value, String> {
        let params = json!({ "id": self.id() });
        match self {
            Self::History(_) => host.detail(&params),
            Self::Favorite(_) => host.favor_get(&params),
        }
    }
    fn save(&self, host: &HostState, patch: Value) -> Result<Value, String> {
        match self {
            Self::History(_) => host.patch_item(&patch),
            Self::Favorite(_) => host.favor_update(&patch),
        }
    }
}

fn qa_fixture_content_allowed(
    target: &EditorTarget,
    fixture_ids: &BTreeSet<String>,
    debug: bool,
    headless: bool,
) -> bool {
    paint_metadata::fixture_content_allowed(
        matches!(target, EditorTarget::History(_)),
        target.id(),
        fixture_ids,
        debug,
        headless,
    )
}

fn text(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap_or_default().to_string()
}
fn title(value: &Value, favorite: bool) -> String {
    let value = text(value, "title");
    if favorite {
        if let Some(rest) = value.strip_prefix("[favorites-add:") {
            if let Some((number, title)) = rest.split_once(']') {
                if !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()) {
                    return title.trim_start().to_string();
                }
            }
        }
        value.trim_start().to_string()
    } else {
        value
    }
}

/// Same newline/comma import and case-insensitive deduplication as WebView.
pub fn parse_file_path_lines(raw: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    raw.split('\n')
        .flat_map(|line| line.split(','))
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .filter(|path| seen.insert(path.to_lowercase()))
        .map(str::to_string)
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DraftSnapshot {
    pub title: String,
    pub body: String,
    pub html: String,
    pub qq: String,
    pub file_paths: Vec<String>,
    pub execute_param: String,
    pub step_data: String,
    pub command: String,
    pub link_url: String,
    pub link_title: String,
}

impl DraftSnapshot {
    pub fn from_detail(detail: &Value, favorite: bool) -> Self {
        Self {
            title: title(detail, favorite),
            body: text(detail, "bodyText"),
            html: text(detail, "htmlData"),
            qq: text(detail, "qqRichText"),
            file_paths: detail["filePaths"]
                .as_array()
                .map(|paths| {
                    paths
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            execute_param: text(detail, "executeParam"),
            step_data: text(detail, "stepData"),
            command: text(detail, "command"),
            link_url: text(detail, "linkUrl"),
            link_title: text(detail, "linkTitle"),
        }
    }

    /// Accept normalization and derived fields independently. A newer title
    /// must not leave HTML's old derived body looking like a text conversion.
    fn merge_committed(&mut self, submitted: &Self, committed: &Self, composing: &[&str]) {
        macro_rules! merge {
            ($($field:ident),+ $(,)?) => { $(
                if self.$field == submitted.$field && !composing.contains(&stringify!($field)) {
                    self.$field.clone_from(&committed.$field);
                }
            )+ };
        }
        merge!(
            title,
            body,
            html,
            qq,
            file_paths,
            execute_param,
            step_data,
            command,
            link_url,
            link_title
        );
    }
}

/// Changed fields only. In particular title edits must never regenerate HTML,
/// QQ, image paths or IM segments, and HTML source edits keep SourceURL.
pub fn build_patch(target: &EditorTarget, detail: &Value, draft: &DraftSnapshot) -> Option<Value> {
    let original = DraftSnapshot::from_detail(detail, target.is_favorite());
    let mut patch = Map::new();
    if draft.title != original.title {
        patch.insert("title".into(), json!(draft.title));
    }
    let kind = detail["kind"].as_str().unwrap_or_default();
    match kind {
        "TextClipItem" if draft.body != original.body => {
            patch.insert("bodyText".into(), json!(draft.body));
        }
        "HtmlClipItem" if draft.html != original.html => {
            patch.insert("htmlData".into(), json!(draft.html));
            let body = html_to_plain_text(&draft.html);
            if body != original.body {
                patch.insert("bodyText".into(), json!(body));
            }
        }
        "HtmlClipItem" if draft.body != original.body => {
            // Explicit edits in the Text tab use the established WebView
            // conversion flow. Merely viewing it never drops original HTML.
            patch.insert("bodyText".into(), json!(draft.body));
            patch.insert("htmlData".into(), json!(""));
            patch.insert("htmlSourceUrl".into(), json!(""));
        }
        "QQRichTextItem" if draft.qq != original.qq => {
            // The core parses the original IM origin, embeds local images and
            // derives bodyText using the same outgoing payload pipeline.
            patch.insert("qqRichText".into(), json!(draft.qq));
        }
        "FileClipItem" => {
            let normalize = |paths: &[String]| {
                paths
                    .iter()
                    .map(|path| path.trim())
                    .filter(|path| !path.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            };
            let paths = normalize(&draft.file_paths);
            if paths != normalize(&original.file_paths) {
                patch.insert("filePaths".into(), json!(paths));
            }
        }
        "ActionClipItem"
            if target.is_favorite() && draft.execute_param != original.execute_param =>
        {
            patch.insert("executeParam".into(), json!(draft.execute_param));
        }
        "StepClipItem" if target.is_favorite() && draft.step_data != original.step_data => {
            patch.insert("stepData".into(), json!(draft.step_data));
        }
        "CmdClipItem" if target.is_favorite() && draft.command != original.command => {
            patch.insert("command".into(), json!(draft.command));
        }
        "LinkClipItem" if target.is_favorite() => {
            if draft.link_url != original.link_url {
                patch.insert("linkUrl".into(), json!(draft.link_url));
            }
            if draft.link_title != original.link_title {
                patch.insert("linkTitle".into(), json!(draft.link_title));
            }
        }
        _ => {}
    }
    if patch.is_empty() {
        return None;
    }
    patch.insert("id".into(), json!(target.id()));
    if let Some(revision) = detail["revision"]
        .as_str()
        .filter(|revision| !revision.is_empty())
    {
        patch.insert("expectedRevision".into(), json!(revision));
    }
    Some(Value::Object(patch))
}

/// UI acceptance order is the write order, including destruction during an
/// outstanding save. Merely locking background jobs would order their arrival
/// instead and could let a late old job overwrite the final draft.
struct WriteQueue {
    id: usize,
    sender: mpsc::Sender<WriteCommand>,
    failed_drafts: Arc<Mutex<HashMap<EditorTarget, FailedEditorDraft>>>,
    retired: Arc<AtomicBool>,
}

/// Emergency destruction failures remain in memory for recovery/reopening.
/// The normal save/close flow retains these in the visible editor as well.
#[derive(Debug, Clone)]
pub struct FailedEditorDraft {
    pub target: EditorTarget,
    pub original: Value,
    pub draft: DraftSnapshot,
    pub error: String,
}

static WRITE_QUEUES: OnceLock<Mutex<Vec<Arc<WriteQueue>>>> = OnceLock::new();
static NEXT_QUEUE_ID: AtomicUsize = AtomicUsize::new(1);

/// Await this after all live `save_and_close` operations succeed and before
/// quitting. This includes final writes from previously destroyed windows.
/// Callers keep the application alive and display any error this returns.
pub async fn flush_editor_writes() -> Result<(), String> {
    let queues = WRITE_QUEUES
        .get_or_init(Default::default)
        .lock()
        .map_err(|_| "Editor queue registry is unavailable.".to_string())?
        .clone();
    let mut errors = Vec::new();
    for queue in &queues {
        match queue.barrier().await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => errors.push(error),
            Err(_) if queue.retired.load(Ordering::Acquire) => {}
            Err(_) => errors.push("Editor write worker is unavailable.".into()),
        }
    }
    if has_active_editors() {
        errors.push(
            tr(
                "仍有编辑窗口打开，请先完成保存或关闭。",
                "Some editors are still open; finish saving or closing them first.",
            )
            .into(),
        );
    }
    if errors.is_empty() {
        // Retire completed queues once no live editor retains them.
        if let Ok(mut registered) = WRITE_QUEUES.get_or_init(Default::default).lock() {
            registered.retain(|queue| Arc::strong_count(queue) > 2);
        }
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

pub fn failed_editor_drafts() -> Vec<FailedEditorDraft> {
    WRITE_QUEUES
        .get_or_init(Default::default)
        .lock()
        .map(|queues| {
            queues
                .iter()
                .flat_map(|queue| {
                    queue
                        .failed_drafts
                        .lock()
                        .map(|drafts| drafts.values().cloned().collect::<Vec<_>>())
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default()
}

enum WriteCommand {
    Persist {
        target: EditorTarget,
        original: Value,
        draft: Box<DraftSnapshot>,
        response: Option<tokio::sync::oneshot::Sender<Result<Value, String>>>,
        release: Option<Arc<EditLease>>,
    },
    Forget(EditorTarget),
    Discard(EditorTarget),
    Release {
        target: EditorTarget,
        lease: Arc<EditLease>,
    },
    Barrier(tokio::sync::oneshot::Sender<Result<(), String>>),
    Retire,
}

impl WriteQueue {
    fn new(host: Arc<HostState>) -> Self {
        Self::with_writer(move |target, patch| target.save(&host, patch))
    }
    fn with_writer(
        write: impl Fn(&EditorTarget, Value) -> Result<Value, String> + Send + 'static,
    ) -> Self {
        let (sender, receiver) = mpsc::channel();
        let id = NEXT_QUEUE_ID.fetch_add(1, Ordering::Relaxed);
        let retired = Arc::new(AtomicBool::new(false));
        let finished = retired.clone();
        let failed_drafts = Arc::new(Mutex::new(HashMap::new()));
        let recovery = failed_drafts.clone();
        let spawned = std::thread::Builder::new().name("clip-editor-writes".into()).spawn(move || {
            let mut committed = HashMap::new();
            let mut failures = HashMap::new();
            while let Ok(command) = receiver.recv() {
                match command {
                    WriteCommand::Forget(target) => { committed.remove(&target); },
                    WriteCommand::Retire => {
                        if failures.is_empty() {
                            if let Ok(mut queues) = WRITE_QUEUES.get_or_init(Default::default).lock() { queues.retain(|queue| queue.id != id); }
                            finished.store(true, Ordering::Release);
                            while let Ok(WriteCommand::Barrier(response)) = receiver.try_recv() { let _ = response.send(Ok(())); }
                            break;
                        }
                    }
                    WriteCommand::Discard(target) => {
                        failures.remove(&target);
                        if let Ok(mut drafts) = recovery.lock() { drafts.remove(&target); }
                    }
                    WriteCommand::Release { target, lease } => {
                        if !failures.contains_key(&target) { lease.release(); }
                    }
                    WriteCommand::Barrier(response) => {
                        let result = failures.values().next().cloned().map_or(Ok(()), Err);
                        let _ = response.send(result);
                    }
                    WriteCommand::Persist { target, original, draft, response, release } => {
                        let base = committed.get(&target).cloned().unwrap_or(original);
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            if let Some(patch) = build_patch(&target, &base, &draft) { write(&target, patch) } else { Ok(base.clone()) }
                        })).unwrap_or_else(|_| Err("Editor write worker panicked; draft was not saved.".into()));
                        match &result {
                            Ok(detail) => {
                                committed.insert(target.clone(), detail.clone()); failures.remove(&target);
                                if let Ok(mut drafts) = recovery.lock() { drafts.remove(&target); }
                            }
                            Err(error) => {
                                if let Ok(mut drafts) = recovery.lock() { drafts.insert(target.clone(), FailedEditorDraft {
                                    target: target.clone(), original: base, draft: *draft, error: error.clone(),
                                }); }
                                failures.insert(target, error.clone());
                            }
                        }
                        if let Some(lease) = release {
                            match &result {
                                Ok(_) => lease.release(),
                                Err(error) => log::warn!("Native editor final save failed; edit session retained: {error}"),
                            }
                        }
                        if let Some(response) = response { let _ = response.send(result); }
                    }
                }
            }
        });
        if let Err(error) = spawned {
            log::warn!("Native editor write worker could not start: {error}");
        }
        Self {
            id,
            sender,
            failed_drafts,
            retired,
        }
    }
    fn persist(
        &self,
        target: EditorTarget,
        original: Value,
        draft: DraftSnapshot,
    ) -> tokio::sync::oneshot::Receiver<Result<Value, String>> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let _ = self.sender.send(WriteCommand::Persist {
            target,
            original,
            draft: Box::new(draft),
            response: Some(sender),
            release: None,
        });
        receiver
    }
    fn forget(&self, target: &EditorTarget) {
        let _ = self.sender.send(WriteCommand::Forget(target.clone()));
    }
    fn discard(&self, target: &EditorTarget) {
        let _ = self.sender.send(WriteCommand::Discard(target.clone()));
    }
    fn retire(&self) {
        let _ = self.sender.send(WriteCommand::Retire);
    }
    fn release_after_writes(&self, target: EditorTarget, lease: Arc<EditLease>) {
        let _ = self.sender.send(WriteCommand::Release { target, lease });
    }
    /// Ordered behind every accepted write, including a destruction flush.
    /// Failed writes remain observable until that target is saved successfully.
    fn barrier(&self) -> tokio::sync::oneshot::Receiver<Result<(), String>> {
        let (response, receiver) = tokio::sync::oneshot::channel();
        if self.retired.load(Ordering::Acquire) {
            let _ = response.send(Ok(()));
            return receiver;
        }
        if let Err(error) = self.sender.send(WriteCommand::Barrier(response)) {
            if let WriteCommand::Barrier(response) = error.0 {
                let result = if self.retired.load(Ordering::Acquire) {
                    Ok(())
                } else {
                    Err("Editor write worker is unavailable.".into())
                };
                let _ = response.send(result);
            }
        }
        receiver
    }
    fn final_flush(
        &self,
        target: EditorTarget,
        original: Value,
        draft: DraftSnapshot,
        lease: Arc<EditLease>,
    ) {
        if self
            .sender
            .send(WriteCommand::Persist {
                target,
                original,
                draft: Box::new(draft),
                response: None,
                release: Some(lease),
            })
            .is_err()
        {
            log::warn!("Native editor final save could not be queued; edit session retained.");
        }
    }
}

struct EditLease {
    host: Arc<HostState>,
    session: String,
    released: AtomicBool,
}
impl EditLease {
    fn begin(host: Arc<HostState>) -> Result<Arc<Self>, String> {
        let session = text(&host.create_session(), "sessionId");
        if session.is_empty() {
            return Err("Core did not create an edit session.".into());
        }
        Ok(Arc::new(Self {
            host,
            session,
            released: AtomicBool::new(false),
        }))
    }
    fn release(&self) {
        if !self.released.swap(true, Ordering::AcqRel) {
            if let Err(error) = self
                .host
                .delete_session(&json!({ "sessionId": self.session }))
            {
                log::warn!("Native editor session release failed: {error}");
            }
        }
    }
}

struct Loaded {
    detail: Value,
    lease: Arc<EditLease>,
    image: Option<PathBuf>,
    image_size: Option<(u32, u32)>,
    folder_children: Vec<Value>,
}

#[derive(Clone)]
struct FilePathDrag {
    target: EditorTarget,
    generation: u64,
    version: u64,
    index: usize,
    label: String,
    position: Point<Pixels>,
    background: gpui::Hsla,
    foreground: gpui::Hsla,
}
impl Render for FilePathDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        rsx! {
            <div pl={self.position.x} pt={self.position.y}>
                <div p={px(6.)} rounded={px(4.)} bg={self.background} text_color={self.foreground}>
                    {self.label.clone()}
                </div>
            </div>
        }
    }
}

fn reorder_file_path(
    paths: &mut Vec<String>,
    from: usize,
    target: usize,
    before: bool,
) -> Option<usize> {
    if from >= paths.len() || target >= paths.len() {
        return None;
    }
    let mut insertion = target + usize::from(!before);
    if from < insertion {
        insertion -= 1;
    }
    if from != insertion {
        let path = paths.remove(from);
        paths.insert(insertion, path);
    }
    Some(insertion)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContentTab {
    Body,
    Html,
    Qq,
    Rich,
}

enum LeaveRequest {
    Close(u64),
    Switch(EditorTarget),
    SwitchDiscard(EditorTarget),
}

/// A list can keep sending the selected item while a save or IME composition
/// is in flight. Keep the newest intent, and do not turn a failed save into a
/// retry loop merely because the list still has that same item selected.
#[derive(Default)]
struct SelectionFollower {
    requested: Option<EditorTarget>,
    suspended: bool,
}

impl SelectionFollower {
    fn request(&mut self, target: EditorTarget) {
        if self.requested.as_ref() != Some(&target) {
            self.requested = Some(target);
            self.suspended = false;
        }
    }

    fn loaded(&mut self, target: &EditorTarget) {
        if self.requested.as_ref() == Some(target) {
            self.clear();
        }
    }

    fn clear(&mut self) {
        self.requested = None;
        self.suspended = false;
    }

    fn next(&mut self, current: &EditorTarget, blocked: bool) -> Option<EditorTarget> {
        if self.requested.as_ref() == Some(current) {
            self.clear();
            return Some(current.clone());
        }
        if blocked || self.suspended {
            None
        } else {
            self.requested.clone()
        }
    }
}

static ACTIVE_EDITORS: AtomicUsize = AtomicUsize::new(0);
static ACTIVE_INPUT_WINDOWS: AtomicUsize = AtomicUsize::new(0);

pub type EditorCloseResult = tokio::sync::oneshot::Receiver<Result<(), String>>;

/// Caller-visible state; a failed/composing editor must remain open on Exit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorSaveState {
    Loading,
    Editing,
    Saving,
    Failed(String),
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolbarCloseBlock {
    Loading,
    Saving,
    Switching,
    Picking,
    Composing,
    Unapplied,
    Unsaved,
}

#[derive(Default)]
struct ToolbarCloseState {
    loading: bool,
    saving: bool,
    switching: bool,
    picking: bool,
    composing: bool,
    unapplied: bool,
    dirty: bool,
}

impl ToolbarCloseState {
    fn blocked_by(&self) -> Option<ToolbarCloseBlock> {
        [
            (self.loading, ToolbarCloseBlock::Loading),
            (self.saving, ToolbarCloseBlock::Saving),
            (self.switching, ToolbarCloseBlock::Switching),
            (self.picking, ToolbarCloseBlock::Picking),
            (self.composing, ToolbarCloseBlock::Composing),
            (self.unapplied, ToolbarCloseBlock::Unapplied),
            (self.dirty, ToolbarCloseBlock::Unsaved),
        ]
        .into_iter()
        .find_map(|(blocked, reason)| blocked.then_some(reason))
    }
}

fn unapplied_bulk_paths(kind: &str, bulk: &str, paths: &[String]) -> bool {
    kind == "FileClipItem"
        && parse_file_path_lines(bulk).iter().any(|candidate| {
            !paths
                .iter()
                .any(|path| path.trim().eq_ignore_ascii_case(candidate.trim()))
        })
}

/// Save/exit accounting includes real item editors only. Other input panes use
/// the shared protection below without appearing as an unfinished item edit.
pub fn has_active_editors() -> bool {
    ACTIVE_EDITORS.load(Ordering::Acquire) > 0
}

/// Both editors and the LAN composer protect real input from the clipboard's
/// keyboard routing and automatic dismiss. Only editors participate in saves.
pub(crate) fn has_active_input_windows() -> bool {
    ACTIVE_INPUT_WINDOWS.load(Ordering::Acquire) > 0
}

pub(crate) struct EditorProtection {
    host: Arc<HostState>,
    editor: bool,
}
impl EditorProtection {
    pub(crate) fn acquire(host: Arc<HostState>) -> Self {
        ACTIVE_EDITORS.fetch_add(1, Ordering::AcqRel);
        Self::acquire_kind(host, true)
    }

    pub(crate) fn acquire_companion(host: Arc<HostState>) -> Self {
        Self::acquire_kind(host, false)
    }

    fn acquire_kind(host: Arc<HostState>, editor: bool) -> Self {
        ACTIVE_INPUT_WINDOWS.fetch_add(1, Ordering::AcqRel);
        host.chrome_web_action(&json!({ "kind":"clipboardHostChrome", "sourceHostWindow":"clipboard", "route":"/clipboard", "isClipboardEditing":true }));
        host.apply_window_action_for("setSpaModalOpen", true, "clipboard");
        Self { host, editor }
    }
}
impl Drop for EditorProtection {
    fn drop(&mut self) {
        if self.editor { ACTIVE_EDITORS.fetch_sub(1, Ordering::AcqRel); }
        if ACTIVE_INPUT_WINDOWS.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.host.chrome_web_action(&json!({ "kind":"clipboardHostChrome", "sourceHostWindow":"clipboard", "route":"/clipboard", "isClipboardEditing":false }));
            self.host
                .apply_window_action_for("setSpaModalOpen", false, "clipboard");
        }
    }
}

pub struct EditorWindow {
    host: Arc<HostState>,
    base_tokens: DesignTokens,
    tokens: DesignTokens,
    dark: bool,
    background_snapshot: Option<Arc<PreparedBackground>>,
    background_options: WindowBackgroundOptions,
    target: EditorTarget,
    detail: Option<Value>,
    folder_children: Vec<Value>,
    draft: DraftSnapshot,
    lease: Option<Arc<EditLease>>,
    writes: Arc<WriteQueue>,
    generation: u64,
    edit_version: u64,
    saving: bool,
    loading: bool,
    leave: Option<LeaveRequest>,
    selection_follower: SelectionFollower,
    error: Option<String>,
    tab: ContentTab,
    props_open: bool,
    properties_view: Entity<properties::EditorProperties>,
    self_handle: gpui::WeakEntity<Self>,
    ui_hovered: HashSet<gpui::ElementId>,
    ui_motion: RefCell<crate::motion::Tweens<gpui::ElementId>>,
    title_input: Entity<EditorTextInput>,
    body_input: Entity<EditorTextInput>,
    body_context: Option<(u64, ContentTab, bool)>,
    rich_input: Entity<RichEditor>,
    link_title_input: Entity<EditorTextInput>,
    path_inputs: Vec<Entity<SearchInput>>,
    _input_subscriptions: Vec<Subscription>,
    path_subscriptions: Vec<Subscription>,
    file_bulk: String,
    file_bulk_open: bool,
    path_picker: picker_lease::EditorPickerState,
    path_drop: Option<(usize, bool)>,
    path_rebuild_pending: bool,
    scroll: ScrollHandle,
    path_scroll: ScrollHandle,
    image: Option<PathBuf>,
    image_size: Option<(u32, u32)>,
    image_scale: Option<f32>,
    image_pan: (f32, f32),
    image_drag: Option<Point<Pixels>>,
    image_bounds: Arc<Mutex<Option<Bounds<Pixels>>>>,
    released: bool,
    protection: Option<EditorProtection>,
    exit_waiters: Vec<tokio::sync::oneshot::Sender<Result<(), String>>>,
    paint_metadata: paint_metadata::EditorPaintMetadata,
}

impl EditorWindow {
    pub fn show(
        existing: &mut Option<WindowHandle<Self>>,
        host: Arc<HostState>,
        tokens: DesignTokens,
        dark: bool,
        item_id: String,
        cx: &mut App,
    ) -> Result<(), String> {
        Self::show_target(
            existing,
            host,
            tokens,
            dark,
            EditorTarget::History(item_id),
            cx,
        )
    }

    pub fn show_favorite(
        existing: &mut Option<WindowHandle<Self>>,
        host: Arc<HostState>,
        tokens: DesignTokens,
        dark: bool,
        item_id: String,
        cx: &mut App,
    ) -> Result<(), String> {
        Self::show_target(
            existing,
            host,
            tokens,
            dark,
            EditorTarget::Favorite(item_id),
            cx,
        )
    }

    pub fn show_target(
        existing: &mut Option<WindowHandle<Self>>,
        host: Arc<HostState>,
        tokens: DesignTokens,
        dark: bool,
        target: EditorTarget,
        cx: &mut App,
    ) -> Result<(), String> {
        if target.id().trim().is_empty() {
            return Err(tr("未选择条目", "No item is selected").into());
        }
        let background_qa = cfg!(debug_assertions) && cliphost_core::is_headless();
        if let Some(handle) = existing {
            if let Ok(reused) = handle.update(cx, |this, window, cx| {
                if this.is_closed() {
                    return Ok(false);
                }
                this.set_design_tokens(tokens.clone(), dark, cx);
                this.select(target.clone(), window, cx);
                crate::platform::dock_editor(window, host.host_hwnd())?;
                if background_qa {
                    crate::platform::register_probe_window(window)
                        .and_then(|_| crate::platform::set_no_activate(window, true))
                        .and_then(|_| crate::platform::show_no_activate(window))?;
                } else {
                    crate::platform::show(window, true, true)?;
                }
                Ok::<_, String>(true)
            }) {
                if reused? {
                    return Ok(());
                }
            }
        }
        let bounds = Bounds::centered(None, size(px(style::metric("width", 480.)), px(600.)), cx);
        let owner = host.host_hwnd();
        let handle = cx
            .open_window(
                WindowOptions {
                    titlebar: None,
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    window_min_size: Some(size(px(style::metric("minWidth", 320.)), px(250.))),
                    focus: !background_qa,
                    show: false,
                    ..Default::default()
                },
                move |window, cx| {
                    window.set_window_title(tr("编辑剪贴板", "Edit clipboard item"));
                    if let Err(error) = crate::platform::register_probe_window(window) {
                        log::warn!("Editor window probe registration failed: {error}");
                    }
                    let entity = cx.new(|cx| Self::new(host, tokens, dark, target, window, cx));
                    let weak = entity.downgrade();
                    window.on_window_should_close(cx, move |window, cx| {
                        if let Some(entity) = weak.upgrade() {
                            entity.update(cx, |this, cx| this.request_close(window, cx));
                            false
                        } else {
                            true
                        }
                    });
                    entity
                },
            )
            .map_err(|error| error.to_string())?;
        handle
            .update(cx, |_, window, _| {
                // Hidden GPUI windows defer their initial placement until
                // GPUI shows them. Our native show must apply it first.
                let result = crate::platform::set_geometry(window, None, Some((f64::from(style::metric("width", 480.)), 600.)))
                    .and_then(|_| crate::platform::dock_editor(window, owner))
                    .and_then(|_| {
                        if background_qa {
                            crate::platform::register_probe_window(window)
                                .and_then(|_| crate::platform::set_no_activate(window, true))
                                .and_then(|_| crate::platform::show_no_activate(window))
                        } else {
                            crate::platform::show(window, true, true)
                        }
                    });
                if result.is_err() {
                    window.remove_window();
                }
                result
            })
            .map_err(|error| error.to_string())??;
        *existing = Some(handle);
        Ok(())
    }

    /// Accept resolved base tokens; visual refresh never reloads editor inputs.
    pub fn set_design_tokens(&mut self, tokens: DesignTokens, dark: bool, cx: &mut Context<Self>) {
        if self.released {
            return;
        }
        self.base_tokens = tokens;
        self.dark = dark;
        self.refresh_window_surfaces(cx);
    }

    pub fn set_window_background(
        &mut self,
        snapshot: Option<Arc<PreparedBackground>>,
        options: WindowBackgroundOptions,
        base_tokens: DesignTokens,
        dark: bool,
        cx: &mut Context<Self>,
    ) {
        if self.released {
            return;
        }
        self.background_snapshot = snapshot;
        self.background_options = options;
        self.base_tokens = base_tokens;
        self.dark = dark;
        self.refresh_window_surfaces(cx);
    }

    fn refresh_window_surfaces(&mut self, cx: &mut Context<Self>) {
        // Derive once from the authoritative base, including its Custom alpha.
        // A configured path without a prepared snapshot uses the normal theme.
        self.tokens = if self.background_snapshot.is_some() {
            crate::window_glass::tokens(&self.base_tokens, &self.background_options)
        } else {
            self.base_tokens.clone()
        };
        self.refresh_input_colors(cx);
        cx.notify();
    }

    fn refresh_input_colors(&mut self, cx: &mut Context<Self>) {
        let theme = self.tokens.theme(self.dark);
        let colors = InputColors {
            background: color(&theme.search_background),
            foreground: color(&theme.text),
            placeholder: color(&theme.text_muted),
            border: color(&theme.border),
            selection: color(&theme.selected),
            caret: color(&theme.accent),
        };
        // Web: HTML container uses tertiary normally and secondary glass with
        // wallpaper. QQ has no wallpaper override for its tertiary surface.
        let html_background = if self.background_snapshot.is_some() {
            color(&theme.secondary)
        } else {
            color(&self.base_tokens.theme(self.dark).tertiary)
        };
        let im_background = color(&self.base_tokens.theme(self.dark).tertiary);
        let code_background = color(&theme.secondary);
        for input in [&self.title_input, &self.link_title_input] {
            input.update(cx, |input, cx| {
                input.set_theme_colors(colors, code_background, cx)
            });
        }
        self.refresh_body_colors(cx);
        for input in &self.path_inputs {
            input.update(cx, |input, cx| input.set_colors(colors, cx));
        }
        let tokens = self.tokens.clone();
        let dark = self.dark;
        self.rich_input.update(cx, |input, cx| {
            input.set_design_tokens(tokens, dark, cx);
            input.set_surface_backgrounds(html_background, im_background, cx);
        });
    }

    fn refresh_body_colors(&mut self, cx: &mut Context<Self>) {
        let theme = self.tokens.theme(self.dark);
        let background = if self.background_snapshot.is_some() {
            color(&theme.search_background)
        } else if self.kind() == "FileClipItem" {
            // File bulk paths use the Web textarea's normal tertiary surface.
            color(&self.base_tokens.theme(self.dark).tertiary)
        } else if matches!(self.kind(), "TextClipItem" | "HtmlClipItem" | "QQRichTextItem")
            || matches!(self.tab, ContentTab::Html | ContentTab::Qq)
        {
            // CodeMirror's no-wallpaper canvas is fixed, independent of Custom
            // shell colors. With wallpaper it uses host-glass-secondary above.
            color(if self.dark { "#1e1e1e" } else { "#ffffff" })
        } else {
            color(&theme.search_background)
        };
        let colors = InputColors {
            background,
            foreground: color(&theme.text),
            placeholder: color(&theme.text_muted),
            border: color(&theme.border),
            selection: color(&theme.selected),
            caret: color(&theme.accent),
        };
        let code_background = color(&theme.secondary);
        self.body_input.update(cx, |input, cx| {
            input.set_theme_colors(colors, code_background, cx)
        });
    }

    /// Core/window shutdown requests still wait for autosave and report errors.
    pub fn close(handle: WindowHandle<Self>, cx: &mut App) {
        let _ = handle.update(cx, |this, window, cx| this.request_close(window, cx));
    }

    /// The clipboard toolbar is a visibility toggle, not a save/cancel action.
    /// Only a settled, unchanged editor may close through this entry point.
    /// A blocked toggle leaves all drafts, pending selections and writes intact.
    pub fn toggle_close_if_clean(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<bool, String> {
        if self.released {
            return Ok(true);
        }
        let state = ToolbarCloseState {
            loading: self.loading,
            saving: self.saving,
            switching: self.leave.is_some()
                || !self.exit_waiters.is_empty()
                || self
                    .selection_follower
                    .requested
                    .as_ref()
                    .is_some_and(|target| target != &self.target),
            picking: self.path_picker.is_pending(),
            composing: self.composing(cx),
            unapplied: self.rich_input.read(cx).unapplied_error().is_some()
                || unapplied_bulk_paths(self.kind(), &self.file_bulk, &self.draft.file_paths),
            dirty: self.dirty(),
        };
        if state.blocked_by().is_some() {
            // The caller shows a toolbar notice. Preserve genuine load/save
            // errors; a blocked visibility toggle is not a failed write.
            cx.notify();
            return Ok(false);
        }
        self.selection_follower.clear();
        self.leave = Some(LeaveRequest::Close(self.edit_version));
        // The regular finalizer releases the lease and protection. Calling
        // request_close here would start an autosave even for this clean path.
        self.finish_leave(self.edit_version, window, cx);
        Ok(self.released)
    }

    /// Start the regular revision-checked save/close flow and await its result.
    /// This does not block the GPUI thread. Quit only after `Ok(())`; errors are
    /// displayed by the editor and its draft/session remain available to retry.
    /// Repeat Exit requests share the current operation, rather than requeueing
    /// an old draft. Input committed during the save is flushed before success.
    pub fn save_and_close(handle: WindowHandle<Self>, cx: &mut App) -> EditorCloseResult {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let mut sender = Some(sender);
        if let Err(error) = handle.update(cx, |this, window, cx| {
            let sender = sender.take().unwrap();
            if this.released {
                let _ = sender.send(Ok(()));
            } else if this.composing(cx) {
                let error = tr(
                    "请先完成输入法组合再退出。",
                    "Finish composing text before exiting.",
                )
                .to_string();
                this.error = Some(error.clone());
                let _ = sender.send(Err(error));
                cx.notify();
            } else {
                this.exit_waiters.push(sender);
                this.request_close(window, cx);
            }
        }) {
            if let Some(sender) = sender {
                // A stale handle is not proof that its destruction flush has
                // completed. The caller must not treat this as a saved draft.
                let _ = sender.send(Err(format!("Editor window is unavailable: {error}")));
            }
        }
        receiver
    }

    pub fn save_state(&self) -> EditorSaveState {
        if self.released {
            EditorSaveState::Closed
        } else if let Some(error) = &self.error {
            EditorSaveState::Failed(error.clone())
        } else if self.saving {
            EditorSaveState::Saving
        } else if self.loading {
            EditorSaveState::Loading
        } else {
            EditorSaveState::Editing
        }
    }
    /// Read-only QA metadata. No source, title, path, or draft values leave the
    /// editor; geometry comes from the native controls' last painted layout.
    pub fn diagnostics(&self, cx: &App) -> Value {
        let paint = self.paint_metadata.diagnostics();
        json!({
            "schemaVersion":1,"loading":self.loading,"saving":self.saving,
            "saveState":match self.save_state() {
                EditorSaveState::Loading=>"loading",EditorSaveState::Editing=>"editing",EditorSaveState::Saving=>"saving",
                EditorSaveState::Failed(_)=>"failed",EditorSaveState::Closed=>"closed",
            },
            "target":{"type":if self.target.is_favorite(){"favorite"}else{"history"},"id":self.target.id()},
            "generation":self.generation,"editVersion":self.edit_version,"documentKind":self.kind(),
            "visibleTab":match self.tab {ContentTab::Body=>"body",ContentTab::Html=>"html-source",ContentTab::Qq=>"qq-source",ContentTab::Rich=>"rich"},
            "visiblePage":if self.props_open {"properties"} else {"content"},
            "composing":self.composing(cx),"hasError":self.error.is_some(),"closed":self.released,
            "pendingSelectionTarget":self.selection_follower.requested.as_ref().map(EditorTarget::id),
            "selectionFollowingPaused":self.selection_follower.suspended,
            "hwnd":paint["hwnd"],"paintFrameId":paint["paintFrameId"],"paintCompleted":paint["paintCompleted"],
            "paint":paint,"controls":paint["controls"],
            "richEditor":self.rich_input.read(cx).diagnostics(cx),
            "inputs":{"title":self.title_input.read(cx).diagnostics(),"body":self.body_input.read(cx).diagnostics(),
                "linkTitle":self.link_title_input.read(cx).diagnostics()},
        })
    }
    /// Exact content is available only to the Debug/headless authored-fixture
    /// harness. Its caller also verifies the isolated seed, nonce and root HWND.
    /// Ordinary diagnostics and unauthorized targets never include these values.
    pub fn content_qa_diagnostics(&self, fixture_ids: &BTreeSet<String>, cx: &App) -> Value {
        if !qa_fixture_content_allowed(
            &self.target,
            fixture_ids,
            cfg!(debug_assertions),
            cliphost_core::is_headless(),
        ) {
            return json!({"available":false,"reason":"authored-debug-history-fixture-required"});
        }
        if self.loading
            || self.released
            || self
                .detail
                .as_ref()
                .is_none_or(|detail| text(detail, "id") != self.target.id())
        {
            return json!({"available":false,"reason":"fixture-session-not-loaded"});
        }
        let source = match self.kind() {
            "HtmlClipItem" => &self.draft.html,
            "QQRichTextItem" => &self.draft.qq,
            _ => &self.draft.body,
        };
        json!({"available":true,"target":{"type":"history","id":self.target.id()},"generation":self.generation,"dirty":self.dirty(),
            "documentKind":self.kind(),"revision":self.detail.as_ref().map(|detail|detail["revision"].clone()),
            "draftSource":source,"draftHtml":self.draft.html,"draftXml":self.draft.qq,"draftBody":self.draft.body,
            "error":self.error,"richEditor":if self.tab==ContentTab::Rich {
                self.rich_input.read(cx).content_qa_diagnostics(source,cx)
            }else{json!({"available":false,"reason":"rich-surface-not-current"})}})
    }
    fn validate_qa_rich_session(
        &self,
        expected_generation: u64,
        expected_frame: u64,
        window: &Window,
        cx: &App,
    ) -> Result<(), String> {
        if !cfg!(debug_assertions) {
            return Err("QA history requires a Debug acceptance harness.".into());
        }
        if self.released || self.loading || self.saving || self.composing(cx) {
            return Err("Editor is closed, busy, or composing.".into());
        }
        if self.generation != expected_generation
            || self.tab != ContentTab::Rich
            || !matches!(self.kind(), "HtmlClipItem" | "QQRichTextItem")
        {
            return Err("Editor session or edit surface changed.".into());
        }
        let paint = self
            .paint_metadata
            .snapshot()
            .ok_or("Editor paint metadata is unavailable")?;
        if !paint.paint_completed
            || paint.frame_id != expected_frame
            || expected_frame == 0
            || paint.generation != self.generation
            || paint.document_kind != self.kind()
            || paint.tab != "rich"
        {
            return Err("Editor paint frame changed or is incomplete.".into());
        }
        if !paint.viewport_matches(window.viewport_size(), window.scale_factor()) {
            return Err("Editor viewport changed since the painted frame.".into());
        }
        #[cfg(windows)]
        if crate::platform::hwnd(window).map(|hwnd| hwnd.0 as isize)? != paint.hwnd
            || paint.hwnd == 0
        {
            return Err("Editor root window changed.".into());
        }
        Ok(())
    }

    pub fn apply_qa_history(
        &mut self,
        redo: bool,
        expected_generation: u64,
        expected_frame: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.validate_qa_rich_session(expected_generation, expected_frame, window, cx)?;
        let source = match self.kind() {
            "HtmlClipItem" => self.draft.html.clone(),
            _ => self.draft.qq.clone(),
        };
        self.rich_input.update(cx, |input, cx| {
            input.apply_qa_history(redo, expected_frame, &source, window, cx)
        })
    }

    /// Wait for writes already accepted by this editor. Application Exit also
    /// needs `flush_editor_writes` after all live windows have saved/closed.
    pub fn write_barrier(&self) -> EditorCloseResult {
        self.writes.barrier()
    }

    fn resolve_exit(&mut self, result: Result<(), String>) {
        for waiter in self.exit_waiters.drain(..) {
            let _ = waiter.send(result.clone());
        }
    }

    pub fn is_closed(&self) -> bool {
        self.released
    }

    #[cfg(debug_assertions)]
    pub fn set_visual_fixture_page(&mut self, properties: bool, cx: &mut Context<Self>) {
        self.props_open = properties;
        cx.notify();
    }

    fn new(
        host: Arc<HostState>,
        tokens: DesignTokens,
        dark: bool,
        target: EditorTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let paint_metadata = paint_metadata::EditorPaintMetadata::default();
        let title_input =
            cx.new(|cx| EditorTextInput::new_plain(tokens.clone(), dark, true, window, cx));
        let body_input =
            cx.new(|cx| EditorTextInput::new_plain(tokens.clone(), dark, false, window, cx));
        let rich_input = cx.new(|cx| RichEditor::new(tokens.clone(), dark, window, cx));
        let link_title_input =
            cx.new(|cx| EditorTextInput::new_plain(tokens.clone(), dark, true, window, cx));
        let properties_view = cx.new(|cx| properties::EditorProperties::new(tokens.clone(), dark, cx));
        for input in [&title_input, &body_input, &link_title_input] {
            input.update(cx, |input, _| {
                input.set_paint_clock(paint_metadata.clock.clone())
            });
        }
        rich_input.update(cx, |input, cx| {
            input.set_paint_clock(paint_metadata.clock.clone(), cx)
        });
        let input_subscriptions = vec![
            cx.subscribe_in(
                &title_input,
                window,
                |this, input, event, window, cx| match event {
                    EditorTextEvent::Edited { .. } => {
                        this.draft.title = input.read(cx).text().to_string();
                        // Both fields map to the existing favorite name. The
                        // confirmed editor exposes a single title for links.
                        if this.target.is_favorite() && this.kind() == "LinkClipItem" {
                            this.draft.link_title.clone_from(&this.draft.title);
                        }
                        this.changed(cx);
                    }
                    EditorTextEvent::Save => this.save(window, cx),
                    EditorTextEvent::Escape => this.request_close(window, cx),
                    _ => {}
                },
            ),
            cx.subscribe_in(
                &body_input,
                window,
                |this, input, event, window, cx| match event {
                    EditorTextEvent::Edited { .. } => {
                        let value = input.read(cx).text().to_string();
                        let kind = this.kind();
                        match this.tab {
                            ContentTab::Html => {
                                this.draft.html = value.clone();
                            }
                            ContentTab::Qq => this.draft.qq = value.clone(),
                            ContentTab::Rich => return,
                            ContentTab::Body => match kind {
                                "ActionClipItem" => this.draft.execute_param = value.clone(),
                                "StepClipItem" => this.draft.step_data = value.clone(),
                                "CmdClipItem" => this.draft.command = value.clone(),
                                "LinkClipItem" => this.draft.link_url = value.clone(),
                                "FileClipItem" => {
                                    this.file_bulk = value.clone();
                                    cx.notify();
                                    return;
                                }
                                _ => this.draft.body = value.clone(),
                            },
                        }
                        this.changed(cx);
                    }
                    EditorTextEvent::Save => this.save(window, cx),
                    EditorTextEvent::Escape => this.request_close(window, cx),
                    _ => {}
                },
            ),
            cx.subscribe_in(
                &link_title_input,
                window,
                |this, input, event, window, cx| match event {
                    EditorTextEvent::Edited { .. } => {
                        this.draft.link_title = input.read(cx).text().to_string();
                        this.changed(cx);
                    }
                    EditorTextEvent::Save => this.save(window, cx),
                    EditorTextEvent::Escape => this.request_close(window, cx),
                    _ => {}
                },
            ),
            cx.subscribe_in(
                &rich_input,
                window,
                |this, _, event, window, cx| match event {
                    RichEditorEvent::Changed(source) => {
                        match this.kind() {
                            "HtmlClipItem" => {
                                this.draft.html = source.clone();
                            }
                            "QQRichTextItem" => this.draft.qq = source.clone(),
                            _ => return,
                        }
                        this.changed(cx);
                    }
                    RichEditorEvent::Unapplied(error) => {
                        this.changed(cx);
                        this.error = Some(error.clone());
                        this.resolve_exit(Err(error.clone()));
                        this.leave = None;
                        cx.notify();
                    }
                    RichEditorEvent::Save => this.save(window, cx),
                    RichEditorEvent::Escape => this.request_close(window, cx),
                },
            ),
        ];
        let writes = Arc::new(WriteQueue::new(host.clone()));
        if let Ok(mut queues) = WRITE_QUEUES.get_or_init(Default::default).lock() {
            queues.push(writes.clone());
        }
        let protection = Some(EditorProtection::acquire(host.clone()));
        let mut this = Self {
            host,
            base_tokens: tokens.clone(),
            tokens,
            dark,
            background_snapshot: None,
            background_options: WindowBackgroundOptions::default(),
            target: target.clone(),
            detail: None,
            folder_children: Vec::new(),
            draft: DraftSnapshot::default(),
            lease: None,
            writes,
            generation: 0,
            edit_version: 0,
            saving: false,
            loading: false,
            leave: None,
            selection_follower: SelectionFollower::default(),
            error: None,
            tab: ContentTab::Body,
            props_open: false,
            properties_view,
            self_handle: cx.entity().downgrade(),
            ui_hovered: HashSet::new(),
            ui_motion: RefCell::new(crate::motion::Tweens::default()),
            title_input,
            body_input,
            body_context: None,
            rich_input,
            link_title_input,
            path_inputs: Vec::new(),
            _input_subscriptions: input_subscriptions,
            path_subscriptions: Vec::new(),
            file_bulk: String::new(),
            file_bulk_open: false,
            path_picker: picker_lease::EditorPickerState::default(),
            path_drop: None,
            path_rebuild_pending: false,
            scroll: ScrollHandle::new(),
            path_scroll: ScrollHandle::new(),
            image: None,
            image_size: None,
            image_scale: None,
            image_pan: (0., 0.),
            image_drag: None,
            image_bounds: Arc::new(Mutex::new(None)),
            released: false,
            protection,
            exit_waiters: Vec::new(),
            paint_metadata,
        };
        this.configure_inputs(window, cx);
        this.load(target, window, cx);
        this
    }

    fn kind(&self) -> &str {
        self.detail
            .as_ref()
            .and_then(|detail| detail["kind"].as_str())
            .unwrap_or_default()
    }
    fn changed(&mut self, cx: &mut Context<Self>) {
        self.edit_version = self.edit_version.wrapping_add(1);
        self.error = None;
        cx.notify();
    }
    fn dirty(&self) -> bool {
        self.detail.as_ref().is_some_and(|detail| {
            self.draft != DraftSnapshot::from_detail(detail, self.target.is_favorite())
        })
    }
    fn composing(&self, cx: &App) -> bool {
        self.title_input.read(cx).is_composing()
            || self.body_input.read(cx).is_composing()
            || self.rich_input.read(cx).is_composing(cx)
            || self.link_title_input.read(cx).is_composing()
            || self
                .path_inputs
                .iter()
                .any(|input| input.read(cx).is_composing())
    }

    fn composing_fields(&self, cx: &App) -> Vec<&'static str> {
        let mut fields = Vec::new();
        if self.title_input.read(cx).is_composing() {
            fields.push("title");
        }
        if self.link_title_input.read(cx).is_composing() {
            fields.push("link_title");
        }
        if self
            .path_inputs
            .iter()
            .any(|input| input.read(cx).is_composing())
        {
            fields.push("file_paths");
        }
        if self.rich_input.read(cx).is_composing(cx)
            || self.rich_input.read(cx).unapplied_error().is_some()
        {
            fields.push(if self.kind() == "HtmlClipItem" {
                "html"
            } else {
                "qq"
            });
        }
        if self.body_input.read(cx).is_composing() {
            fields.push(match self.tab {
                ContentTab::Html => "html",
                ContentTab::Qq => "qq",
                _ => match self.kind() {
                    "ActionClipItem" => "execute_param",
                    "StepClipItem" => "step_data",
                    "CmdClipItem" => "command",
                    "LinkClipItem" => "link_url",
                    _ => "body",
                },
            });
        }
        fields
    }

    fn sync_merged_fields(
        &mut self,
        before: &DraftSnapshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.draft.title != before.title {
            let title = self.draft.title.clone();
            self.title_input
                .update(cx, |input, cx| input.set_text(title, window, cx));
        }
        if self.draft.link_title != before.link_title {
            let title = self.draft.link_title.clone();
            self.link_title_input
                .update(cx, |input, cx| input.set_text(title, window, cx));
        }
        if self.draft.file_paths != before.file_paths {
            self.rebuild_paths(window, cx);
        }
        let content_changed = self.draft.body != before.body
            || self.draft.html != before.html
            || self.draft.qq != before.qq
            || self.draft.execute_param != before.execute_param
            || self.draft.step_data != before.step_data
            || self.draft.command != before.command
            || self.draft.link_url != before.link_url;
        if content_changed
            && !self.body_input.read(cx).is_composing()
            && !self.rich_input.read(cx).is_composing(cx)
        {
            self.sync_body(window, cx);
        }
    }

    fn configure_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let theme = self.tokens.theme(self.dark);
        let colors = InputColors {
            background: color(&theme.search_background),
            foreground: color(&theme.text),
            placeholder: color(&theme.text_muted),
            border: color(&theme.border),
            selection: color(&theme.selected),
            caret: color(&theme.accent),
        };
        let font_size = self.tokens.typography.font_size;
        for input in [&self.title_input, &self.link_title_input] {
            input.update(cx, |input, cx| {
                input.set_colors(colors, cx);
                input.set_metrics(style::metric("controlHeight", 30.), font_size, 8., cx);
                input.set_padding(style::metric("controlPaddingX", 10.), style::metric("fieldGap", 4.), cx);
                input.set_placeholder(tr("为条目添加标题", "Add an item title"), cx);
            });
        }
        self.body_input.update(cx, |input, cx| {
            input.set_colors(colors, cx);
            input.set_metrics(300., font_size, 8., cx);
        });
        self.refresh_input_colors(cx);
        self.sync_inputs(window, cx);
    }

    fn sync_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let title = self.draft.title.clone();
        self.title_input
            .update(cx, |input, cx| input.reset_text(title, window, cx));
        let link_title = self.draft.link_title.clone();
        self.link_title_input
            .update(cx, |input, cx| input.reset_text(link_title, window, cx));
        self.sync_body(window, cx);
        self.rebuild_paths(window, cx);
    }

    fn sync_body(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let editable_kind = matches!(self.kind(), "TextClipItem" | "HtmlClipItem" | "QQRichTextItem" | "FileClipItem")
            || self.target.is_favorite() && matches!(self.kind(), "ActionClipItem" | "StepClipItem" | "CmdClipItem" | "LinkClipItem");
        let body = match self.tab {
            ContentTab::Html => self.draft.html.clone(),
            ContentTab::Qq => self.draft.qq.clone(),
            ContentTab::Rich => {
                self.body_context = None;
                let (kind, source) = if self.kind() == "HtmlClipItem" {
                    (RichEditorKind::Html, self.draft.html.clone())
                } else {
                    (RichEditorKind::Im, self.draft.qq.clone())
                };
                let origin = RichOrigin::from_token(
                    self.detail
                        .as_ref()
                        .and_then(|detail| detail["imRichOrigin"].as_str())
                        .unwrap_or("unknown"),
                );
                self.rich_input.update(cx, |input, cx| {
                    input.set_source(kind, source, origin, window, cx)
                });
                return;
            }
            ContentTab::Body => match self.kind() {
                "HtmlClipItem"
                    if self
                        .detail
                        .as_ref()
                        .is_some_and(|detail| self.draft.html != text(detail, "htmlData")) =>
                {
                    html_to_plain_text(&self.draft.html)
                }
                "ActionClipItem" if self.target.is_favorite() => self.draft.execute_param.clone(),
                "StepClipItem" if self.target.is_favorite() => self.draft.step_data.clone(),
                "CmdClipItem" if self.target.is_favorite() => self.draft.command.clone(),
                "LinkClipItem" if self.target.is_favorite() => self.draft.link_url.clone(),
                "FileClipItem" => self.file_bulk.clone(),
                _ if !editable_kind => self.detail.as_ref().map(|detail| {
                    let raw = text(detail, "rawJson");
                    if raw.is_empty() { serde_json::to_string_pretty(detail).unwrap_or_default() } else { raw }
                }).unwrap_or_default(),
                _ => self.draft.body.clone(),
            },
        };
        let readonly = !editable_kind || self.kind() == "QQRichTextItem" && self.tab == ContentTab::Body;
        let fill = !(self.target.is_favorite() && self.kind() == "LinkClipItem");
        let monospace = readonly || matches!(self.tab, ContentTab::Html | ContentTab::Qq)
            || self.target.is_favorite() && matches!(self.kind(), "CmdClipItem" | "ActionClipItem" | "StepClipItem");
        let context = (self.generation, self.tab, self.file_bulk_open);
        let new_context = self.body_context != Some(context);
        self.body_context = Some(context);
        self.body_input.update(cx, |input, cx| {
            input.set_read_only(readonly, cx);
            input.set_monospace(monospace, cx);
            input.set_fill_height(fill, cx);
            input.set_padding(style::metric("controlPaddingX", 10.), if fill { style::metric("contentInputPaddingY", 9.) } else { 6. }, cx);
            if !fill {
                input.set_metrics(style::metric("contentInputHeight", 34.), self.tokens.typography.font_size, 8., cx);
            }
            if new_context {
                input.reset_text(body, window, cx);
            } else if input.text() != body {
                input.set_text(body, window, cx);
            }
        });
        self.refresh_body_colors(cx);
    }

    fn load(&mut self, target: EditorTarget, window: &mut Window, cx: &mut Context<Self>) {
        self.selection_follower.loaded(&target);
        // A retired modal still owns the pending slot until its actual reply.
        // Loading another item must not allow a second picker to overlap it.
        self.path_picker.retire();
        // A switch that reaches load has already saved or explicitly discarded
        // the previous draft. Clear that lifecycle even if the new load fails.
        self.rich_input
            .update(cx, |input, cx| input.reset_history(window, cx));
        self.body_context = None;
        for input in [&self.title_input, &self.body_input, &self.link_title_input] {
            input.update(cx, |input, cx| input.reset_text(String::new(), window, cx));
        }
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.target = target.clone();
        self.detail = None;
        self.folder_children.clear();
        self.draft = DraftSnapshot::default();
        self.image = None;
        self.image_size = None;
        self.image_scale = None;
        self.image_pan = (0., 0.);
        self.image_drag = None;
        self.loading = true;
        self.leave = None;
        self.error = None;
        self.props_open = false;
        self.ui_hovered.clear();
        self.ui_motion.borrow_mut().clear();
        self.file_bulk.clear();
        self.file_bulk_open = false;
        self.path_drop = None;
        self.path_rebuild_pending = false;
        self.writes.forget(&target);
        cx.notify();
        let host = self.host.clone();
        cx.spawn_in(window, async move |this, cx| {
            let requested = target.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let lease = EditLease::begin(host.clone())?;
                    let detail = match requested.load(&host) {
                        Ok(detail) if detail["id"].as_str() == Some(requested.id()) => detail,
                        Ok(_) => {
                            lease.release();
                            return Err("Editor received a different item.".into());
                        }
                        Err(error) => {
                            lease.release();
                            return Err(error);
                        }
                    };
                    let image = if detail["kind"] == "ImageClipItem" {
                        host.hover_image_file(requested.id(), requested.is_favorite())
                            .ok()
                    } else {
                        None
                    };
                    let image_size = image
                        .as_ref()
                        .and_then(|path| image::image_dimensions(path).ok());
                    let folder_children = if requested.is_favorite() && detail["isFolder"] == true {
                        host.favor_get_by_ids(&json!({"ids": detail["children"]}))
                            .ok().and_then(|value| value.as_array().cloned()).unwrap_or_default()
                    } else { Vec::new() };
                    Ok::<_, String>(Loaded {
                        detail,
                        lease,
                        image,
                        image_size,
                        folder_children,
                    })
                })
                .await;
            let unclaimed = result.as_ref().ok().map(|loaded| loaded.lease.clone());
            let accepted = this.update_in(cx, |this, window, cx| {
                if this.released || this.generation != generation || this.target != target {
                    if let Ok(loaded) = result {
                        loaded.lease.release();
                    }
                    return;
                }
                this.loading = false;
                match result {
                    Ok(loaded) => {
                        this.draft =
                            DraftSnapshot::from_detail(&loaded.detail, target.is_favorite());
                        this.detail = Some(loaded.detail);
                        this.folder_children = loaded.folder_children;
                        this.lease = Some(loaded.lease);
                        this.image = loaded.image;
                        this.image_size = loaded.image_size;
                        this.tab = match this.kind() {
                            "HtmlClipItem" | "QQRichTextItem" => ContentTab::Rich,
                            _ => ContentTab::Body,
                        };
                        this.edit_version = 0;
                        this.sync_inputs(window, cx);
                        if this.tab == ContentTab::Rich {
                            this.rich_input
                                .update(cx, |input, cx| input.focus_first(window, cx));
                        } else {
                            let focus = if matches!(
                                this.kind(),
                                "TextClipItem" | "HtmlClipItem" | "QQRichTextItem"
                            ) || target.is_favorite()
                                && matches!(
                                    this.kind(),
                                    "ActionClipItem"
                                        | "StepClipItem"
                                        | "CmdClipItem"
                                        | "LinkClipItem"
                                ) {
                                this.body_input.read(cx).focus_handle()
                            } else {
                                this.title_input.read(cx).focus_handle()
                            };
                            window.focus(&focus);
                        }
                    }
                    Err(error) => this.error = Some(error),
                }
                cx.notify();
            });
            if accepted.is_err() {
                if let Some(lease) = unclaimed {
                    lease.release();
                }
            }
        })
        .detach();
    }

    pub fn select(&mut self, target: EditorTarget, window: &mut Window, cx: &mut Context<Self>) {
        self.selection_follower.clear();
        self.select_target(target, window, cx);
    }

    /// Follow an already-open list without showing or activating this window.
    /// History drafts use the regular revision-checked save before switching;
    /// composition and unapplied rich edits retain the latest selection until
    /// they can safely leave. Repeating a selection never retries a failed save.
    pub fn follow_selection(
        &mut self,
        target: EditorTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if target.id().trim().is_empty()
            || self.released
            || !self.exit_waiters.is_empty()
            || matches!(self.leave, Some(LeaveRequest::Close(_)))
        {
            return;
        }
        self.selection_follower.request(target);
        self.apply_follow_selection(window, cx);
    }

    fn apply_follow_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.released
            || !self.exit_waiters.is_empty()
            || matches!(self.leave, Some(LeaveRequest::Close(_)))
        {
            return;
        }
        let blocked = self.composing(cx)
            || self.rich_input.read(cx).unapplied_error().is_some();
        let Some(target) = self.selection_follower.next(&self.target, blocked) else {
            return;
        };
        self.select_target(target, window, cx);
    }

    fn select_target(&mut self, target: EditorTarget, window: &mut Window, cx: &mut Context<Self>) {
        if target == self.target {
            // Selecting A again while A's save is preparing a switch to B
            // cancels B. A close request remains owned by its explicit caller.
            if matches!(self.leave, Some(LeaveRequest::Switch(_) | LeaveRequest::SwitchDiscard(_))) {
                self.leave = None;
            }
            return;
        }
        if self.composing(cx) {
            return;
        }
        if !self.target.is_favorite() && self.guard_unapplied(cx) {
            return;
        }
        self.path_picker.retire();
        self.resolve_exit(Err(tr(
            "退出已取消：编辑条目已切换。",
            "Exit cancelled: the edited item changed.",
        )
        .into()));
        if self.target.is_favorite() {
            if self.saving {
                self.leave = Some(LeaveRequest::SwitchDiscard(target));
            } else {
                self.writes.discard(&self.target);
                if let Some(lease) = self.lease.take() {
                    self.writes.release_after_writes(self.target.clone(), lease);
                }
                self.load(target, window, cx);
            }
            return;
        }
        self.leave = Some(LeaveRequest::Switch(target));
        if !self.saving {
            self.save(window, cx);
        }
    }

    pub fn request_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.composing(cx) {
            return;
        }
        self.selection_follower.clear();
        self.path_picker.retire();
        self.leave = Some(LeaveRequest::Close(self.edit_version));
        if !self.saving {
            self.save(window, cx);
        }
    }

    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.target.is_favorite() {
            self.request_close(window, cx);
            return;
        }
        if self.saving || self.composing(cx) {
            return;
        }
        self.selection_follower.clear();
        self.path_picker.retire();
        self.released = true;
        self.writes.discard(&self.target);
        if let Some(lease) = self.lease.take() {
            self.writes.release_after_writes(self.target.clone(), lease);
        }
        self.protection = None;
        self.resolve_exit(Err("Exit cancelled by the editor.".into()));
        window.remove_window();
    }

    fn finish_leave(
        &mut self,
        submitted_version: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Exit must include input committed while the previous write was in
        // flight. Normal close still lets further editing cancel dismissal.
        if !self.exit_waiters.is_empty() && self.dirty() {
            self.leave = Some(LeaveRequest::Close(self.edit_version));
            self.save(window, cx);
            return;
        }
        if !self.exit_waiters.is_empty() {
            self.leave = Some(LeaveRequest::Close(self.edit_version));
        }
        match self.leave.take() {
            Some(LeaveRequest::Close(version))
                if (self.edit_version == submitted_version && version == submitted_version)
                    || !self.exit_waiters.is_empty() =>
            {
                self.path_picker.retire();
                self.released = true;
                if let Some(lease) = self.lease.take() {
                    lease.release();
                }
                self.protection = None;
                self.resolve_exit(Ok(()));
                window.remove_window();
            }
            Some(LeaveRequest::Close(version))
                if version > submitted_version && self.edit_version == version =>
            {
                self.leave = Some(LeaveRequest::Close(version));
                self.save(window, cx);
            }
            Some(LeaveRequest::Switch(target)) => {
                if self.dirty() {
                    self.leave = Some(LeaveRequest::Switch(target));
                    self.save(window, cx);
                } else {
                    if let Some(lease) = self.lease.take() {
                        lease.release();
                    }
                    self.load(target, window, cx);
                }
            }
            Some(LeaveRequest::SwitchDiscard(target)) => {
                if let Some(lease) = self.lease.take() {
                    lease.release();
                }
                self.load(target, window, cx);
            }
            _ => {}
        }
    }

    pub fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.guard_unapplied(cx) {
            self.leave = None;
            return;
        }
        if self.saving || self.composing(cx) {
            return;
        }
        let Some(original) = self.detail.clone() else {
            match self.leave.take() {
                Some(LeaveRequest::Switch(target)) => self.load(target, window, cx),
                Some(LeaveRequest::Close(_)) => {
                    self.path_picker.retire();
                    self.released = true;
                    self.protection = None;
                    self.resolve_exit(Ok(()));
                    window.remove_window();
                }
                _ => {}
            }
            return;
        };
        let target = self.target.clone();
        let draft = self.draft.clone();
        let submitted = draft.clone();
        let version = self.edit_version;
        let generation = self.generation;
        self.saving = true;
        self.error = None;
        cx.notify();
        let receiver = self.writes.persist(target.clone(), original, draft);
        cx.spawn_in(window, async move |this, cx| {
            let result = receiver
                .await
                .unwrap_or_else(|_| Err("Editor write worker is unavailable.".into()));
            let _ = this.update_in(cx, |this, window, cx| {
                if this.released || this.generation != generation || this.target != target {
                    return;
                }
                this.saving = false;
                match result {
                    Ok(detail) => {
                        this.selection_follower.suspended = false;
                        // Advance the revision even when newer input exists.
                        // Only this exact submitted draft can be replaced.
                        let before = this.draft.clone();
                        let committed = DraftSnapshot::from_detail(&detail, target.is_favorite());
                        let composing_fields = this.composing_fields(cx);
                        this.draft
                            .merge_committed(&submitted, &committed, &composing_fields);
                        this.detail = Some(detail);
                        this.sync_merged_fields(&before, window, cx);
                        let composing = this.composing(cx);
                        if composing {
                            // Composing text has not emitted Changed yet. It is
                            // still newer input and must never be cleared by a
                            // save reply or dismissed by its close request.
                            this.leave = None;
                            let error = tr(
                                "请先完成输入法组合再退出。",
                                "Finish composing text before exiting.",
                            )
                            .to_string();
                            this.resolve_exit(Err(error.clone()));
                            this.error = Some(error);
                        } else {
                            this.finish_leave(version, window, cx);
                        }
                    }
                    Err(error) => {
                        this.selection_follower.suspended = true;
                        this.resolve_exit(Err(error.clone()));
                        this.error = Some(error);
                        this.leave = None;
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn rebuild_paths(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.path_rebuild_pending = false;
        self.path_inputs.clear();
        self.path_subscriptions.clear();
        let colors = self.tokens.theme(self.dark);
        let input_colors = InputColors {
            background: color(&colors.search_background),
            foreground: color(&colors.text),
            placeholder: color(&colors.text_muted),
            border: color(&colors.border),
            selection: color(&colors.selected),
            caret: color(&colors.accent),
        };
        for (index, path) in self.draft.file_paths.iter().cloned().enumerate() {
            let input = cx.new(|cx| {
                let mut input = SearchInput::new(window, cx);
                input.set_double_click_clears(false);
                input.set_colors(input_colors, cx);
                input.set_placeholder(tr("文件路径", "File path"), cx);
                input.set_metrics(style::metric("filePathHeight", 29.), self.tokens.typography.small_font_size, style::metric("filePathPaddingX", 7.), cx);
                input.set_text(path, window, cx);
                input
            });
            let subscription = cx.subscribe_in(
                &input,
                window,
                move |this, _, event, window, cx| match event {
                    SearchEvent::Changed(value) => {
                        if let Some(path) = this.draft.file_paths.get_mut(index) {
                            *path = value.clone();
                            this.changed(cx);
                        }
                    }
                    SearchEvent::Submit => {
                        if this.composing(cx) {
                            return;
                        }
                        if index + 1 == this.path_inputs.len() {
                            this.draft.file_paths.push(String::new());
                            this.changed(cx);
                            this.rebuild_paths(window, cx);
                        }
                        if let Some(next) = this.path_inputs.get(index + 1) {
                            window.focus(&next.read(cx).focus_handle());
                        }
                    }
                    SearchEvent::Escape => this.request_close(window, cx),
                },
            );
            self.path_inputs.push(input);
            self.path_subscriptions.push(subscription);
        }
    }

    fn change_tab(&mut self, tab: ContentTab, window: &mut Window, cx: &mut Context<Self>) {
        if tab == self.tab || self.composing(cx) || self.guard_unapplied(cx) {
            return;
        }
        if self.tab == ContentTab::Rich || tab == ContentTab::Rich {
            // WebView mounts separate Quill/CodeMirror components for each
            // surface. Their undo stacks do not survive this surface switch.
            self.rich_input
                .update(cx, |input, cx| input.reset_history(window, cx));
        }
        self.tab = tab;
        self.sync_body(window, cx);
        if tab == ContentTab::Rich {
            self.rich_input
                .update(cx, |input, cx| input.focus_first(window, cx));
        } else {
            window.focus(&self.body_input.read(cx).focus_handle());
        }
        cx.notify();
    }

    fn guard_unapplied(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(error) = self
            .rich_input
            .read(cx)
            .unapplied_error()
            .map(str::to_string)
        else {
            return false;
        };
        self.error = Some(error.clone());
        self.resolve_exit(Err(error));
        cx.notify();
        true
    }

    fn append_paths(&mut self, paths: Vec<String>, window: &mut Window, cx: &mut Context<Self>) {
        if paths.is_empty() {
            return;
        }
        let mut seen = self
            .draft
            .file_paths
            .iter()
            .map(|path| path.trim().to_lowercase())
            .collect::<HashSet<_>>();
        self.draft.file_paths.extend(
            paths
                .into_iter()
                .filter(|path| seen.insert(path.trim().to_lowercase())),
        );
        self.changed(cx);
        if self.composing(cx) {
            self.path_rebuild_pending = true;
        } else {
            self.rebuild_paths(window, cx);
        }
    }

    fn pick_paths(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.path_picker.is_pending()
            || self.composing(cx)
            || self.released
            || self.loading
            || self.saving
            || self.leave.is_some()
            || self.kind() != "FileClipItem"
        {
            return;
        }
        // Capture this editor's actual native owner on its UI thread. The Core
        // guard verifies this HWND and this one-way request lease throughout
        // prepare/Show/result; it does not borrow the main-window picker epoch.
        let owner = match crate::platform::hwnd(window) {
            Ok(hwnd) => hwnd.0 as isize,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        let Some(lease) = self.path_picker.begin() else {
            return;
        };
        let generation = self.generation;
        let target = self.target.clone();
        let host = self.host.clone();
        self.error = None;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let worker_lease = lease.clone();
            let result = crate::native_dialog::run(move || {
                host.pick_files_native_owner_guarded(&json!({}), owner, move || {
                    worker_lease.is_active()
                })
            })
            .await;
            let _ = this.update_in(cx, |this, window, cx| {
                // Even a retired reply releases its own pending slot first.
                // An old or duplicate reply cannot release a newer request.
                let Some(was_active) = this.path_picker.complete(&lease) else {
                    return;
                };
                cx.notify();
                if !was_active
                    || this.released
                    || this.loading
                    || this.leave.is_some()
                    || this.generation != generation
                    || this.target != target
                    || this.kind() != "FileClipItem"
                    || crate::platform::hwnd(window)
                        .map_or(true, |current| current.0 as isize != owner)
                {
                    return;
                }
                match result {
                    Ok(value) => {
                        let paths = value["paths"]
                            .as_array()
                            .map(|paths| {
                                paths
                                    .iter()
                                    .filter_map(Value::as_str)
                                    .map(str::to_string)
                                    .collect()
                            })
                            .unwrap_or_default();
                        this.append_paths(paths, window, cx);
                    }
                    Err(error) => this.error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.key == "escape" && cx.has_active_drag() {
            self.path_drop = None;
            cx.stop_active_drag(window);
            cx.stop_propagation();
            cx.notify();
            return;
        }
        if self.composing(cx) || event.is_held {
            return;
        }
        if event.keystroke.key == "escape" && event.keystroke.modifiers.number_of_modifiers() == 0 {
            cx.stop_propagation();
            self.request_close(window, cx);
        } else if event.keystroke.key == "s" && event.keystroke.modifiers.control {
            cx.stop_propagation();
            self.save(window, cx);
        }
    }

    fn button(
        &self,
        id: impl Into<gpui::ElementId>,
        label: impl Into<gpui::SharedString>,
        primary: bool,
    ) -> gpui::Stateful<gpui::Div> {
        let t = self.tokens.theme(self.dark);
        let accent = gpui::Rgba::from(color(&t.accent));
        let primary_hover = gpui::Hsla::from(gpui::Rgba {
            r: (accent.r * 1.08).min(1.),
            g: (accent.g * 1.08).min(1.),
            b: (accent.b * 1.08).min(1.),
            a: accent.a,
        });
        self.surface_button(id, label,
            color(if primary { &t.surface } else { &t.text }),
            color(if primary { &t.accent } else { &t.surface }),
            if primary { primary_hover } else { color(&t.hover) })
    }

    fn button_with_background(
        &self,
        id: impl Into<gpui::ElementId>,
        label: impl Into<gpui::SharedString>,
        background: gpui::Hsla,
        hover: gpui::Hsla,
    ) -> gpui::Stateful<gpui::Div> {
        self.surface_button(id, label, color(&self.tokens.theme(self.dark).text), background, hover)
    }

    fn surface_button(
        &self,
        id: impl Into<gpui::ElementId>,
        label: impl Into<gpui::SharedString>,
        foreground: gpui::Hsla,
        background: gpui::Hsla,
        hover: gpui::Hsla,
    ) -> gpui::Stateful<gpui::Div> {
        let id = id.into();
        let progress = self.ui_motion.borrow_mut().value(id.clone(),
            if self.ui_hovered.contains(&id) { 1. } else { 0. },
            crate::motion::HOVER, crate::motion::EASE, Instant::now());
        let weak = self.self_handle.clone();
        rsx! {
            <div id={id.clone()} flex items_center justify_center
                h={px(style::metric("controlHeight", 30.))}
                px={px(style::metric("controlPaddingX", 10.))} border_1
                border_color={color(&self.tokens.theme(self.dark).border)}
                rounded={px(style::metric("controlRadius", 4.))}
                text_size={px(self.tokens.typography.font_size)} text_color={foreground}
                bg={crate::motion::mix_hsla(background, hover, progress)} cursor_pointer />
        }
            .on_hover(move |hovered: &bool, _, cx| {
                let _ = weak.update(cx, |this, cx| {
                    let changed = if *hovered { this.ui_hovered.insert(id.clone()) } else { this.ui_hovered.remove(&id) };
                    if changed { cx.notify(); }
                });
            })
            .child(label.into())
    }

    fn change_page(&mut self, properties: bool, window: &mut Window, cx: &mut Context<Self>) {
        if properties == self.props_open || self.detail.is_none() || self.loading || self.released
            || self.composing(cx) || self.guard_unapplied(cx) { return; }
        self.props_open = properties;
        if !properties {
            if self.tab == ContentTab::Rich {
                self.rich_input.update(cx, |input, cx| input.focus_first(window, cx));
            } else if matches!(self.kind(), "ImageClipItem" | "FileClipItem" | "FavoriteClipItem") {
                window.focus(&self.title_input.read(cx).focus_handle());
            } else {
                window.focus(&self.body_input.read(cx).focus_handle());
            }
        }
        cx.notify();
    }

    fn qa_button(
        &self,
        id: &'static str,
        label: impl Into<gpui::SharedString>,
        primary: bool,
        enabled: bool,
    ) -> gpui::Stateful<gpui::Div> {
        self.button(id, label, primary)
            .relative()
            .child(self.paint_metadata.control(id, enabled))
    }

}

fn zoom_image_pan(pan: (f32, f32), anchor: (f32, f32), ratio: f32) -> (f32, f32) {
    (
        anchor.0 - (anchor.0 - pan.0) * ratio,
        anchor.1 - (anchor.1 - pan.1) * ratio,
    )
}

impl Drop for EditorWindow {
    fn drop(&mut self) {
        self.path_picker.retire();
        self.resolve_exit(Err(
            "Editor was destroyed before save/close completed.".into()
        ));
        self.protection = None;
        if self.released {
            self.writes.retire();
            return;
        }
        let Some(lease) = self.lease.take() else {
            self.writes.retire();
            return;
        };
        let Some(original) = self.detail.clone() else {
            lease.release();
            self.writes.retire();
            return;
        };
        if self.target.is_favorite() {
            self.writes.release_after_writes(self.target.clone(), lease);
            self.writes.retire();
            return;
        }
        // Destruction during an outstanding save still queues the final draft
        // behind that save. Never release a session after a failed final write.
        self.writes
            .final_flush(self.target.clone(), original, self.draft.clone(), lease);
        self.writes.retire();
    }
}

impl Render for EditorWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.apply_follow_selection(window, cx);
        if !cx.has_active_drag() {
            self.path_drop = None;
        }
        if self.path_rebuild_pending && !self.composing(cx) {
            self.rebuild_paths(window, cx);
        }
        let tokens = self.tokens.clone();
        let t = tokens.theme(self.dark);
        let kind = self.kind().to_string();
        let display_kind = if self.target.is_favorite()
            && self.detail.as_ref().is_some_and(|detail| detail["isFolder"] == true)
        {
            "FavoriteClipItem"
        } else {
            kind.as_str()
        };
        let composing = self.composing(cx);
        let unapplied = self.rich_input.read(cx).unapplied_error().is_some();
        let tab_enabled =
            self.detail.is_some() && !self.loading && !self.released && !composing && !unapplied;
        let dirty = self.dirty();
        let save_enabled = tab_enabled && !self.saving && dirty;
        let close_enabled = !self.released && !composing && !unapplied;
        let favorite = self.target.is_favorite();
        // Favorites may always discard a rejected rich draft through Cancel;
        // history uses save-and-close and therefore keeps the unapplied guard.
        let cancel_enabled = !self.released && !composing && !self.saving && (favorite || !unapplied);

        let page = if self.props_open {
            self.properties_view.update(cx, |view, cx| {
                view.sync(self.detail.as_ref(), favorite, &self.draft, self.image_size,
                    &tokens, self.dark, window, cx);
            });
            rsx! {
                <div flex flex_col flex_1 min_h={px(0.)} min_w={px(0.)}>
                    {self.properties_view.clone()}
                </div>
            }.into_any_element()
        } else if self.detail.is_some() {
            let content = self.content_view(window, cx);
            rsx! {
                <div flex flex_col flex_1 min_h={px(0.)} min_w={px(0.)} />
            }
                .id("editor-scroll")
                .overflow_y_scroll().track_scroll(&self.scroll)
                .smooth_wheel("editor-scroll", &self.scroll, cx)
                .px(px(style::metric("contentPaddingX", 16.)))
                .py(px(style::metric("contentPaddingY", 12.)))
                .gap(px(style::metric("contentGap", 10.)))
                .child(
                    rsx! {
                        <div flex flex_col flex_none gap={px(style::metric("fieldGap", 4.))}>
                            <div text_size={px(tokens.typography.small_font_size)}
                                text_color={color(&t.text_muted)}>
                                {tr("标题", "Title")}
                            </div>
                            <div h={px(style::metric("controlHeight", 30.))} w_full border_1
                                border_color={color(&t.border)}
                                rounded={px(style::metric("controlRadius", 4.))} overflow_hidden>
                                {self.title_input.clone()}
                            </div>
                        </div>
                    }
                )
                .child(content).into_any_element()
        } else {
            rsx! {
                <div flex flex_col flex_1 min_h={px(0.)} items_center justify_center
                    text_color={color(&t.text_muted)} />
            }
                .child(if self.loading { tr("加载中…", "Loading…") }
                       else { tr("选择一个条目开始编辑", "Select an item to edit") })
                .into_any_element()
        };

        let mut page_tabs = rsx! {
            <div flex flex_none items_end h={px(style::metric("pageTabsHeight", 40.))}
                px={px(style::metric("contentPaddingX", 16.))}
                gap={px(style::metric("pageTabGap", 4.))} border_b_1
                border_color={color(&t.border)} />
        };
        for (id, label, properties) in [
            ("editor-content-page", tr("内容", "Content"), false),
            ("editor-props", tr("属性", "Properties"), true),
        ] {
            let active = self.props_open == properties;
            page_tabs = page_tabs.child(
                self.button_with_background(id, label, color(&t.background), color(&t.hover))
                    .relative().h_full()
                    .px(px(style::metric("pageTabPaddingX", 12.)))
                    .rounded(px(0.)).border_0()
                    .text_color(color(if active { &t.text } else { &t.text_muted }))
                    .when(active, |button| button.font_weight(FontWeight::SEMIBOLD)
                        .child(rsx! {
                            <div absolute h={px(1.)} bottom={px(0.)}
                                left={px(style::metric("pageTabPaddingX", 12.))}
                                right={px(style::metric("pageTabPaddingX", 12.))}
                                bg={color(&t.text)} />
                        }))
                    .when(!tab_enabled, |button| button.opacity(0.5))
                    .child(self.paint_metadata.control(id, tab_enabled))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.change_page(properties, window, cx)
                    })),
            );
        }

        let status = if self.loading { tr("加载中…", "Loading…") }
            else if self.saving { tr("保存中…", "Saving…") }
            else if self.error.is_some() { tr("需处理", "Needs attention") }
            else if dirty { tr("未保存", "Unsaved") }
            else { tr("已保存", "Saved") };
        let status_color = color(if self.error.is_some() { &t.error }
            else if dirty { &t.accent } else { &t.text_muted });
        let mut status_view = rsx! {
            <div flex items_center flex_1 min_w={px(0.)} gap={px(6.)}
                text_size={px(tokens.typography.small_font_size)} text_color={color(&t.text_muted)}>
                <div flex_none size={px(style::metric("statusDotSize", 5.))} rounded_full
                    bg={status_color} />
                {status}
            </div>
        };
        if kind == "TextClipItem" {
            let characters = style::text_stats(&self.draft.body).0;
            status_view = status_view.child(div().truncate().child(
                format!("{characters} {}", tr("字符", "characters"))));
        }
        let tab_name = if self.props_open { "properties" } else { match self.tab {
            ContentTab::Body => "body",
            ContentTab::Html => "html-source",
            ContentTab::Qq => "qq-source",
            ContentTab::Rich => "rich",
        }};
        let paint_begin = self.paint_metadata.begin(self.generation, kind.clone(), tab_name);
        let paint_finish = self.paint_metadata.finish();
        let wallpaper_layer = self.background_snapshot.as_ref().map(|image| {
            crate::window_background::render_background_image(image, &self.background_options)
        });
        let wallpaper_tint = self.background_snapshot.as_ref().map(|_| {
            div().absolute().inset_0().bg(crate::window_glass::page_tint(
                self.base_tokens.theme(self.dark), &self.background_options))
        });

        let result = rsx! {
            <div relative size_full flex flex_col overflow_hidden
                bg={color(&self.base_tokens.theme(self.dark).background)} border_l_1
                border_color={color(&t.border)} text_color={color(&t.text)}
                font_family={tokens.typography.font_family.clone()}
                text_size={px(tokens.typography.font_size)} />
        }
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" && cx.has_active_drag() {
                    this.key_down(event, window, cx);
                }
            }))
            .on_key_down(cx.listener(Self::key_down))
            .children(wallpaper_layer).children(wallpaper_tint).child(paint_begin)
            .child(
                rsx! {
                    <div flex flex_none items_center h={px(style::metric("headerHeight", 36.))}
                        px={px(style::metric("contentPaddingX", 16.))} gap={px(8.)}>
                        {crate::crisp_svg::svg()
                        .path(properties::kind_icon_asset(display_kind))
                        .size(px(16.)).flex_none().text_color(color(&t.text_muted))}
                        <div min_w={px(0.)} truncate font_weight={FontWeight::SEMIBOLD}>
                            {format!("{} · {}", tr("编辑", "Edit"),
                            properties::kind_label(display_kind))}
                        </div>
                    </div>
                }
                    .child(rsx! {
                        <div flex_none px={px(style::metric("targetPaddingX", 6.))}
                            rounded={px(style::metric("targetRadius", 4.))}
                            text_size={px(tokens.typography.small_font_size)}
                            text_color={color(&t.text_muted)} bg={color(&t.secondary)} />
                    }
                        .child(if favorite { tr("收藏", "Favorite") } else { tr("历史", "History") }))
                    .child(div().flex_1())
                    .child(self.button_with_background("editor-close", "", color(&t.background), color(&t.hover))
                        .relative().child(self.paint_metadata.control("editor-close", close_enabled))
                        .size(px(style::metric("controlHeight", 30.))).p(px(0.))
                        .border_0()
                        .when(!close_enabled, |button| button.opacity(0.5))
                        .child(crate::crisp_svg::svg().path("close.svg").size(px(14.))
                            .text_color(color(&t.text_muted)))
                        .on_click(cx.listener(|this, _, window, cx| this.request_close(window, cx)))),
            )
            .child(page_tabs).child(page)
            .children(self.error.clone().map(|error| rsx! {
                <div flex_none px={px(style::metric("contentPaddingX", 16.))} py={px(5.)}
                    text_size={px(tokens.typography.small_font_size)} text_color={color(&t.error)}>
                    {error}
                </div>
            }))
            .child(rsx! {
                <div flex flex_none items_center h={px(style::metric("footerHeight", 50.))}
                    gap={px(style::metric("footerGap", 8.))}
                    px={px(style::metric("contentPaddingX", 16.))} border_t_1
                    border_color={color(&t.border)}>
                    {status_view}
                </div>
            }
                .child(self.qa_button("editor-cancel",
                        if favorite { tr("取消", "Cancel") } else { tr("关闭", "Close") },
                        false, cancel_enabled)
                    .when(!cancel_enabled, |button| button.opacity(0.5))
                    .on_click(cx.listener(|this, _, window, cx| this.cancel(window, cx))))
                .child(self.qa_button("editor-save", tr("保存", "Save"), true, save_enabled)
                    .when(!save_enabled, |button| button.opacity(0.5))
                    .on_click(cx.listener(|this, _, window, cx| this.save(window, cx)))))
            .child(paint_finish);
        if self.ui_motion.borrow().any_active(Instant::now()) {
            window.request_animation_frame();
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn toolbar_toggle_closes_only_a_clean_settled_editor() {
        assert_eq!(ToolbarCloseState::default().blocked_by(), None);
        for (state, reason) in [
            (
                ToolbarCloseState {
                    dirty: true,
                    ..Default::default()
                },
                ToolbarCloseBlock::Unsaved,
            ),
            (
                ToolbarCloseState {
                    loading: true,
                    ..Default::default()
                },
                ToolbarCloseBlock::Loading,
            ),
            (
                ToolbarCloseState {
                    saving: true,
                    ..Default::default()
                },
                ToolbarCloseBlock::Saving,
            ),
            (
                ToolbarCloseState {
                    switching: true,
                    ..Default::default()
                },
                ToolbarCloseBlock::Switching,
            ),
            (
                ToolbarCloseState {
                    picking: true,
                    ..Default::default()
                },
                ToolbarCloseBlock::Picking,
            ),
            (
                ToolbarCloseState {
                    composing: true,
                    ..Default::default()
                },
                ToolbarCloseBlock::Composing,
            ),
            (
                ToolbarCloseState {
                    unapplied: true,
                    ..Default::default()
                },
                ToolbarCloseBlock::Unapplied,
            ),
        ] {
            assert_eq!(state.blocked_by(), Some(reason));
        }
        // A pending write is protected even after its payload equals the
        // displayed baseline; callers cannot infer clean-close from dirty alone.
        assert_eq!(
            ToolbarCloseState {
                saving: true,
                dirty: false,
                ..Default::default()
            }
            .blocked_by(),
            Some(ToolbarCloseBlock::Saving)
        );
    }

    #[test]
    fn toolbar_toggle_protects_bulk_paths_not_yet_added_to_the_draft() {
        let paths = vec![r"C:\资料\a.png".into()];
        assert!(!unapplied_bulk_paths("FileClipItem", "", &paths));
        assert!(!unapplied_bulk_paths("FileClipItem", "\r\n  ", &paths));
        assert!(!unapplied_bulk_paths(
            "FileClipItem",
            r"c:\资料\a.png",
            &paths
        ));
        assert!(unapplied_bulk_paths(
            "FileClipItem",
            "C:\\资料\\a.png\r\nC:\\资料\\b.png",
            &paths
        ));
        assert!(!unapplied_bulk_paths("TextClipItem", "a text draft", &[]));
    }

    #[test]
    fn selection_follow_keeps_only_latest_item_during_composition_or_save() {
        let current = EditorTarget::History("a".into());
        let mut follower = SelectionFollower::default();
        follower.request(EditorTarget::History("b".into()));
        assert_eq!(follower.next(&current, true), None);
        follower.request(EditorTarget::History("c".into()));
        assert_eq!(follower.next(&current, true), None);
        assert_eq!(follower.next(&current, false), Some(EditorTarget::History("c".into())));
        // Reading the request while an old draft is saving does not consume it.
        assert_eq!(follower.next(&current, false), Some(EditorTarget::History("c".into())));
        follower.loaded(&EditorTarget::History("b".into()));
        assert_eq!(follower.next(&current, false), Some(EditorTarget::History("c".into())));
        follower.loaded(&EditorTarget::History("c".into()));
        assert_eq!(follower.next(&current, false), None);
    }

    #[test]
    fn selection_follow_can_cancel_pending_switch_by_returning_to_current_item() {
        let current = EditorTarget::History("a".into());
        let mut follower = SelectionFollower::default();
        follower.request(EditorTarget::History("b".into()));
        assert_eq!(follower.next(&current, false), Some(EditorTarget::History("b".into())));
        follower.request(current.clone());
        // This must reach select_target even during composition so it cancels
        // an earlier LeaveRequest::Switch instead of later jumping back to B.
        assert_eq!(follower.next(&current, true), Some(current.clone()));
        assert_eq!(follower.next(&current, false), None);
    }

    #[test]
    fn selection_follow_does_not_retry_failed_save_when_maintenance_repeats_target() {
        let current = EditorTarget::History("a".into());
        let next = EditorTarget::History("b".into());
        let mut follower = SelectionFollower::default();
        follower.request(next.clone());
        assert_eq!(follower.next(&current, false), Some(next.clone()));
        follower.suspended = true;
        for _ in 0..10 {
            follower.request(next.clone());
            assert_eq!(follower.next(&current, false), None);
        }
        // An explicit successful save resumes the pending selection.
        follower.suspended = false;
        assert_eq!(follower.next(&current, false), Some(next.clone()));
        follower.suspended = true;
        let different = EditorTarget::History("c".into());
        follower.request(different.clone());
        assert_eq!(follower.next(&current, false), Some(different));
    }

    #[test]
    fn selection_follow_keeps_history_and_favorite_identity_distinct() {
        let current = EditorTarget::History("same-id".into());
        let favorite = EditorTarget::Favorite("same-id".into());
        let mut follower = SelectionFollower::default();
        follower.request(favorite.clone());
        follower.loaded(&current);
        assert_eq!(follower.next(&current, false), Some(favorite));
        follower.clear();
        assert_eq!(follower.next(&current, false), None);
    }

    #[test]
    fn html_reply_merges_derived_body_while_preserving_newer_title() {
        let original = json!({"id":"a","kind":"HtmlClipItem","title":"old","bodyText":"A","htmlData":"<b>A</b>","htmlSourceUrl":"https://example.test","revision":"r1"});
        let mut submitted = DraftSnapshot::from_detail(&original, false);
        submitted.html = "<b>B</b>".into();
        let mut current = submitted.clone();
        current.title = "typed while saving".into();
        let committed = json!({"id":"a","kind":"HtmlClipItem","title":"old","bodyText":"B","htmlData":"<b>B</b>","htmlSourceUrl":"https://example.test","revision":"r2"});
        current.merge_committed(
            &submitted,
            &DraftSnapshot::from_detail(&committed, false),
            &[],
        );
        assert_eq!(current.body, "B");
        assert_eq!(
            build_patch(&EditorTarget::History("a".into()), &committed, &current),
            Some(json!({"id":"a","title":"typed while saving","expectedRevision":"r2"}))
        );
    }

    #[test]
    fn reply_merge_preserves_explicit_new_body_and_composing_source() {
        let submitted = DraftSnapshot {
            body: "old".into(),
            html: "<b>A</b>".into(),
            ..Default::default()
        };
        let committed = DraftSnapshot {
            body: "derived".into(),
            html: "<strong>A</strong>".into(),
            ..Default::default()
        };
        let mut current = submitted.clone();
        current.body = "new explicit body".into();
        current.merge_committed(&submitted, &committed, &["html"]);
        assert_eq!(current.body, "new explicit body");
        assert_eq!(current.html, submitted.html);
    }

    #[test]
    fn successful_retirement_removes_registry_and_stops_accepting_writes() {
        let queue = Arc::new(WriteQueue::with_writer(|_, patch| Ok(patch)));
        WRITE_QUEUES
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .push(queue.clone());
        queue.retire();
        let result = queue.barrier().blocking_recv();
        assert!(matches!(result, Ok(Ok(()))) || queue.retired.load(Ordering::Acquire));
        assert!(queue.retired.load(Ordering::Acquire));
        assert!(!WRITE_QUEUES
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .iter()
            .any(|registered| registered.id == queue.id));
        assert!(queue.barrier().blocking_recv().unwrap().is_ok());
    }

    #[test]
    fn retirement_keeps_failed_draft_until_explicit_discard() {
        let queue = Arc::new(WriteQueue::with_writer(|_, _| Err("conflict".into())));
        WRITE_QUEUES
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .push(queue.clone());
        let original = json!({"id":"a","kind":"TextClipItem","bodyText":"old","revision":"r1"});
        let mut draft = DraftSnapshot::from_detail(&original, false);
        draft.body = "keep me".into();
        let target = EditorTarget::History("a".into());
        assert!(queue
            .persist(target.clone(), original, draft)
            .blocking_recv()
            .unwrap()
            .is_err());
        queue.retire();
        assert_eq!(
            queue.barrier().blocking_recv().unwrap().unwrap_err(),
            "conflict"
        );
        assert!(!queue.retired.load(Ordering::Acquire));
        assert_eq!(
            queue.failed_drafts.lock().unwrap()[&target].draft.body,
            "keep me"
        );
        queue.discard(&target);
        queue.retire();
        let result = queue.barrier().blocking_recv();
        assert!(matches!(result, Ok(Ok(()))) || queue.retired.load(Ordering::Acquire));
        assert!(queue.retired.load(Ordering::Acquire));
    }
    #[test]
    fn explicit_html_text_conversion_matches_webview() {
        let original = json!({"id":"a", "kind":"HtmlClipItem", "htmlData":"<b>A</b>", "htmlSourceUrl":"https://example.test/", "bodyText":"A", "revision":"r1"});
        let mut draft = DraftSnapshot::from_detail(&original, false);
        draft.body = "plain edited".into();
        assert_eq!(
            build_patch(&EditorTarget::History("a".into()), &original, &draft),
            Some(
                json!({"id":"a", "bodyText":"plain edited", "htmlData":"", "htmlSourceUrl":"", "expectedRevision":"r1"})
            )
        );
    }

    #[test]
    fn file_import_keeps_order_and_deduplicates_case_insensitively() {
        assert_eq!(
            parse_file_path_lines(" C:\\A.txt, c:\\a.TXT\r\nD:\\图.png,\n  "),
            vec!["C:\\A.txt", "D:\\图.png"]
        );
    }
    #[test]
    fn path_drag_reordering_adjusts_the_gap_after_removal() {
        let mut paths = vec!["a".into(), "b".into(), "c".into()];
        assert_eq!(reorder_file_path(&mut paths, 0, 1, false), Some(1));
        assert_eq!(paths, vec!["b", "a", "c"]);
        assert_eq!(reorder_file_path(&mut paths, 2, 0, true), Some(0));
        assert_eq!(paths, vec!["c", "b", "a"]);
        assert_eq!(reorder_file_path(&mut paths, 9, 0, true), None);
    }

    #[test]
    fn image_zoom_keeps_the_pointer_on_the_same_pixel() {
        let pan = (20., -10.);
        let anchor = (80., 30.);
        let adjusted = zoom_image_pan(pan, anchor, 2.);
        assert_eq!(adjusted, (-40., -50.));
        assert_eq!((anchor.0 - adjusted.0) / 2., anchor.0 - pan.0);
        assert_eq!((anchor.1 - adjusted.1) / 2., anchor.1 - pan.1);
    }
    #[test]
    fn exit_barrier_waits_for_final_write_and_reports_failures() {
        let original = json!({"id":"a", "kind":"TextClipItem", "bodyText":"old", "revision":"r1"});
        let (started_tx, started_rx) = mpsc::channel();
        let (continue_tx, continue_rx) = mpsc::channel();
        let queue = WriteQueue::with_writer(move |_, patch| {
            started_tx.send(()).unwrap();
            continue_rx.recv().unwrap();
            if patch["bodyText"] == "fail" {
                return Err("revision conflict".into());
            }
            Ok(
                json!({"id":"a", "kind":"TextClipItem", "bodyText":patch["bodyText"], "revision":"r2"}),
            )
        });
        let mut draft = DraftSnapshot::from_detail(&original, false);
        draft.body = "fail".into();
        let written = queue.persist(
            EditorTarget::History("a".into()),
            original.clone(),
            draft.clone(),
        );
        started_rx.recv().unwrap();
        let mut barrier = queue.barrier();
        assert!(matches!(
            barrier.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        continue_tx.send(()).unwrap();
        assert_eq!(
            written.blocking_recv().unwrap().unwrap_err(),
            "revision conflict"
        );
        assert_eq!(
            barrier.blocking_recv().unwrap().unwrap_err(),
            "revision conflict"
        );
        assert_eq!(
            queue.failed_drafts.lock().unwrap()[&EditorTarget::History("a".into())]
                .draft
                .body,
            "fail"
        );
        draft.body = "retry succeeds".into();
        let written = queue.persist(EditorTarget::History("a".into()), original, draft);
        started_rx.recv().unwrap();
        continue_tx.send(()).unwrap();
        assert!(written.blocking_recv().unwrap().is_ok());
        assert!(queue.barrier().blocking_recv().unwrap().is_ok());
        assert!(queue.failed_drafts.lock().unwrap().is_empty());
    }
    #[test]
    fn title_save_keeps_html_qq_and_source_url_out_of_patch() {
        for kind in ["HtmlClipItem", "QQRichTextItem"] {
            let detail = json!({ "id":"a", "kind":kind, "title":"old", "revision":"r1", "bodyText":"中", "htmlData":"<b>中</b>", "htmlSourceUrl":"https://example.test", "qqRichText":"<QQ><pic/></QQ>" });
            let mut draft = DraftSnapshot::from_detail(&detail, false);
            draft.title = "new".into();
            assert_eq!(
                build_patch(&EditorTarget::History("a".into()), &detail, &draft),
                Some(json!({"id":"a", "title":"new", "expectedRevision":"r1"}))
            );
        }
    }
    #[test]
    fn html_source_save_derives_body_and_retains_source_url() {
        let detail = json!({ "id":"a", "kind":"HtmlClipItem", "revision":"r1", "htmlData":"<b>A</b>", "bodyText":"A", "htmlSourceUrl":"https://example.test" });
        let mut draft = DraftSnapshot::from_detail(&detail, false);
        draft.html = "<p>中😀</p><p>next</p>".into();
        let patch = build_patch(&EditorTarget::History("a".into()), &detail, &draft).unwrap();
        assert_eq!(patch["htmlData"], draft.html);
        assert_eq!(patch["bodyText"], "中😀\nnext");
        assert!(patch.get("htmlSourceUrl").is_none());
        assert_eq!(patch["expectedRevision"], "r1");
    }
    #[test]
    fn qq_source_edit_only_submits_original_payload_field() {
        let detail = json!({ "id":"a", "kind":"QQRichTextItem", "qqRichText":"old", "bodyText":"old body", "revision":"r2" });
        let mut draft = DraftSnapshot::from_detail(&detail, false);
        draft.qq = "<QQ><text><![CDATA[中]]></text></QQ>".into();
        let patch = build_patch(&EditorTarget::History("a".into()), &detail, &draft).unwrap();
        assert_eq!(patch["qqRichText"], draft.qq);
        assert!(patch.get("bodyText").is_none());
    }
    #[test]
    fn file_order_and_favorite_specific_fields_use_existing_contract() {
        let detail =
            json!({"id":"a","kind":"FileClipItem","filePaths":[" a ","b"],"revision":"r1"});
        let mut draft = DraftSnapshot::from_detail(&detail, false);
        assert!(build_patch(&EditorTarget::History("a".into()), &detail, &draft).is_none());
        draft.file_paths = vec!["b".into(), " a ".into(), " ".into()];
        assert_eq!(
            build_patch(&EditorTarget::History("a".into()), &detail, &draft).unwrap()["filePaths"],
            json!(["b", "a"])
        );
        let detail = json!({"id":"b","kind":"CmdClipItem","command":"old","revision":"r2"});
        let mut draft = DraftSnapshot::from_detail(&detail, true);
        draft.command = "new".into();
        assert_eq!(
            build_patch(&EditorTarget::Favorite("b".into()), &detail, &draft).unwrap()["command"],
            "new"
        );
        assert!(build_patch(&EditorTarget::History("b".into()), &detail, &draft).is_none());
    }

    #[test]
    fn queued_newer_input_waits_for_the_previous_committed_revision() {
        let original =
            json!({ "id":"a", "kind":"TextClipItem", "bodyText":"old", "revision":"r1" });
        let current = Arc::new(Mutex::new(original.clone()));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (stored, written) = (current.clone(), calls.clone());
        let queue = WriteQueue::with_writer(move |_, patch| {
            let mut stored = stored.lock().unwrap();
            assert_eq!(patch["expectedRevision"], stored["revision"]);
            written
                .lock()
                .unwrap()
                .push((text(&patch, "bodyText"), text(&patch, "expectedRevision")));
            let revision = if stored["revision"] == "r1" {
                "r2"
            } else {
                "r3"
            };
            stored["bodyText"] = patch["bodyText"].clone();
            stored["revision"] = json!(revision);
            Ok(stored.clone())
        });
        let target = EditorTarget::History("a".into());
        let mut first = DraftSnapshot::from_detail(&original, false);
        first.body = "first".into();
        let mut final_draft = first.clone();
        final_draft.body = "typed while saving".into();
        let response1 = queue.persist(target.clone(), original.clone(), first);
        // This is the same original snapshot a destruction flush or second
        // close captures before the first save response can update the UI.
        let response2 = queue.persist(target, original, final_draft);
        assert_eq!(
            response1.blocking_recv().unwrap().unwrap()["revision"],
            "r2"
        );
        assert_eq!(
            response2.blocking_recv().unwrap().unwrap()["revision"],
            "r3"
        );
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                ("first".into(), "r1".into()),
                ("typed while saving".into(), "r2".into())
            ]
        );
        assert_eq!(current.lock().unwrap()["bodyText"], "typed while saving");
    }

    #[test]
    fn failed_write_keeps_the_successful_revision_for_the_next_queued_draft() {
        let original =
            json!({ "id":"a", "kind":"TextClipItem", "bodyText":"old", "revision":"r1" });
        let stored = Arc::new(Mutex::new(original.clone()));
        let current = stored.clone();
        let queue = WriteQueue::with_writer(move |_, patch| {
            let mut current = current.lock().unwrap();
            assert_eq!(patch["expectedRevision"], current["revision"]);
            if patch["bodyText"] == "reject" {
                return Err("external revision conflict".into());
            }
            current["bodyText"] = patch["bodyText"].clone();
            current["revision"] = json!(if current["revision"] == "r1" {
                "r2"
            } else {
                "r3"
            });
            Ok(current.clone())
        });
        let target = EditorTarget::History("a".into());
        let responses = ["first", "reject", "final"]
            .into_iter()
            .map(|body| {
                let mut draft = DraftSnapshot::from_detail(&original, false);
                draft.body = body.into();
                queue.persist(target.clone(), original.clone(), draft)
            })
            .collect::<Vec<_>>();
        let mut responses = responses.into_iter();
        assert_eq!(
            responses.next().unwrap().blocking_recv().unwrap().unwrap()["revision"],
            "r2"
        );
        assert_eq!(
            responses
                .next()
                .unwrap()
                .blocking_recv()
                .unwrap()
                .unwrap_err(),
            "external revision conflict"
        );
        assert_eq!(
            responses.next().unwrap().blocking_recv().unwrap().unwrap()["revision"],
            "r3"
        );
        assert_eq!(stored.lock().unwrap()["bodyText"], "final");
    }
}
