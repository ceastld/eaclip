//! Main history Add Text and deferred-clear UI. The shared core owns the
//! records, the five-second hidden snapshot, cancellation and final deletion.
//! The parent places `banner_view` in normal flow and `dialog_view` last over
//! the history root; it forwards `Changed` to its existing list refresh.

use gpui_rsx::rsx;
use crate::smooth_wheel::SmoothWheelExt;
use std::{
    collections::BTreeMap,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{mpsc, Arc, Mutex, OnceLock, Weak},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use cliphost_core::{i18n::tr, HostState};
use gpui::{
    div, prelude::*, px, AnyElement, Context, Entity, EventEmitter, FocusHandle, FontWeight,
    KeyDownEvent, ScrollHandle, Subscription, Window,
};
use serde_json::{json, Value};

use crate::{
    input::InputColors,
    multiline::{MultilineEvent, MultilineInput},
    native_host::NativeWindow,
    platform,
    preview::DesignTokens,
    view::color,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClearScope {
    All,
    NonTop,
}

impl ClearScope {
    fn non_top(self) -> bool {
        self == Self::NonTop
    }
    fn heading(self) -> &'static str {
        match self {
            Self::All => tr("清空全部剪贴板历史", "Clear all clipboard history"),
            Self::NonTop => tr("清空非置顶记录", "Clear unpinned history"),
        }
    }
    fn description(self) -> &'static str {
        match self {
            Self::All => tr(
                "将立即从列表中隐藏全部记录（含置顶）。约 5 秒内可撤销；超时后才会永久删除，且无法恢复。",
                "All records, including pinned items, will be hidden immediately. Undo is available for about 5 seconds before permanent deletion.",
            ),
            Self::NonTop => tr(
                "将立即从列表中隐藏所有非置顶记录。约 5 秒内可撤销；超时后才会永久删除，且无法恢复。",
                "Unpinned records will be hidden immediately. Undo is available for about 5 seconds before permanent deletion.",
            ),
        }
    }
    fn confirm(self, scoped: bool) -> &'static str {
        match (self, scoped) {
            (Self::All, true) => tr("确认清空全部", "Confirm clear all"),
            (Self::All, false) => tr("清空全部", "Clear all"),
            (Self::NonTop, true) => tr("确认清空非置顶", "Confirm clear unpinned"),
            (Self::NonTop, false) => tr("清空非置顶", "Clear unpinned"),
        }
    }
}

#[derive(Debug, Clone)]
pub enum HistoryActionsEvent {
    /// A completed mutation or a changed core undo snapshot needs a list refresh.
    Changed,
    /// Include `has_dialog()` in the parent's aggregate overlay guard. On false,
    /// restore the parent's own browsing FocusHandle after this notification.
    DialogChanged(bool),
    /// Also show outside the dialog using the parent's existing error surface.
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingClear {
    pub clear_id: String,
    pub scope: ClearScope,
    pub item_count: u64,
    pub expires_at_unix_ms: i64,
}

impl PendingClear {
    fn from_value(value: &Value) -> Option<Self> {
        let clear_id = value["clearId"].as_str()?.trim().to_string();
        let item_count = value["itemCount"].as_u64()?;
        if clear_id.is_empty() || item_count == 0 {
            return None;
        }
        Some(Self {
            clear_id,
            scope: if value["mode"] == "all" {
                ClearScope::All
            } else {
                ClearScope::NonTop
            },
            item_count,
            expires_at_unix_ms: value["expiresAtUnixMs"].as_i64().unwrap_or(0),
        })
    }

    /// An expired display deadline is not proof of core deletion. Keep the
    /// banner until clear_pending actually retires the operation.
    pub fn seconds_left_at(&self, now_ms: i64) -> u64 {
        let remaining = self.expires_at_unix_ms.saturating_sub(now_ms).max(0) as u64;
        remaining / 1000 + u64::from(remaining % 1000 != 0)
    }

    fn label(&self, now_ms: i64) -> String {
        let count = self.item_count;
        let start = match self.scope {
            ClearScope::All => cliphost_core::trf!(
                "已安排清空全部记录（{count} 条）",
                "Clear scheduled for all records ({count} items)"
            ),
            ClearScope::NonTop => cliphost_core::trf!(
                "已安排清空非置顶记录（{count} 条）",
                "Clear scheduled for unpinned records ({count} items)"
            ),
        };
        let seconds = self.seconds_left_at(now_ms);
        let end = if seconds > 0 {
            cliphost_core::trf!(
                "{seconds} 秒后永久删除",
                "Permanent deletion in {seconds} seconds"
            )
        } else {
            tr("即将永久删除", "Permanent deletion is pending").to_string()
        };
        format!("{start} · {end}")
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dialog {
    AddText,
    Clear(Option<ClearScope>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AddDraft {
    text: String,
    line_by_line: bool,
}
impl Default for AddDraft {
    fn default() -> Self {
        Self {
            text: String::new(),
            line_by_line: true,
        }
    }
}
impl AddDraft {
    fn request(&self) -> Value {
        // ClipboardPage submits addTextDraft verbatim, unlike FavoritesPage.
        json!({"text":self.text,"lineByLine":self.line_by_line})
    }
}

#[derive(Debug, Clone)]
enum HistoryWrite {
    Add(AddDraft),
    Clear(ClearScope),
    Cancel(String),
}

#[derive(Debug, Clone)]
struct FailedWrite {
    id: u64,
    write: HistoryWrite,
    error: String,
}

#[derive(Default)]
struct WriteState {
    next_id: u64,
    pending: usize,
    failures: BTreeMap<u64, FailedWrite>,
}

type WriteReply = tokio::sync::oneshot::Receiver<Result<Value, String>>;
type WriteBarrier = tokio::sync::oneshot::Receiver<Result<(), String>>;

enum WriteCommand {
    Write {
        host: Arc<HostState>,
        id: u64,
        write: HistoryWrite,
        replaces: Option<u64>,
        reply: tokio::sync::oneshot::Sender<Result<Value, String>>,
    },
    Barrier(tokio::sync::oneshot::Sender<Result<(), String>>),
}

/// The sender is registered before an observer future is spawned. The worker
/// never owns a UI entity, and each accepted command keeps its core alive.
struct HistoryWriteQueue {
    host: Weak<HostState>,
    sender: mpsc::Sender<WriteCommand>,
    state: Arc<Mutex<WriteState>>,
}

impl HistoryWriteQueue {
    fn new(host: &Arc<HostState>) -> Arc<Self> {
        Self::with_writer(host, |host, write| match write {
            HistoryWrite::Add(draft) => host.add_text(&draft.request()),
            HistoryWrite::Clear(scope) => host.clear_all(scope.non_top()),
            // The ID belongs to the accepted Undo, never a replacement clear.
            HistoryWrite::Cancel(clear_id) => host.clear_cancel(&json!({"clearId":clear_id})),
        })
    }

    fn with_writer(
        host: &Arc<HostState>,
        mut writer: impl FnMut(&HostState, &HistoryWrite) -> Result<Value, String> + Send + 'static,
    ) -> Arc<Self> {
        let (sender, receiver) = mpsc::channel();
        let state = Arc::new(Mutex::new(WriteState::default()));
        let worker_state = state.clone();
        std::thread::spawn(move || {
            while let Ok(command) = receiver.recv() {
                match command {
                    WriteCommand::Write {
                        host,
                        id,
                        write,
                        replaces,
                        reply,
                    } => {
                        let result = catch_unwind(AssertUnwindSafe(|| writer(&host, &write)))
                            .unwrap_or_else(|_| {
                                Err(tr("历史操作线程失败", "History operation worker failed")
                                    .into())
                            });
                        {
                            let mut state = worker_state.lock().unwrap_or_else(|e| e.into_inner());
                            state.pending = state.pending.saturating_sub(1);
                            if let Some(replaces) = replaces {
                                state.failures.remove(&replaces);
                            }
                            if let Err(error) = &result {
                                state.failures.insert(
                                    id,
                                    FailedWrite {
                                        id,
                                        write,
                                        error: error.clone(),
                                    },
                                );
                            }
                        }
                        let _ = reply.send(result);
                    }
                    WriteCommand::Barrier(reply) => {
                        let state = worker_state.lock().unwrap_or_else(|e| e.into_inner());
                        let result = state
                            .failures
                            .values()
                            .next()
                            .map_or(Ok(()), |failure| Err(failure.error.clone()));
                        let _ = reply.send(result);
                    }
                }
            }
        });
        Arc::new(Self {
            host: Arc::downgrade(host),
            sender,
            state,
        })
    }

    fn submit(
        &self,
        host: Arc<HostState>,
        write: HistoryWrite,
        replaces: Option<u64>,
    ) -> (u64, WriteReply) {
        let (reply, receiver) = tokio::sync::oneshot::channel();
        // Both acceptance and the barrier enqueue under this lock. A barrier
        // cannot pass an accepted command whose worker has not started yet.
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.next_id = state.next_id.wrapping_add(1);
        let id = state.next_id;
        state.pending += 1;
        if let Err(error) = self.sender.send(WriteCommand::Write {
            host,
            id,
            write,
            replaces,
            reply,
        }) {
            if let WriteCommand::Write { write, reply, .. } = error.0 {
                state.pending -= 1;
                let error =
                    tr("历史操作线程不可用", "History operation worker unavailable").to_string();
                state.failures.insert(
                    id,
                    FailedWrite {
                        id,
                        write,
                        error: error.clone(),
                    },
                );
                let _ = reply.send(Err(error));
            }
        }
        (id, receiver)
    }

    fn barrier(&self) -> WriteBarrier {
        let (reply, receiver) = tokio::sync::oneshot::channel();
        let _state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(error) = self.sender.send(WriteCommand::Barrier(reply)) {
            if let WriteCommand::Barrier(reply) = error.0 {
                let _ = reply.send(Err(tr(
                    "历史操作线程不可用",
                    "History operation worker unavailable",
                )
                .into()));
            }
        }
        receiver
    }

    fn latest_failed_add(&self) -> Option<FailedWrite> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .failures
            .values()
            .rev()
            .find(|failure| matches!(failure.write, HistoryWrite::Add(_)))
            .cloned()
    }

    fn failures(&self) -> Vec<FailedWrite> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .failures
            .values()
            .cloned()
            .collect()
    }

    fn acknowledge(&self, id: u64) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .failures
            .remove(&id);
    }

    fn is_pending(&self) -> bool {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).pending != 0
    }
}

static HISTORY_WRITE_QUEUES: OnceLock<Mutex<Vec<Arc<HistoryWriteQueue>>>> = OnceLock::new();

fn history_queue(host: &Arc<HostState>) -> Arc<HistoryWriteQueue> {
    let mut queues = HISTORY_WRITE_QUEUES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(queue) = queues
        .iter()
        .find(|queue| Weak::ptr_eq(&queue.host, &Arc::downgrade(host)))
    {
        return queue.clone();
    }
    let queue = HistoryWriteQueue::new(host);
    queues.push(queue.clone());
    queue
}

/// Wait for accepted Add/Clear/Undo transactions, including destroyed owners.
/// This does not submit an unaccepted draft or finalize the core's five-second
/// clear snapshot. The root must gate new actions with `set_exit_pending` first.
pub async fn flush_history_action_writes() -> Result<(), String> {
    let Some(registry) = HISTORY_WRITE_QUEUES.get() else {
        return Ok(());
    };
    flush_registered_writes(registry).await
}

async fn flush_registered_writes(
    registry: &Mutex<Vec<Arc<HistoryWriteQueue>>>,
) -> Result<(), String> {
    let mut first_error = None;
    loop {
        let queues = registry.lock().unwrap_or_else(|e| e.into_inner()).clone();
        // Enqueue every barrier before awaiting any of them.
        let barriers = queues
            .iter()
            .map(|queue| queue.barrier())
            .collect::<Vec<_>>();
        for barrier in barriers {
            let result = barrier.await.unwrap_or_else(|_| {
                Err(tr("历史操作线程不可用", "History operation worker unavailable").into())
            });
            if let Err(error) = result {
                first_error.get_or_insert(error);
            }
        }
        let mut registered = registry.lock().unwrap_or_else(|e| e.into_inner());
        if registered.len() != queues.len() || registered.iter().any(|queue| queue.is_pending()) {
            continue;
        }
        // Non-draft errors have now reached the exit error surface. Failed Add
        // drafts remain recoverable until retry or explicit dialog Cancel.
        for queue in registered.iter() {
            let mut state = queue.state.lock().unwrap_or_else(|e| e.into_inner());
            state
                .failures
                .retain(|_, failure| matches!(failure.write, HistoryWrite::Add(_)));
        }
        registered.retain(|queue| Arc::strong_count(queue) > 2 || !queue.failures().is_empty());
        return first_error.map_or(Ok(()), Err);
    }
}

#[derive(Debug, Clone, Copy)]
struct AcceptedOperation {
    operation: u64,
    dialog_generation: u64,
    draft_revision: u64,
}
impl AcceptedOperation {
    fn owns_dialog(self, operation: u64, generation: u64, revision: u64, composing: bool) -> bool {
        self.operation == operation
            && self.dialog_generation == generation
            && self.draft_revision == revision
            && !composing
    }
}

pub struct HistoryActions {
    host: Arc<HostState>,
    writes: Arc<HistoryWriteQueue>,
    tokens: DesignTokens,
    dark: bool,
    active: bool,
    dialog: Option<Dialog>,
    generation: u64,
    revision: u64,
    operation: u64,
    busy: bool,
    exit_pending: bool,
    dialog_failure: Option<u64>,
    last_failure_seen: u64,
    undo_busy: bool,
    undo_operation: u64,
    draft: AddDraft,
    body: Entity<MultilineInput>,
    dialog_focus: FocusHandle,
    scroll: ScrollHandle,
    error: Option<String>,
    pending: Option<PendingClear>,
    seconds_left: u64,
    last_pending_check: Instant,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<HistoryActionsEvent> for HistoryActions {}

impl HistoryActions {
    pub fn new(
        host: Arc<HostState>,
        tokens: DesignTokens,
        dark: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let body = cx.new(|cx| MultilineInput::new(window, cx));
        let subscription =
            cx.subscribe_in(
                &body,
                window,
                |this, input, event, window, cx| match event {
                    MultilineEvent::Changed(_) => {
                        // Read the control instead of replaying an event queued
                        // before a fresh dialog reset its text.
                        if this.active && this.dialog == Some(Dialog::AddText) {
                            let text = input.read(cx).text().to_string();
                            if this.draft.text != text {
                                this.draft.text = text;
                                this.revision = this.revision.wrapping_add(1);
                                this.error = None;
                            }
                            cx.notify();
                        }
                    }
                    MultilineEvent::Save => this.submit_add(window, cx),
                    MultilineEvent::Escape => {
                        this.close_dialog(window, cx);
                    }
                    MultilineEvent::Submit | MultilineEvent::PasteRequested(_) => {}
                },
            );
        let mut actions = Self {
            pending: PendingClear::from_value(&host.clear_pending()),
            writes: history_queue(&host),
            host,
            tokens,
            dark,
            active: true,
            dialog: None,
            generation: 0,
            revision: 0,
            operation: 0,
            busy: false,
            exit_pending: false,
            dialog_failure: None,
            last_failure_seen: 0,
            undo_busy: false,
            undo_operation: 0,
            draft: AddDraft::default(),
            body,
            dialog_focus: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            error: None,
            seconds_left: 0,
            last_pending_check: Instant::now(),
            _subscriptions: vec![subscription],
        };
        actions.configure_input(cx);
        actions.seconds_left = actions
            .pending
            .as_ref()
            .map_or(0, |pending| pending.seconds_left_at(now_ms()));
        actions
    }

    pub fn has_dialog(&self) -> bool {
        self.active && self.dialog.is_some()
    }
    pub fn pending_clear(&self) -> Option<&PendingClear> {
        self.pending.as_ref()
    }

    /// Root exit waits only accepted writes. It must release this gate after a
    /// failed flush so the original failed draft can be retried or cancelled.
    pub fn set_exit_pending(&mut self, pending: bool, cx: &mut Context<Self>) {
        self.exit_pending = pending;
        // Never cancel an in-progress IME composition just to gate actions.
        if !self.body.read(cx).is_composing() {
            self.body.update(cx, |input, cx| {
                input.set_read_only(self.busy || pending, cx)
            });
        }
        cx.notify();
    }

    pub fn set_theme(&mut self, dark: bool, cx: &mut Context<Self>) {
        if self.dark == dark {
            return;
        }
        self.dark = dark;
        self.refresh_input_colors(cx);
        cx.notify();
    }

    /// Apply the parent's current palette even when light/dark is unchanged.
    /// Updating the existing input's colors preserves the active dialog draft,
    /// selection, IME composition and in-flight operation state.
    pub fn set_design_tokens(&mut self, tokens: DesignTokens, dark: bool, cx: &mut Context<Self>) {
        self.tokens = tokens;
        self.dark = dark;
        self.refresh_input_colors(cx);
        cx.notify();
    }

    fn configure_input(&mut self, cx: &mut Context<Self>) {
        self.refresh_input_colors(cx);
        self.body.update(cx, |input, cx| {
            input.set_metrics(160., self.tokens.typography.font_size, 8., cx);
        });
    }

    fn refresh_input_colors(&mut self, cx: &mut Context<Self>) {
        let t = self.tokens.theme(self.dark);
        let colors = InputColors {
            // app.css `.clip-add-text-textarea`: surface-tertiary.
            background: color(&t.tertiary),
            foreground: color(&t.text),
            placeholder: color(&t.text_muted),
            border: color(&t.border),
            selection: color(&t.selected),
            caret: color(&t.accent),
        };
        self.body.update(cx, |input, cx| {
            input.set_colors(colors, cx);
        });
    }

    fn protect_modal(&self) {
        self.host.apply_window_action(
            "setSpaModalOpen",
            self.has_dialog() || crate::editor::has_active_input_windows(),
        );
    }

    fn open(&mut self, dialog: Dialog, window: &mut Window, cx: &mut Context<Self>) {
        if self.exit_pending || self.busy || self.has_dialog() && self.body.read(cx).is_composing()
        {
            return;
        }
        if !self.has_dialog() && self.body.read(cx).is_composing() {
            let text = self.body.read(cx).text().to_string();
            self.body
                .update(cx, |input, cx| input.set_text(text, window, cx));
        }
        self.active = true;
        self.generation = self.generation.wrapping_add(1);
        self.dialog = Some(dialog);
        self.error = None;
        self.protect_modal();
        cx.emit(HistoryActionsEvent::DialogChanged(true));
        let generation = self.generation;
        cx.defer_in(window, move |this, window, cx| {
            if !this.has_dialog() || this.generation != generation {
                return;
            }
            match NativeWindow::request_search_focus(window, &this.host) {
                Ok(()) => {
                    let focus = if this.dialog == Some(Dialog::AddText) {
                        this.body.read(cx).focus_handle()
                    } else {
                        this.dialog_focus.clone()
                    };
                    window.focus(&focus);
                }
                Err(error) => this.report_error(error, cx),
            }
            cx.notify();
        });
        cx.notify();
    }

    pub fn open_add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.exit_pending || self.busy || self.has_dialog() && self.body.read(cx).is_composing()
        {
            return;
        }
        let recovered = self.writes.latest_failed_add();
        self.draft = recovered
            .as_ref()
            .and_then(|failure| match &failure.write {
                HistoryWrite::Add(draft) => Some(draft.clone()),
                _ => None,
            })
            .unwrap_or_default();
        self.dialog_failure = recovered.as_ref().map(|failure| failure.id);
        self.revision = self.revision.wrapping_add(1);
        let text = self.draft.text.clone();
        self.body.update(cx, |input, cx| {
            input.set_read_only(false, cx);
            input.set_text(text, window, cx);
        });
        self.open(Dialog::AddText, window, cx);
        if let Some(failure) = recovered {
            self.last_failure_seen = self.last_failure_seen.max(failure.id);
            self.report_error(failure.error, cx);
        }
    }

    /// ClipboardPage's existing one-step choice: all / unpinned / cancel.
    pub fn open_clear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open(Dialog::Clear(None), window, cx);
    }

    /// Explicit scope entry points share ClipClearConfirmDialog's copy.
    pub fn open_clear_scope(
        &mut self,
        scope: ClearScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open(Dialog::Clear(Some(scope)), window, cx);
    }

    pub fn close_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.has_dialog() || self.exit_pending || self.busy || self.body.read(cx).is_composing()
        {
            return false;
        }
        // Only an explicit Cancel/backdrop/Escape discards a recovered failure;
        // hiding or destroying this owner keeps the accepted draft in the queue.
        if self.dialog == Some(Dialog::AddText) {
            if let Some(id) = self.dialog_failure.take() {
                self.writes.acknowledge(id);
            }
        }
        self.retire_dialog(window, cx);
        true
    }

    fn retire_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let had_dialog = self.dialog.take().is_some();
        self.generation = self.generation.wrapping_add(1);
        self.operation = self.operation.wrapping_add(1);
        self.busy = false;
        self.dialog_failure = None;
        // Do not rewrite composing input or discard failed accepted drafts.
        if had_dialog {
            self.body
                .update(cx, |input, cx| input.set_read_only(self.exit_pending, cx));
            window.blur();
            self.protect_modal();
            if self.active {
                self.host
                    .chrome_web_action(&json!({"kind":"clipboardHostChrome",
                    "route":"/clipboard", "sourceHostWindow":"clipboard",
                    "isClipboardEditing":crate::editor::has_active_input_windows()}));
                if let Err(error) =
                    platform::set_no_activate(window, self.host.clipboard_no_activate())
                {
                    self.report_error(error, cx);
                }
            }
            cx.emit(HistoryActionsEvent::DialogChanged(false));
        }
        cx.notify();
    }

    /// Hide/tab departure retires dialog replies, while the core undo snapshot
    /// remains alive and is read again on resume. Accepted writes still finish.
    pub fn suspend(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.active = false;
        self.retire_dialog(window, cx);
    }

    pub fn resume(&mut self, cx: &mut Context<Self>) {
        self.active = true;
        self.refresh_pending(cx);
        self.report_unseen_failures(cx);
        cx.notify();
    }

    /// Call from the root's one maintenance pump; this creates no worker/poll
    /// owner and never invokes commit_pending_clear on the UI thread.
    pub fn maintain(&mut self, cx: &mut Context<Self>) {
        if self.active && self.last_pending_check.elapsed() >= Duration::from_millis(250) {
            self.refresh_pending(cx);
            self.report_unseen_failures(cx);
        }
    }

    pub fn refresh_pending(&mut self, cx: &mut Context<Self>) {
        self.last_pending_check = Instant::now();
        let pending = PendingClear::from_value(&self.host.clear_pending());
        let seconds_left = pending
            .as_ref()
            .map_or(0, |pending| pending.seconds_left_at(now_ms()));
        let changed = pending != self.pending;
        if changed || seconds_left != self.seconds_left {
            self.pending = pending;
            self.seconds_left = seconds_left;
            if changed {
                cx.emit(HistoryActionsEvent::Changed);
            }
            cx.notify();
        }
    }

    fn report_error(&mut self, error: String, cx: &mut Context<Self>) {
        self.error = Some(error.clone());
        cx.emit(HistoryActionsEvent::Error(error));
        cx.notify();
    }

    fn report_unseen_failures(&mut self, cx: &mut Context<Self>) {
        for failure in self.writes.failures() {
            if failure.id <= self.last_failure_seen {
                continue;
            }
            self.last_failure_seen = failure.id;
            self.report_error(failure.error, cx);
            if !matches!(failure.write, HistoryWrite::Add(_)) {
                self.writes.acknowledge(failure.id);
            }
        }
    }

    fn accept(&mut self) -> AcceptedOperation {
        self.operation = self.operation.wrapping_add(1);
        self.busy = true;
        self.error = None;
        AcceptedOperation {
            operation: self.operation,
            dialog_generation: self.generation,
            draft_revision: self.revision,
        }
    }

    pub fn submit_add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.has_dialog()
            || self.dialog != Some(Dialog::AddText)
            || self.busy
            || self.exit_pending
            || self.body.read(cx).is_composing()
        {
            return;
        }
        // Copy actual control text before blur; subscription delivery may still
        // be pending in this GPUI update. Preserve whitespace and CRLF exactly.
        self.draft.text = self.body.read(cx).text().to_string();
        let accepted = self.accept();
        let (write_id, reply) = self.writes.submit(
            self.host.clone(),
            HistoryWrite::Add(self.draft.clone()),
            self.dialog_failure,
        );
        self.body
            .update(cx, |input, cx| input.set_read_only(true, cx));
        window.blur();
        cx.spawn_in(window, async move |this, cx| {
            let result = reply.await.unwrap_or_else(|_| {
                Err(tr("历史操作线程不可用", "History operation worker unavailable").into())
            });
            let _ = this.update_in(cx, |this, window, cx| {
                if result.is_ok() {
                    cx.emit(HistoryActionsEvent::Changed);
                }
                if accepted.operation != this.operation
                    || accepted.dialog_generation != this.generation
                {
                    return;
                }
                this.busy = false;
                this.body
                    .update(cx, |input, cx| input.set_read_only(this.exit_pending, cx));
                match result {
                    Ok(_)
                        if accepted.owns_dialog(
                            this.operation,
                            this.generation,
                            this.revision,
                            this.body.read(cx).is_composing(),
                        ) =>
                    {
                        this.retire_dialog(window, cx)
                    }
                    Ok(_) => { /* A newer draft/IME must remain available. */ }
                    Err(error) => {
                        this.dialog_failure = Some(write_id);
                        this.last_failure_seen = this.last_failure_seen.max(write_id);
                        this.report_error(error, cx);
                        this.body.read(cx).focus_handle().focus(window);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub fn confirm_clear(
        &mut self,
        scope: ClearScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(Dialog::Clear(selected)) = self.dialog else {
            return;
        };
        if !self.active
            || self.busy
            || self.exit_pending
            || selected.is_some_and(|selected| selected != scope)
        {
            return;
        }
        let accepted = self.accept();
        let (write_id, reply) =
            self.writes
                .submit(self.host.clone(), HistoryWrite::Clear(scope), None);
        cx.spawn_in(window, async move |this, cx| {
            let result = reply.await.unwrap_or_else(|_| {
                Err(tr("历史操作线程不可用", "History operation worker unavailable").into())
            });
            let _ = this.update_in(cx, |this, window, cx| {
                // Read the authoritative snapshot, never let a late old result
                // replace a newer clear scheduled through another surface.
                this.refresh_pending(cx);
                if result.is_ok() {
                    cx.emit(HistoryActionsEvent::Changed);
                }
                if accepted.operation != this.operation
                    || accepted.dialog_generation != this.generation
                {
                    return;
                }
                this.busy = false;
                match result {
                    Ok(_) => this.retire_dialog(window, cx),
                    Err(error) => {
                        this.last_failure_seen = this.last_failure_seen.max(write_id);
                        this.report_error(error, cx);
                        this.writes.acknowledge(write_id);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub fn undo(&mut self, cx: &mut Context<Self>) {
        if !self.active || self.undo_busy || self.exit_pending {
            return;
        }
        let Some(pending) = &self.pending else {
            return;
        };
        let clear_id = pending.clear_id.clone();
        self.undo_busy = true;
        self.undo_operation = self.undo_operation.wrapping_add(1);
        let operation = self.undo_operation;
        let (write_id, reply) =
            self.writes
                .submit(self.host.clone(), HistoryWrite::Cancel(clear_id), None);
        cx.spawn(async move |this, cx| {
            let result = reply.await.unwrap_or_else(|_| {
                Err(tr("历史操作线程不可用", "History operation worker unavailable").into())
            });
            let _ = this.update(cx, |this, cx| {
                if operation != this.undo_operation {
                    return;
                }
                this.undo_busy = false;
                this.refresh_pending(cx);
                match result {
                    Ok(_) => cx.emit(HistoryActionsEvent::Changed),
                    Err(error) if this.active => {
                        this.last_failure_seen = this.last_failure_seen.max(write_id);
                        this.report_error(error, cx);
                        this.writes.acknowledge(write_id);
                    }
                    Err(_) => {}
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub fn handle_escape(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.has_dialog() {
            return false;
        }
        if !self.body.read(cx).is_composing() {
            self.close_dialog(window, cx);
        }
        true
    }

    pub fn key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.has_dialog() {
            return false;
        }
        if event.keystroke.key == "escape" && event.keystroke.modifiers.number_of_modifiers() == 0 {
            if !event.is_held {
                self.handle_escape(window, cx);
            }
            return true;
        }
        // MultilineInput handles Ctrl+S; plain Enter remains a newline. Clearing
        // has no implicit Enter/default destructive button in ClipboardPage.
        false
    }

    fn button(
        &self,
        id: &'static str,
        label: &'static str,
        danger: bool,
    ) -> gpui::Stateful<gpui::Div> {
        let t = self.tokens.theme(self.dark);
        let hover = color(&t.hover);
        rsx! {
            <div id={id} px={px(9.)} py={px(5.)} rounded={px(5.)} cursor_pointer border_1
                border_color={color(&t.border_secondary)} bg={color(&t.tertiary)} />
        }
            .text_color(color(if danger { &t.error } else { &t.text }))
            .hover(move |style| style.bg(hover))
            .opacity(if self.busy || self.exit_pending {
                0.55
            } else {
                1.
            })
            .child(label)
    }

    /// Normal-flow banner, normally immediately above the history footer.
    pub fn banner_view(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.active {
            return None;
        }
        let pending = self.pending.as_ref()?;
        let label = pending.label(now_ms());
        let t = self.tokens.theme(self.dark);
        Some(
            rsx! {
                <div id={"history-deferred-clear"} flex flex_wrap items_center gap={px(6.)}
                    px={px(8.)} py={px(6.)} w_full flex_shrink_0 bg={color(&t.tab_active)}
                    border_t_1 border_color={color(&t.border)}
                    text_size={px(self.tokens.typography.small_font_size)}
                    text_color={color(&t.text_secondary)}>
                    <div flex_1 min_w_0>
                        {label}
                    </div>
                </div>
            }
                .child(
                    rsx! {
                        <div id={"history-clear-undo"} px={px(8.)} py={px(3.)} rounded={px(4.)}
                            cursor_pointer text_color={color(&t.accent)} />
                    }
                        .opacity(if self.undo_busy || self.exit_pending {
                            0.5
                        } else {
                            1.
                        })
                        .child(if self.undo_busy {
                            tr("处理中…", "Working…")
                        } else {
                            tr("撤销", "Undo")
                        })
                        .on_click(cx.listener(|this, _, _, cx| this.undo(cx))),
                )
                .into_any_element(),
        )
    }

    /// Absolute overlay. The card scrolls inside small windows; its backdrop
    /// closes only unsubmitted, non-composing dialogs.
    pub fn dialog_view(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.has_dialog() {
            return None;
        }
        let dialog = self.dialog?;
        let t = self.tokens.theme(self.dark).clone();
        let (heading, description) = match dialog {
            Dialog::AddText => (tr("添加文本", "Add text"), tr(
                "将内容加入剪贴历史。勾选「每行一条」时按换行拆成多条；否则整段作为一条。",
                "Add text to clipboard history. With one record per line enabled, split at line breaks; otherwise keep the complete text as one record.")),
            Dialog::Clear(Some(scope)) => (scope.heading(), scope.description()),
            Dialog::Clear(None) => (tr("清空剪贴历史", "Clear clipboard history"), tr(
                "确认后将立即从列表隐藏所选范围；约 5 秒内可撤销，超时后才会永久删除。",
                "The chosen records will be hidden immediately. Undo is available for about 5 seconds before permanent deletion.")),
        };
        let mut card = rsx! {
            <div id={"history-action-card"} w_full max_w={px(420.)} min_h_0 max_h_full
                p={px(14.)} flex flex_col gap={px(10.)} rounded={px(8.)} border_1
                border_color={color(&t.border)} bg={color(&t.secondary)} shadow_lg />
        }
            .on_any_mouse_down(|_, _, cx| cx.stop_propagation())
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(div().font_weight(FontWeight::MEDIUM).child(heading))
            .child(
                rsx! {
                    <div text_size={px(self.tokens.typography.small_font_size)}
                        text_color={color(&t.text_secondary)}>
                        {description}
                    </div>
                },
            );
        if dialog == Dialog::AddText {
            let checked = self.draft.line_by_line;
            card = card
                .child(
                    rsx! {
                        <div id={"history-add-line-mode"} flex items_center gap={px(6.)}
                            cursor_pointer />
                    }
                        .opacity(if self.busy || self.exit_pending {
                            0.55
                        } else {
                            1.
                        })
                        .child(if checked { "☑" } else { "☐" })
                        .child(tr("每行一条记录", "One record per line"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            if !this.busy
                                && !this.exit_pending
                                && !this.body.read(cx).is_composing()
                            {
                                this.draft.line_by_line = !this.draft.line_by_line;
                                this.revision = this.revision.wrapping_add(1);
                                cx.notify();
                            }
                        })),
                )
                .child(self.body.clone());
        }
        if let Some(error) = &self.error {
            card = card.child(
                rsx! {
                    <div text_size={px(self.tokens.typography.small_font_size)}
                        text_color={color(&t.error)}>
                        {error.clone()}
                    </div>
                },
            );
        }
        let mut buttons = rsx! {
            <div flex flex_wrap justify_end gap={px(6.)} />
        };
        match dialog {
            Dialog::AddText => {
                buttons = buttons.child(
                    self.button(
                        "history-add-submit",
                        if self.busy {
                            tr("处理中…", "Working…")
                        } else {
                            tr("添加", "Add")
                        },
                        false,
                    )
                    .on_click(cx.listener(|this, _, window, cx| this.submit_add(window, cx))),
                )
            }
            Dialog::Clear(selected) => {
                for scope in [ClearScope::All, ClearScope::NonTop] {
                    if selected.is_some_and(|selected| selected != scope) {
                        continue;
                    }
                    buttons = buttons.child(
                        self.button(
                            if scope == ClearScope::All {
                                "history-clear-all"
                            } else {
                                "history-clear-non-top"
                            },
                            if self.busy {
                                tr("处理中…", "Working…")
                            } else {
                                scope.confirm(selected.is_some())
                            },
                            scope == ClearScope::All,
                        )
                        .on_click(cx.listener(
                            move |this, _, window, cx| this.confirm_clear(scope, window, cx),
                        )),
                    );
                }
            }
        }
        card = card.child(
            buttons.child(
                self.button("history-action-cancel", tr("取消", "Cancel"), false)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.close_dialog(window, cx);
                    })),
            ),
        );
        Some(
            div()
                .id("history-action-overlay")
                .absolute()
                .inset_0()
                .p(px(8.))
                .flex()
                .items_center()
                .justify_center()
                .track_focus(&self.dialog_focus)
                .font_family(self.tokens.typography.font_family.clone())
                .text_size(px(self.tokens.typography.font_size))
                .text_color(color(&t.text))
                .on_key_down(cx.listener(|this, event, window, cx| {
                    if this.key_down(event, window, cx) {
                        cx.stop_propagation();
                    }
                }))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.close_dialog(window, cx);
                }))
                .child(
                    rsx! {
                        <div id={"history-action-scroll"} w_full max_w={px(420.)} max_h_full />
                    }
                        .overflow_y_scroll()
                        .track_scroll(&self.scroll)
                        .smooth_wheel("history-action-scroll", &self.scroll, cx)
                        .child(card),
                )
                .into_any_element(),
        )
    }
}

impl Drop for HistoryActions {
    fn drop(&mut self) {
        if self.dialog.is_some() {
            self.host
                .apply_window_action("setSpaModalOpen", crate::editor::has_active_input_windows());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn isolated_host() -> (tempfile::TempDir, cliphost_core::AppPaths, Arc<HostState>) {
        cliphost_core::force_headless(true);
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .unwrap()
            .join(".tmp")
            .join("history-actions");
        std::fs::create_dir_all(&root).unwrap();
        let dir = tempfile::Builder::new()
            .prefix("case-")
            .tempdir_in(root)
            .unwrap();
        let paths = cliphost_core::AppPaths::from_data_root(dir.path().join("data"));
        let host = Arc::new(HostState::open(paths.clone(), 28965).unwrap());
        (dir, paths, host)
    }

    #[test]
    fn accepted_requests_survive_dropped_owner_and_observers_and_barrier_keeps_fifo() {
        let (_dir, _paths, host) = isolated_host();
        let weak_host = Arc::downgrade(&host);
        let (started_tx, started_rx) = mpsc::channel();
        let (continue_tx, continue_rx) = mpsc::channel();
        let completed = Arc::new(Mutex::new(Vec::new()));
        let worker_completed = completed.clone();
        let queue = HistoryWriteQueue::with_writer(&host, move |_, write| {
            started_tx.send(()).unwrap();
            continue_rx.recv().unwrap();
            worker_completed.lock().unwrap().push(match write {
                HistoryWrite::Add(draft) => draft.text.clone(),
                HistoryWrite::Clear(_) => "clear".into(),
                HistoryWrite::Cancel(id) => format!("cancel:{id}"),
            });
            Ok(json!({"ok":true}))
        });
        let (_, add) = queue.submit(
            host.clone(),
            HistoryWrite::Add(AddDraft {
                text: "accepted".into(),
                line_by_line: false,
            }),
            None,
        );
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let (_, clear) = queue.submit(host.clone(), HistoryWrite::Clear(ClearScope::NonTop), None);
        let (_, cancel) = queue.submit(
            host.clone(),
            HistoryWrite::Cancel("original-id".into()),
            None,
        );
        // A GPUI entity and its observer tasks may all disappear after accept.
        drop((add, clear, cancel, host));
        let mut barrier = queue.barrier();
        assert!(matches!(
            barrier.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        assert!(weak_host.upgrade().is_some());
        continue_tx.send(()).unwrap();
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(
            barrier.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        continue_tx.send(()).unwrap();
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(
            barrier.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        continue_tx.send(()).unwrap();
        assert_eq!(barrier.blocking_recv().unwrap(), Ok(()));
        assert_eq!(
            *completed.lock().unwrap(),
            ["accepted", "clear", "cancel:original-id"]
        );
        assert!(weak_host.upgrade().is_none());
    }

    #[test]
    fn failed_accepted_draft_survives_flush_and_retry_only_retires_that_draft() {
        let (_dir, _paths, host) = isolated_host();
        let (started_tx, started_rx) = mpsc::channel();
        let (continue_tx, continue_rx) = mpsc::channel();
        let queue = HistoryWriteQueue::with_writer(&host, move |_, write| {
            started_tx.send(()).unwrap();
            continue_rx.recv().unwrap();
            match write {
                HistoryWrite::Add(draft) if draft.text == "retry" => Ok(json!({"ok":true})),
                _ => Err("disk full".into()),
            }
        });
        let original = AddDraft {
            text: "  中😀\r\n\n末尾 \n".into(),
            line_by_line: false,
        };
        let (first_id, first_reply) =
            queue.submit(host.clone(), HistoryWrite::Add(original.clone()), None);
        drop(first_reply);
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let mut barrier = queue.barrier();
        assert!(matches!(
            barrier.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        continue_tx.send(()).unwrap();
        assert_eq!(barrier.blocking_recv().unwrap(), Err("disk full".into()));
        let recovered = queue.latest_failed_add().unwrap();
        assert_eq!(recovered.id, first_id);
        assert!(matches!(recovered.write, HistoryWrite::Add(draft) if draft == original));
        let second = AddDraft {
            text: "another accepted draft".into(),
            line_by_line: true,
        };
        let (second_id, reply) =
            queue.submit(host.clone(), HistoryWrite::Add(second.clone()), None);
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        continue_tx.send(()).unwrap();
        assert!(reply.blocking_recv().unwrap().is_err());
        let (_, reply) = queue.submit(
            host.clone(),
            HistoryWrite::Add(AddDraft {
                text: "retry".into(),
                line_by_line: false,
            }),
            Some(first_id),
        );
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        continue_tx.send(()).unwrap();
        assert!(reply.blocking_recv().unwrap().is_ok());
        assert_eq!(queue.failures().len(), 1);
        assert_eq!(queue.latest_failed_add().unwrap().id, second_id);
        assert!(
            matches!(queue.latest_failed_add().unwrap().write, HistoryWrite::Add(draft) if draft == second)
        );
        // The existing explicit Cancel may discard this recovered draft.
        queue.acknowledge(second_id);
        assert_eq!(queue.barrier().blocking_recv().unwrap(), Ok(()));
    }

    #[test]
    fn exit_flush_finds_registered_worker_after_ui_queue_handle_is_gone() {
        let (_dir, _paths, host) = isolated_host();
        let (started_tx, started_rx) = mpsc::channel();
        let (continue_tx, continue_rx) = mpsc::channel();
        let queue = HistoryWriteQueue::with_writer(&host, move |_, _| {
            started_tx.send(()).unwrap();
            continue_rx.recv().unwrap();
            Err("save failed".into())
        });
        let registry = Mutex::new(vec![queue.clone()]);
        let (_, reply) = queue.submit(
            host,
            HistoryWrite::Add(AddDraft {
                text: "keep this draft".into(),
                line_by_line: false,
            }),
            None,
        );
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        drop((reply, queue));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let mut flush = Box::pin(flush_registered_writes(&registry));
        // Poll the actual async barrier once while the accepted worker is held.
        let waker = std::task::Waker::noop();
        let mut context = std::task::Context::from_waker(waker);
        assert!(std::future::Future::poll(flush.as_mut(), &mut context).is_pending());
        continue_tx.send(()).unwrap();
        assert_eq!(runtime.block_on(flush), Err("save failed".into()));
        let queues = registry.lock().unwrap();
        assert_eq!(queues.len(), 1);
        assert!(
            matches!(queues[0].latest_failed_add().unwrap().write, HistoryWrite::Add(draft) if draft.text == "keep this draft")
        );
    }

    #[test]
    fn real_core_add_clear_and_undo_wait_without_forcing_final_deletion() {
        let (_dir, paths, host) = isolated_host();
        let queue = HistoryWriteQueue::new(&host);
        let draft = AddDraft {
            text: "  中😀\r\n\n末尾 \n".into(),
            line_by_line: false,
        };
        let (_, reply) = queue.submit(host.clone(), HistoryWrite::Add(draft.clone()), None);
        assert_eq!(reply.blocking_recv().unwrap().unwrap()["count"], 1);
        let (_, reply) = queue.submit(host.clone(), HistoryWrite::Clear(ClearScope::All), None);
        let scheduled = reply.blocking_recv().unwrap().unwrap();
        assert_eq!(scheduled["itemCount"], 1);
        assert_eq!(queue.barrier().blocking_recv().unwrap(), Ok(()));
        host.flush_for_shutdown();
        let store = cliphost_core::AppStore::open(&paths.db).unwrap();
        assert_eq!(
            store.list_clips().unwrap()[0].body_text.as_deref(),
            Some(draft.text.as_str())
        );
        assert_eq!(host.clear_pending()["clearId"], scheduled["clearId"]);
        let (_, reply) = queue.submit(
            host.clone(),
            HistoryWrite::Cancel(scheduled["clearId"].as_str().unwrap().into()),
            None,
        );
        assert_eq!(reply.blocking_recv().unwrap().unwrap()["ok"], true);
        assert_eq!(queue.barrier().blocking_recv().unwrap(), Ok(()));
        assert!(host.clear_pending().is_null());
        assert_eq!(store.list_clips().unwrap().len(), 1);
        // Merely constructing an unsubmitted draft never queues or saves it.
        let _unsubmitted = AddDraft {
            text: "not submitted".into(),
            line_by_line: false,
        };
        assert_eq!(queue.barrier().blocking_recv().unwrap(), Ok(()));
        assert_eq!(store.list_clips().unwrap().len(), 1);
    }

    #[test]
    fn add_text_keeps_original_whitespace_newlines_and_default_line_mode() {
        let mut draft = AddDraft::default();
        draft.text = "  中😀\r\n\n末尾 \n".into();
        assert_eq!(
            draft.request(),
            json!({"text":"  中😀\r\n\n末尾 \n","lineByLine":true})
        );
        draft.line_by_line = false;
        assert_eq!(draft.request()["text"], "  中😀\r\n\n末尾 \n");
        assert_eq!(draft.request()["lineByLine"], false);
    }

    #[test]
    fn accepted_add_cannot_close_reopened_or_newer_composing_draft() {
        let submitted = AcceptedOperation {
            operation: 8,
            dialog_generation: 3,
            draft_revision: 5,
        };
        assert!(submitted.owns_dialog(8, 3, 5, false));
        assert!(!submitted.owns_dialog(9, 4, 5, false)); // hide then reopen
        assert!(!submitted.owns_dialog(8, 3, 6, false)); // newer committed text
        assert!(!submitted.owns_dialog(8, 3, 5, true)); // IME has not emitted Changed
        assert!(!submitted.owns_dialog(9, 3, 5, false)); // replacement operation
    }

    #[test]
    fn pending_snapshot_keeps_expired_banner_until_core_retires_it() {
        let raw = json!({"clearId":"a","mode":"all","itemCount":4,"expiresAtUnixMs":6000});
        let pending = PendingClear::from_value(&raw).unwrap();
        assert_eq!(pending.seconds_left_at(1000), 5);
        assert_eq!(pending.seconds_left_at(1001), 5);
        assert_eq!(pending.seconds_left_at(5000), 1);
        assert_eq!(pending.seconds_left_at(5999), 1);
        assert_eq!(pending.seconds_left_at(6000), 0);
        assert!(PendingClear::from_value(&raw).is_some());
        assert!(PendingClear::from_value(&Value::Null).is_none());
    }

    #[test]
    fn inactive_or_empty_clear_results_never_create_an_undo_banner() {
        for raw in [
            Value::Null,
            json!({"clearId":"","itemCount":3}),
            json!({"clearId":" \t","itemCount":3}),
            json!({"clearId":"a","itemCount":0}),
        ] {
            assert!(PendingClear::from_value(&raw).is_none());
        }
        let pending = PendingClear::from_value(&json!({"clearId":"b","mode":"nonTop",
            "itemCount":2,"expiresAtUnixMs":8000}))
        .unwrap();
        assert_eq!(pending.scope, ClearScope::NonTop);
        assert_eq!(pending.clear_id, "b");
    }
}
