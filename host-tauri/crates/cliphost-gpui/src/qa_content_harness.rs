//! Debug-only, explicit isolated-content fixture channel. This module is a
//! private child of `view`: it adds no Bridge, HTTP or production entry point.
//! Geometry is recorded by transparent Element.paint delegates, never inferred
//! from design tokens, projected line counts or layout requests.

use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    rc::Rc,
};

use gpui::{
    prelude::*, AnyElement, App, Bounds, Context, Element, ElementId, GlobalElementId,
    InspectorElementId, LayoutId, Pixels, Window,
};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{ClipboardPreview, UiModel};
use crate::native_content::{model::ContentMode, ContentPaintObserver, ContentPaintPart};

const MAX_COMMAND_BYTES: u64 = 16 * 1024;
const ACTIONS: [&str; 6] = [
    "editor.undo",
    "editor.redo",
    "list.show",
    "theme",
    "mode",
    "list.reveal",
];

/// Owned by the isolated preview. There is no process-global command registry.
pub(super) struct Harness {
    paths: QaPaths,
    fixture_ids: BTreeSet<String>,
    commands: Rc<RefCell<CommandState>>,
    paint: Rc<RefCell<PaintState>>,
    last_file_digest: Option<blake3::Hash>,
}

#[derive(Clone)]
struct QaPaths {
    run: PathBuf,
    data_root: PathBuf,
    report_file: PathBuf,
    command_file: PathBuf,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Command {
    schema_version: u32,
    nonce: String,
    sequence: u64,
    command: String,
    value: Option<String>,
    target: Target,
}

#[derive(Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Target {
    surface: String,
    hwnd: isize,
    fixture_id: Option<String>,
    generation: u64,
    frame_id: u64,
}

struct CommandState {
    nonce: String,
    sequence: u64,
    pending: Option<u64>,
    last: Value,
}

impl CommandState {
    fn accept(&mut self, command: &Command) -> Result<(), String> {
        if command.schema_version != 1 {
            return Err("Unsupported QA command schema.".into());
        }
        if command.nonce != self.nonce {
            return Err("QA command nonce does not belong to this run.".into());
        }
        if command.sequence == 0
            || command.sequence > i64::MAX as u64
            || command.sequence <= self.sequence
        {
            return Err("QA command sequence was already consumed or is invalid.".into());
        }
        if self.pending.is_some() {
            return Err("A QA command is still awaiting its owning window.".into());
        }
        // Consume this sequence before dispatch; even a rejected action cannot
        // be replayed later against a different window/frame.
        self.sequence = command.sequence;
        self.pending = Some(command.sequence);
        Ok(())
    }

    fn finish(&mut self, command: &Command, status: &str, error: Option<String>) {
        if self.pending == Some(command.sequence) {
            self.pending = None;
        }
        self.last = json!({"nonce":command.nonce,"sequence":command.sequence,
            "command":command.command,"status":status,"error":error});
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ListIdentity {
    hwnd: isize,
    generation: u64,
    session: u64,
    fixture_id: Option<String>,
    mode: &'static str,
    theme: &'static str,
}

#[derive(Default)]
struct PaintState {
    frame_id: u64,
    identity: Option<ListIdentity>,
    painting: bool,
    completed: bool,
    scale: f32,
    width: f32,
    height: f32,
    bounds: Option<Bounds<Pixels>>,
    rows: BTreeMap<String, PaintedRow>,
}

#[derive(Clone)]
struct RowMetadata {
    id: String,
    kind: String,
    resource_ready: bool,
    background: String,
    step_count: usize,
}

struct PaintedRow {
    metadata: RowMetadata,
    row_bounds: Option<Bounds<Pixels>>,
    content_bounds: Option<Bounds<Pixels>>,
    primary_image_bounds: Option<Bounds<Pixels>>,
    primary_image_layout_bounds: Option<Bounds<Pixels>>,
    parts: BTreeMap<(&'static str, usize), ContentPaintPart>,
}

/// Capture once when constructing the ordinary root/list renderer, then wrap
/// that renderer with `wrap_content_qa`. Recording starts only in its paint pass.
#[derive(Clone)]
pub(super) struct ListPaintObserver {
    state: Rc<RefCell<PaintState>>,
    identity: ListIdentity,
}

#[derive(Clone)]
pub(super) struct RowPaintObserver {
    state: Rc<RefCell<PaintState>>,
    identity: ListIdentity,
    metadata: RowMetadata,
}

impl ListPaintObserver {
    fn begin(&self, bounds: Bounds<Pixels>, window: &Window) -> u64 {
        let mut state = self.state.borrow_mut();
        let frame_id = state.frame_id.checked_add(1).unwrap_or(1);
        let mut identity = self.identity.clone();
        identity.hwnd = window_hwnd(window);
        *state = PaintState {
            frame_id,
            identity: Some(identity),
            painting: true,
            completed: false,
            scale: window.scale_factor(),
            width: f32::from(window.viewport_size().width),
            height: f32::from(window.viewport_size().height),
            bounds: Some(bounds.intersect(&window.content_mask().bounds)),
            rows: BTreeMap::new(),
        };
        frame_id
    }

    fn finish(&self, frame: u64) {
        let mut state = self.state.borrow_mut();
        if state.frame_id == frame && state.identity.as_ref() == Some(&self.identity) {
            state.painting = false;
            state.completed = true;
        }
    }
}

impl RowPaintObserver {
    /// Pass directly to native_content::render_content_with_observer. This
    /// observer accepts only measurements emitted inside the current root paint.
    pub(super) fn content_observer(&self) -> ContentPaintObserver {
        let observer = self.clone();
        Rc::new(move |part| observer.record_part(part))
    }

    fn with_row(&self, record: impl FnOnce(&mut PaintedRow)) {
        let mut state = self.state.borrow_mut();
        if !state.painting || state.identity.as_ref() != Some(&self.identity) {
            return;
        }
        let row = state
            .rows
            .entry(self.metadata.id.clone())
            .or_insert_with(|| PaintedRow {
                metadata: self.metadata.clone(),
                row_bounds: None,
                content_bounds: None,
                primary_image_bounds: None,
                primary_image_layout_bounds: None,
                parts: BTreeMap::new(),
            });
        record(row);
    }

    pub(super) fn record_row(&self, bounds: Bounds<Pixels>, window: &Window) {
        let bounds = clipped(bounds, window);
        if let Some(bounds) = bounds {
            self.with_row(|row| row.row_bounds = Some(bounds));
        }
    }

    pub(super) fn record_content(&self, bounds: Bounds<Pixels>, window: &Window) {
        let bounds = clipped(bounds, window);
        if let Some(bounds) = bounds {
            self.with_row(|row| row.content_bounds = Some(bounds));
        }
    }

    fn record_primary_image(&self, bounds: Bounds<Pixels>, window: &Window) {
        if !positive(bounds) {
            return;
        }
        if let Some(bounds) = clipped(bounds, window) {
            self.with_row(|row| row.primary_image_bounds = Some(bounds));
        }
        self.with_row(|row| row.primary_image_layout_bounds = Some(bounds));
    }

    fn record_part(&self, part: ContentPaintPart) {
        if !positive(part.bounds) {
            return;
        }
        // Roles are emitted by native_content, but keep the reporting schema
        // fixed so a future path/text-bearing observer cannot export payloads.
        if !matches!(
            part.role,
            "step-icon"
                | "step-colour"
                | "step-text"
                | "step-stats"
                | "text"
                | "image"
                | "action-icon"
                | "action-text"
                | "file-image"
                | "file-icon"
                | "text-image"
        ) {
            return;
        }
        self.with_row(|row| {
            row.parts.insert((part.role, part.index), part);
        });
    }
}

enum PaintObserver {
    List(ListPaintObserver),
    Row(RowPaintObserver),
    Content(RowPaintObserver),
    PrimaryImage(RowPaintObserver),
}

struct ObservedPaint {
    child: AnyElement,
    observer: PaintObserver,
}

pub(super) fn wrap_content_qa(
    child: AnyElement,
    observer: Option<ListPaintObserver>,
) -> AnyElement {
    match observer {
        Some(observer) => ObservedPaint {
            child,
            observer: PaintObserver::List(observer),
        }
        .into_any_element(),
        None => child,
    }
}

pub(super) fn wrap_content_qa_row(
    child: AnyElement,
    observer: Option<RowPaintObserver>,
) -> AnyElement {
    match observer {
        Some(observer) => ObservedPaint {
            child,
            observer: PaintObserver::Row(observer),
        }
        .into_any_element(),
        None => child,
    }
}

pub(super) fn wrap_content_qa_content(
    child: AnyElement,
    observer: Option<RowPaintObserver>,
) -> AnyElement {
    match observer {
        Some(observer) => ObservedPaint {
            child,
            observer: PaintObserver::Content(observer),
        }
        .into_any_element(),
        None => child,
    }
}

/// Bounds of the actual main image element, including its existing fallback.
/// This observes geometry only; asynchronous GPUI resource readiness must be
/// proven from captured pixels, rather than inferred from a completed wrapper.
pub(super) fn wrap_content_qa_primary_image(
    child: AnyElement,
    observer: Option<RowPaintObserver>,
) -> AnyElement {
    match observer {
        Some(observer) => ObservedPaint {
            child,
            observer: PaintObserver::PrimaryImage(observer),
        }
        .into_any_element(),
        None => child,
    }
}

impl IntoElement for ObservedPaint {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for ObservedPaint {
    type RequestLayoutState = ();
    type PrepaintState = ();
    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }
    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.child.request_layout(window, cx), ())
    }
    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.prepaint(window, cx);
    }
    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let frame = match &self.observer {
            PaintObserver::List(observer) => Some(observer.begin(bounds, window)),
            _ => None,
        };
        self.child.paint(window, cx);
        match &self.observer {
            PaintObserver::List(observer) => observer.finish(frame.unwrap()),
            PaintObserver::Row(observer) => observer.record_row(bounds, window),
            PaintObserver::Content(observer) => observer.record_content(bounds, window),
            PaintObserver::PrimaryImage(observer) => observer.record_primary_image(bounds, window),
        }
    }
}

impl ClipboardPreview {
    /// Called only after the Debug importer has seeded its authored fixture IDs.
    /// Path checks use direct Core data_root, never health/data-paths discovery.
    pub fn enable_content_qa(
        &mut self,
        fixture_ids: Vec<String>,
        report_path: PathBuf,
    ) -> Result<(), String> {
        if self.qa_harness.is_some() {
            return Err("Content QA was already enabled for this preview.".into());
        }
        let UiModel::Live(service) = &self.model else {
            return Err("Content QA requires an isolated live host.".into());
        };
        if !cfg!(debug_assertions) || !service.isolated() || !cliphost_core::is_headless() {
            return Err("Content QA requires Debug, test-instance and headless.".into());
        }
        let fixture_ids = validate_ids(fixture_ids)?;
        let paths = QaPaths::new(&service.state().data_root(), &report_path)?;
        let nonce = new_nonce()?;
        self.qa_harness = Some(Harness {
            paths,
            fixture_ids,
            commands: Rc::new(RefCell::new(CommandState {
                nonce,
                sequence: 0,
                pending: None,
                last: Value::Null,
            })),
            paint: Rc::new(RefCell::new(PaintState::default())),
            last_file_digest: None,
        });
        Ok(())
    }

    fn qa_identity(&self) -> ListIdentity {
        let hwnd = self.model.host().map(|host| host.host_hwnd()).unwrap_or(0);
        ListIdentity {
            hwnd,
            generation: self.generation,
            session: self.native_session,
            fixture_id: self.qa_harness.as_ref().and_then(|qa| {
                qa.fixture_ids
                    .contains(&self.selected_id)
                    .then(|| self.selected_id.clone())
            }),
            mode: match self.content_mode() {
                ContentMode::Minimal => "Minimal",
                ContentMode::Standard => "Standard",
            },
            theme: if self.dark { "dark" } else { "light" },
        }
    }

    pub(super) fn content_qa_paint_observer(&self) -> Option<ListPaintObserver> {
        self.qa_harness.as_ref().map(|qa| ListPaintObserver {
            state: qa.paint.clone(),
            identity: self.qa_identity(),
        })
    }

    pub(super) fn content_qa_row_observer(
        &self,
        id: &str,
        kind: &str,
        resource_ready: bool,
        background: &str,
        step_count: usize,
    ) -> Option<RowPaintObserver> {
        let qa = self.qa_harness.as_ref()?;
        if !qa.fixture_ids.contains(id)
            || self.loading
            || self.favorites_active
            || self.page_generation != self.generation
        {
            return None;
        }
        // Fixed kind whitelist and RGB-only background cannot expose labels,
        // specifications, source URLs, file paths or document contents.
        if !matches!(kind, "HtmlClipItem" | "QQRichTextItem" | "StepClipItem" | "ImageClipItem") {
            return None;
        }
        let background = if background.len() == 7
            && background.starts_with('#')
            && background[1..].bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            background.to_string()
        } else {
            String::new()
        };
        Some(RowPaintObserver {
            state: qa.paint.clone(),
            identity: self.qa_identity(),
            metadata: RowMetadata {
                id: id.to_string(),
                kind: kind.to_string(),
                resource_ready,
                background,
                step_count,
            },
        })
    }

    /// The existing explicit report pump invokes this. It performs bounded local
    /// file I/O only while this exact isolated data/report boundary still exists.
    pub fn poll_content_qa(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.exit_pending {
            return;
        }
        let Some(qa) = self.qa_harness.as_ref() else {
            return;
        };
        let state = qa.commands.clone();
        if state.borrow().pending.is_some() {
            return;
        }
        if let Err(error) = self.qa_runtime_current() {
            state.borrow_mut().last = json!({"status":"rejected","error":error});
            return;
        }
        let bytes = match qa.paths.read_command() {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return,
            Err(error) => {
                state.borrow_mut().last = json!({"status":"rejected","error":error});
                return;
            }
        };
        let digest = blake3::hash(&bytes);
        if self.qa_harness.as_ref().unwrap().last_file_digest == Some(digest) {
            return;
        }
        self.qa_harness.as_mut().unwrap().last_file_digest = Some(digest);
        let command: Command = match serde_json::from_slice(&bytes) {
            Ok(command) => command,
            Err(_) => {
                state.borrow_mut().last =
                    json!({"status":"rejected","error":"Invalid or unknown QA command fields."});
                return;
            }
        };
        let acceptance = state.borrow_mut().accept(&command);
        if let Err(error) = acceptance {
            state.borrow_mut().finish(&command, "rejected", Some(error));
            return;
        }
        let result = self.validate_qa_command(&command, window, cx);
        if let Err(error) = result {
            state.borrow_mut().finish(&command, "rejected", Some(error));
            return;
        }
        if matches!(command.command.as_str(), "editor.undo" | "editor.redo") {
            let Some(editor) = self.editor_window else {
                state.borrow_mut().finish(
                    &command,
                    "rejected",
                    Some("Editor window is unavailable.".into()),
                );
                return;
            };
            let owner = cx.weak_entity();
            let redo = command.command == "editor.redo";
            // The current clipboard Window is borrowed by the report pump.
            // Defer before acquiring the separate editor Window and revalidate
            // both owner identity and actual editor diagnostics at dispatch.
            cx.defer(move |cx| {
                let validation = owner
                    .update(cx, |this, cx| {
                        this.qa_runtime_current()?;
                        if this.exit_pending || this.editor_window != Some(editor) {
                            return Err("QA editor owner changed.".into());
                        }
                        this.validate_qa_editor_target(&command.target, cx)
                    })
                    .map_err(|_| "QA preview owner was destroyed.".to_string())
                    .and_then(|result| result);
                let result = validation.and_then(|_| {
                    editor
                        .update(cx, |editor, window, cx| {
                            if window_hwnd(window) != command.target.hwnd
                                || !owned_hwnd(command.target.hwnd)
                            {
                                return Err("QA editor root HWND changed.".into());
                            }
                            editor.apply_qa_history(
                                redo,
                                command.target.generation,
                                command.target.frame_id,
                                window,
                                cx,
                            )
                        })
                        .map_err(|_| "QA editor was destroyed.".to_string())
                        .and_then(|result| result)
                });
                let (status, error) = match result {
                    Ok(()) => ("applied", None),
                    Err(error) => ("rejected", Some(error)),
                };
                state.borrow_mut().finish(&command, status, error);
                let _ = owner.update(cx, |_, cx| cx.notify());
            });
            return;
        }
        let result = match command.command.as_str() {
            "list.show" => {
                // Core headless prevents capture/receiver resolution; use the
                // ordinary owning window's Show command and maintenance path.
                let host = self.model.host().unwrap();
                host.config_update(&json!({"enableNoFocusMode":true}))
                    .map(|_| {
                        self.merge_authoritative_config(&host.config_get(), cx);
                        host.apply_window_action("setPinned", true);
                        host.apply_window_action("setCollapsed", false);
                        host.request_show();
                    })
            }
            "theme" => {
                self.save_appearance(json!({"uiThemeMode":command.value.as_deref().unwrap()}), cx);
                self.refresh_theme(window, cx);
                Ok(())
            }
            "mode" => {
                let slot = if command.value.as_deref() == Some("Minimal") {
                    "simple"
                } else {
                    "standard"
                };
                self.save_appearance(json!({"clipListAppearanceSlot":slot}), cx);
                Ok(())
            }
            "list.reveal" => {
                let id = command.value.as_deref().unwrap();
                match self.page.rows.iter().position(|row| row.item.id == id) {
                    Some(index) if !self.loading && self.page_generation == self.generation => {
                        self.select_single(index);
                        crate::history_scroll::reveal(&self.list, index);
                        cx.notify();
                        Ok(())
                    }
                    _ => Err("Authored fixture is not in the current loaded list.".into()),
                }
            }
            _ => Err("Unsupported QA command.".into()),
        };
        let (status, error) = match result {
            Ok(()) => ("applied", None),
            Err(error) => ("rejected", Some(error)),
        };
        state.borrow_mut().finish(&command, status, error);
    }

    fn qa_runtime_current(&self) -> Result<(), String> {
        let Some(qa) = self.qa_harness.as_ref() else {
            return Err("Content QA is disabled.".into());
        };
        let UiModel::Live(service) = &self.model else {
            return Err("Content QA live owner changed.".into());
        };
        if !cfg!(debug_assertions) || !service.isolated() || !cliphost_core::is_headless() {
            return Err("Content QA isolation changed.".into());
        }
        qa.paths.verify(&service.state().data_root())
    }

    fn validate_qa_command(
        &self,
        command: &Command,
        window: &Window,
        cx: &App,
    ) -> Result<(), String> {
        let qa = self.qa_harness.as_ref().unwrap();
        validate_action(command, &qa.fixture_ids)?;
        if command.target.surface == "editor" {
            return self.validate_qa_editor_target(&command.target, cx);
        }
        let identity = self.qa_identity();
        if self.favorites_active
            || command.target.hwnd != window_hwnd(window)
            || identity.hwnd != window_hwnd(window)
            || !owned_hwnd(command.target.hwnd)
        {
            return Err("QA list root HWND or route changed.".into());
        }
        let paint = qa.paint.borrow();
        let current = paint.identity.as_ref().filter(|stamp| **stamp == identity);
        let target = Target {
            surface: "list".into(),
            hwnd: identity.hwnd,
            fixture_id: identity.fixture_id,
            generation: identity.generation,
            frame_id: paint.frame_id,
        };
        validate_target(&command.target, &target, &qa.fixture_ids)?;
        if command.command != "list.show"
            && (!paint.completed
                || current.is_none()
                || paint.width != f32::from(window.viewport_size().width)
                || paint.height != f32::from(window.viewport_size().height)
                || paint.scale != window.scale_factor())
        {
            return Err("QA list paint frame is stale or incomplete.".into());
        }
        Ok(())
    }

    fn validate_qa_editor_target(&self, target: &Target, cx: &App) -> Result<(), String> {
        let qa = self.qa_harness.as_ref().ok_or("Content QA is disabled")?;
        let editor = self.editor_window.ok_or("QA editor is unavailable")?;
        let diagnostics = editor
            .read(cx)
            .map_err(|_| "QA editor was destroyed")?
            .diagnostics(cx);
        let id = diagnostics["target"]["id"]
            .as_str()
            .ok_or("QA editor target is unavailable")?;
        let current = Target {
            surface: "editor".into(),
            hwnd: diagnostics["hwnd"].as_i64().unwrap_or(0) as isize,
            fixture_id: Some(id.to_string()),
            generation: diagnostics["generation"].as_u64().unwrap_or(0),
            frame_id: diagnostics["paintFrameId"].as_u64().unwrap_or(0),
        };
        validate_target(target, &current, &qa.fixture_ids)?;
        if !owned_hwnd(current.hwnd)
            || diagnostics["paintCompleted"] != true
            || current.frame_id == 0
            || diagnostics["closed"] == true
            || diagnostics["loading"] == true
            || diagnostics["saving"] == true
            || diagnostics["composing"] == true
        {
            return Err("QA editor is unpainted, closed, busy, or composing.".into());
        }
        Ok(())
    }

    /// Output shape can be merged directly into the parent's ordinary report.
    /// Plain authored editor source is added only by its dedicated guarded
    /// diagnostics API; this adapter never reaches private editor content.
    pub fn content_qa_report(&self, window: &Window, cx: &App) -> Value {
        let Some(qa) = self.qa_harness.as_ref() else {
            return Value::Null;
        };
        if self.qa_runtime_current().is_err() {
            return json!({"qa":{"list":null,"editor":null},"qaHarness":{"status":"rejected",
                "error":"Content QA isolation or report boundary changed."}});
        }
        let commands = qa.commands.borrow();
        let identity = self.qa_identity();
        let paint = qa.paint.borrow();
        let current = !self.favorites_active
            && !self.loading
            && self.page_generation == self.generation
            && paint.identity.as_ref() == Some(&identity)
            && identity.hwnd == window_hwnd(window)
            && owned_hwnd(identity.hwnd)
            && paint.width == f32::from(window.viewport_size().width)
            && paint.height == f32::from(window.viewport_size().height)
            && paint.scale == window.scale_factor();
        let rows = if current && paint.completed {
            paint
                .rows
                .values()
                .filter_map(row_report)
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let list = json!({"hwnd":identity.hwnd,"frameId":paint.frame_id,"generation":identity.generation,
            "session":identity.session,"fixtureId":identity.fixture_id,"mode":identity.mode,"theme":identity.theme,
            "scaleFactor":window.scale_factor(),"width":f32::from(window.viewport_size().width),
            "height":f32::from(window.viewport_size().height),"paintCompleted":current&&paint.completed,
            "coordinateSpace":"window-client-logical-pixels","bounds":paint.bounds.map(rect),"rows":rows});
        let editor = self
            .editor_window
            .and_then(|editor| {
                editor.read(cx).ok().map(|editor| {
                    let metadata = editor.diagnostics(cx);
                    let authored = editor.content_qa_diagnostics(&qa.fixture_ids, cx);
                    merge_editor_report(metadata, authored)
                })
            })
            .filter(|source| {
                source["target"]["id"]
                    .as_str()
                    .is_some_and(|id| qa.fixture_ids.contains(id))
            })
            .map(editor_report)
            .unwrap_or(Value::Null);
        json!({"qaHarness":{"schemaVersion":1,"nonce":commands.nonce,"commandFile":qa.paths.command_file,
            "actions":ACTIONS,"lastCommand":commands.last},"qa":{"list":list,"editor":editor}})
    }
}

fn merge_editor_report(mut metadata: Value, authored: Value) -> Value {
    if authored["available"] != true
        || authored["generation"] != metadata["generation"]
        || authored["target"] != metadata["target"]
    {
        return metadata;
    }
    // Only the editor's explicitly guarded authored diagnostics can supply
    // these payload fields. Geometry/focus/history always remain actual paint.
    for field in [
        "draftSource",
        "draftHtml",
        "draftXml",
        "draftBody",
        "revision",
        "dirty",
        "error",
    ] {
        if let Some(value) = authored.get(field) {
            metadata[field] = value.clone();
        }
    }
    if authored["richEditor"]["available"] == true {
        metadata["authoredRich"] = authored["richEditor"].clone();
    }
    metadata
}

fn editor_report(mut source: Value) -> Value {
    // Preserve actual diagnostics first; aliases merely adapt the fixture driver.
    let mut canvas = if source["canvas"].is_object() {
        source["canvas"].clone()
    } else {
        source["richEditor"]["canvas"].clone()
    };
    if !canvas.is_object() {
        canvas = json!({});
    }
    if canvas.get("scaleFactor").is_none() {
        canvas["scaleFactor"] = source["paint"]["scaleFactor"].clone();
    }
    if let Some(range) = canvas["selectionUtf16"]
        .as_array()
        .filter(|range| range.len() == 2)
    {
        canvas["selectionUtf16"] = json!({"start":range[0],"end":range[1]});
    }
    if canvas.get("focusIsFocused").is_none() {
        canvas["focusIsFocused"] = canvas["focusObserved"].clone();
    }
    if canvas.get("atoms").is_none() {
        canvas["atoms"] = canvas["objects"].clone();
    }
    if source["authoredRich"]["available"] == true {
        canvas["projection"] = source["authoredRich"]["canvasProjection"].clone();
        canvas["unappliedError"] = source["authoredRich"]["unappliedError"].clone();
    }
    if source.get("source").is_none() {
        if let Some(value) = source.get("draftSource") {
            source["source"] = value.clone();
        }
    }
    for (alias, path) in [
        ("fixtureId", &["target", "id"][..]),
        ("itemId", &["target", "id"][..]),
        ("kind", &["documentKind"][..]),
        ("tab", &["visibleTab"][..]),
    ] {
        if source.get(alias).is_none() {
            let value = path.iter().fold(&source, |value, key| &value[*key]).clone();
            source[alias] = value;
        }
    }
    source["canvas"] = canvas;
    source
}

fn row_report(row: &PaintedRow) -> Option<Value> {
    let bounds = row.row_bounds?;
    let parts = row
        .parts
        .values()
        .map(|part| {
            json!({"role":part.role,"index":part.index,
        "bounds":rect(part.bounds),"available":part.available})
        })
        .collect::<Vec<_>>();
    let indexes = row
        .parts
        .keys()
        .filter_map(|(role, index)| matches!(*role, "step-icon" | "step-text").then_some(*index))
        .collect::<BTreeSet<_>>();
    let lines = indexes
        .into_iter()
        .map(|index| {
            let part = |role| row.parts.get(&(role, index)).map(|part| rect(part.bounds));
            json!({"index":index,"iconBounds":part("step-icon"),"textBounds":part("step-text"),
            "statsBounds":row.parts.get(&("step-stats",0)).map(|part|rect(part.bounds))})
        })
        .collect::<Vec<_>>();
    Some(
        json!({"fixtureId":row.metadata.id,"itemId":row.metadata.id,"visible":true,
        "resourceReady":if row.metadata.kind=="ImageClipItem" { Value::Null } else { json!(row.metadata.resource_ready&&row.parts.values().all(|part|part.available)) },
        "primaryImageBounds":row.primary_image_bounds.map(rect),
        "primaryImageLayoutBounds":row.primary_image_layout_bounds.map(rect),
        "primaryImageResourceReady":Value::Null,
        "contentKind":row.metadata.kind,"rowBounds":rect(bounds),"contentBounds":row.content_bounds.map(rect),
        "background":row.metadata.background,"stepCount":row.metadata.step_count,
        "renderedLineCount":lines.len(),"stepLines":lines,"parts":parts,
        "unavailableFields":["iconInkBounds","textInkBounds","expectedIconSpec","resolvedFill","opacity","fallback","primaryImageResourceReady"]}),
    )
}

fn validate_ids(ids: Vec<String>) -> Result<BTreeSet<String>, String> {
    if ids.is_empty() || ids.len() > 32 {
        return Err("Content QA requires 1 to 32 imported authored IDs.".into());
    }
    let len = ids.len();
    if ids.iter().any(|id| {
        id.is_empty()
            || id.len() > 128
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    }) {
        return Err("Invalid authored fixture ID.".into());
    }
    let ids = ids.into_iter().collect::<BTreeSet<_>>();
    if ids.len() != len {
        return Err("Duplicate authored fixture ID.".into());
    }
    Ok(ids)
}

fn validate_action(command: &Command, ids: &BTreeSet<String>) -> Result<(), String> {
    let (surface, valid) = match command.command.as_str() {
        "editor.undo" | "editor.redo" => ("editor", command.value.is_none()),
        "list.show" => ("list", command.value.is_none()),
        "theme" => (
            "list",
            matches!(command.value.as_deref(), Some("light" | "dark")),
        ),
        "mode" => (
            "list",
            matches!(command.value.as_deref(), Some("Standard" | "Minimal")),
        ),
        "list.reveal" => (
            "list",
            command.value.as_ref().is_some_and(|id| ids.contains(id)),
        ),
        _ => return Err("Unsupported QA command.".into()),
    };
    if !valid || command.target.surface != surface {
        return Err("Invalid QA action value or target surface.".into());
    }
    Ok(())
}

fn validate_target(
    target: &Target,
    current: &Target,
    ids: &BTreeSet<String>,
) -> Result<(), String> {
    if target.hwnd == 0 || target != current {
        return Err("QA target HWND, fixture, generation or paint frame changed.".into());
    }
    if !target
        .fixture_id
        .as_ref()
        .is_some_and(|id| ids.contains(id))
    {
        return Err("QA target is not an imported authored fixture.".into());
    }
    Ok(())
}

impl QaPaths {
    fn new(data_root: &Path, report: &Path) -> Result<Self, String> {
        if !report.is_absolute() || report.file_name().is_none() {
            return Err("QA report requires an explicit absolute file path.".into());
        }
        let tmp = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../..")
            .join(".tmp");
        let tmp = canonical_safe(&tmp)?;
        let run = canonical_safe(report.parent().ok_or("QA report parent is missing")?)?;
        let data_root = canonical_safe(data_root)?;
        if !run.starts_with(&tmp) || run == tmp || data_root.parent() != Some(run.as_path()) {
            return Err(
                "QA report and Core data root must share one fresh repository .tmp run.".into(),
            );
        }
        if report.exists() {
            let resolved = canonical_safe(report)?;
            if resolved.parent() != Some(run.as_path()) {
                return Err("QA report escaped its run.".into());
            }
        }
        Ok(Self {
            command_file: run.join("qa-command.json"),
            report_file: run.join(report.file_name().unwrap()),
            run,
            data_root,
        })
    }

    fn verify(&self, data_root: &Path) -> Result<(), String> {
        if canonical_safe(&self.run)? != self.run
            || canonical_safe(data_root)? != self.data_root
            || self.data_root.parent() != Some(self.run.as_path())
        {
            return Err("QA isolated owner or paths changed.".into());
        }
        match fs::symlink_metadata(&self.report_file) {
            Ok(metadata) if !metadata.is_file() => return Err("QA report path changed.".into()),
            Ok(_) if canonical_safe(&self.report_file)? != self.report_file => {
                return Err("QA report escaped its run.".into())
            }
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err("QA report metadata is unavailable.".into())
            }
            _ => {}
        }
        Ok(())
    }

    fn read_command(&self) -> Result<Option<Vec<u8>>, String> {
        match fs::symlink_metadata(&self.command_file) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err("QA command metadata is unavailable.".into()),
            Ok(metadata) if !metadata.is_file() || metadata.len() > MAX_COMMAND_BYTES => {
                return Err("QA command must be a file no larger than 16 KiB.".into())
            }
            Ok(_) => {}
        }
        let resolved = canonical_safe(&self.command_file)?;
        if resolved.parent() != Some(self.run.as_path()) || resolved != self.command_file {
            return Err("QA command escaped its report directory.".into());
        }
        let mut file = File::open(&resolved).map_err(|_| "QA command cannot be opened.")?;
        let mut bytes = Vec::new();
        file.by_ref()
            .take(MAX_COMMAND_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "QA command cannot be read.")?;
        if bytes.len() as u64 > MAX_COMMAND_BYTES || canonical_safe(&self.command_file)? != resolved
        {
            return Err("QA command size or path changed while reading.".into());
        }
        Ok(Some(bytes))
    }
}

fn canonical_safe(path: &Path) -> Result<PathBuf, String> {
    for ancestor in path
        .ancestors()
        .filter(|ancestor| !ancestor.as_os_str().is_empty())
    {
        reject_reparse(ancestor)?;
    }
    let path = fs::canonicalize(path).map_err(|_| "QA path must exist and be canonicalizable.")?;
    // canonicalize would hide a symlink in the supplied path. Inspect both the
    // original route and resolved route, including all ancestors.
    for ancestor in path.ancestors() {
        reject_reparse(ancestor)?;
    }
    Ok(path)
}

fn reject_reparse(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(|_| "QA path metadata is unavailable.")?;
    if metadata.file_type().is_symlink() {
        return Err("QA paths may not contain symlinks or reparse points.".into());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err("QA paths may not contain reparse points.".into());
        }
    }
    Ok(())
}

fn new_nonce() -> Result<String, String> {
    #[cfg(windows)]
    {
        let guid = unsafe { windows_compat::Win32::System::Com::CoCreateGuid() }
            .map_err(|_| "QA nonce could not be generated.")?;
        return Ok(format!(
            "{:08x}{:04x}{:04x}{}",
            guid.data1,
            guid.data2,
            guid.data3,
            guid.data4
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ));
    }
    #[cfg(not(windows))]
    {
        let mut bytes = [0u8; 16];
        File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(&mut bytes))
            .map_err(|_| "QA nonce could not be generated.")?;
        Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
    }
}

fn window_hwnd(window: &Window) -> isize {
    #[cfg(windows)]
    {
        crate::platform::hwnd(window)
            .map(|hwnd| hwnd.0 as isize)
            .unwrap_or(0)
    }
    #[cfg(not(windows))]
    {
        let _ = window;
        0
    }
}

fn owned_hwnd(hwnd: isize) -> bool {
    if hwnd == 0 {
        return false;
    }
    #[cfg(windows)]
    {
        let mut pid = 0;
        unsafe {
            windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(
                windows::Win32::Foundation::HWND(hwnd as *mut _),
                Some(&mut pid),
            );
        }
        pid == std::process::id()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn positive(bounds: Bounds<Pixels>) -> bool {
    [
        bounds.origin.x,
        bounds.origin.y,
        bounds.size.width,
        bounds.size.height,
    ]
    .into_iter()
    .all(|p| f32::from(p).is_finite())
        && bounds.size.width > gpui::px(0.)
        && bounds.size.height > gpui::px(0.)
}
fn clipped(bounds: Bounds<Pixels>, window: &Window) -> Option<Bounds<Pixels>> {
    let bounds = bounds.intersect(&window.content_mask().bounds);
    positive(bounds).then_some(bounds)
}
fn rect(bounds: Bounds<Pixels>) -> Value {
    json!({"x":f32::from(bounds.origin.x),"y":f32::from(bounds.origin.y),
    "width":f32::from(bounds.size.width),"height":f32::from(bounds.size.height)})
}

#[cfg(test)]
mod model_tests {
    use super::*;
    fn command() -> Command {
        Command {
            schema_version: 1,
            nonce: "a".repeat(32),
            sequence: 1,
            command: "list.reveal".into(),
            value: Some("qa-step".into()),
            target: Target {
                surface: "list".into(),
                hwnd: 12,
                fixture_id: Some("qa-step".into()),
                generation: 4,
                frame_id: 8,
            },
        }
    }
    fn state() -> CommandState {
        CommandState {
            nonce: "a".repeat(32),
            sequence: 0,
            pending: None,
            last: Value::Null,
        }
    }
    fn ids() -> BTreeSet<String> {
        validate_ids(vec!["qa-step".into(), "qa-rich".into()]).unwrap()
    }

    #[test]
    fn nonce_and_repeat_cannot_dispatch_again() {
        let mut state = state();
        let mut command = command();
        command.nonce = "b".repeat(32);
        assert!(state.accept(&command).is_err());
        assert_eq!(state.sequence, 0);
        command.nonce = "a".repeat(32);
        state.accept(&command).unwrap();
        assert!(state.accept(&command).is_err());
        state.finish(&command, "applied", None);
        assert!(state.accept(&command).is_err());
        command.sequence = 2;
        state.accept(&command).unwrap();
        assert_eq!(state.pending, Some(2));
    }

    #[test]
    fn owner_id_generation_and_real_frame_are_exact() {
        let command = command();
        let current = command.target.clone();
        validate_target(&command.target, &current, &ids()).unwrap();
        for target in [
            Target {
                hwnd: 13,
                ..current.clone()
            },
            Target {
                fixture_id: Some("user-content".into()),
                ..current.clone()
            },
            Target {
                generation: 5,
                ..current.clone()
            },
            Target {
                frame_id: 9,
                ..current.clone()
            },
            Target {
                surface: "editor".into(),
                ..current.clone()
            },
        ] {
            assert!(validate_target(&target, &current, &ids()).is_err());
        }
        let unknown = Target {
            fixture_id: Some("unknown".into()),
            ..current
        };
        assert!(validate_target(&unknown, &unknown, &ids()).is_err());
    }

    #[test]
    fn fixed_actions_do_not_admit_arbitrary_methods_or_values() {
        let mut command = command();
        validate_action(&command, &ids()).unwrap();
        for (action, value) in [
            ("paste", None),
            ("theme", Some("auto")),
            ("mode", Some("Custom")),
            ("list.reveal", Some("unimported")),
            ("editor.undo", None),
        ] {
            command.command = action.into();
            command.value = value.map(str::to_string);
            assert!(validate_action(&command, &ids()).is_err());
        }
        let mut value=serde_json::to_value(json!({"schemaVersion":1,"nonce":"a".repeat(32),"sequence":1,"command":"list.show","value":null,
            "target":{"surface":"list","hwnd":12,"fixtureId":"qa-step","generation":4,"frameId":8}})).unwrap();
        value["method"] = json!("arbitrary");
        assert!(serde_json::from_value::<Command>(value).is_err());
    }

    #[test]
    fn ids_and_nonce_are_bounded_and_run_specific() {
        assert!(validate_ids(vec![]).is_err());
        assert!(validate_ids(vec!["same".into(), "same".into()]).is_err());
        assert!(validate_ids(vec!["../user".into()]).is_err());
        let a = new_nonce().unwrap();
        let b = new_nonce().unwrap();
        assert_ne!(a, b);
        assert_eq!(a.len(), 32);
        assert!(a.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    #[test]
    fn editor_adapter_keeps_actual_geometry_and_does_not_invent_source() {
        let report = editor_report(
            json!({"target":{"id":"qa-rich"},"documentKind":"HtmlClipItem","visibleTab":"rich",
            "hwnd":11,"generation":7,"controls":{"save":{"frameId":23}},"paint":{"scaleFactor":1.25},
            "richEditor":{"canvas":{"frameId":23,"bounds":{"x":9},"caretStops":[{"utf16":4,"x":10,"y":20}]}}}),
        );
        assert_eq!(report["canvas"]["frameId"], 23);
        assert_eq!(report["canvas"]["caretStops"][0]["x"], 10);
        assert_eq!(report["canvas"]["scaleFactor"], 1.25);
        assert_eq!(report["fixtureId"], "qa-rich");
        assert!(report.get("source").is_none());
        assert!(report["canvas"].get("projection").is_none());
    }

    #[test]
    fn authored_report_requires_same_owner_and_preserves_actual_paint() {
        let metadata = json!({"target":{"type":"history","id":"qa-rich"},"generation":7,
            "paint":{"scaleFactor":1.25},"richEditor":{"canvas":{"frameId":23,
            "selectionUtf16":[4,4],"focusObserved":true,"objects":[],"bounds":{"x":9}}}});
        let authored = json!({"available":true,"target":{"type":"history","id":"qa-rich"},"generation":7,
            "draftSource":"authored source","dirty":true,"revision":"fixture-revision",
            "richEditor":{"available":true,"canvasProjection":"fixture projection","unappliedError":null}});
        let report = editor_report(merge_editor_report(metadata.clone(), authored.clone()));
        assert_eq!(report["source"], "authored source");
        assert_eq!(report["dirty"], true);
        assert_eq!(report["canvas"]["projection"], "fixture projection");
        assert_eq!(report["canvas"]["frameId"], 23);
        assert_eq!(report["canvas"]["bounds"]["x"], 9);
        assert_eq!(
            report["canvas"]["selectionUtf16"],
            json!({"start":4,"end":4})
        );
        assert_eq!(report["canvas"]["focusIsFocused"], true);
        let mut old = authored;
        old["generation"] = json!(6);
        let report = editor_report(merge_editor_report(metadata, old));
        assert!(report.get("source").is_none());
        assert!(report["canvas"].get("projection").is_none());
    }

    #[test]
    fn command_file_remains_in_explicit_run_and_is_bounded() {
        let tmp = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../..")
            .join(".tmp");
        let run = tempfile::Builder::new()
            .prefix("qa-content-model-")
            .tempdir_in(&tmp)
            .unwrap();
        let data = run.path().join("data");
        fs::create_dir(&data).unwrap();
        let paths = QaPaths::new(&data, &run.path().join("report.json")).unwrap();
        assert!(paths.read_command().unwrap().is_none());
        fs::write(&paths.command_file, b"{}").unwrap();
        assert_eq!(paths.read_command().unwrap().unwrap(), b"{}");
        fs::write(
            &paths.command_file,
            vec![b' '; MAX_COMMAND_BYTES as usize + 1],
        )
        .unwrap();
        assert!(paths.read_command().is_err());
        let other = run.path().join("other");
        fs::create_dir(&other).unwrap();
        assert!(paths.verify(&other).is_err());
        assert!(QaPaths::new(&data, &other.join("report.json")).is_err());
    }
}
