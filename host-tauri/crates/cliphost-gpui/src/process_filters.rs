//! Web-compatible source-process presets and their native editor.
//!
//! Presets currently live in WebView localStorage, not ClipConfig. The owner
//! must supply an authoritative snapshot and a durable writer after importing
//! that origin's data. This module never treats an unavailable WebView store
//! as an empty collection and never opens a host/data directory on its own.

use cliphost_core::i18n::tr;
use crate::smooth_wheel::SmoothWheelExt;
use gpui_rsx::rsx;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const WEB_STORAGE_KEY: &str = "intellitools.clip.customProcessFilters";
pub const FOREGROUND_TOKEN: &str = "foreground";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessPreset {
    pub id: String,
    pub display_name: String,
    pub include_exe_names: Vec<String>,
}

impl ProcessPreset {
    /// Stored names and tokens are never translated or rewritten for display.
    pub fn label(&self) -> String {
        let custom = self.display_name.trim();
        if custom.is_empty() {
            auto_title(
                &self.include_exe_names,
                tr("前台", "Foreground"),
                tr("进程筛选", "Process filter"),
            )
        } else {
            custom.to_string()
        }
    }

    pub fn spec(&self) -> Value {
        json!({ "includeExeNames": self.include_exe_names })
    }

    pub fn uses_foreground(&self) -> bool {
        self.include_exe_names
            .iter()
            .any(|name| is_foreground(name))
    }
}

pub fn is_foreground(name: &str) -> bool {
    name.trim().eq_ignore_ascii_case(FOREGROUND_TOKEN)
}

pub fn strip_exe_suffix(name: &str) -> &str {
    let name = name.trim();
    if name
        .get(name.len().saturating_sub(4)..)
        .is_some_and(|suffix| suffix.eq_ignore_ascii_case(".exe"))
    {
        &name[..name.len() - 4]
    } else {
        name
    }
}

/// `buildAutoProcessFilterTitle`, with presentation-only localized labels.
/// The first two apps are named; more show as 「等 N 个」 (2026-10-24 design).
pub fn auto_title(names: &[String], foreground_label: &str, fallback: &str) -> String {
    let exes = names
        .iter()
        .filter(|name| !is_foreground(name))
        .map(|name| strip_exe_suffix(name))
        .collect::<Vec<_>>();
    let mut exe_part = exes.iter().take(2).copied().collect::<Vec<_>>().join(" · ");
    if exes.len() > 2 {
        exe_part.push_str(&format!("{}{}{}", tr(" 等 ", " +"), exes.len() - 2, tr(" 个", "")));
    }
    match (
        exe_part.is_empty(),
        names.iter().any(|name| is_foreground(name)),
    ) {
        (false, true) => format!("{exe_part} · {foreground_label}"),
        (true, true) => foreground_label.to_string(),
        (false, false) => exe_part,
        (true, false) => fallback.to_string(),
    }
}

/// Import result retains the exact source for backup/recovery. Invalid rows
/// follow the web decoder's exclusion rules, but are reported to the owner.
#[derive(Clone, Debug)]
pub struct DecodedPresets {
    pub source_json: String,
    pub presets: Vec<ProcessPreset>,
    pub skipped_rows: usize,
}

/// Absence is distinct from an empty array. The caller decides whether an
/// absent localStorage key was actually read, or its origin was unavailable.
pub fn decode_web_presets(raw: &str) -> Result<DecodedPresets, String> {
    let value: Value = serde_json::from_str(raw).map_err(|error| error.to_string())?;
    let rows = value
        .as_array()
        .ok_or_else(|| "Process preset storage must be an array.".to_string())?;
    let mut presets = Vec::new();
    let mut skipped_rows = 0;
    for row in rows {
        let id = row["id"].as_str().unwrap_or_default();
        let names: Vec<String> = row["includeExeNames"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .collect();
        if id.is_empty() || names.is_empty() {
            skipped_rows += 1;
            continue;
        }
        presets.push(ProcessPreset {
            id: id.to_string(),
            display_name: row["displayName"].as_str().unwrap_or_default().to_string(),
            include_exe_names: names,
        });
    }
    Ok(DecodedPresets {
        source_json: raw.to_string(),
        presets,
        skipped_rows,
    })
}

pub fn encode_web_presets(presets: &[ProcessPreset]) -> Result<String, String> {
    serde_json::to_string(presets).map_err(|error| error.to_string())
}

/// Session selection is not persisted, matching the WebView warm-page cache.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcessSelection {
    pub active_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessTab {
    pub id: String,
    pub label: String,
    pub tooltip: String,
    pub active: bool,
}

/// Place after category tabs and before Add/Customize; include this count in
/// the owner's existing responsive category layout calculation.
pub fn preset_tabs(presets: &[ProcessPreset], selection: &ProcessSelection) -> Vec<ProcessTab> {
    presets
        .iter()
        .map(|preset| ProcessTab {
            id: preset.id.clone(),
            label: preset.label(),
            tooltip: preset
                .include_exe_names
                .iter()
                .map(|name| strip_exe_suffix(name))
                .collect::<Vec<_>>()
                .join(", "),
            active: selection.active_id.as_deref() == Some(preset.id.as_str()),
        })
        .collect()
}

impl ProcessSelection {
    pub fn toggle(&mut self, id: &str, presets: &[ProcessPreset]) {
        if self.active_id.as_deref() == Some(id) {
            self.active_id = None;
        } else if presets.iter().any(|preset| preset.id == id) {
            self.active_id = Some(id.to_string());
        }
    }

    pub fn reconcile(&mut self, presets: &[ProcessPreset]) {
        if self
            .active_id
            .as_ref()
            .is_some_and(|id| !presets.iter().any(|preset| &preset.id == id))
        {
            self.active_id = None;
        }
    }

    pub fn clear(&mut self) {
        self.active_id = None;
    }

    pub fn active<'a>(&self, presets: &'a [ProcessPreset]) -> Option<&'a ProcessPreset> {
        presets
            .iter()
            .find(|preset| Some(preset.id.as_str()) == self.active_id.as_deref())
    }

    pub fn spec(&self, presets: &[ProcessPreset]) -> Option<Value> {
        self.active(presets).map(ProcessPreset::spec)
    }

    pub fn uses_foreground(&self, presets: &[ProcessPreset]) -> bool {
        self.active(presets)
            .is_some_and(ProcessPreset::uses_foreground)
    }
}

/// Ordered Set semantics: deselect/reselect moves that exe to the end. There
/// is no free-text/member editor for existing presets in the current WebView.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcessDraft {
    pub display_name: String,
    pub selected_exes: Vec<String>,
    pub include_foreground: bool,
}

impl ProcessDraft {
    pub fn toggle_exe(&mut self, exe: &str) {
        if let Some(index) = self.selected_exes.iter().position(|name| name == exe) {
            self.selected_exes.remove(index);
        } else {
            self.selected_exes.push(exe.to_string());
        }
    }

    pub fn preset(&self, id: String) -> Option<ProcessPreset> {
        let mut names = self.selected_exes.clone();
        if self.include_foreground {
            names.push(FOREGROUND_TOKEN.to_string());
        }
        if names.is_empty() || id.is_empty() {
            return None;
        }
        Some(ProcessPreset {
            id,
            display_name: self.display_name.trim().to_string(),
            include_exe_names: names,
        })
    }
}

pub fn rename_preset(presets: &mut [ProcessPreset], id: &str, draft: &str) -> bool {
    let Some(preset) = presets.iter_mut().find(|preset| preset.id == id) else {
        return false;
    };
    let trimmed = draft.trim();
    let automatic = auto_title(
        &preset.include_exe_names,
        tr("前台", "Foreground"),
        tr("进程筛选", "Process filter"),
    );
    let next = if trimmed.is_empty() || trimmed == automatic {
        ""
    } else {
        trimmed
    };
    if preset.display_name == next {
        return false;
    }
    preset.display_name = next.to_string();
    true
}

// Native editor. Persistence is injected, so the coordinator can share one
// compatible store with Tauri instead of creating a second preset authority.
use crate::{
    input::{InputColors, SearchEvent, SearchInput},
    preview::{DesignTokens, ThemeTokens},
    view::color,
};
use cliphost_core::host::HostState;
use cliphost_core::i18n::{displayed_language, LanguageMode};
use gpui::{
    div, img, prelude::*, px, AnyElement, App, Context, Entity, EventEmitter, FocusHandle,
    FontWeight, Image, ImageFormat, SharedString, Subscription, Window,
};
use std::{collections::HashMap, rc::Rc, sync::Arc};

/// Must serialize whole-collection writes and check the editor's captured
/// baseline revision before atomically persisting, or return a visible conflict.
/// Close only invalidates UI callbacks; reopen must await an outstanding write
/// and reload, or use compare-and-swap. Called on the background executor.
/// Supply this only after legacy-store recovery; never blindly replace a store.
pub type PresetWriter = Arc<dyn Fn(Vec<ProcessPreset>) -> Result<(), String> + Send + Sync>;

/// Only explicitly accepted operations enter this registry. Unsaved input is
/// never submitted by flush, invalidate, Drop or application Exit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessWriteIntent {
    Add(String),
    /// Members or name of an existing preset changed in the edit popover.
    Update(String),
    Rename(String),
    Delete(String),
}

#[derive(Clone, Debug)]
pub struct ProcessWriteSnapshot {
    pub intent: ProcessWriteIntent,
    pub presets: Vec<ProcessPreset>,
    pub selection: ProcessSelection,
    pub draft: ProcessDraft,
    pub rename: Option<(String, String)>,
}

/// Survives hidden/destroyed editors. Recover via restore_failed_write, or
/// explicitly retry with the original CAS writer; never silently rebase it.
#[derive(Clone, Debug)]
pub struct ProcessFilterWriteFailure {
    pub write_id: u64,
    pub error: String,
    pub snapshot: ProcessWriteSnapshot,
    /// Recovery-only newer input; it was never implicitly sent to the writer.
    pub latest_draft: ProcessDraft,
    pub latest_rename: Option<(String, String)>,
}

impl ProcessFilterWriteFailure {
    /// Preserve the original intent, IDs and complete accepted collection.
    /// Newer unrelated input remains recovery-only; it cannot become an Add.
    fn retry_snapshot(
        &self,
        draft: ProcessDraft,
        rename: Option<(String, String)>,
    ) -> Result<ProcessWriteSnapshot, String> {
        let mut snapshot = self.snapshot.clone();
        match &snapshot.intent {
            ProcessWriteIntent::Add(id) | ProcessWriteIntent::Update(id) => {
                let replacement = draft
                    .preset(id.clone())
                    .ok_or("The saved process preset draft has no selected processes.")?;
                let row = snapshot
                    .presets
                    .iter_mut()
                    .find(|preset| &preset.id == id)
                    .ok_or(
                        "The original added process preset is missing from its accepted snapshot.",
                    )?;
                *row = replacement;
            }
            ProcessWriteIntent::Rename(id) => {
                if !snapshot.presets.iter().any(|preset| &preset.id == id) {
                    return Err("The original renamed process preset is missing from its accepted snapshot.".into());
                }
                let name = rename
                    .as_ref()
                    .filter(|(target, _)| target == id)
                    .or_else(|| {
                        self.latest_rename
                            .as_ref()
                            .filter(|(target, _)| target == id)
                    })
                    .or_else(|| {
                        self.snapshot
                            .rename
                            .as_ref()
                            .filter(|(target, _)| target == id)
                    })
                    .map(|(_, name)| name.as_str())
                    .ok_or("The original rename draft is unavailable.")?;
                rename_preset(&mut snapshot.presets, id, name);
            }
            ProcessWriteIntent::Delete(id) => {
                if snapshot.presets.iter().any(|preset| &preset.id == id) {
                    return Err("The accepted delete snapshot still contains its target.".into());
                }
            }
        }
        snapshot.draft = draft;
        snapshot.rename = rename;
        Ok(snapshot)
    }
}

// Independent of GPUI so the acceptance-before-executor race can be tested
// with a delayed writer without a window, runtime, database or OS clipboard.
mod accepted_writes {
    use super::{PresetWriter, ProcessDraft, ProcessFilterWriteFailure, ProcessWriteSnapshot};
    use std::{
        collections::BTreeMap,
        future::poll_fn,
        panic::{catch_unwind, AssertUnwindSafe},
        sync::{Arc, Mutex, OnceLock},
        task::{Poll, Waker},
    };

    #[derive(Default)]
    struct ReceiptState {
        result: Option<Result<(), String>>,
        waiters: Vec<Waker>,
    }

    struct WriteEntry {
        id: u64,
        snapshot: ProcessWriteSnapshot,
        writer: PresetWriter,
        supersedes: Option<u64>,
        receipt: Mutex<ReceiptState>,
        recovery: Mutex<(ProcessDraft, Option<(String, String)>)>,
    }

    #[derive(Default)]
    struct BarrierState {
        next_id: u64,
        pending: BTreeMap<u64, Arc<WriteEntry>>,
        failed: BTreeMap<u64, Arc<WriteEntry>>,
        waiters: Vec<Waker>,
    }

    #[derive(Default)]
    pub(super) struct WriteBarrier {
        state: Mutex<BarrierState>,
    }

    fn register_waker(waiters: &mut Vec<Waker>, waker: &Waker) {
        if !waiters.iter().any(|existing| existing.will_wake(waker)) {
            waiters.push(waker.clone());
        }
    }

    fn wake(waiters: Vec<Waker>) {
        for waiter in waiters {
            waiter.wake();
        }
    }

    /// Registration happens synchronously on UI acceptance, before spawn.
    /// Dropping this execution ticket records failure rather than losing work.
    #[must_use]
    pub(super) struct AcceptedWrite {
        barrier: Arc<WriteBarrier>,
        entry: Arc<WriteEntry>,
        finished: bool,
    }

    #[derive(Clone)]
    pub struct ProcessWriteReceipt {
        entry: Arc<WriteEntry>,
    }

    impl ProcessWriteReceipt {
        pub fn write_id(&self) -> u64 {
            self.entry.id
        }

        pub fn result(&self) -> Option<Result<(), String>> {
            match self.entry.receipt.lock() {
                Ok(receipt) => receipt.result.clone(),
                Err(_) => Some(Err("Process preset save receipt is unavailable.".into())),
            }
        }

        /// Preserve newer UI input for a failure after hide/destruction. This
        /// does not alter the immutable submitted snapshot or enqueue a write.
        pub fn retain_draft(&self, draft: ProcessDraft, rename: Option<(String, String)>) {
            if let Ok(mut recovery) = self.entry.recovery.lock() {
                *recovery = (draft, rename);
            }
        }

        pub async fn wait(&self) -> Result<(), String> {
            poll_fn(|cx| match self.entry.receipt.lock() {
                Ok(mut receipt) => {
                    if let Some(result) = &receipt.result {
                        Poll::Ready(result.clone())
                    } else {
                        register_waker(&mut receipt.waiters, cx.waker());
                        Poll::Pending
                    }
                }
                Err(_) => Poll::Ready(Err("Process preset save receipt is unavailable.".into())),
            })
            .await
        }
    }

    impl WriteBarrier {
        pub(super) fn accept(
            self: &Arc<Self>,
            snapshot: ProcessWriteSnapshot,
            writer: PresetWriter,
            supersedes: Option<u64>,
        ) -> Result<AcceptedWrite, String> {
            let mut state = self
                .state
                .lock()
                .map_err(|_| "Process preset write registry is unavailable.".to_string())?;
            if let Some(failed_id) = supersedes {
                let previous = state
                    .failed
                    .get(&failed_id)
                    .ok_or("The failed process preset save is no longer current.")?;
                if previous.snapshot.intent != snapshot.intent {
                    return Err(
                        "A different process preset operation cannot replace this failed save."
                            .into(),
                    );
                }
                if state
                    .pending
                    .values()
                    .any(|entry| entry.supersedes == Some(failed_id))
                {
                    return Err("The failed process preset save is already being retried.".into());
                }
            }
            state.next_id = state
                .next_id
                .checked_add(1)
                .ok_or("Process preset write sequence is exhausted.")?;
            let recovery = Mutex::new((snapshot.draft.clone(), snapshot.rename.clone()));
            let entry = Arc::new(WriteEntry {
                id: state.next_id,
                snapshot,
                writer,
                supersedes,
                receipt: Mutex::new(ReceiptState::default()),
                recovery,
            });
            state.pending.insert(entry.id, entry.clone());
            Ok(AcceptedWrite {
                barrier: self.clone(),
                entry,
                finished: false,
            })
        }

        pub(super) fn retry(
            self: &Arc<Self>,
            id: u64,
            snapshot: Option<ProcessWriteSnapshot>,
        ) -> Result<AcceptedWrite, String> {
            let previous = self
                .state
                .lock()
                .map_err(|_| "Process preset write registry is unavailable.".to_string())?
                .failed
                .get(&id)
                .cloned()
                .ok_or("The failed process preset save is no longer current.")?;
            self.accept(
                snapshot.unwrap_or_else(|| previous.snapshot.clone()),
                previous.writer.clone(),
                Some(id),
            )
        }

        fn complete(&self, entry: &Arc<WriteEntry>, result: Result<(), String>) {
            // Registry status is independent of any UI callback. Keep the
            // failure + submitted snapshot before releasing pending work.
            let (registry_waiters, receipt_waiters) = match self.state.lock() {
                Ok(mut state) => {
                    if state.pending.remove(&entry.id).is_none() {
                        return;
                    }
                    let receipt_waiters = match entry.receipt.lock() {
                        Ok(mut receipt) => {
                            receipt.result = Some(result.clone());
                            std::mem::take(&mut receipt.waiters)
                        }
                        Err(_) => Vec::new(),
                    };
                    match &result {
                        Ok(()) => {
                            if let Some(id) = entry.supersedes {
                                state.failed.remove(&id);
                            }
                        }
                        Err(_) => {
                            if let Some(id) = entry.supersedes {
                                state.failed.remove(&id);
                            }
                            state.failed.insert(entry.id, entry.clone());
                        }
                    }
                    (std::mem::take(&mut state.waiters), receipt_waiters)
                }
                Err(_) => {
                    let receipt_waiters = match entry.receipt.lock() {
                        Ok(mut receipt) => {
                            receipt.result = Some(Err("Process preset write registry is unavailable; the submitted draft was retained.".into()));
                            std::mem::take(&mut receipt.waiters)
                        }
                        Err(_) => Vec::new(),
                    };
                    (Vec::new(), receipt_waiters)
                }
            };
            // Never wake executors while either lock is held.
            wake(receipt_waiters);
            wake(registry_waiters);
        }

        pub(super) fn failures(&self) -> Vec<ProcessFilterWriteFailure> {
            self.state
                .lock()
                .map(|state| {
                    state
                        .failed
                        .values()
                        .filter_map(|entry| {
                            let receipt = entry.receipt.lock().ok()?;
                            let error = receipt.result.as_ref()?.as_ref().err()?.clone();
                            let (latest_draft, latest_rename) = entry.recovery.lock().ok()?.clone();
                            Some(ProcessFilterWriteFailure {
                                write_id: entry.id,
                                error,
                                snapshot: entry.snapshot.clone(),
                                latest_draft,
                                latest_rename,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default()
        }

        pub(super) fn is_current_failure(&self, id: u64) -> bool {
            self.state
                .lock()
                .map(|state| {
                    state.failed.contains_key(&id)
                        && !state
                            .pending
                            .values()
                            .any(|entry| entry.supersedes == Some(id))
                })
                .unwrap_or(true)
        }

        fn recovery(entry: &WriteEntry) -> Result<ProcessFilterWriteFailure, String> {
            let error = entry
                .receipt
                .lock()
                .map_err(|_| "Process preset save receipt is unavailable.".to_string())?
                .result
                .as_ref()
                .and_then(|result| result.as_ref().err())
                .cloned()
                .ok_or("The process preset save has not failed.")?;
            let (latest_draft, latest_rename) = entry
                .recovery
                .lock()
                .map_err(|_| "Process preset recovery draft is unavailable.".to_string())?
                .clone();
            Ok(ProcessFilterWriteFailure {
                write_id: entry.id,
                error,
                snapshot: entry.snapshot.clone(),
                latest_draft,
                latest_rename,
            })
        }

        pub(super) fn current_failure(&self, id: u64) -> Result<ProcessFilterWriteFailure, String> {
            self.restore(id).map(|(failure, _)| failure)
        }

        pub(super) fn restore(
            &self,
            id: u64,
        ) -> Result<(ProcessFilterWriteFailure, ProcessWriteReceipt), String> {
            let state = self
                .state
                .lock()
                .map_err(|_| "Process preset write registry is unavailable.".to_string())?;
            if state
                .pending
                .values()
                .any(|entry| entry.supersedes == Some(id))
            {
                return Err("The failed process preset save is already being retried.".into());
            }
            let entry = state
                .failed
                .get(&id)
                .ok_or("The failed process preset save is no longer current.")?;
            Ok((
                Self::recovery(entry)?,
                ProcessWriteReceipt {
                    entry: entry.clone(),
                },
            ))
        }

        /// Only explicit user discard may resolve a persistent CAS conflict
        /// without writing; ordinary popup cancel/hide never calls this.
        pub(super) fn discard(&self, id: u64) -> Result<ProcessFilterWriteFailure, String> {
            let (failure, waiters) = {
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| "Process preset write registry is unavailable.".to_string())?;
                if state
                    .pending
                    .values()
                    .any(|entry| entry.supersedes == Some(id))
                {
                    return Err("The failed process preset save is still being retried.".into());
                }
                let entry = state
                    .failed
                    .get(&id)
                    .ok_or("The failed process preset save is no longer current.")?;
                let failure = Self::recovery(entry)?;
                state.failed.remove(&id);
                (failure, std::mem::take(&mut state.waiters))
            };
            wake(waiters);
            Ok(failure)
        }

        pub(super) async fn flush(&self) -> Result<(), String> {
            poll_fn(|cx| match self.state.lock() {
                Ok(mut state) => {
                    if !state.pending.is_empty() {
                        // Checking pending and registering the waiter under
                        // the same lock prevents completion from losing wake.
                        register_waker(&mut state.waiters, cx.waker());
                        return Poll::Pending;
                    }
                    let errors: Vec<_> = state
                        .failed
                        .values()
                        .map(|entry| {
                            entry
                                .receipt
                                .lock()
                                .ok()
                                .and_then(|receipt| receipt.result.clone())
                                .and_then(Result::err)
                                .unwrap_or_else(|| {
                                    "Process preset save failed; the submitted draft was retained."
                                        .into()
                                })
                        })
                        .collect();
                    Poll::Ready(if errors.is_empty() {
                        Ok(())
                    } else {
                        Err(errors.join("\n"))
                    })
                }
                Err(_) => Poll::Ready(Err("Process preset write registry is unavailable.".into())),
            })
            .await
        }
    }

    impl AcceptedWrite {
        pub(super) fn receipt(&self) -> ProcessWriteReceipt {
            ProcessWriteReceipt {
                entry: self.entry.clone(),
            }
        }

        pub(super) fn execute(mut self) {
            let result = catch_unwind(AssertUnwindSafe(|| {
                (self.entry.writer)(self.entry.snapshot.presets.clone())
            }))
            .unwrap_or_else(|_| {
                Err("Process preset writer panicked; the submitted draft was retained.".into())
            });
            self.finished = true;
            self.barrier.complete(&self.entry, result);
        }
    }

    impl Drop for AcceptedWrite {
        fn drop(&mut self) {
            if !self.finished {
                self.barrier.complete(
                    &self.entry,
                    Err(
                        "Process preset save was interrupted; the submitted draft was retained."
                            .into(),
                    ),
                );
            }
        }
    }

    pub(super) fn shared() -> &'static Arc<WriteBarrier> {
        static WRITES: OnceLock<Arc<WriteBarrier>> = OnceLock::new();
        WRITES.get_or_init(|| Arc::new(WriteBarrier::default()))
    }

    #[cfg(test)]
    mod tests {
        use super::super::{ProcessDraft, ProcessPreset, ProcessSelection, ProcessWriteIntent};
        use super::*;
        use std::{
            future::Future,
            pin::Pin,
            sync::atomic::{AtomicUsize, Ordering},
            task::{Context, Wake},
        };

        #[derive(Default)]
        struct Counter(AtomicUsize);
        impl Wake for Counter {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        fn poll<F: Future>(future: Pin<&mut F>, counter: &Arc<Counter>) -> Poll<F::Output> {
            let waker = Waker::from(counter.clone());
            future.poll(&mut Context::from_waker(&waker))
        }
        fn submitted(id: &str, name: &str) -> ProcessWriteSnapshot {
            ProcessWriteSnapshot {
                intent: ProcessWriteIntent::Add(id.into()),
                presets: vec![ProcessPreset {
                    id: id.into(),
                    display_name: name.into(),
                    include_exe_names: vec!["Code.exe".into()],
                }],
                selection: ProcessSelection {
                    active_id: Some(id.into()),
                },
                draft: ProcessDraft {
                    display_name: name.into(),
                    selected_exes: vec!["Code.exe".into()],
                    include_foreground: false,
                },
                rename: None,
            }
        }

        #[test]
        fn accepted_before_executor_or_storage_mutex_is_already_a_barrier() {
            let barrier = Arc::new(WriteBarrier::default());
            let gate = Arc::new(Mutex::new(()));
            let held = gate.lock().unwrap();
            let worker_gate = gate.clone();
            let writer: PresetWriter = Arc::new(move |_| {
                let _guard = worker_gate.lock().unwrap();
                Ok(())
            });
            let accepted = barrier
                .accept(submitted("id", "draft"), writer, None)
                .unwrap();
            // Destroying a UI-owned receipt cannot remove accepted work.
            drop(accepted.receipt());
            let counter = Arc::new(Counter::default());
            let mut flush = Box::pin(barrier.flush());
            assert!(poll(flush.as_mut(), &counter).is_pending());
            let worker = std::thread::spawn(move || accepted.execute());
            assert!(poll(flush.as_mut(), &counter).is_pending());
            drop(held);
            worker.join().unwrap();
            assert_eq!(poll(flush.as_mut(), &counter), Poll::Ready(Ok(())));
            assert!(counter.0.load(Ordering::SeqCst) > 0);
        }

        #[test]
        fn hidden_owner_failure_is_sticky_and_keeps_the_submitted_draft() {
            let barrier = Arc::new(WriteBarrier::default());
            let accepted = barrier
                .accept(
                    submitted("stable-id", "用户草稿"),
                    Arc::new(|_| Err("disk failed".into())),
                    None,
                )
                .unwrap();
            let receipt = accepted.receipt();
            let id = receipt.write_id();
            let mut newer = submitted("stable-id", "保存期间的新输入").draft;
            newer.include_foreground = true;
            receipt.retain_draft(newer, None);
            drop(receipt);
            std::thread::spawn(move || accepted.execute())
                .join()
                .unwrap();
            let counter = Arc::new(Counter::default());
            for _ in 0..2 {
                let mut flush = Box::pin(barrier.flush());
                assert_eq!(
                    poll(flush.as_mut(), &counter),
                    Poll::Ready(Err("disk failed".into()))
                );
            }
            let failures = barrier.failures();
            assert_eq!(failures.len(), 1);
            assert_eq!(failures[0].write_id, id);
            assert_eq!(failures[0].snapshot.draft.display_name, "用户草稿");
            assert_eq!(failures[0].latest_draft.display_name, "保存期间的新输入");
            assert!(failures[0].latest_draft.include_foreground);
            assert_eq!(failures[0].snapshot.presets[0].id, "stable-id");
        }

        #[test]
        fn multiple_flush_waiters_and_later_accepted_work_are_not_lost() {
            let barrier = Arc::new(WriteBarrier::default());
            let first = barrier
                .accept(submitted("a", "A"), Arc::new(|_| Ok(())), None)
                .unwrap();
            let a = Arc::new(Counter::default());
            let b = Arc::new(Counter::default());
            let mut one = Box::pin(barrier.flush());
            let mut two = Box::pin(barrier.flush());
            assert!(poll(one.as_mut(), &a).is_pending());
            assert!(poll(two.as_mut(), &b).is_pending());
            let second = barrier
                .accept(submitted("b", "B"), Arc::new(|_| Ok(())), None)
                .unwrap();
            first.execute();
            assert!(poll(one.as_mut(), &a).is_pending());
            second.execute();
            assert_eq!(poll(one.as_mut(), &a), Poll::Ready(Ok(())));
            assert_eq!(poll(two.as_mut(), &b), Poll::Ready(Ok(())));
            assert!(a.0.load(Ordering::SeqCst) > 0 && b.0.load(Ordering::SeqCst) > 0);
        }

        #[test]
        fn cancelled_execution_ticket_reports_failure_instead_of_hanging() {
            let barrier = Arc::new(WriteBarrier::default());
            let ticket = barrier
                .accept(submitted("id", "draft"), Arc::new(|_| Ok(())), None)
                .unwrap();
            let receipt = ticket.receipt();
            let counter = Arc::new(Counter::default());
            let mut flush = Box::pin(barrier.flush());
            assert!(poll(flush.as_mut(), &counter).is_pending());
            drop(ticket);
            let Poll::Ready(Err(error)) = poll(flush.as_mut(), &counter) else {
                panic!("cancelled work must finish with failure")
            };
            assert!(error.contains("interrupted"));
            assert!(matches!(receipt.result(), Some(Err(_))));
            assert_eq!(barrier.failures().len(), 1);
        }

        #[test]
        fn retry_preserves_add_id_uses_new_explicit_input_and_original_writer() {
            let barrier = Arc::new(WriteBarrier::default());
            let attempts = Arc::new(AtomicUsize::new(0));
            let committed = Arc::new(Mutex::new(Vec::new()));
            let (calls, saved) = (attempts.clone(), committed.clone());
            let writer: PresetWriter = Arc::new(move |presets| {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Err("temporary failure".into());
                }
                *saved.lock().unwrap() = presets;
                Ok(())
            });
            let accepted = barrier
                .accept(submitted("same-id", "old"), writer, None)
                .unwrap();
            let old_receipt = accepted.receipt();
            let failed_id = old_receipt.write_id();
            accepted.execute();
            let retry = barrier
                .retry(failed_id, Some(submitted("same-id", "new explicit draft")))
                .unwrap();
            assert!(!barrier.is_current_failure(failed_id));
            assert!(barrier.retry(failed_id, None).is_err());
            retry.execute();
            assert_eq!(attempts.load(Ordering::SeqCst), 2);
            let rows = committed.lock().unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].id, "same-id");
            assert_eq!(rows[0].display_name, "new explicit draft");
            // The old receipt still says Err but cannot resurrect a current
            // failure after its successful replacement.
            assert!(matches!(old_receipt.result(), Some(Err(_))));
            assert!(!barrier.is_current_failure(failed_id));
            assert!(barrier.failures().is_empty());
            let counter = Arc::new(Counter::default());
            assert_eq!(
                poll(Box::pin(barrier.flush()).as_mut(), &counter),
                Poll::Ready(Ok(()))
            );
        }

        #[test]
        fn one_editor_retry_never_clears_another_cas_failure() {
            let barrier = Arc::new(WriteBarrier::default());
            let calls = Arc::new(AtomicUsize::new(0));
            let writer_calls = calls.clone();
            let writer: PresetWriter = Arc::new(move |_| {
                if writer_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    Err("A failed".into())
                } else {
                    Ok(())
                }
            });
            let a = barrier.accept(submitted("a", "A"), writer, None).unwrap();
            let a_id = a.receipt().write_id();
            a.execute();
            let b = barrier
                .accept(
                    submitted("b", "B"),
                    Arc::new(|_| Err("B CAS conflict".into())),
                    None,
                )
                .unwrap();
            b.execute();
            assert!(barrier
                .retry(a_id, Some(submitted("different-id", "wrong")))
                .is_err());
            barrier.retry(a_id, None).unwrap().execute();
            let failures = barrier.failures();
            assert_eq!(failures.len(), 1);
            assert_eq!(failures[0].snapshot.presets[0].id, "b");
            let counter = Arc::new(Counter::default());
            assert_eq!(
                poll(Box::pin(barrier.flush()).as_mut(), &counter),
                Poll::Ready(Err("B CAS conflict".into()))
            );
        }

        #[test]
        fn conflict_recovery_rejects_pending_retry_and_requires_explicit_discard() {
            let barrier = Arc::new(WriteBarrier::default());
            let accepted = barrier
                .accept(
                    submitted("id", "retained"),
                    Arc::new(|_| Err("CAS conflict".into())),
                    None,
                )
                .unwrap();
            let first_id = accepted.receipt().write_id();
            accepted.execute();
            let retry = barrier.retry(first_id, None).unwrap();
            assert!(barrier.current_failure(first_id).is_err());
            assert!(barrier.discard(first_id).is_err());
            retry.execute();
            assert!(barrier.current_failure(first_id).is_err());
            let failure = barrier.failures().pop().unwrap();
            let recovery = barrier.discard(failure.write_id).unwrap();
            assert_eq!(recovery.latest_draft.display_name, "retained");
            assert_eq!(recovery.snapshot.presets[0].id, "id");
            let counter = Arc::new(Counter::default());
            assert_eq!(
                poll(Box::pin(barrier.flush()).as_mut(), &counter),
                Poll::Ready(Ok(()))
            );
        }

        #[test]
        fn restored_operations_keep_original_ids_collection_and_distinct_intents() {
            let kept = ProcessPreset {
                id: "kept".into(),
                display_name: "用户名称".into(),
                include_exe_names: vec!["missing.exe".into(), "foreground".into()],
            };
            let mut original = submitted("target", "accepted");
            original.presets.insert(0, kept.clone());
            let mut failure = ProcessFilterWriteFailure {
                write_id: 1,
                error: "failed".into(),
                latest_draft: original.draft.clone(),
                latest_rename: None,
                snapshot: original,
            };
            let mut newer = failure.latest_draft.clone();
            newer.display_name = "updated explicit input".into();
            newer.include_foreground = true;
            let added = failure.retry_snapshot(newer.clone(), None).unwrap();
            assert_eq!(added.intent, ProcessWriteIntent::Add("target".into()));
            assert_eq!(added.presets.len(), 2);
            assert_eq!(added.presets[0], kept);
            assert_eq!(added.presets[1].id, "target");
            assert_eq!(added.presets[1].display_name, "updated explicit input");

            failure.snapshot.intent = ProcessWriteIntent::Rename("target".into());
            failure.snapshot.rename = Some(("target".into(), "accepted rename".into()));
            let renamed = failure
                .retry_snapshot(newer.clone(), Some(("target".into(), "new rename".into())))
                .unwrap();
            assert_eq!(renamed.intent, ProcessWriteIntent::Rename("target".into()));
            assert_eq!(renamed.presets.len(), 2);
            assert_eq!(renamed.presets[0], kept);
            assert_eq!(renamed.presets[1].display_name, "new rename");
            assert_eq!(renamed.presets[1].include_exe_names, ["Code.exe"]);
            assert_eq!(renamed.draft, newer); // new Add draft is recovery-only

            failure.snapshot.intent = ProcessWriteIntent::Delete("target".into());
            failure
                .snapshot
                .presets
                .retain(|preset| preset.id != "target");
            let deleted = failure.retry_snapshot(newer, None).unwrap();
            assert_eq!(deleted.intent, ProcessWriteIntent::Delete("target".into()));
            assert_eq!(deleted.presets, [kept]);
        }

        #[test]
        fn restored_receipt_keeps_new_input_without_auto_saving_or_changing_payload() {
            let barrier = Arc::new(WriteBarrier::default());
            let calls = Arc::new(AtomicUsize::new(0));
            let writer_calls = calls.clone();
            let accepted = barrier
                .accept(
                    submitted("id", "accepted"),
                    Arc::new(move |_| {
                        writer_calls.fetch_add(1, Ordering::SeqCst);
                        Err("failed".into())
                    }),
                    None,
                )
                .unwrap();
            let id = accepted.receipt().write_id();
            accepted.execute();
            let (_, restored_receipt) = barrier.restore(id).unwrap();
            restored_receipt.retain_draft(
                submitted("id", "new unsubmitted input").draft,
                Some(("id".into(), "new rename draft".into())),
            );
            drop(restored_receipt); // restored editor is hidden/destroyed again
            let (failure, _) = barrier.restore(id).unwrap();
            assert_eq!(failure.latest_draft.display_name, "new unsubmitted input");
            assert_eq!(failure.latest_rename.unwrap().1, "new rename draft");
            assert_eq!(failure.snapshot.presets[0].display_name, "accepted");
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            let counter = Arc::new(Counter::default());
            assert_eq!(
                poll(Box::pin(barrier.flush()).as_mut(), &counter),
                Poll::Ready(Err("failed".into()))
            );
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }
    }
}

pub use accepted_writes::ProcessWriteReceipt;

/// Call after freezing new UI intents and before App::quit. Includes writes
/// accepted by subsequently hidden/destroyed editors. Errors stay registered
/// until their own explicit retry succeeds; flush never auto-saves drafts.
pub async fn flush_process_filter_writes() -> Result<(), String> {
    accepted_writes::shared().flush().await
}

pub fn failed_process_filter_writes() -> Vec<ProcessFilterWriteFailure> {
    accepted_writes::shared().failures()
}

/// Only after the user explicitly discards this accepted failed save, never
/// on ordinary Cancel/hide/Exit. Returns recovery data; does not write/rebase.
pub fn discard_failed_process_filter_write(id: u64) -> Result<ProcessFilterWriteFailure, String> {
    accepted_writes::shared().discard(id)
}

/// Explicit owner recovery of a destroyed editor's accepted save. Keeps the
/// original CAS writer, accepted preset IDs and snapshot; no silent rebase.
pub fn retry_failed_process_filter_write(id: u64, cx: &App) -> Result<ProcessWriteReceipt, String> {
    let accepted = accepted_writes::shared().retry(id, None)?;
    let receipt = accepted.receipt();
    cx.background_executor()
        .spawn(async move {
            accepted.execute();
        })
        .detach();
    Ok(receipt)
}

pub type ProcessTabHandler = Rc<dyn Fn(String, &mut Window, &mut App)>;

/// Carries no classification intent: the owner toggles ProcessSelection and
/// refreshes the composed ListRequest. Bind tab.tooltip to the native hover UI.
pub fn render_preset_tab(
    tab: &ProcessTab,
    tokens: &DesignTokens,
    dark: bool,
    handler: ProcessTabHandler,
) -> gpui::Stateful<gpui::Div> {
    let t = tokens.theme(dark);
    let l = &tokens.layout;
    let id = tab.id.clone();
    let hover = color(&t.hover);
    // Outlined capsule; the active preset uses the neutral selected fill
    // (ui-design/README.md, 2026-10-14).
    rsx! {
        <div id={SharedString::from(format!("process-tab-{id}"))}
            h={px(l.chip_height - 2.0)} flex items_center flex_none
            px={px(l.chip_padding_x)} rounded={px((l.chip_height - 2.0) / 2.0)} border_1
            border_color={if tab.active {
                gpui::transparent_black()
            } else {
                color(&t.border)
            }}
            text_size={px(tokens.typography.category_font_size)}
            text_color={color(if tab.active {
                &t.text
            } else {
                &t.text_secondary
            })} />
    }
        .when(tab.active, |tab| tab.bg(color(&t.neutral_active)))
        .cursor_pointer()
        .hover(move |tab| tab.bg(hover))
        .on_click(move |_, window, cx| handler(id.clone(), window, cx))
        .child(tab.label.clone())
}

/// Logical edge of source-process icons in rows and the preset editor.
pub const SOURCE_ICON_SIZE: f32 = 16.0;

/// Popover content height; short windows clip it to the space below the row.
pub const PREFERRED_HEIGHT: f32 = 520.0;

/// Inputs sit inside drawn containers, so their own chrome is transparent.
fn input_colors(t: &ThemeTokens) -> InputColors {
    InputColors {
        background: gpui::transparent_black(),
        foreground: color(&t.text),
        placeholder: color(&t.text_muted),
        border: gpui::transparent_black(),
        selection: color(&t.tab_active),
        caret: color(&t.accent),
    }
}

/// 「刚刚 / 今天 09:41 / 昨天 / 10-21」 for a source's last copy.
fn seen_label(raw: &str) -> String {
    use chrono::{Local, TimeZone};
    let Ok(time) = chrono::DateTime::parse_from_rfc3339(raw) else {
        return raw.chars().take(10).collect();
    };
    let time = Local.from_utc_datetime(&time.naive_utc());
    let now = Local::now();
    if now.signed_duration_since(time).num_minutes() < 1 {
        return tr("刚刚", "just now").into();
    }
    match now.date_naive().signed_duration_since(time.date_naive()).num_days() {
        0 => format!("{}{}", tr("今天 ", "today "), time.format("%H:%M")),
        1 => tr("昨天", "yesterday").into(),
        _ if time.format("%Y").to_string() == now.format("%Y").to_string() => {
            time.format("%m-%d").to_string()
        }
        _ => time.format("%Y-%m-%d").to_string(),
    }
}

#[derive(Clone)]
pub struct SourceIcon {
    pub image: Arc<Image>,
    /// Transparent, near-black monochrome artwork that needs contrast on a
    /// dark surface. Icons with their own background or bright detail stay intact.
    pub dark: bool,
}

/// Device-pixel edge for `SOURCE_ICON_SIZE` at this window scale.
pub fn source_icon_pixels(scale: f32) -> u32 {
    let scale = if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };
    (SOURCE_ICON_SIZE * scale).round() as u32
}

/// Run on an icon-cache worker. A missing/dead exe does not remove its member.
pub fn load_source_icon(host: &HostState, exe: &str, pixels: u32) -> Result<SourceIcon, String> {
    let bytes = host.process_icon_png_sized(exe, pixels)?;
    let dark = icon_is_dark(&bytes);
    Ok(SourceIcon {
        image: Arc::new(Image::from_bytes(ImageFormat::Png, bytes)),
        dark,
    })
}

fn surface_is_dark(hex: &str) -> bool {
    let Ok(rgb) = u32::from_str_radix(hex.trim().trim_start_matches('#').get(..6).unwrap_or(""), 16) else {
        return false;
    };
    // Linearized like `custom_theme::luminance`, so the cut agrees with it.
    let channel = |shift: u32| {
        let c = ((rgb >> shift) & 255) as f32 / 255.0;
        if c <= 0.03928 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * channel(16) + 0.7152 * channel(8) + 0.0722 * channel(0) < 0.45
}

fn icon_is_dark(png: &[u8]) -> bool {
    let Ok(image) = image::load_from_memory(png) else {
        return false;
    };
    let (mut near_black, mut bright, mut weight) = (0.0f32, 0.0f32, 0.0f32);
    let mut transparent = false;
    for px in image.to_rgba8().pixels() {
        if px[3] <= 32 {
            transparent = true;
            continue;
        }
        let alpha = f32::from(px[3]) / 255.0;
        let l = (0.2126 * f32::from(px[0]) + 0.7152 * f32::from(px[1]) + 0.0722 * f32::from(px[2]))
            / 255.0;
        if px[0].max(px[1]).max(px[2]) < 96 {
            near_black += alpha;
        }
        if l >= 0.55 {
            bright += alpha;
        }
        weight += alpha;
    }
    // Average luminance alone mistakes dark tiles with a bright logo (Cursor,
    // for example) for invisible black glyphs and adds a white rim around them.
    transparent && weight > 0.0 && near_black / weight >= 0.95 && bright / weight < 0.02
}

/// Decides the plate from the painted surface, not the window's light/dark
/// flag: a Custom palette keeps the system flag but may be dark either way.
/// Same 0.45 cut as the Custom palette's own `dark` (frontend `darkChrome`).
pub fn source_icon_element(icon: &SourceIcon, surface: &str) -> AnyElement {
    let image = img(icon.image.clone()).size(px(SOURCE_ICON_SIZE));
    if !(icon.dark && surface_is_dark(surface)) {
        return image.into_any_element();
    }
    rsx! {
        <div relative size={px(SOURCE_ICON_SIZE)}>
            <div absolute top={px(-1.0)} left={px(-1.0)} size={px(SOURCE_ICON_SIZE + 2.0)}
                rounded={px(4.0)} bg={gpui::rgba(0xf2f2f2e6)} />
            {image}
        </div>
    }
        .into_any_element()
}

pub enum ProcessFiltersEvent {
    /// Emitted after durable persistence. Update owner tabs/query and selection.
    Changed {
        presets: Vec<ProcessPreset>,
        selection: ProcessSelection,
    },
    /// A new preset was durably saved and enabled; close this editor.
    Saved,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceProcess {
    pub exe_name: String,
    #[serde(default)]
    pub last_seen: String,
    #[serde(default)]
    pub item_count: i32,
}

pub struct ProcessFilterEditor {
    host: Arc<HostState>,
    writer: PresetWriter,
    tokens: DesignTokens,
    dark: bool,
    language: LanguageMode,
    presets: Vec<ProcessPreset>,
    selection: ProcessSelection,
    /// Preset opened from its category chip; `None` adds a new one.
    editing: Option<String>,
    draft: ProcessDraft,
    name: Entity<SearchInput>,
    name_placeholder: String,
    query: Entity<SearchInput>,
    rename: Entity<SearchInput>,
    rename_id: Option<String>,
    rename_revision: u64,
    catalog: Vec<SourceProcess>,
    /// Exe FileDescription per source, filled after the catalog loads.
    display_names: HashMap<String, String>,
    catalog_seeded: bool,
    body_scroll: gpui::ScrollHandle,
    source_icons: HashMap<String, SourceIcon>,
    loading: bool,
    catalog_error: Option<String>,
    save_error: Option<String>,
    saving: bool,
    /// Retirement blocks fresh requests independently of the reply epoch.
    retired: bool,
    accepted_write: Option<ProcessWriteReceipt>,
    failed_write_id: Option<u64>,
    draft_revision: u64,
    lifetime: u64,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ProcessFiltersEvent> for ProcessFilterEditor {}

impl ProcessFilterEditor {
    pub fn new(
        host: Arc<HostState>,
        tokens: DesignTokens,
        dark: bool,
        presets: Vec<ProcessPreset>,
        selection: ProcessSelection,
        editing: Option<String>,
        writer: PresetWriter,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name = Self::input(&tokens, dark, tr("先选择软件", "Select apps first"), window, cx);
        let query = Self::input(
            &tokens,
            dark,
            tr("搜索软件名或进程名", "Search apps or processes"),
            window,
            cx,
        );
        let rename = Self::input(&tokens, dark, tr("筛选名称", "Filter name"), window, cx);
        let rename_focus = rename.read(cx).focus_handle();
        let subscriptions = vec![
            cx.subscribe_in(&name, window, |this, _, event, window, cx| match event {
                SearchEvent::Changed(text) => {
                    if this.retired {
                        return;
                    }
                    this.draft.display_name = text.clone();
                    this.draft_revision = this.draft_revision.wrapping_add(1);
                    this.retain_current_draft(cx);
                    cx.notify();
                }
                SearchEvent::Escape => this.escape(window, cx),
                SearchEvent::Submit => {}
            }),
            cx.subscribe_in(&rename, window, |this, _, event, window, cx| match event {
                SearchEvent::Submit => this.commit_rename(cx),
                SearchEvent::Escape => this.cancel_rename(window, cx),
                SearchEvent::Changed(_) => {
                    if this.retired {
                        return;
                    }
                    this.rename_revision = this.rename_revision.wrapping_add(1);
                    this.retain_current_draft(cx);
                }
            }),
            cx.on_blur(&rename_focus, window, |this, _, cx| this.commit_rename(cx)),
            cx.subscribe_in(&query, window, |this, _, event, window, cx| match event {
                SearchEvent::Changed(_) => {
                    this.body_scroll.set_offset(gpui::point(px(0.0), px(0.0)));
                    cx.notify();
                }
                SearchEvent::Escape => {
                    if this.query.read(cx).text().is_empty() {
                        this.escape(window, cx);
                    } else {
                        this.query
                            .update(cx, |input, cx| input.set_text(String::new(), window, cx));
                    }
                }
                SearchEvent::Submit => {}
            }),
        ];
        let editing = editing.filter(|id| presets.iter().any(|preset| &preset.id == id));
        let draft = editing
            .as_ref()
            .and_then(|id| presets.iter().find(|preset| &preset.id == id))
            .map(|preset| ProcessDraft {
                display_name: preset.display_name.clone(),
                selected_exes: preset
                    .include_exe_names
                    .iter()
                    .filter(|name| !is_foreground(name))
                    .cloned()
                    .collect(),
                include_foreground: preset.uses_foreground(),
            })
            .unwrap_or_default();
        if !draft.display_name.is_empty() {
            name.update(cx, |input, cx| input.set_text(draft.display_name.clone(), window, cx));
        }
        let mut editor = Self {
            host,
            writer,
            tokens,
            dark,
            language: displayed_language(),
            presets,
            selection,
            editing,
            draft,
            name,
            name_placeholder: String::new(),
            query,
            rename,
            rename_id: None,
            rename_revision: 0,
            catalog: Vec::new(),
            display_names: HashMap::new(),
            catalog_seeded: false,
            body_scroll: gpui::ScrollHandle::new(),
            source_icons: HashMap::new(),
            loading: false,
            catalog_error: None,
            save_error: None,
            saving: false,
            retired: false,
            accepted_write: None,
            failed_write_id: None,
            draft_revision: 0,
            lifetime: 0,
            _subscriptions: subscriptions,
        };
        editor.selection.reconcile(&editor.presets);
        editor.load_catalog(cx);
        editor
    }

    fn input(
        tokens: &DesignTokens,
        dark: bool,
        placeholder: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<SearchInput> {
        let t = tokens.theme(dark).clone();
        let height = 26.0;
        cx.new(|cx| {
            let mut input = SearchInput::new(window, cx);
            input.set_colors(
                input_colors(&t),
                cx,
            );
            input.set_metrics(height, 12.0, 0.0, cx);
            input.set_placeholder(placeholder, cx);
            input.set_double_click_clears(false);
            input
        })
    }

    pub fn input_focus_handles(&self, cx: &gpui::App) -> Vec<FocusHandle> {
        vec![
            self.query.read(cx).focus_handle(),
            self.name.read(cx).focus_handle(),
            self.rename.read(cx).focus_handle(),
        ]
    }

    /// Parent calls before retiring the editor on route/Exit. Includes an
    /// accepted write whose UI callback has been invalidated by hiding.
    pub fn has_active_save_or_composition(&self, cx: &App) -> bool {
        self.saving
            || self
                .accepted_write
                .as_ref()
                .is_some_and(|receipt| receipt.result().is_none())
            || (!self.retired
                && (self.name.read(cx).is_composing() || self.rename.read(cx).is_composing()))
    }

    /// Failure is separate from an active save; a parent must display this
    /// before leaving, and also handle the application-wide flush error.
    pub fn close_error(&self) -> Option<String> {
        if let Some(id) = self.failed_write_id {
            if !accepted_writes::shared().is_current_failure(id) {
                return None;
            }
        }
        self.save_error.clone().or_else(|| {
            self.accepted_write.as_ref().and_then(|receipt| {
                accepted_writes::shared()
                    .is_current_failure(receipt.write_id())
                    .then(|| receipt.result())
                    .flatten()
                    .and_then(Result::err)
            })
        })
    }

    /// Explicit recovery after an editor was destroyed. Restores only the
    /// submitted draft; never writes it until Save/Retry is clicked. The
    /// original writer and its CAS baseline remain in the shared registry.
    pub fn restore_failed_write(
        &mut self,
        id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if self.retired {
            return Err(tr(
                "进程筛选编辑器已关闭。",
                "The process filter editor has closed.",
            )
            .into());
        }
        if self.has_active_save_or_composition(cx) {
            return Err(tr(
                "保存或输入法组合仍在进行。",
                "Saving or IME composition is still active.",
            )
            .into());
        }
        let (failure, receipt) = accepted_writes::shared().restore(id)?;
        self.editing = match &failure.snapshot.intent {
            ProcessWriteIntent::Update(id) => Some(id.clone()),
            _ => None,
        };
        self.accepted_write = Some(receipt);
        self.draft = failure.latest_draft;
        self.name.update(cx, |input, cx| {
            input.set_text(self.draft.display_name.clone(), window, cx)
        });
        self.rename_id = failure.latest_rename.as_ref().map(|(id, _)| id.clone());
        if let Some((_, text)) = failure.latest_rename {
            self.rename
                .update(cx, |input, cx| input.set_text(text, window, cx));
        }
        self.failed_write_id = Some(id);
        self.save_error = Some(failure.error);
        cx.notify();
        Ok(())
    }

    /// Owner generation-checks background cache completion before calling.
    pub fn set_source_icon(&mut self, exe: String, icon: SourceIcon, cx: &mut Context<Self>) {
        if self.retired {
            return;
        }
        self.source_icons.insert(exe, icon);
        cx.notify();
    }

    pub fn catalog(&self) -> &[SourceProcess] {
        &self.catalog
    }

    /// First Esc cancels an inline rename only. Name inputs consume Esc, so
    /// their subscribers forward it here instead of relying on root bubbling.
    pub fn escape(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.retired {
            return;
        }
        if self.rename_id.is_some() {
            self.cancel_rename(window, cx);
        } else {
            self.cancel(window, cx);
        }
    }

    /// Cancel/backdrop/hide/fold/route exit close the whole editor, even during
    /// an inline rename. A pending save cannot revive this editor.
    pub fn cancel(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if self.retired {
            return;
        }
        self.invalidate(cx);
        cx.emit(ProcessFiltersEvent::Cancelled);
    }

    /// Close/replace lifecycle, including forced dismissal while saving. The
    /// owner must reload its durable snapshot if that write finishes after close.
    pub fn invalidate(&mut self, cx: &mut Context<Self>) {
        if self.retired {
            return;
        }
        self.retired = true;
        self.retain_current_draft(cx);
        self.lifetime = self.lifetime.wrapping_add(1);
        self.saving = false;
        self.loading = false;
        self.rename_id = None;
        cx.notify();
    }

    fn retain_current_draft(&self, cx: &App) {
        if let Some(receipt) = &self.accepted_write {
            let mut draft = self.draft.clone();
            draft.display_name = self.name.read(cx).text().to_string();
            let rename = self
                .rename_id
                .as_ref()
                .map(|id| (id.clone(), self.rename.read(cx).text().to_string()));
            receipt.retain_draft(draft, rename);
        }
    }

    pub fn set_dark(&mut self, dark: bool, cx: &mut Context<Self>) {
        self.set_design_tokens(self.tokens.clone(), dark, cx);
    }

    pub fn set_design_tokens(&mut self, tokens: DesignTokens, dark: bool, cx: &mut Context<Self>) {
        if self.dark == dark && self.tokens.theme(dark) == tokens.theme(dark) {
            return;
        }
        self.tokens = tokens;
        self.dark = dark;
        let t = self.tokens.theme(dark);
        for input in [&self.name, &self.query, &self.rename] {
            input.update(cx, |input, cx| input.set_colors(input_colors(t), cx));
        }
        cx.notify();
    }

    fn load_catalog(&mut self, cx: &mut Context<Self>) {
        if self.loading {
            return;
        }
        self.loading = true;
        self.catalog_error = None;
        let host = self.host.clone();
        let lifetime = self.lifetime;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    host.source_processes().and_then(|rows| {
                        serde_json::from_value::<Vec<SourceProcess>>(rows)
                            .map_err(|error| error.to_string())
                    })
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.lifetime != lifetime {
                    return;
                }
                this.loading = false;
                if this.catalog_seeded {
                    return;
                }
                match result {
                    Ok(rows) => {
                        this.catalog = rows;
                        this.load_display_names(cx);
                    }
                    Err(error) => this.catalog_error = Some(error),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Version resources are read off the UI thread; rows show the process
    /// name until then, and keep it when the exe cannot be found.
    fn load_display_names(&mut self, cx: &mut Context<Self>) {
        let host = self.host.clone();
        let lifetime = self.lifetime;
        let mut exes: Vec<String> = self.catalog.iter().map(|row| row.exe_name.clone()).collect();
        exes.extend(self.draft.selected_exes.iter().cloned());
        cx.spawn(async move |this, cx| {
            let names = cx
                .background_executor()
                .spawn(async move {
                    exes.into_iter()
                        .filter_map(|exe| host.process_display_name(&exe).map(|name| (exe, name)))
                        .collect::<HashMap<_, _>>()
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.lifetime == lifetime && !this.retired {
                    this.display_names.extend(names);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn automatic_name(&self) -> String {
        let mut names = self.draft.selected_exes.clone();
        if self.draft.include_foreground {
            names.push(FOREGROUND_TOKEN.to_string());
        }
        auto_title(&names, tr("前台", "Foreground"), tr("进程筛选", "Process filter"))
    }

    fn source_title(&self, exe: &str) -> String {
        self.display_names
            .get(exe)
            .cloned()
            .unwrap_or_else(|| strip_exe_suffix(exe).to_string())
    }

    /// Catalog rows matching `query` (lowercase), after selected members that
    /// have no recorded items any more, so an edit can still remove them.
    fn visible_sources(&self, query: &str) -> Vec<SourceProcess> {
        let matches = |exe: &str| {
            query.is_empty()
                || exe.to_lowercase().contains(query)
                || self.source_title(exe).to_lowercase().contains(query)
        };
        let missing = self
            .draft
            .selected_exes
            .iter()
            .filter(|exe| !self.catalog.iter().any(|row| row.exe_name.eq_ignore_ascii_case(exe)))
            .map(|exe| SourceProcess {
                exe_name: exe.clone(),
                last_seen: String::new(),
                item_count: 0,
            });
        missing
            .chain(self.catalog.iter().cloned())
            .filter(|row| matches(&row.exe_name))
            .collect()
    }

    pub fn body_scroll(&self) -> gpui::ScrollHandle {
        self.body_scroll.clone()
    }

    fn icon(&self, key: &str, size: f32, tint: gpui::Hsla) -> AnyElement {
        crate::crisp_svg::svg()
            .path(self.tokens.icons.get(key).cloned().unwrap_or_default())
            .size(px(size))
            .flex_none()
            .text_color(tint)
            .into_any_element()
    }

    fn save_edit(&mut self, cx: &mut Context<Self>) {
        if self.retired
            || self.saving
            || self.name.read(cx).is_composing()
            || self.rename.read(cx).is_composing()
        {
            return;
        }
        let Some(id) = self.editing.clone() else {
            return;
        };
        let Some(preset) = self.draft.preset(id.clone()) else {
            return;
        };
        let mut next = self.presets.clone();
        let Some(row) = next.iter_mut().find(|row| row.id == id) else {
            self.save_error = Some(tr("这个进程筛选已被删除。", "This process filter was deleted.").into());
            cx.notify();
            return;
        };
        if *row == preset {
            cx.emit(ProcessFiltersEvent::Saved);
            return;
        }
        *row = preset;
        let submitted = ProcessWriteSnapshot {
            intent: ProcessWriteIntent::Update(id),
            presets: next,
            selection: self.selection.clone(),
            draft: self.draft.clone(),
            rename: None,
        };
        self.accept_snapshot(submitted, cx);
    }

    fn delete_editing(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.editing.clone() {
            self.remove(&id, cx);
        }
    }

    #[cfg(debug_assertions)]
    pub fn diagnostics(&self, cx: &App) -> Value {
        json!({"editing":self.editing,"selected":self.draft.selected_exes,"foreground":self.draft.include_foreground,
            "query":self.query.read(cx).text(),"catalog":self.catalog.len(),"displayNames":self.display_names.len(),
            "automaticName":self.automatic_name(),"saving":self.saving,"error":self.save_error})
    }

    /// Fixed sources for the isolated visual fixture; the empty headless
    /// catalog load is ignored afterwards.
    #[cfg(debug_assertions)]
    pub fn seed_catalog_visual(
        &mut self,
        rows: Vec<SourceProcess>,
        names: HashMap<String, String>,
        cx: &mut Context<Self>,
    ) {
        self.catalog_seeded = true;
        self.loading = false;
        self.catalog = rows;
        self.display_names = names;
        cx.notify();
    }

    fn cancel_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.retired {
            return;
        }
        self.rename_id = None;
        self.rename
            .update(cx, |input, cx| input.set_text(String::new(), window, cx));
        cx.notify();
    }

    fn commit_rename(&mut self, cx: &mut Context<Self>) {
        if self.retired || self.saving || self.rename.read(cx).is_composing() {
            return;
        }
        let Some(id) = self.rename_id.clone() else {
            return;
        };
        if self
            .failed_write_id
            .and_then(|write_id| accepted_writes::shared().current_failure(write_id).ok())
            .is_some_and(|failure| {
                failure.snapshot.intent == ProcessWriteIntent::Rename(id.clone())
            })
        {
            self.retry_failed_save(cx);
            return;
        }
        let mut next = self.presets.clone();
        let name = self.rename.read(cx).text().to_string();
        if rename_preset(&mut next, &id, &name) {
            self.persist(next, self.selection.clone(), false, Some(id), cx);
        } else {
            self.rename_id = None;
            cx.notify();
        }
    }

    fn remove(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.retired || self.saving {
            return;
        }
        if self
            .failed_write_id
            .and_then(|write_id| accepted_writes::shared().current_failure(write_id).ok())
            .is_some_and(|failure| {
                failure.snapshot.intent == ProcessWriteIntent::Delete(id.to_string())
            })
        {
            self.retry_failed_save(cx);
            return;
        }
        let mut next = self.presets.clone();
        next.retain(|preset| preset.id != id);
        let mut selection = self.selection.clone();
        selection.reconcile(&next);
        self.persist(next, selection, false, Some(id.to_string()), cx);
    }

    fn add(&mut self, cx: &mut Context<Self>) {
        if self.retired
            || self.saving
            || self.name.read(cx).is_composing()
            || self.rename.read(cx).is_composing()
        {
            return;
        }
        // Unique only within this collection; blake3 is an existing dependency.
        let seed = format!(
            "{}:{}:{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default(),
            std::process::id(),
            self.presets.len()
        );
        let retained_id = self
            .failed_write_id
            .and_then(|id| {
                failed_process_filter_writes()
                    .into_iter()
                    .find(|failure| failure.write_id == id)
            })
            .and_then(|failure| match failure.snapshot.intent {
                ProcessWriteIntent::Add(id) => Some(id),
                _ => None,
            });
        let retry = retained_id.is_some();
        let mut id =
            retained_id.unwrap_or_else(|| format!("pf_{}", blake3::hash(seed.as_bytes()).to_hex()));
        if retry && self.presets.iter().any(|preset| preset.id == id) {
            self.save_error = Some(tr("原保存的预设 ID 已存在，请保留草稿并核对当前预设。", "The original preset ID already exists; keep this draft and review the current presets.").into());
            cx.notify();
            return;
        }
        while self.presets.iter().any(|preset| preset.id == id) {
            id.push('x');
        }
        let Some(preset) = self.draft.preset(id.clone()) else {
            return;
        };
        let mut next = self.presets.clone();
        next.push(preset);
        let rename_id = self.rename_id.clone();
        if let Some(rename_id) = &rename_id {
            rename_preset(&mut next, rename_id, self.rename.read(cx).text());
        }
        self.persist(
            next,
            ProcessSelection {
                active_id: Some(id),
            },
            true,
            rename_id,
            cx,
        );
    }

    fn retry_failed_save(&mut self, cx: &mut Context<Self>) {
        if self.retired || self.has_active_save_or_composition(cx) {
            return;
        }
        let Some(failure) = self.failed_write_id.and_then(|id| {
            failed_process_filter_writes()
                .into_iter()
                .find(|failure| failure.write_id == id)
        }) else {
            return;
        };
        let rename = self
            .rename_id
            .as_ref()
            .map(|id| (id.clone(), self.rename.read(cx).text().to_string()));
        match failure.retry_snapshot(self.draft.clone(), rename) {
            Ok(snapshot) => self.accept_snapshot(snapshot, cx),
            Err(error) => {
                self.save_error = Some(error);
                cx.notify();
            }
        }
    }

    fn persist(
        &mut self,
        next: Vec<ProcessPreset>,
        selection: ProcessSelection,
        close: bool,
        rename_to_close: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.retired || self.saving {
            return;
        }
        let intent = if close {
            ProcessWriteIntent::Add(selection.active_id.clone().unwrap_or_default())
        } else if let Some(id) = &rename_to_close {
            if next.iter().any(|preset| &preset.id == id) {
                ProcessWriteIntent::Rename(id.clone())
            } else {
                ProcessWriteIntent::Delete(id.clone())
            }
        } else {
            self.save_error = Some("Process preset save intent is unavailable.".into());
            cx.notify();
            return;
        };
        let submitted = ProcessWriteSnapshot {
            intent,
            presets: next.clone(),
            selection: selection.clone(),
            draft: self.draft.clone(),
            rename: self
                .rename_id
                .as_ref()
                .map(|id| (id.clone(), self.rename.read(cx).text().to_string())),
        };
        self.accept_snapshot(submitted, cx);
    }

    fn accept_snapshot(&mut self, submitted: ProcessWriteSnapshot, cx: &mut Context<Self>) {
        if self.retired || self.saving {
            return;
        }
        let next = submitted.presets.clone();
        let selection = submitted.selection.clone();
        // Add and edit close on success; deleting the edited preset does too.
        let close = match &submitted.intent {
            ProcessWriteIntent::Add(_) | ProcessWriteIntent::Update(_) => true,
            ProcessWriteIntent::Delete(id) => self.editing.as_ref() == Some(id),
            ProcessWriteIntent::Rename(_) => false,
        };
        let rename_to_close = match &submitted.intent {
            ProcessWriteIntent::Rename(id) | ProcessWriteIntent::Delete(id) => Some(id.clone()),
            ProcessWriteIntent::Add(_) | ProcessWriteIntent::Update(_) => {
                submitted.rename.as_ref().map(|(id, _)| id.clone())
            }
        };
        // Reserve synchronously, before any executor job can be delayed. Exit
        // observes this even while writer has not entered its storage Mutex.
        let accepted = match self.failed_write_id {
            Some(id) => accepted_writes::shared().retry(id, Some(submitted)),
            None => accepted_writes::shared().accept(submitted, self.writer.clone(), None),
        };
        let accepted = match accepted {
            Ok(accepted) => accepted,
            Err(error) => {
                self.save_error = Some(error);
                cx.notify();
                return;
            }
        };
        let receipt = accepted.receipt();
        let write_id = receipt.write_id();
        self.accepted_write = Some(receipt.clone());
        self.saving = true;
        self.save_error = None;
        let lifetime = self.lifetime;
        let draft_revision = self.draft_revision;
        let rename_revision = self.rename_revision;
        cx.notify();
        // Detach the worker immediately; its lifetime is independent of the
        // weak Entity/future below. Destroying UI cannot cancel accepted work.
        cx.background_executor()
            .spawn(async move {
                accepted.execute();
            })
            .detach();
        cx.spawn(async move |this, cx| {
            let result = receipt.wait().await;
            let _ = this.update(cx, |this, cx| {
                if this.retired
                    || this.lifetime != lifetime
                    || this
                        .accepted_write
                        .as_ref()
                        .map(ProcessWriteReceipt::write_id)
                        != Some(write_id)
                {
                    return;
                }
                this.saving = false;
                match result {
                    Ok(()) => {
                        this.failed_write_id = None;
                        this.presets = next;
                        this.selection = selection;
                        if (rename_to_close.is_some()
                            && this.rename_id == rename_to_close
                            && this.rename_revision == rename_revision)
                            || this.rename_id.as_ref().is_some_and(|id| {
                                !this.presets.iter().any(|preset| &preset.id == id)
                            })
                        {
                            this.rename_id = None;
                        }
                        cx.emit(ProcessFiltersEvent::Changed {
                            presets: this.presets.clone(),
                            selection: this.selection.clone(),
                        });
                        // Preserve edits made while saving instead of closing over them.
                        if close
                            && this.draft_revision == draft_revision
                            && this.rename_revision == rename_revision
                        {
                            cx.emit(ProcessFiltersEvent::Saved);
                        }
                    }
                    Err(error) => {
                        // A retry accepted/completed elsewhere makes this old
                        // receipt stale; never resurrect its error in new UI.
                        if accepted_writes::shared().is_current_failure(write_id) {
                            this.failed_write_id = Some(write_id);
                            this.save_error = Some(error);
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn button(
        &self,
        id: SharedString,
        label: &'static str,
        enabled: bool,
        t: &ThemeTokens,
    ) -> gpui::Stateful<gpui::Div> {
        let hover = color(&t.tertiary);
        rsx! {
            <div id={id} h={px(28.0)} px={px(12.0)} flex flex_none items_center rounded={px(6.0)}
                border_1 border_color={color(&t.border_secondary)} text_size={px(12.0)}
                text_color={color(&t.text_secondary)} whitespace_nowrap />
        }
            .when(enabled, |b| b.cursor_pointer().hover(move |s| s.bg(hover)))
            .when(!enabled, |b| b.opacity(0.4))
            .child(label)
    }
}

impl Render for ProcessFilterEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self
            .failed_write_id
            .is_some_and(|id| !accepted_writes::shared().is_current_failure(id))
        {
            self.failed_write_id = None;
            self.save_error = None;
        }
        // A hidden editor can miss its generation-guarded UI callback. The
        // receipt still owns the result; make it visible on reopening.
        if self.save_error.is_none() {
            if let Some(receipt) = &self.accepted_write {
                if let Some(Err(error)) = receipt.result() {
                    if accepted_writes::shared().is_current_failure(receipt.write_id()) {
                        self.failed_write_id = Some(receipt.write_id());
                        self.save_error = Some(error);
                    }
                }
            }
        }
        // Translation and selection refresh placeholders only, preserving all
        // draft text. Notify children only on change.
        let language = displayed_language();
        if self.language != language {
            self.language = language;
            self.query.update(cx, |input, cx| {
                input.set_placeholder(tr("搜索软件名或进程名", "Search apps or processes"), cx)
            });
            self.rename.update(cx, |input, cx| {
                input.set_placeholder(tr("筛选名称", "Filter name"), cx)
            });
        }
        let count = self.draft.selected_exes.len() + usize::from(self.draft.include_foreground);
        let automatic = self.automatic_name();
        let placeholder = if count == 0 {
            tr("先选择软件", "Select apps first").to_string()
        } else {
            automatic
        };
        if self.name_placeholder != placeholder {
            self.name_placeholder = placeholder.clone();
            self.name
                .update(cx, |input, cx| input.set_placeholder(placeholder, cx));
        }
        let tokens = self.tokens.clone();
        let t = tokens.theme(self.dark);
        let enabled = !self.saving
            && !self.retired
            && !self.name.read(cx).is_composing()
            && !self.rename.read(cx).is_composing()
            && (self.failed_write_id.is_some() || count > 0);
        let accent = color(&t.accent);
        let hover = color(&t.hover);
        let selected_bg = color(&t.neutral_active);
        let scrolled = f32::from(self.body_scroll.offset().y) < -0.5
            && f32::from(self.body_scroll.max_offset().height) > 0.5;
        let query = self.query.read(cx).text().trim().to_lowercase();
        let check = |on: bool| {
            rsx! {
                <div size={px(16.0)} flex_none flex items_center justify_center rounded_full
                    border_1 />
            }
                .border_color(if on { accent } else { color(&t.border_secondary) })
                .when(on, |circle| {
                    circle.bg(accent).child(self.icon("check", 10.0, gpui::white()))
                })
        };
        let row_shell = |id: SharedString, on: bool| {
            rsx! {
                <div id={id} min_h={px(40.0)} flex items_center gap={px(10.0)} px={px(8.0)}
                    py={px(4.0)} rounded={px(6.0)} cursor_pointer />
            }
                .when(on, |row| row.bg(selected_bg))
                .when(!on, |row| row.hover(move |style| style.bg(hover)))
        };
        let text_block = |title: String, detail: String| {
            rsx! {
                <div flex_1 min_w_0 overflow_hidden flex flex_col>
                    <div overflow_hidden text_size={px(12.0)} font_weight={FontWeight::MEDIUM} whitespace_nowrap
                        text_ellipsis>
                        {title}
                    </div>
                    <div overflow_hidden text_size={px(11.0)} text_color={color(&t.text_muted)} whitespace_nowrap
                        text_ellipsis>
                        {detail}
                    </div>
                </div>
            }
        };
        let header = rsx! {
            <div flex_none flex flex_col gap={px(8.0)} pl={px(14.0)} pr={px(10.0)} pt={px(10.0)}
                pb={px(8.0)} border_b_1 />
        }
            .border_color(if scrolled { color(&t.border) } else { gpui::transparent_black() })
            .child(
                rsx! {
                    <div h={px(24.0)} flex items_center gap={px(8.0)}>
                        <div text_size={px(13.0)} font_weight={FontWeight::SEMIBOLD} flex_none
                            whitespace_nowrap>
                            {if self.editing.is_some() {
                                tr("编辑进程筛选", "Edit process filter")
                            } else {
                                tr("添加进程筛选", "Add process filter")
                            }}
                        </div>
                        <div flex_1 min_w_0 overflow_hidden text_size={px(11.5)} text_color={color(&t.text_muted)}
                            whitespace_nowrap text_ellipsis>
                            {if count > 0 {
                                format!("{}{count}{}", tr("已选 ", ""), tr(" 项", " selected"))
                            } else {
                                tr("只看指定软件复制的内容", "Only show what chosen apps copied").to_string()
                            }}
                        </div>
                    </div>
                }
                    .child(
                        rsx! {
                            <div id={"pf-close"} size={px(24.0)} flex_none flex items_center
                                justify_center rounded={px(5.0)} cursor_pointer />
                        }
                            .hover(move |style| style.bg(selected_bg))
                            .child(self.icon("close", 14.0, color(&t.text_muted)))
                            .on_click(cx.listener(|this, _, window, cx| this.cancel(window, cx))),
                    ),
            )
            .child(
                rsx! {
                    <div h={px(30.0)} mr={px(4.0)} flex items_center gap={px(6.0)} pl={px(9.0)}
                        pr={px(4.0)} rounded={px(6.0)} border_1 bg={color(&t.background)} />
                }
                    .border_color(if self.query.read(cx).focus_handle().is_focused(window) {
                        accent
                    } else {
                        color(&t.border)
                    })
                    .child(self.icon("search", 13.0, color(&t.text_muted)))
                    .child(div().flex_1().min_w_0().child(self.query.clone())),
            );
        let mut body = div()
            .id("pf-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.body_scroll)
            .smooth_wheel("pf-body", &self.body_scroll, cx)
            .px(px(6.0))
            .pb(px(6.0));
        // Recovered inline rename from an earlier failed write.
        if self.rename_id.is_some() {
            body = body.child(
                rsx! {
                    <div flex flex_col gap={px(4.0)} px={px(8.0)} py={px(6.0)}
                        text_size={px(11.0)} text_color={color(&t.text_muted)}>
                        {tr("未保存的重命名", "Unsaved rename")}
                    </div>
                }
                .child(self.rename.clone()),
            );
        }
        if query.is_empty() {
            let on = self.draft.include_foreground;
            body = body
                .child(
                    row_shell("pf-foreground".into(), on)
                        .child(
                            rsx! {
                                <div size={px(20.0)} flex_none flex items_center justify_center
                                    rounded={px(5.0)} bg={accent.opacity(0.16)} />
                            }
                            .child(self.icon("desktop", 13.0, accent)),
                        )
                        .child(text_block(
                            tr("当前前台软件", "Current foreground app").into(),
                            tr("切到哪个软件，就只看它复制的内容", "Follows whichever app is in front").into(),
                        ))
                        .child(check(on))
                        .on_click(cx.listener(|this, _, _, cx| {
                            if this.retired {
                                return;
                            }
                            this.draft.include_foreground = !this.draft.include_foreground;
                            this.draft_revision = this.draft_revision.wrapping_add(1);
                            this.retain_current_draft(cx);
                            cx.notify();
                        })),
                )
                .child(
                    rsx! {
                        <div mx={px(8.0)} mt={px(8.0)} mb={px(3.0)} text_size={px(11.0)}
                            text_color={color(&t.text_muted)}>
                            {tr("复制来源 · 最近使用在前", "Copied from · most recent first")}
                        </div>
                    },
                );
        }
        if let Some(error) = self.catalog_error.clone() {
            body = body.child(
                div().px(px(8.0)).py(px(6.0)).text_size(px(12.0)).text_color(color(&t.error))
                    .child(format!("{}{error}", tr("加载进程列表失败：", "Failed to load processes: "))),
            );
        }
        let rows = self.visible_sources(&query);
        if self.loading && rows.is_empty() {
            body = body.child(
                div().px(px(8.0)).py(px(10.0)).text_size(px(12.0)).text_color(color(&t.text_muted))
                    .child(tr("加载中…", "Loading…")),
            );
        } else if rows.is_empty() && self.catalog_error.is_none() {
            body = body.child(
                rsx! {
                    <div flex flex_col items_center gap={px(2.0)} py={px(28.0)}
                        text_size={px(12.0)} text_color={color(&t.text_secondary)} />
                }
                .child(if query.is_empty() {
                    tr("还没有记录到复制来源", "No copy sources recorded yet").to_string()
                } else {
                    format!("{}{}{}", tr("没有匹配「", "No app matches \""), query, tr("」的软件", "\""))
                })
                .child(
                    div().text_size(px(11.0)).text_color(color(&t.text_muted))
                        .child(tr("只列出复制过内容的软件", "Only apps you copied from are listed")),
                ),
            );
        }
        for (index, source) in rows.into_iter().enumerate() {
            let on = self.draft.selected_exes.contains(&source.exe_name);
            let exe = source.exe_name.clone();
            let detail = if source.item_count > 0 {
                format!(
                    "{} · {}{} · {}",
                    source.exe_name,
                    source.item_count,
                    tr(" 条", " items"),
                    seen_label(&source.last_seen)
                )
            } else {
                format!("{} · {}", source.exe_name, tr("暂无记录", "no items"))
            };
            body = body.child(
                row_shell(SharedString::from(format!("pf-source-{index}")), on)
                    .child(
                        div().size(px(20.0)).flex_none().flex().items_center().justify_center()
                            .when_some(self.source_icons.get(&source.exe_name), |slot, icon| {
                                slot.child(source_icon_element(icon, &t.surface))
                            }),
                    )
                    .child(text_block(self.source_title(&source.exe_name), detail))
                    .child(check(on))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.retired {
                            return;
                        }
                        this.draft.toggle_exe(&exe);
                        this.draft_revision = this.draft_revision.wrapping_add(1);
                        this.retain_current_draft(cx);
                        cx.notify();
                    })),
            );
        }
        let first_icon = self
            .draft
            .selected_exes
            .first()
            .and_then(|exe| self.source_icons.get(exe));
        let footer = rsx! {
            <div flex_none flex flex_col gap={px(8.0)} px={px(10.0)} pt={px(8.0)} pb={px(10.0)}
                border_t_1 border_color={color(&t.border)} />
        }
            .child(
                rsx! {
                    <div flex items_center gap={px(8.0)}>
                        <div flex_none text_size={px(12.0)} text_color={color(&t.text_secondary)}>
                            {tr("标签名", "Label")}
                        </div>
                    </div>
                }
                .child(
                    rsx! {
                        <div flex_1 min_w_0 h={px(28.0)} flex items_center gap={px(6.0)}
                            pl={px(8.0)} pr={px(6.0)} rounded={px(14.0)} border_1
                            bg={color(&t.background)} />
                    }
                    .border_color(color(&t.border))
                    .when_some(first_icon, |pill, icon| {
                        pill.child(div().flex_none().child(source_icon_element(icon, &t.surface)))
                    })
                    .when(first_icon.is_none() && self.draft.include_foreground, |pill| {
                        pill.child(self.icon("desktop", 12.0, accent))
                    })
                    .child(div().flex_1().min_w_0().child(self.name.clone()))
                    .when(count > 0 && self.name.read(cx).text().trim().is_empty(), |pill| {
                        pill.child(
                            rsx! {
                                <div flex_none px={px(5.0)} rounded={px(4.0)}
                                    bg={color(&t.secondary)} text_size={px(10.5)}
                                    text_color={color(&t.text_muted)}>
                                    {tr("自动", "Auto")}
                                </div>
                            },
                        )
                    }),
                ),
            )
            .when_some(self.save_error.clone(), |footer, error| {
                footer.child(
                    div().text_size(px(12.0)).text_color(color(&t.error))
                        .child(format!("{}{error}", tr("保存失败：", "Save failed: "))),
                )
            })
            .child(
                rsx! {
                    <div flex items_center gap={px(6.0)} />
                }
                .when(self.editing.is_some(), |actions| {
                    actions.child(
                        self.button("pf-delete".into(), "", !self.saving && !self.retired, t)
                            .border_color(gpui::transparent_black())
                            .px(px(8.0))
                            .gap(px(4.0))
                            .text_color(color(&t.error))
                            .child(self.icon("delete", 13.0, color(&t.error)))
                            .child(tr("删除", "Delete"))
                            .on_click(cx.listener(|this, _, _, cx| this.delete_editing(cx))),
                    )
                })
                .child(div().flex_1())
                .child(
                    self.button("pf-cancel".into(), tr("取消", "Cancel"), true, t)
                        .bg(color(&t.secondary))
                        .text_color(color(&t.text))
                        .on_click(cx.listener(|this, _, window, cx| this.cancel(window, cx))),
                )
                .child(
                    self.button(
                        "pf-add".into(),
                        if self.saving {
                            tr("保存中…", "Saving…")
                        } else if self.failed_write_id.is_some() {
                            tr("重试保存", "Retry save")
                        } else if self.editing.is_some() {
                            tr("保存", "Save")
                        } else {
                            tr("添加并启用", "Add and enable")
                        },
                        enabled,
                        t,
                    )
                    .border_color(gpui::transparent_black())
                    .bg(accent)
                    .text_color(gpui::white())
                    .font_weight(FontWeight::MEDIUM)
                    .on_click(cx.listener(|this, _, _, cx| {
                        if this.failed_write_id.is_some() {
                            this.retry_failed_save(cx);
                        } else if this.editing.is_some() {
                            this.save_edit(cx);
                        } else {
                            this.add(cx);
                        }
                    })),
                ),
            );
        rsx! {
            <div size_full min_h_0 flex flex_col text_color={color(&t.text)} />
        }
            .child(header)
            .child(body)
            .child(footer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_icon_plate_follows_painted_surface_and_artwork() {
        assert!(surface_is_dark("#262626"));
        assert!(surface_is_dark("#1f3a2b"), "dark custom palette");
        assert!(!surface_is_dark("#ffffff"));
        assert!(!surface_is_dark("not-a-color"));
        let png = |image: image::RgbaImage| {
            let mut out = std::io::Cursor::new(Vec::new());
            image.write_to(&mut out, image::ImageFormat::Png).unwrap();
            out.into_inner()
        };
        let mut glyph = image::RgbaImage::from_pixel(20, 20, image::Rgba([40, 40, 48, 255]));
        // Hidden RGB in transparent pixels must not affect the classification.
        glyph.put_pixel(0, 0, image::Rgba([255, 255, 255, 0]));
        assert!(icon_is_dark(&png(glyph.clone())), "transparent near-black glyph needs contrast");
        assert!(!icon_is_dark(&png(image::RgbaImage::from_pixel(20, 20, image::Rgba([40, 40, 48, 255])))),
            "an opaque tile keeps its original background");
        assert!(!icon_is_dark(&png(image::RgbaImage::new(20, 20))), "fully transparent art has no tone");

        // Even a small bright emblem is visible without an added white plate.
        for y in 8..11 {
            for x in 8..11 {
                glyph.put_pixel(x, y, image::Rgba([240, 240, 240, 255]));
            }
        }
        assert!(!icon_is_dark(&png(glyph)), "dark tile with bright emblem keeps its transparent edge");
        let mut blue = image::RgbaImage::from_pixel(20, 20, image::Rgba([0, 0, 200, 255]));
        blue.put_pixel(0, 0, image::Rgba([0, 0, 0, 0]));
        assert!(!icon_is_dark(&png(blue)), "saturated color is not a black glyph");
    }

    const LEGACY: &str = r#"[{"id":"saved-id","displayName":"  工作  ","includeExeNames":[" Chrome.EXE ","foreground","missing.exe","Chrome.EXE",4," "]},{"id":"fg","displayName":"","includeExeNames":[" FOREGROUND "]},{"id":"invalid","includeExeNames":[]}]"#;

    #[test]
    fn decoding_keeps_ids_names_order_duplicates_and_missing_exes() {
        let decoded = decode_web_presets(LEGACY).unwrap();
        assert_eq!(decoded.source_json, LEGACY);
        assert_eq!(decoded.skipped_rows, 1);
        assert_eq!(decoded.presets.len(), 2);
        assert_eq!(decoded.presets[0].display_name, "  工作  ");
        assert_eq!(
            decoded.presets[0].include_exe_names,
            ["Chrome.EXE", "foreground", "missing.exe", "Chrome.EXE"]
        );
        assert_eq!(decoded.presets[1].include_exe_names, ["FOREGROUND"]);
        let roundtrip = decode_web_presets(&encode_web_presets(&decoded.presets).unwrap()).unwrap();
        assert_eq!(roundtrip.presets, decoded.presets);
        assert!(decode_web_presets("{}").is_err());
        assert!(decode_web_presets("broken").is_err());
        assert_eq!(decode_web_presets("[]").unwrap().presets.len(), 0);
    }

    #[test]
    fn automatic_labels_only_change_presentation() {
        assert_eq!(
            auto_title(
                &["Chrome.EXE".into(), "foreground".into()],
                "前台",
                "进程筛选"
            ),
            "Chrome · 前台"
        );
        assert_eq!(
            auto_title(&[" foreground ".into()], "前台", "进程筛选"),
            "前台"
        );
        assert_eq!(auto_title(&[], "前台", "进程筛选"), "进程筛选");
        let many: Vec<String> = ["chrome.exe", "WeChat.exe", "Code.exe", "QQ.exe"]
            .into_iter()
            .map(Into::into)
            .collect();
        cliphost_core::i18n::set_language(cliphost_core::i18n::LanguageMode::Chinese);
        assert_eq!(auto_title(&many[..2], "前台", "进程筛选"), "chrome · WeChat");
        assert_eq!(auto_title(&many, "前台", "进程筛选"), "chrome · WeChat 等 2 个");
        assert_eq!(strip_exe_suffix("应用.exe"), "应用");
        assert_eq!(strip_exe_suffix("应用"), "应用");
        assert_eq!(
            decode_web_presets(LEGACY).unwrap().presets[0].label(),
            "工作"
        );
    }

    #[test]
    fn draft_preserves_selection_order_and_foreground_is_last() {
        let mut draft = ProcessDraft::default();
        assert!(draft.preset("id".into()).is_none());
        draft.toggle_exe("Code.exe");
        draft.toggle_exe("Chrome.exe");
        draft.toggle_exe("Code.exe");
        draft.toggle_exe("Code.exe");
        draft.include_foreground = true;
        draft.display_name = "  工作  ".into();
        let preset = draft.preset("new-id".into()).unwrap();
        assert_eq!(
            preset.include_exe_names,
            ["Chrome.exe", "Code.exe", "foreground"]
        );
        assert_eq!(preset.display_name, "工作");
    }

    #[test]
    fn selection_toggle_rename_delete_leave_other_filters_orthogonal() {
        let mut presets = decode_web_presets(LEGACY).unwrap().presets;
        let mut selection = ProcessSelection::default();
        selection.toggle("saved-id", &presets);
        assert!(selection.uses_foreground(&presets));
        assert_eq!(
            selection.spec(&presets).unwrap()["includeExeNames"][2],
            "missing.exe"
        );
        selection.toggle("saved-id", &presets);
        assert_eq!(selection.spec(&presets), None);
        selection.toggle("fg", &presets);
        assert!(rename_preset(&mut presets, "fg", " My foreground "));
        assert_eq!(presets[1].display_name, "My foreground");
        let automatic = auto_title(
            &presets[1].include_exe_names,
            tr("前台", "Foreground"),
            tr("进程筛选", "Process filter"),
        );
        assert!(rename_preset(&mut presets, "fg", &automatic));
        assert_eq!(presets[1].display_name, "");
        presets.retain(|preset| preset.id != "fg");
        selection.reconcile(&presets);
        assert_eq!(selection.active_id, None);
        assert_eq!(presets[0].id, "saved-id");
    }
}
