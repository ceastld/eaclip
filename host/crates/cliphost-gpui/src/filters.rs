//! Category tabs from `ClipFilterConfig`, the search-box filter state and the
//! category tag editor. Semantics follow the web `ClipboardPage`: tabs come
//! from the core's filter items, the pinned-only tab exists only while
//! 「单独分栏显示置顶项」 is on (and then the other tabs exclude pinned rows).

use gpui_rsx::rsx;
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, OnceLock,
    },
};

use cliphost_core::{
    clip_filter,
    host::HostState,
    i18n::{displayed_language, tr, tr_for, LanguageMode},
};
use gpui::{
    div, prelude::*, px, AnyElement, Context, Entity, EventEmitter, FontWeight, SharedString,
    Subscription, Window,
};
use serde_json::{json, Value};

use crate::{
    smooth_wheel::SmoothWheelExt,
    input::{InputColors, SearchEvent, SearchInput},
    preview::{DesignTokens, ThemeTokens},
    settings::SettingsPalette,
    view::color,
};

/// `(type, itemType, option label, default tab name)` — web `CLIP_FILTER_TYPE_OPTIONS`.
pub const TYPE_OPTIONS: &[(&str, i32, &str, &str)] = &[
    ("all", clip_filter::ALL, "全部", "默认"),
    ("text", clip_filter::TEXT, "所有文本", "文本"),
    ("plaintext", clip_filter::PLAIN_TEXT, "纯文本", "纯文本"),
    ("richtext", clip_filter::RICH_TEXT, "富文本", "富文本"),
    ("image", clip_filter::IMAGE, "图片", "图片"),
    ("file", clip_filter::FILE, "文件", "文件"),
    ("url", clip_filter::URL, "网址", "网址"),
    ("other", clip_filter::STEP, "步骤", "步骤"),
];

pub const PINNED_TAB_KEY: &str = "top";

/// Card width not available to preview chips: header padding plus strip padding.
const PREVIEW_INSET: f32 = 34.0;

fn type_option(type_key: &str, item_type: i32) -> (&'static str, i32, &'static str, &'static str) {
    let key = type_key.trim().to_ascii_lowercase();
    TYPE_OPTIONS
        .iter()
        .find(|option| option.0 == key)
        .or_else(|| TYPE_OPTIONS.iter().find(|option| option.1 == item_type))
        .copied()
        .unwrap_or(TYPE_OPTIONS[0])
}

fn type_label(type_key: &str, item_type: i32) -> &'static str {
    match type_option(type_key, item_type).0 {
        "all" => tr("全部", "All"),
        "text" => tr("所有文本", "All text"),
        "plaintext" => tr("纯文本", "Plain text"),
        "richtext" => tr("富文本", "Rich text"),
        "image" => tr("图片", "Images"),
        "file" => tr("文件", "Files"),
        "url" => tr("网址", "URLs"),
        "other" => tr("步骤", "Steps"),
        _ => unreachable!(),
    }
}

/// A label whose built-in origin is known by the caller. Stored names, UUIDs
/// and matching default filter structures do not establish that origin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuiltinTabLabel {
    Default,
    Text,
    PlainText,
    RichText,
    Images,
    Files,
    Urls,
    Steps,
    Pinned,
}

/// Translate only an explicitly identified built-in label. With no such
/// identity, preserve the configured name verbatim, including whitespace.
pub fn category_tab_label(
    display_name: &str,
    builtin_label: Option<BuiltinTabLabel>,
    language: LanguageMode,
) -> String {
    let Some(label) = builtin_label else {
        return display_name.to_string();
    };
    let (chinese, english) = match label {
        BuiltinTabLabel::Default => ("默认", "Default"),
        BuiltinTabLabel::Text => ("文本", "Text"),
        BuiltinTabLabel::PlainText => ("纯文本", "Plain text"),
        BuiltinTabLabel::RichText => ("富文本", "Rich text"),
        BuiltinTabLabel::Images => ("图片", "Images"),
        BuiltinTabLabel::Files => ("文件", "Files"),
        BuiltinTabLabel::Urls => ("网址", "URLs"),
        BuiltinTabLabel::Steps => ("步骤", "Steps"),
        BuiltinTabLabel::Pinned => ("置顶", "Pinned"),
    };
    tr_for(language, chinese, english).to_string()
}

/// The empty display-name contract explicitly requests a generated label.
/// A nonempty configured name is never inferred to be a built-in label.
fn generated_default_tab_label(type_key: &str, item_type: i32) -> BuiltinTabLabel {
    match type_option(type_key, item_type).0 {
        "all" => BuiltinTabLabel::Default,
        "text" => BuiltinTabLabel::Text,
        "plaintext" => BuiltinTabLabel::PlainText,
        "richtext" => BuiltinTabLabel::RichText,
        "image" => BuiltinTabLabel::Images,
        "file" => BuiltinTabLabel::Files,
        "url" => BuiltinTabLabel::Urls,
        "other" => BuiltinTabLabel::Steps,
        _ => unreachable!(),
    }
}

fn automatic_tab_name(type_key: &str, item_type: i32, language: LanguageMode) -> String {
    category_tab_label(
        "",
        Some(generated_default_tab_label(type_key, item_type)),
        language,
    )
}

/// Search-box filter popover state. Only `sort_type` is persisted
/// (`autoSortAfterPaste`); the rest lasts for the window session.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AdvancedFilter {
    pub sort_type: i32,
    pub usage_filter: i32,
    pub create_time_filter: i32,
    pub start: String,
    pub end: String,
    pub date: String,
}

impl AdvancedFilter {
    /// The web trigger shows `has-filter` for any non-default choice.
    pub fn is_active(&self) -> bool {
        self.sort_type != 0 || self.usage_filter != 0 || self.create_time_filter != 0
    }
}

/// One list query: the selected tab's filter item plus the advanced filter.
#[derive(Clone, Debug, PartialEq)]
pub struct ListRequest {
    pub type_key: String,
    pub filter_item: Option<Value>,
    pub advanced: AdvancedFilter,
    /// Orthogonal source-process constraint, using the core's existing
    /// `{ includeExeNames: [...] }` contract. Keep `foreground` unresolved:
    /// the core resolves it against the current external foreground per query.
    pub process_filter: Option<Value>,
}

impl ListRequest {
    /// Built-in category without a configured filter item (fixtures, tests).
    pub fn category(category: &str) -> Self {
        Self {
            type_key: if category == PINNED_TAB_KEY {
                "star"
            } else {
                category
            }
            .to_string(),
            filter_item: None,
            advanced: AdvancedFilter::default(),
            process_filter: None,
        }
    }

    pub fn with_process_filter(mut self, process_filter: Option<Value>) -> Self {
        self.process_filter = process_filter;
        self
    }

    pub fn params(&self, query: &str, skip: usize, take: usize, session: &str) -> Value {
        let optional = |text: &str| {
            let text = text.trim();
            if text.is_empty() {
                Value::Null
            } else {
                json!(text)
            }
        };
        json!({
            "type": self.type_key,
            "filterItem": self.filter_item,
            "processFilter": self.process_filter,
            "query": query,
            "skip": skip.min(i32::MAX as usize),
            "take": if take == 0 { 100 } else { take.min(500) },
            "sortType": self.advanced.sort_type,
            "usageFilter": self.advanced.usage_filter,
            "createTimeFilter": self.advanced.create_time_filter,
            "createTimeFilterStart": optional(&self.advanced.start),
            "createTimeFilterEnd": optional(&self.advanced.end),
            "createTimeFilterDate": optional(&self.advanced.date),
            "webSessionId": session,
        })
    }

    /// The design fixture knows the default categories and pinned rows.
    pub fn fixture_category(&self) -> &str {
        match self.type_key.as_str() {
            "star" => PINNED_TAB_KEY,
            "text" | "plaintext" | "richtext" => "text",
            "image" => "image",
            "file" => "file",
            "url" => "url",
            _ => "all",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CategoryTab {
    pub key: String,
    pub label: String,
    pub icon: &'static str,
    pub request_type: String,
    pub filter_item: Option<Value>,
}

impl CategoryTab {
    pub fn is_pinned(&self) -> bool {
        self.key == PINNED_TAB_KEY
    }

    pub fn request(&self, advanced: &AdvancedFilter) -> ListRequest {
        ListRequest {
            type_key: self.request_type.clone(),
            filter_item: self.filter_item.clone(),
            advanced: advanced.clone(),
            process_filter: None,
        }
    }
}

fn tab_icon(type_key: &str, item_type: i32) -> &'static str {
    match type_key {
        "plaintext" => return "text",
        "richtext" => return "code",
        "other" => return "list",
        "url" => return "link",
        _ => {}
    }
    match item_type {
        clip_filter::PLAIN_TEXT | clip_filter::TEXT => "text",
        clip_filter::RICH_TEXT => "code",
        clip_filter::STEP => "list",
        clip_filter::IMAGE => "image",
        clip_filter::FILE => "file",
        clip_filter::URL => "link",
        _ => "all",
    }
}

/// Web `buildBaseFiltersWithOptionalPinned` + `resolveRequestFilter`.
pub fn build_tabs(filters: &Value, separate_pinned: bool) -> Vec<CategoryTab> {
    build_tabs_for_language(filters, separate_pinned, displayed_language())
}

/// Build labels for a selected language without changing stored filter data.
/// Empty names opt into automatic type/language labels. Nonempty configured
/// names are literal overrides, including names identical to a built-in label.
pub fn build_tabs_for_language(
    filters: &Value,
    separate_pinned: bool,
    language: LanguageMode,
) -> Vec<CategoryTab> {
    let items: Vec<Value> = filters
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter(|item| {
                    let type_key = item["type"].as_str().unwrap_or_default();
                    item["starType"].as_i64().unwrap_or(0) != 1
                        && !type_key.eq_ignore_ascii_case("star")
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let items = if items.is_empty() {
        serde_json::to_value(clip_filter::create_default_tabs())
            .ok()
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default()
            .into_iter()
            .map(|mut item| {
                // Defaults are not persisted yet; key them by kind, not a fresh UUID.
                item["id"] = json!("");
                item
            })
            .collect()
    } else {
        items
    };
    let mut tabs: Vec<CategoryTab> = Vec::new();
    for item in items {
        let raw_type = item["type"].as_str().unwrap_or("all");
        let type_key = if matches!(raw_type, "" | "default" | "star") {
            "all"
        } else {
            raw_type
        };
        let item_type = item["itemType"].as_i64().unwrap_or(clip_filter::ALL as i64) as i32;
        let id = item["id"].as_str().unwrap_or_default().trim().to_string();
        let mut key = if id.is_empty() {
            type_key.to_string()
        } else {
            format!("id:{id}")
        };
        if tabs.iter().any(|tab| tab.key == key) {
            if !id.is_empty() {
                continue;
            }
            key = format!("{key}#{}", tabs.len());
        }
        let mut filter_item = item.clone();
        filter_item["type"] = json!(type_key);
        filter_item["starType"] = json!(if separate_pinned { 2 } else { 0 });
        tabs.push(CategoryTab {
            key,
            label: category_tab_label(
                item["displayName"].as_str().unwrap_or_default(),
                item["displayName"]
                    .as_str()
                    .unwrap_or_default()
                    .trim()
                    .is_empty()
                    .then(|| generated_default_tab_label(type_key, item_type)),
                language,
            ),
            icon: tab_icon(type_key, item_type),
            request_type: type_key.to_string(),
            filter_item: Some(filter_item),
        });
    }
    if separate_pinned {
        let after_default = tabs
            .iter()
            .position(|tab| tab.request_type == "all")
            .map_or(if tabs.is_empty() { 0 } else { 1 }, |index| index + 1);
        tabs.insert(
            after_default,
            CategoryTab {
                key: PINNED_TAB_KEY.into(),
                label: category_tab_label("置顶", Some(BuiltinTabLabel::Pinned), language),
                icon: "pin-top",
                request_type: "star".into(),
                filter_item: Some(json!({ "id": "", "displayName": "置顶", "type": "star",
                    "itemType": clip_filter::ALL, "starType": 0, "icon": "" })),
            },
        );
    }
    tabs
}

pub type FilterTabWriteResult = tokio::sync::oneshot::Receiver<Result<(), String>>;
type FilterTabWriter = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

/// Recovery snapshot of an explicitly accepted Save, retained even after its
/// editor is destroyed. `id` identifies one accepted transaction, never a
/// replacement failure; `draft_filter_items` preserves pre-normalization input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilterTabWriteFailure {
    pub id: u64,
    pub filter_items: String,
    pub draft_filter_items: String,
    pub data_root: String,
    pub error: String,
}

#[derive(Clone)]
struct FailedFilterTabWrite {
    owner: u64,
    snapshot: FilterTabWriteFailure,
    writer: FilterTabWriter,
}

#[derive(Clone)]
struct FilterTabDraft {
    filter_items: String,
    draft_filter_items: String,
    data_root: String,
}

struct FilterTabWrite {
    owner: u64,
    id: u64,
    draft: FilterTabDraft,
    writer: FilterTabWriter,
    reply: tokio::sync::oneshot::Sender<Result<(), String>>,
}

#[derive(Default)]
struct FilterTabWriteState {
    exit_pending: bool,
    next_id: u64,
    pending: VecDeque<FilterTabWrite>,
    pending_owners: HashSet<u64>,
    running: bool,
    failures: BTreeMap<u64, FailedFilterTabWrite>,
    waiters: Vec<tokio::sync::oneshot::Sender<Result<(), String>>>,
}

impl FilterTabWriteState {
    fn outcome(&self) -> Result<(), String> {
        if self.failures.is_empty() {
            return Ok(());
        }
        Err(self
            .failures
            .values()
            .map(|failure| {
                format!(
                    "{} #{}: {}",
                    tr("分类保存失败", "Category save failed"),
                    failure.snapshot.id,
                    failure.snapshot.error
                )
            })
            .collect::<Vec<_>>()
            .join("\n"))
    }
}

#[derive(Default)]
struct FilterTabWrites {
    state: Mutex<FilterTabWriteState>,
}

impl FilterTabWrites {
    fn set_exit_pending(&self, pending: bool) {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .exit_pending = pending;
    }

    /// Registration happens synchronously in Save's UI turn, before either
    /// worker startup or observer creation. No Entity/Window belongs to a job.
    #[cfg(test)]
    fn enqueue(
        &self,
        owner: u64,
        writer: FilterTabWriter,
        draft: FilterTabDraft,
    ) -> Result<(FilterTabWriteResult, bool), String> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        Self::enqueue_locked(&mut state, owner, writer, draft)
            .map(|(_, receiver, start)| (receiver, start))
    }

    fn enqueue_locked(
        state: &mut FilterTabWriteState,
        owner: u64,
        writer: FilterTabWriter,
        draft: FilterTabDraft,
    ) -> Result<(u64, FilterTabWriteResult, bool), String> {
        if state.exit_pending {
            return Err(tr(
                "正在退出，未接受新的分类保存；草稿已保留。",
                "Exit is pending. The new category save was not accepted; its draft was kept.",
            )
            .into());
        }
        if state.pending_owners.contains(&owner) {
            return Err(tr(
                "分类仍在保存，请等待完成。",
                "Categories are still saving. Wait for completion.",
            )
            .into());
        }
        state.next_id = state
            .next_id
            .checked_add(1)
            .ok_or_else(|| "Category write ID exhausted.".to_string())?;
        let id = state.next_id;
        state.pending_owners.insert(owner);
        let (reply, receiver) = tokio::sync::oneshot::channel();
        state.pending.push_back(FilterTabWrite {
            owner,
            id,
            writer,
            draft,
            reply,
        });
        let start = !state.running;
        state.running = true;
        Ok((id, receiver, start))
    }

    #[cfg(test)]
    fn submit(
        self: &Arc<Self>,
        owner: u64,
        writer: FilterTabWriter,
        draft: FilterTabDraft,
    ) -> Result<(u64, FilterTabWriteResult), String> {
        self.submit_recovery(owner, None, writer, draft)
    }

    fn submit_recovery(
        self: &Arc<Self>,
        owner: u64,
        expected_failure: Option<u64>,
        writer: FilterTabWriter,
        draft: FilterTabDraft,
    ) -> Result<(u64, FilterTabWriteResult), String> {
        let accepted = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if expected_failure.is_some_and(|id| {
                state
                    .failures
                    .get(&owner)
                    .is_none_or(|failure| failure.snapshot.id != id)
            }) {
                return Err(tr(
                    "失败分类保存已更新，请重新打开最新草稿后再保存。",
                    "The failed category save changed. Reopen its latest draft before saving.",
                )
                .into());
            }
            Self::enqueue_locked(&mut state, owner, writer, draft)?
        };
        Ok(self.start(accepted))
    }

    fn start(
        self: &Arc<Self>,
        (id, receiver, start): (u64, FilterTabWriteResult, bool),
    ) -> (u64, FilterTabWriteResult) {
        if start {
            let registry = self.clone();
            if let Err(error) = std::thread::Builder::new()
                .name("clip-filter-tab-writes".into())
                .spawn(move || registry.run())
            {
                self.reject_start(format!(
                    "{}: {error}",
                    tr(
                        "分类保存线程无法启动",
                        "Category save worker could not start"
                    )
                ));
            }
        }
        (id, receiver)
    }

    fn complete(&self, job: FilterTabWrite, result: Result<(), String>) {
        {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state.pending_owners.remove(&job.owner);
            match &result {
                Ok(()) => {
                    state.failures.remove(&job.owner);
                }
                Err(error) => {
                    state.failures.insert(
                        job.owner,
                        FailedFilterTabWrite {
                            owner: job.owner,
                            snapshot: FilterTabWriteFailure {
                                id: job.id,
                                filter_items: job.draft.filter_items,
                                draft_filter_items: job.draft.draft_filter_items,
                                data_root: job.draft.data_root,
                                error: error.clone(),
                            },
                            writer: job.writer,
                        },
                    );
                }
            }
        }
        // A dropped UI observer cannot cancel or hide the completed outcome.
        let _ = job.reply.send(result);
    }

    fn run(&self) {
        loop {
            let job = {
                let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
                match state.pending.pop_front() {
                    Some(job) => job,
                    None => {
                        state.running = false;
                        let result = state.outcome();
                        for waiter in std::mem::take(&mut state.waiters) {
                            let _ = waiter.send(result.clone());
                        }
                        return;
                    }
                }
            };
            let result = catch_unwind(AssertUnwindSafe(|| (job.writer)(&job.draft.filter_items)))
                .unwrap_or_else(|_| {
                    Err(tr(
                        "分类保存线程失败，请重试或明确放弃该保存。",
                        "Category save worker failed. Retry or explicitly discard this save.",
                    )
                    .into())
                });
            if let Err(error) = &result {
                log::error!("category save failed ({}): {error}", job.owner);
            }
            self.complete(job, result);
        }
    }

    fn reject_start(&self, error: String) {
        // Nothing entered the worker. Preserve every accepted snapshot and
        // settle its observer/barrier instead of leaving running=true forever.
        loop {
            let job = {
                let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
                match state.pending.pop_front() {
                    Some(job) => job,
                    None => {
                        state.running = false;
                        let result = state.outcome();
                        for waiter in std::mem::take(&mut state.waiters) {
                            let _ = waiter.send(result.clone());
                        }
                        return;
                    }
                }
            };
            self.complete(job, Err(error.clone()));
        }
    }

    fn barrier(&self) -> FilterTabWriteResult {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.running || !state.pending_owners.is_empty() {
            state.waiters.push(sender);
        } else {
            let _ = sender.send(state.outcome());
        }
        receiver
    }

    async fn flush(&self) -> Result<(), String> {
        loop {
            let _ = self.barrier().await.map_err(|_| {
                tr(
                    "分类保存屏障不可用。",
                    "Category save barrier is unavailable.",
                )
                .to_string()
            })?;
            let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if !state.running && state.pending_owners.is_empty() {
                return state.outcome();
            }
        }
    }

    fn failures(&self) -> Vec<FilterTabWriteFailure> {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .failures
            .values()
            .map(|failure| failure.snapshot.clone())
            .collect()
    }

    fn failure(&self, id: u64) -> Result<FailedFilterTabWrite, String> {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let failure = state
            .failures
            .values()
            .find(|failure| failure.snapshot.id == id)
            .cloned()
            .ok_or_else(|| {
                tr(
                    "没有该失败分类保存。",
                    "No failed category save with this ID.",
                )
                .to_string()
            })?;
        if state.pending_owners.contains(&failure.owner) {
            return Err(tr(
                "分类仍在保存，请完成后再恢复草稿。",
                "Categories are still saving. Wait before restoring the draft.",
            )
            .into());
        }
        Ok(failure)
    }

    fn retry(self: &Arc<Self>, id: u64) -> Result<FilterTabWriteResult, String> {
        let accepted = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            let failure = state
                .failures
                .values()
                .find(|failure| failure.snapshot.id == id)
                .cloned()
                .ok_or_else(|| {
                    tr(
                        "没有该失败分类保存。",
                        "No failed category save with this ID.",
                    )
                    .to_string()
                })?;
            let draft = FilterTabDraft {
                filter_items: failure.snapshot.filter_items,
                draft_filter_items: failure.snapshot.draft_filter_items,
                data_root: failure.snapshot.data_root,
            };
            // Lookup and acceptance share a lock: a stale recovery operation
            // cannot borrow a later failure's ID or overwrite a newer save.
            Self::enqueue_locked(&mut state, failure.owner, failure.writer, draft)?
        };
        Ok(self.start(accepted).1)
    }

    fn discard(&self, id: u64) -> Result<FilterTabWriteFailure, String> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let owner = state
            .failures
            .values()
            .find(|failure| failure.snapshot.id == id)
            .map(|failure| failure.owner)
            .ok_or_else(|| {
                tr(
                    "没有该失败分类保存。",
                    "No failed category save with this ID.",
                )
                .to_string()
            })?;
        if state.pending_owners.contains(&owner) {
            return Err(tr(
                "分类仍在保存，不能放弃已接受的写入。",
                "Categories are still saving; an accepted write cannot be discarded.",
            )
            .into());
        }
        state
            .failures
            .remove(&owner)
            .map(|failure| failure.snapshot)
            .ok_or_else(|| {
                tr(
                    "没有该失败分类保存。",
                    "No failed category save with this ID.",
                )
                .into()
            })
    }

    fn discard_owner(&self, owner: u64, expected_failure: Option<u64>) -> Result<(), String> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.pending_owners.contains(&owner) {
            return Err("Category save is still pending.".into());
        }
        if state
            .failures
            .get(&owner)
            .is_some_and(|failure| Some(failure.snapshot.id) != expected_failure)
        {
            return Err(tr(
                "失败分类保存已更新，未放弃较新的草稿。",
                "The failed category save changed. Its newer draft was not discarded.",
            )
            .into());
        }
        state.failures.remove(&owner);
        Ok(())
    }
}

static FILTER_TAB_WRITES: OnceLock<Arc<FilterTabWrites>> = OnceLock::new();
static FILTER_TAB_OWNER_ID: AtomicU64 = AtomicU64::new(1);

fn filter_tab_writes() -> &'static Arc<FilterTabWrites> {
    FILTER_TAB_WRITES.get_or_init(Default::default)
}

/// Set after Root's IME preflight, before awaiting exit barriers. Acceptance
/// checks this flag under the SAME registry lock, including stale UI callbacks
/// and explicit recovery retries. Unfreezing never replays rejected drafts.
pub fn set_filter_tab_exit_pending(pending: bool) {
    filter_tab_writes().set_exit_pending(pending);
}

/// Root first checks each live editor's is_composing(), then locks new Save
/// intents in the same UI turn. This awaits only accepted writes, including
/// destroyed owners; it never automatically saves an unsubmitted draft.
pub async fn flush_filter_tab_writes() -> Result<(), String> {
    filter_tab_writes().flush().await
}

pub fn failed_filter_tab_writes() -> Vec<FilterTabWriteFailure> {
    filter_tab_writes().failures()
}

/// Explicit recovery of a destroyed editor's latest accepted snapshot. For a
/// live editor use its Save button so its own feedback also updates normally.
pub fn retry_filter_tab_write(id: u64) -> Result<FilterTabWriteResult, String> {
    filter_tab_writes().retry(id)
}

/// Explicitly discard a failed save, returning its original recovery snapshot.
/// Ordinary editor destruction/hide/Exit never acknowledges failure implicitly.
pub fn discard_failed_filter_tab_write(id: u64) -> Result<FilterTabWriteFailure, String> {
    filter_tab_writes().discard(id)
}

/// Allocate one stable owner for a settings form's explicit category rollback.
/// Dropping its form/observer never removes accepted jobs or recovery snapshots.
pub fn new_filter_tab_write_owner() -> u64 {
    FILTER_TAB_OWNER_ID.fetch_add(1, Ordering::Relaxed)
}

/// Accept an explicit replacement through the SAME FIFO as category Save.
/// `expected_failure` is the previous accepted ID when retrying this owner.
/// Every accepted replacement, including a retry, returns a NEW transaction ID;
/// retain that ID on failure and clear it only after a successful receipt.
pub fn submit_filter_tab_replacement(
    owner: u64,
    expected_failure: Option<u64>,
    host: Arc<HostState>,
    filter_items: String,
) -> Result<(u64, FilterTabWriteResult), String> {
    let registry = filter_tab_writes();
    let current_root = host.data_root().to_string_lossy().into_owned();
    let (writer, data_root): (FilterTabWriter, String) = match expected_failure {
        Some(id) => {
            let failure = registry.failure(id)?;
            if failure.owner != owner {
                return Err(tr(
                    "失败分类保存属于另一设置会话，未提交替换。",
                    "The failed category save belongs to another settings session. Replacement was not accepted.",
                ).into());
            }
            if failure.snapshot.data_root != current_root {
                return Err(tr(
                    "失败分类保存属于另一数据目录，未提交替换。",
                    "The failed category save belongs to another data root. Replacement was not accepted.",
                ).into());
            }
            // Do not redirect a failed transaction to a replacement HostState.
            // submit_recovery rechecks this exact ID under the acceptance lock.
            (failure.writer, failure.snapshot.data_root)
        }
        None => {
            let writer: FilterTabWriter = Arc::new(move |value| {
                host.settings_form_update(
                    &json!({ "configId": "clip", "values": { "FilterItems": value } }),
                )
                .and_then(|form| crate::settings::form_save_error(&form).map_or(Ok(()), Err))
            });
            (writer, current_root)
        }
    };
    let draft = FilterTabDraft {
        filter_items: filter_items.clone(),
        draft_filter_items: filter_items,
        data_root,
    };
    registry.submit_recovery(owner, expected_failure, writer, draft)
}

pub enum FilterTabsEvent {
    Saved,
    Cancelled,
}

use crate::settings::settings_choice::{
    SettingsChoiceDismissal, SettingsChoiceEvent, SettingsChoiceOption, SettingsChoicePopup,
};

type EditorAnchor = Arc<Mutex<Option<gpui::Bounds<gpui::Pixels>>>>;

#[derive(Clone)]
struct EditorRow {
    id: String,
    name: Entity<SearchInput>,
    type_key: &'static str,
    item_type: i32,
    type_anchor: EditorAnchor,
    more_anchor: EditorAnchor,
    type_focus: gpui::FocusHandle,
    more_focus: gpui::FocusHandle,
    drag_focus: gpui::FocusHandle,
}

impl EditorRow {
    fn key(&self) -> u64 {
        self.name.entity_id().as_u64()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EditorPopupKind {
    Type,
    More,
    Preview,
}

struct EditorPopup {
    row: u64,
    kind: EditorPopupKind,
    popup: Entity<SettingsChoicePopup>,
    _subscription: Subscription,
}

enum EditorUndo {
    Removed {
        index: usize,
        row: EditorRow,
    },
    Defaults {
        rows: Vec<EditorRow>,
        subscriptions: Vec<Subscription>,
        selected: Option<u64>,
    },
}

#[derive(Clone)]
struct CategoryRowDrag {
    owner: u64,
    row: u64,
    label: String,
    position: gpui::Point<gpui::Pixels>,
    background: gpui::Hsla,
    foreground: gpui::Hsla,
}

impl Render for CategoryRowDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        rsx! {
            <div pl={self.position.x} pt={self.position.y}>
                <div px={px(10.0)} py={px(6.0)} rounded={px(6.0)} bg={self.background}
                    text_color={self.foreground} text_size={px(12.0)}>
                    {self.label.clone()}
                </div>
            </div>
        }
    }
}

/// Web `SettingsFilterTabsEditor`: rename, retype, reorder, add, remove and
/// restore default tabs; saved through the clip settings form `FilterItems`.
pub struct FilterTabsEditor {
    host: Arc<HostState>,
    tokens: DesignTokens,
    dark: bool,
    rows: Vec<EditorRow>,
    popup: Option<EditorPopup>,
    undo: Option<EditorUndo>,
    drop_target: Option<(u64, bool)>,
    preview_selected: Option<u64>,
    preview_anchor: EditorAnchor,
    preview_focus: gpui::FocusHandle,
    body_scroll: gpui::ScrollHandle,
    width: f32,
    baseline: String,
    error: Option<String>,
    save_failed: Option<String>,
    saving: bool,
    leave_locked: bool,
    write_owner: u64,
    recovery_writer: Option<FilterTabWriter>,
    recovery_id: Option<u64>,
    language: LanguageMode,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<FilterTabsEvent> for FilterTabsEditor {}

impl FilterTabsEditor {
    pub fn new(
        host: Arc<HostState>,
        tokens: DesignTokens,
        dark: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut editor = Self {
            host,
            tokens,
            dark,
            rows: Vec::new(),
            popup: None,
            undo: None,
            drop_target: None,
            preview_selected: None,
            preview_anchor: Arc::new(Mutex::new(None)),
            preview_focus: cx.focus_handle().tab_stop(true),
            body_scroll: gpui::ScrollHandle::new(),
            width: 352.0,
            baseline: String::new(),
            error: None,
            save_failed: None,
            saving: false,
            leave_locked: false,
            write_owner: FILTER_TAB_OWNER_ID.fetch_add(1, Ordering::Relaxed),
            recovery_writer: None,
            recovery_id: None,
            language: displayed_language(),
            _subscriptions: Vec::new(),
        };
        editor.reload(window, cx);
        editor
    }

    pub fn set_theme(&mut self, dark: bool, cx: &mut Context<Self>) {
        self.refresh_control_theme(dark, cx);
    }

    /// Update custom colors without replacing category drafts or their inputs.
    pub fn set_design_tokens(&mut self, tokens: DesignTokens, dark: bool, cx: &mut Context<Self>) {
        self.tokens = tokens;
        self.refresh_control_theme(dark, cx);
    }

    fn refresh_control_theme(&mut self, dark: bool, cx: &mut Context<Self>) {
        self.dark = dark;
        let t = self.tokens.theme(dark);
        let palette = SettingsPalette::new(t);
        let colors = InputColors {
            background: gpui::transparent_black(),
            foreground: color(&t.text),
            placeholder: color(&t.text_muted),
            border: gpui::transparent_black(),
            selection: color(&t.tab_active),
            caret: color(&t.accent),
        };
        for row in &self.rows {
            row.name
                .update(cx, |input, cx| input.set_colors(colors, cx));
        }
        if let Some(session) = &self.popup {
            session
                .popup
                .update(cx, |popup, cx| popup.update_colors(t, palette, cx));
        }
        cx.notify();
    }

    pub fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.leave_locked || self.saving || self.save_failed.is_some() {
            return;
        }
        let filters = self.host.filters_get();
        let tabs: Vec<Value> = filters.as_array().cloned().unwrap_or_default();
        self.set_rows(tabs, window, cx);
    }

    fn set_rows(&mut self, tabs: Vec<Value>, window: &mut Window, cx: &mut Context<Self>) {
        if self.leave_locked || self.saving {
            return;
        }
        self.error = None;
        self.save_failed = None;
        self._subscriptions.clear();
        self.popup = None;
        self.undo = None;
        self.drop_target = None;
        self.rows = tabs
            .into_iter()
            .filter(|tab| tab["starType"].as_i64().unwrap_or(0) == 0)
            .map(|tab| {
                let option = type_option(
                    tab["type"].as_str().unwrap_or("all"),
                    tab["itemType"].as_i64().unwrap_or(1) as i32,
                );
                EditorRow {
                    id: tab["id"].as_str().unwrap_or_default().to_string(),
                    name: self.make_input(
                        tab["displayName"].as_str().unwrap_or_default().to_string(),
                        option.0,
                        option.1,
                        window,
                        cx,
                    ),
                    type_key: option.0,
                    item_type: option.1,
                    type_anchor: Arc::new(Mutex::new(None)),
                    more_anchor: Arc::new(Mutex::new(None)),
                    type_focus: cx.focus_handle().tab_stop(true),
                    more_focus: cx.focus_handle().tab_stop(true),
                    drag_focus: cx.focus_handle().tab_stop(true),
                }
            })
            .collect();
        self.preview_selected = self.rows.first().map(EditorRow::key);
        self.baseline = self.serialized(cx, false);
        cx.notify();
    }

    fn make_input(
        &mut self,
        text: String,
        type_key: &str,
        item_type: i32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<SearchInput> {
        let t = self.tokens.theme(self.dark).clone();
        let height = 30.0;
        let placeholder = automatic_tab_name(type_key, item_type, self.language);
        let input = cx.new(|cx| {
            let mut input = SearchInput::new(window, cx);
            input.set_colors(
                InputColors {
                    background: gpui::transparent_black(),
                    foreground: color(&t.text),
                    placeholder: color(&t.text_muted),
                    border: gpui::transparent_black(),
                    selection: color(&t.tab_active),
                    caret: color(&t.accent),
                },
                cx,
            );
            input.set_metrics(height, 12.0, 4.0, cx);
            input.set_placeholder(placeholder, cx);
            input.set_double_click_clears(false);
            input.set_text(text, window, cx);
            input
        });
        self._subscriptions.push(cx.subscribe_in(
            &input,
            window,
            |this, _, event, _, cx| match event {
                SearchEvent::Submit => this.save(cx),
                SearchEvent::Changed(_) => {
                    this.undo = None;
                    cx.notify();
                }
                SearchEvent::Escape => {}
            },
        ));
        input
    }

    pub fn input_focus_handles(&self, cx: &gpui::App) -> Vec<gpui::FocusHandle> {
        self.rows
            .iter()
            .flat_map(|row| {
                [
                    row.drag_focus.clone(),
                    row.name.read(cx).focus_handle(),
                    row.type_focus.clone(),
                    row.more_focus.clone(),
                ]
            })
            .collect()
    }

    pub fn projection_busy(&self, cx: &gpui::App) -> bool {
        self.popup.is_some() || cx.has_active_drag() || self.is_composing(cx)
    }

    /// Ordinary unsubmitted category drafts remain explicit-save drafts.
    /// A selected Save or provisional IME input must finish before the owner leaves.
    pub fn has_active_save_or_composition(&self, cx: &gpui::App) -> bool {
        self.is_saving() || self.is_composing(cx)
    }

    pub fn is_composing(&self, cx: &gpui::App) -> bool {
        self.rows.iter().any(|row| row.name.read(cx).is_composing())
    }

    pub fn is_saving(&self) -> bool {
        self.saving
    }

    /// Root can mount a fresh editor and explicitly recover a destroyed
    /// owner's draft. No write occurs here, and exact draft names are restored.
    /// The original writer/target is retained; a different data root is rejected.
    pub fn restore_failed_write(
        &mut self,
        id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if self.leave_locked || self.has_active_save_or_composition(cx) {
            return Err(tr(
                "请先完成当前输入或保存，再恢复分类草稿。",
                "Finish the current input or save before restoring a category draft.",
            )
            .into());
        }
        let failure = filter_tab_writes().failure(id)?;
        if failure.snapshot.data_root != self.host.data_root().to_string_lossy() {
            return Err(tr("失败分类保存属于另一数据目录，未覆盖当前草稿。", "The failed category save belongs to another data root. The current draft was not replaced.").into());
        }
        let tabs: Vec<Value> = serde_json::from_str(&failure.snapshot.draft_filter_items)
            .map_err(|error| error.to_string())?;
        self.set_rows(tabs, window, cx);
        self.write_owner = failure.owner;
        self.recovery_writer = Some(failure.writer);
        self.recovery_id = Some(failure.snapshot.id);
        self.save_failed = Some(failure.snapshot.error.clone());
        self.error = Some(failure.snapshot.error);
        cx.notify();
        Ok(())
    }

    fn discard_failed_draft(&mut self, cx: &mut Context<Self>) -> bool {
        if self.leave_locked || self.saving {
            return false;
        }
        if let Err(error) = filter_tab_writes().discard_owner(self.write_owner, self.recovery_id) {
            self.error = Some(error);
            cx.notify();
            return false;
        }
        self.save_failed = None;
        self.error = None;
        self.recovery_writer = None;
        self.recovery_id = None;
        true
    }

    /// A selected save failed: retain this draft until a successful retry or
    /// an explicit Cancel/reset, without implicitly submitting ordinary drafts.
    pub fn save_failure(&self) -> Option<&str> {
        self.save_failed.as_deref()
    }

    pub fn set_leave_locked(&mut self, locked: bool, cx: &mut Context<Self>) {
        self.leave_locked = locked;
        if locked {
            self.popup = None;
            self.drop_target = None;
        }
        for row in &self.rows {
            row.name.update(cx, |input, cx| {
                input.set_read_only(locked || self.saving, cx)
            });
        }
        cx.notify();
    }

    fn set_type(
        &mut self,
        index: usize,
        option: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.can_edit(cx) {
            return;
        }
        let (type_key, item_type, _, _) = TYPE_OPTIONS[option];
        let Some(row) = self.rows.get_mut(index) else {
            return;
        };
        row.type_key = type_key;
        row.item_type = item_type;
        // Only the placeholder follows the type; every non-empty name is custom.
        let placeholder = automatic_tab_name(type_key, item_type, self.language);
        row.name
            .update(cx, |input, cx| input.set_placeholder(placeholder, cx));
        self.undo = None;
        self.popup = None;
        cx.notify();
    }

    fn serialized(&self, cx: &gpui::App, normalize_names: bool) -> String {
        let rows: Vec<Value> = self
            .rows
            .iter()
            .map(|row| {
                let raw_name = row.name.read(cx).text();
                let name = if normalize_names {
                    raw_name.trim()
                } else {
                    raw_name
                };
                json!({
                    "id": row.id,
                    "displayName": name,
                    "type": row.type_key,
                    "itemType": row.item_type,
                    "starType": 0,
                })
            })
            .collect();
        serde_json::to_string(&rows).unwrap_or_else(|_| "[]".into())
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        if self.saving || self.leave_locked {
            return;
        }
        if self.has_active_save_or_composition(cx) {
            self.error = Some(
                tr(
                    "请先完成输入法组合，再保存分类。",
                    "Finish composing text before saving categories.",
                )
                .into(),
            );
            cx.notify();
            return;
        }
        let host = self.host.clone();
        let writer: FilterTabWriter = self.recovery_writer.clone().unwrap_or_else(|| {
            Arc::new(move |value| {
                host.settings_form_update(
                    &json!({ "configId": "clip", "values": { "FilterItems": value } }),
                )
                .and_then(|form| crate::settings::form_save_error(&form).map_or(Ok(()), Err))
            })
        });
        let draft = FilterTabDraft {
            filter_items: self.serialized(cx, true),
            draft_filter_items: self.serialized(cx, false),
            data_root: self.host.data_root().to_string_lossy().into_owned(),
        };
        // Accept and dispatch independently BEFORE spawning the UI observer.
        let (write_id, accepted) = match filter_tab_writes().submit_recovery(
            self.write_owner,
            self.recovery_id,
            writer,
            draft,
        ) {
            Ok(accepted) => accepted,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        self.saving = true;
        self.popup = None;
        self.drop_target = None;
        for row in &self.rows {
            row.name
                .update(cx, |input, cx| input.set_read_only(true, cx));
        }
        self.error = None;
        self.save_failed = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = accepted.await.unwrap_or_else(|_| {
                Err(tr(
                    "分类保存线程不可用。",
                    "Category save worker is unavailable.",
                )
                .into())
            });
            let _ = this.update(cx, |this, cx| {
                this.saving = false;
                for row in &this.rows {
                    row.name
                        .update(cx, |input, cx| input.set_read_only(this.leave_locked, cx));
                }
                match result {
                    Ok(_) => {
                        this.baseline = this.serialized(cx, false);
                        this.undo = None;
                        this.recovery_id = None;
                        cx.emit(FilterTabsEvent::Saved);
                    }
                    Err(error) => {
                        this.recovery_id = Some(write_id);
                        this.save_failed = Some(error.clone());
                        this.error = Some(error);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Content height of the popover card: header, rows, add row, notice and footer.
    pub fn preferred_height(&self) -> f32 {
        let header = 88.0;
        let body = 26.0 + self.rows.len() as f32 * 42.0 + 38.0 + 8.0;
        let notice = if self.undo.is_some() { 33.0 } else { 0.0 };
        let error = if self.error.is_some() { 30.0 } else { 0.0 };
        let footer = 47.0;
        header + body + notice + error + footer + 2.0
    }

    pub fn body_scroll(&self) -> gpui::ScrollHandle {
        self.body_scroll.clone()
    }

    #[cfg(debug_assertions)]
    pub fn diagnostics(&self, cx: &gpui::App) -> Value {
        json!({"rows":self.rows.len(),"dirty":self.serialized(cx, false) != self.baseline,
            "undo":self.undo.as_ref().map(|undo| match undo {
                EditorUndo::Removed { .. } => "removed",
                EditorUndo::Defaults { .. } => "defaults",
            }),"saving":self.saving,"error":self.error})
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        if self.can_edit(cx) && self.discard_failed_draft(cx) {
            self.popup = None;
            cx.emit(FilterTabsEvent::Cancelled);
        }
    }

    fn add_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_edit(cx) {
            return;
        }
        self.popup = None;
        self.undo = None;
        let name = self.make_input(String::new(), "all", clip_filter::ALL, window, cx);
        let row = EditorRow {
            id: clip_filter::new_tab("", "all", clip_filter::ALL).id,
            name,
            type_key: "all",
            item_type: clip_filter::ALL,
            type_anchor: Arc::new(Mutex::new(None)),
            more_anchor: Arc::new(Mutex::new(None)),
            type_focus: cx.focus_handle().tab_stop(true),
            more_focus: cx.focus_handle().tab_stop(true),
            drag_focus: cx.focus_handle().tab_stop(true),
        };
        window.focus(&row.type_focus);
        self.rows.push(row);
        // Reveal the new row once laid out; the footer no longer scrolls.
        self.body_scroll.scroll_to_bottom();
        cx.notify();
    }

    fn undo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_edit(cx) {
            return;
        }
        let Some(undo) = self.undo.take() else {
            return;
        };
        match undo {
            EditorUndo::Removed { index, row } => {
                window.focus(&row.more_focus);
                self.rows.insert(index.min(self.rows.len()), row);
            }
            EditorUndo::Defaults {
                rows,
                subscriptions,
                selected,
            } => {
                self.rows = rows;
                self._subscriptions = subscriptions;
                self.preview_selected = selected;
            }
        }
        for row in &self.rows {
            let placeholder = automatic_tab_name(row.type_key, row.item_type, self.language);
            row.name
                .update(cx, |input, cx| input.set_placeholder(placeholder, cx));
        }
        self.refresh_control_theme(self.dark, cx);
        self.popup = None;
        cx.notify();
    }

    /// Replace the draft with factory tabs; undoable until the next edit.
    pub fn restore_defaults(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.can_edit(cx) || !self.discard_failed_draft(cx) {
            return false;
        }
        let baseline = self.baseline.clone();
        let rows = std::mem::take(&mut self.rows);
        let subscriptions = std::mem::take(&mut self._subscriptions);
        let selected = self.preview_selected;
        let defaults = serde_json::to_value(clip_filter::create_default_tabs())
            .ok()
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default();
        self.set_rows(defaults, window, cx);
        self.baseline = baseline;
        self.undo = Some(EditorUndo::Defaults {
            rows,
            subscriptions,
            selected,
        });
        true
    }

    fn can_edit(&self, cx: &gpui::App) -> bool {
        !self.leave_locked && !self.saving && !self.is_composing(cx)
    }

    fn row_label(&self, row: &EditorRow, cx: &gpui::App) -> String {
        let name = row.name.read(cx).text();
        if name.trim().is_empty() {
            automatic_tab_name(row.type_key, row.item_type, self.language)
        } else {
            name.to_string()
        }
    }

    fn move_row(&mut self, key: u64, destination: usize, cx: &mut Context<Self>) {
        if !self.can_edit(cx) || destination >= self.rows.len() {
            return;
        }
        let Some(index) = self.rows.iter().position(|row| row.key() == key) else {
            return;
        };
        if index != destination {
            let row = self.rows.remove(index);
            self.rows.insert(destination, row);
            self.undo = None;
        }
        self.popup = None;
        self.drop_target = None;
        cx.notify();
    }

    fn remove_row(&mut self, key: u64, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_edit(cx) || self.rows.len() <= 1 {
            return;
        }
        let Some(index) = self.rows.iter().position(|row| row.key() == key) else {
            return;
        };
        let row = self.rows.remove(index);
        self.undo = Some(EditorUndo::Removed { index, row });
        self.popup = None;
        self.drop_target = None;
        let next = &self.rows[index.min(self.rows.len() - 1)];
        if self.preview_selected == Some(key) {
            self.preview_selected = Some(next.key());
        }
        window.focus(&next.more_focus);
        cx.notify();
    }

    fn preview_visibility(&self, window: &Window, cx: &gpui::App) -> Vec<bool> {
        let first_all = self.rows.iter().position(|row| row.type_key == "all");
        let widths = self
            .rows
            .iter()
            .enumerate()
            .map(|(index, row)| {
                if Some(index) == first_all {
                    30.0
                } else {
                    crate::view::label_width(
                        &self.row_label(row, cx),
                        12.0,
                        &self.tokens.typography.font_family,
                        window,
                    ) + 20.0
                }
            })
            .collect::<Vec<_>>();
        let available = (self.width - PREVIEW_INSET).max(48.0);
        if widths.iter().sum::<f32>() + self.rows.len().saturating_sub(1) as f32 * 3.0 <= available
        {
            return vec![true; self.rows.len()];
        }
        let required = self
            .rows
            .iter()
            .map(|row| Some(row.key()) == self.preview_selected)
            .collect::<Vec<_>>();
        crate::view::visible_strip_entries(&widths, &required, available - 33.0, 3.0)
    }

    fn toggle_popup(
        &mut self,
        key: u64,
        kind: EditorPopupKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.can_edit(cx) {
            return;
        }
        if self
            .popup
            .as_ref()
            .is_some_and(|popup| popup.row == key && popup.kind == kind)
        {
            self.popup = None;
            cx.notify();
            return;
        }
        let row_index = self.rows.iter().position(|row| row.key() == key);
        let (anchor, focus, selected, options) = if kind == EditorPopupKind::Preview {
            let visible = self.preview_visibility(window, cx);
            let options = self
                .rows
                .iter()
                .enumerate()
                .filter(|(index, _)| !visible[*index])
                .map(|(_, row)| SettingsChoiceOption {
                    key: row.key().to_string(),
                    name: self.row_label(row, cx),
                    des: String::new(),
                })
                .collect::<Vec<_>>();
            (
                self.preview_anchor.clone(),
                self.preview_focus.clone(),
                String::new(),
                options,
            )
        } else {
            let Some(index) = row_index else {
                return;
            };
            let row = &self.rows[index];
            if kind == EditorPopupKind::Type {
                let options = TYPE_OPTIONS
                    .iter()
                    .map(|(key, item_type, _, _)| {
                        let des = match *key {
                            "text" => tr("包含纯文本与富文本", "Includes plain and rich text"),
                            "plaintext" => tr("仅显示普通文本条目", "Only plain text items"),
                            "richtext" => tr(
                                "带格式的文本（HTML、QQ 富文本）",
                                "Formatted text (HTML and QQ rich text)",
                            ),
                            _ => "",
                        };
                        SettingsChoiceOption {
                            key: (*key).into(),
                            name: type_label(key, *item_type).into(),
                            des: des.into(),
                        }
                    })
                    .collect();
                (
                    row.type_anchor.clone(),
                    row.type_focus.clone(),
                    row.type_key.to_string(),
                    options,
                )
            } else {
                let mut options = Vec::new();
                if index > 0 {
                    options.push(SettingsChoiceOption {
                        key: "up".into(),
                        name: tr("上移", "Move up").into(),
                        des: String::new(),
                    });
                }
                if index + 1 < self.rows.len() {
                    options.push(SettingsChoiceOption {
                        key: "down".into(),
                        name: tr("下移", "Move down").into(),
                        des: String::new(),
                    });
                }
                if self.rows.len() > 1 {
                    options.push(SettingsChoiceOption {
                        key: "remove".into(),
                        name: tr("移除分类", "Remove category").into(),
                        des: tr("保留剪贴板内容", "Clipboard items are kept").into(),
                    });
                }
                (
                    row.more_anchor.clone(),
                    row.more_focus.clone(),
                    String::new(),
                    options,
                )
            }
        };
        if options.is_empty() {
            return;
        }
        let Some(bounds) = *anchor.lock().unwrap() else {
            return;
        };
        let t = self.tokens.theme(self.dark).clone();
        let popup = cx.new(|cx| {
            SettingsChoicePopup::new(
                format!("category-{key}"),
                selected,
                options,
                &t,
                SettingsPalette::new(&t),
                bounds,
                cx,
            )
        });
        let subscription =
            cx.subscribe_in(&popup, window, move |this, popup, event, window, cx| {
                if this.leave_locked
                    || this.saving
                    || !this
                        .popup
                        .as_ref()
                        .is_some_and(|session| session.popup.entity_id() == popup.entity_id())
                {
                    return;
                }
                this.popup = None;
                if !matches!(
                    event,
                    SettingsChoiceEvent::Dismissed(SettingsChoiceDismissal::Outside)
                ) {
                    window.focus(&focus);
                }
                match event {
                    SettingsChoiceEvent::Chosen { value, .. } => {
                        if kind == EditorPopupKind::Preview {
                            if let Ok(row) = value.parse::<u64>() {
                                if this.rows.iter().any(|item| item.key() == row) {
                                    this.preview_selected = Some(row);
                                }
                            }
                        } else if let Some(index) =
                            this.rows.iter().position(|row| row.key() == key)
                        {
                            if kind == EditorPopupKind::Type {
                                if let Some(option) =
                                    TYPE_OPTIONS.iter().position(|option| option.0 == value)
                                {
                                    this.set_type(index, option, window, cx);
                                }
                            } else {
                                match value.as_str() {
                                    "up" if index > 0 => this.move_row(key, index - 1, cx),
                                    "down" if index + 1 < this.rows.len() => {
                                        this.move_row(key, index + 1, cx)
                                    }
                                    "remove" => this.remove_row(key, window, cx),
                                    _ => {}
                                }
                            }
                        }
                    }
                    SettingsChoiceEvent::Dismissed(SettingsChoiceDismissal::Tab { reverse }) => {
                        let handles = this.input_focus_handles(cx);
                        let next =
                            handles
                                .iter()
                                .position(|handle| *handle == focus)
                                .and_then(|index| {
                                    if *reverse {
                                        index.checked_sub(1)
                                    } else {
                                        Some(index + 1).filter(|next| *next < handles.len())
                                    }
                                });
                        if let Some(next) = next {
                            window.focus(&handles[next]);
                        } else if *reverse {
                            window.focus_prev();
                        } else {
                            window.focus_next();
                        }
                    }
                    SettingsChoiceEvent::Dismissed(_) => {}
                }
                cx.notify();
            });
        self.popup = Some(EditorPopup {
            row: key,
            kind,
            popup: popup.clone(),
            _subscription: subscription,
        });
        popup.update(cx, |popup, cx| popup.focus(window, cx));
        cx.notify();
    }

    fn anchor_element(&self, anchor: EditorAnchor, key: u64, kind: EditorPopupKind) -> AnyElement {
        let popup = self
            .popup
            .as_ref()
            .filter(|session| session.row == key && session.kind == kind)
            .map(|session| session.popup.clone());
        gpui::canvas(
            move |bounds, _, cx| {
                *anchor.lock().unwrap() = Some(bounds);
                if let Some(popup) = &popup {
                    popup.update(cx, |popup, cx| popup.set_anchor(bounds, cx));
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full()
        .into_any_element()
    }

    fn icon(&self, key: &str, size: f32, tint: gpui::Hsla) -> AnyElement {
        crate::crisp_svg::svg()
            .path(self.tokens.icons.get(key).cloned().unwrap_or_default())
            .size(px(size))
            .flex_none()
            .text_color(tint)
            .into_any_element()
    }

    fn button(
        &self,
        id: SharedString,
        label: &'static str,
        enabled: bool,
        t: &ThemeTokens,
    ) -> gpui::Stateful<gpui::Div> {
        let palette = SettingsPalette::new(t);
        rsx! {
            <div id={id} h={px(30.0)} px={px(8.0)} flex items_center flex_none rounded={px(6.0)}
                text_size={px(12.0)} text_color={color(&t.text_secondary)} />
        }
            .when(enabled, |b| {
                b.cursor_pointer()
                    .hover(move |s| s.bg(palette.control_hover))
            })
            .when(!enabled, |b| b.opacity(0.4))
            .child(label)
    }

    fn row(
        &self,
        index: usize,
        row: &EditorRow,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = self.tokens.theme(self.dark);
        let palette = SettingsPalette::new(t);
        let key = row.key();
        let editable = self.can_edit(cx);
        let focused = row.name.read(cx).focus_handle().is_focused(window);
        let type_width = if self.width < 430.0 { 104.0 } else { 136.0 };
        let transparent = gpui::transparent_black();
        let row_drag = CategoryRowDrag {
            owner: cx.entity_id().as_u64(),
            row: key,
            label: self.row_label(row, cx),
            position: gpui::point(px(0.0), px(0.0)),
            background: palette.card,
            foreground: color(&t.text),
        };
        let grip = self
            .button(
                SharedString::from(format!("tab-drag-{key}")),
                "⠿",
                editable,
                t,
            )
            .w(px(26.0))
            .px(px(0.0))
            .justify_center()
            .track_focus(&row.drag_focus)
            .focus(move |style| style.bg(palette.control_hover))
            .tooltip(crate::settings::settings_help(
                tr(
                    "拖动排序，或按 Alt + ↑ / ↓",
                    "Drag to reorder, or press Alt + Up / Down",
                )
                .into(),
                t.clone(),
            ))
            .when(editable, |grip| {
                grip.cursor(gpui::CursorStyle::OpenHand).on_drag(
                    row_drag,
                    |drag, position, _, cx| {
                        let mut preview = drag.clone();
                        preview.position = position;
                        cx.new(|_| preview)
                    },
                )
            });
        let type_button = self
            .button(
                SharedString::from(format!("tab-type-{key}")),
                "",
                editable,
                t,
            )
            .relative()
            .w(px(type_width))
            .min_w_0()
            .justify_between()
            .gap(px(4.0))
            .track_focus(&row.type_focus)
            .focus(move |style| style.bg(palette.control_hover))
            .child(
                rsx! {
                    <div min_w_0 flex_1 whitespace_nowrap text_ellipsis>
                        {type_label(row.type_key, row.item_type)}
                    </div>
                },
            )
            .child(self.icon("collapse", 10.0, color(&t.text_muted)))
            .child(self.anchor_element(row.type_anchor.clone(), key, EditorPopupKind::Type))
            .on_mouse_down(gpui::MouseButton::Left, |_, window, _| {
                window.prevent_default()
            })
            .when(editable, |button| {
                button
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.toggle_popup(key, EditorPopupKind::Type, window, cx)
                    }))
                    .on_key_down(cx.listener(
                        move |this, event: &gpui::KeyDownEvent, window, cx| {
                            if matches!(
                                event.keystroke.key.as_str(),
                                "enter" | "space" | " " | "down"
                            ) && !event.is_held
                            {
                                window.prevent_default();
                                cx.stop_propagation();
                                this.toggle_popup(key, EditorPopupKind::Type, window, cx);
                            }
                        },
                    ))
            });
        let more = self
            .button(
                SharedString::from(format!("tab-more-{key}")),
                "",
                editable && self.rows.len() > 1,
                t,
            )
            .relative()
            .w(px(28.0))
            .px(px(0.0))
            .justify_center()
            .track_focus(&row.more_focus)
            .focus(move |style| style.bg(palette.control_hover))
            .child(self.icon("more", 15.0, color(&t.text_muted)))
            .tooltip(crate::settings::settings_help(
                tr("更多操作", "More actions").into(),
                t.clone(),
            ))
            .child(self.anchor_element(row.more_anchor.clone(), key, EditorPopupKind::More))
            .on_mouse_down(gpui::MouseButton::Left, |_, window, _| {
                window.prevent_default()
            })
            .when(editable && self.rows.len() > 1, |button| {
                button
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.toggle_popup(key, EditorPopupKind::More, window, cx)
                    }))
                    .on_key_down(cx.listener(
                        move |this, event: &gpui::KeyDownEvent, window, cx| {
                            if matches!(
                                event.keystroke.key.as_str(),
                                "enter" | "space" | " " | "down"
                            ) && !event.is_held
                            {
                                window.prevent_default();
                                cx.stop_propagation();
                                this.toggle_popup(key, EditorPopupKind::More, window, cx);
                            }
                        },
                    ))
            });
        rsx! {
            <div id={SharedString::from(format!("tab-row-{key}"))} w_full min_w_0 h={px(42.0)}
                flex_none flex items_center gap={px(3.0)} rounded={px(6.0)} border_b_1 />
        }
            .border_color(if index + 1 < self.rows.len() {
                palette.row_border
            } else {
                transparent
            })
            .when(focused, |row| row.bg(palette.control_hover))
            .hover(move |style| style.bg(palette.control_hover))
            .when(self.drop_target == Some((key, true)), |row| {
                row.border_t_2().border_color(color(&t.accent))
            })
            .when(self.drop_target == Some((key, false)), |row| {
                row.border_b_2().border_color(color(&t.accent))
            })
            .on_key_down(
                cx.listener(move |this, event: &gpui::KeyDownEvent, window, cx| {
                    if !event.keystroke.modifiers.alt || !this.can_edit(cx) {
                        return;
                    }
                    let Some(index) = this.rows.iter().position(|row| row.key() == key) else {
                        return;
                    };
                    let next = match event.keystroke.key.as_str() {
                        "up" if index > 0 => index - 1,
                        "down" if index + 1 < this.rows.len() => index + 1,
                        _ => return,
                    };
                    window.prevent_default();
                    cx.stop_propagation();
                    this.move_row(key, next, cx);
                }),
            )
            .on_drag_move(cx.listener(
                move |this, event: &gpui::DragMoveEvent<CategoryRowDrag>, _, cx| {
                    if !event.bounds.contains(&event.event.position) || !this.can_edit(cx) {
                        return;
                    }
                    let before = event.event.position.y
                        < event.bounds.origin.y + event.bounds.size.height / 2.0;
                    if this.drop_target != Some((key, before)) {
                        this.drop_target = Some((key, before));
                        cx.notify();
                    }
                },
            ))
            .on_drop(cx.listener(move |this, drag: &CategoryRowDrag, _, cx| {
                if drag.owner != cx.entity_id().as_u64() || !this.can_edit(cx) {
                    return;
                }
                let Some(from) = this.rows.iter().position(|row| row.key() == drag.row) else {
                    return;
                };
                let Some(to) = this.rows.iter().position(|row| row.key() == key) else {
                    return;
                };
                let before = this
                    .drop_target
                    .take()
                    .filter(|(target, _)| *target == key)
                    .map_or(true, |(_, before)| before);
                let mut destination = to + usize::from(!before);
                if from < destination {
                    destination -= 1;
                }
                this.move_row(drag.row, destination, cx);
            }))
            .child(grip)
            .child(
                rsx! {
                    <div flex items_center flex_1 min_w_0 gap={px(4.0)} />
                }
                    .when(self.width >= 430.0, |name| {
                        name.child(self.icon(
                            tab_icon(row.type_key, row.item_type),
                            14.0,
                            color(&t.text_muted),
                        ))
                    })
                    .child(
                        rsx! {
                            <div flex items_center flex_1 min_w_0 rounded={px(5.0)} border_1 />
                        }
                            .border_color(if focused {
                                color(&t.accent)
                            } else {
                                transparent
                            })
                            .child(rsx! {
                                <div flex_1 min_w_0>
                                    {row.name.clone()}
                                </div>
                            })
                            ,
                    ),
            )
            .child(type_button)
            .child(more)
            .into_any_element()
    }

    fn preview(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let t = self.tokens.theme(self.dark);
        let palette = SettingsPalette::new(t);
        let visible = self.preview_visibility(window, cx);
        let first_all = self.rows.iter().position(|row| row.type_key == "all");
        let overflow = visible.iter().any(|visible| !visible);
        let max_chip_width = (self.width - PREVIEW_INSET - if overflow { 33.0 } else { 0.0 }).max(32.0);
        let chips = self
            .rows
            .iter()
            .enumerate()
            .filter(|(index, _)| visible[*index])
            .map(|(index, row)| {
                let key = row.key();
                let label = self.row_label(row, cx);
                let active = self.preview_selected == Some(key);
                let icon_only = Some(index) == first_all;
                rsx! {
                    <div id={SharedString::from(format!("tab-preview-{key}"))} h={px(30.0)}
                        flex_none max_w={px(max_chip_width)} flex items_center justify_center
                        rounded={px(6.0)} text_size={px(12.0)} text_color={color(&t.text)}
                        cursor_pointer />
                }
                    .when(active, |chip| chip.bg(palette.selected))
                    .when(!active, |chip| {
                        chip.hover(move |style| style.bg(palette.control_hover))
                    })
                    .when(icon_only, |chip| {
                        chip.w(px(30.0))
                            .child(self.icon("all", 15.0, color(&t.text_secondary)))
                    })
                    .when(!icon_only, |chip| {
                        chip.px(px(10.0)).child(
                            rsx! {
                                <div min_w_0 whitespace_nowrap text_ellipsis>
                                    {label.clone()}
                                </div>
                            },
                        )
                    })
                    .tooltip(crate::settings::settings_help(label, t.clone()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.preview_selected = Some(key);
                        this.popup = None;
                        cx.notify();
                    }))
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        rsx! {
            <div w_full min_w_0 h={px(38.0)} flex items_center px={px(5.0)} rounded={px(7.0)}
                bg={palette.control} />
        }
            .child(
                rsx! {
                    <div w_full min_w_0 flex items_center gap={px(3.0)} overflow_hidden />
                }
                    .children(chips)
                    .when(overflow, |strip| {
                        strip.child(
                            self.button(
                                "tab-preview-more".into(),
                                "",
                                !self.saving && !self.leave_locked,
                                t,
                            )
                            .relative()
                            .w(px(30.0))
                            .px(px(0.0))
                            .justify_center()
                            .track_focus(&self.preview_focus)
                            .child(self.icon("more", 15.0, color(&t.text_secondary)))
                            .tooltip(crate::settings::settings_help(
                                tr("更多分类", "More categories").into(),
                                t.clone(),
                            ))
                            .child(self.anchor_element(
                                self.preview_anchor.clone(),
                                0,
                                EditorPopupKind::Preview,
                            ))
                            .on_click(cx.listener(
                                |this, _, window, cx| {
                                    this.toggle_popup(0, EditorPopupKind::Preview, window, cx)
                                },
                            )),
                        )
                    }),
            )
            .into_any_element()
    }
}

impl Render for FilterTabsEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let current_language = displayed_language();
        if current_language != self.language {
            self.language = current_language;
            self.popup = None;
            for row in &self.rows {
                let placeholder = automatic_tab_name(row.type_key, row.item_type, current_language);
                row.name
                    .update(cx, |input, cx| input.set_placeholder(placeholder, cx));
            }
        }
        if !cx.has_active_drag() {
            self.drop_target = None;
        }
        let tokens = self.tokens.clone();
        let t = tokens.theme(self.dark);
        let palette = SettingsPalette::new(t);
        let editable = self.can_edit(cx);
        let dirty = self.serialized(cx, false) != self.baseline;
        let type_width = if self.width < 430.0 { 104.0 } else { 136.0 };
        let rows = self
            .rows
            .clone()
            .iter()
            .enumerate()
            .map(|(index, row)| self.row(index, row, window, cx))
            .collect::<Vec<_>>();
        let owner = cx.entity().downgrade();
        let preview = self.preview(window, cx);
        // Header and footer stay put; only the rows scroll (2026-10-24: Save
        // used to scroll away, so Restore defaults showed no way to save).
        let scrolled = f32::from(self.body_scroll.offset().y) < -0.5
            && f32::from(self.body_scroll.max_offset().height) > 0.5;
        let hover = palette.control_hover;
        let header = rsx! {
            <div flex_none flex flex_col gap={px(8.0)} pl={px(14.0)} pr={px(10.0)} pt={px(10.0)}
                pb={px(8.0)} border_b_1 />
        }
            .border_color(if scrolled { color(&t.border) } else { gpui::transparent_black() })
            .child(
                rsx! {
                    <div h={px(24.0)} flex items_center gap={px(6.0)}>
                        <div text_size={px(13.0)} font_weight={FontWeight::SEMIBOLD}
                            whitespace_nowrap>
                            {tr("自定义分类", "Custom categories")}
                        </div>
                    </div>
                }
                    .child(
                        rsx! {
                            <div id={"tab-editor-help"} size={px(14.0)} flex_none flex items_center
                                justify_center rounded_full border_1
                                border_color={color(&t.border_secondary)} text_size={px(10.0)}
                                text_color={color(&t.text_muted)}>
                                {"?"}
                            </div>
                        }
                            .tooltip(crate::settings::settings_help(
                                tr(
                                    "可增删、改名、改类型与排序；名称留空时随类型与界面语言自动显示。置顶分栏由「单独分栏显示置顶项」控制。更改分类只改变筛选栏，不修改剪贴板内容。",
                                    "Add, remove, rename, retype or reorder tabs; empty names follow the type and interface language. Pinned tabs follow the separate pinned items setting. Categories only change the filter bar, never clipboard items.",
                                )
                                .into(),
                                t.clone(),
                            )),
                    )
                    .child(div().flex_1())
                    .child(
                        rsx! {
                            <div id={"tab-editor-close"} size={px(24.0)} flex_none flex items_center
                                justify_center rounded={px(5.0)} />
                        }
                            .when(editable, |close| {
                                close.cursor_pointer().hover(move |s| s.bg(hover))
                            })
                            .child(self.icon("close", 14.0, color(&t.text_muted)))
                            .tooltip(crate::settings::settings_help(
                                tr("关闭（不保存）", "Close without saving").into(),
                                t.clone(),
                            ))
                            .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                    ),
            )
            .child(preview);
        let body = div()
            .id("tag-editor-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.body_scroll)
            .smooth_wheel("tag-editor-body", &self.body_scroll, cx)
            .px(px(8.0))
            .pb(px(8.0))
            .child(
                rsx! {
                    <div h={px(26.0)} flex items_center gap={px(3.0)} text_size={px(11.0)}
                        text_color={color(&t.text_muted)}>
                        <div w={px(26.0)} flex_none />
                        <div flex_1 min_w_0 pl={px(6.0)}>
                            {tr("名称（留空自动）", "Name (empty = auto)")}
                        </div>
                        <div w={px(type_width)} flex_none px={px(8.0)}>
                            {tr("内容类型", "Content type")}
                        </div>
                        <div w={px(28.0)} flex_none />
                    </div>
                },
            )
            .child(rsx! {
                <div w_full min_w_0 flex flex_col />
            }.children(rows))
            .child(
                rsx! {
                    <div id={"tab-add"} h={px(32.0)} mt={px(6.0)} flex items_center justify_center
                        gap={px(6.0)} rounded={px(6.0)} border_1 border_dashed
                        border_color={color(&t.border_secondary)} text_size={px(12.0)}
                        text_color={color(&t.text_secondary)} />
                }
                    .when(editable, |add| {
                        add.cursor_pointer().hover(move |s| s.bg(hover))
                    })
                    .when(!editable, |add| add.opacity(0.4))
                    .child(self.icon("plus", 13.0, color(&t.text_secondary)))
                    .child(tr("添加分类", "Add category"))
                    .on_click(cx.listener(|this, _, window, cx| this.add_row(window, cx))),
            );
        let notice = self.undo.as_ref().map(|undo| match undo {
            EditorUndo::Removed { row, .. } => format!(
                "{}{}",
                tr("已移除：", "Removed: "),
                self.row_label(row, cx)
            ),
            EditorUndo::Defaults { .. } => tr("已恢复默认分类", "Default categories restored").into(),
        });
        let accent = color(&t.accent);
        let status = if self.saving {
            tr("保存中…", "Saving…")
        } else if dirty {
            tr("未保存", "Unsaved")
        } else {
            ""
        };
        rsx! {
            <div relative size_full min_h_0 flex flex_col />
        }
            .capture_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" && cx.has_active_drag() {
                    this.drop_target = None;
                    cx.stop_active_drag(window);
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .child(
                gpui::canvas(
                    move |bounds, _, cx| {
                        let width = f32::from(bounds.size.width);
                        let _ = owner.update(cx, |this, cx| {
                            if (this.width - width).abs() > 0.5 {
                                this.width = width;
                                cx.notify();
                            }
                        });
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .when(self.leave_locked, |root| {
                root.capture_any_mouse_down(|_, _, cx| cx.stop_propagation())
            })
            .child(header)
            .child(body)
            .when_some(notice, |root, label| {
                root.child(
                    rsx! {
                        <div flex_none min_h={px(32.0)} flex items_center gap={px(8.0)}
                            pl={px(14.0)} pr={px(8.0)} border_t_1 border_color={color(&t.border)}
                            bg={accent.opacity(0.09)} text_size={px(12.0)}
                            text_color={color(&t.text_secondary)}>
                            <div flex_1 min_w_0>
                                {format!("{label}{}", tr("，保存后生效", " — save to apply"))}
                            </div>
                        </div>
                    }
                        .child(
                            self.button("tab-undo-remove".into(), tr("撤销", "Undo"), editable, t)
                                .h(px(24.0))
                                .text_color(accent)
                                .font_weight(FontWeight::MEDIUM)
                                .on_click(cx.listener(|this, _, window, cx| this.undo(window, cx))),
                        ),
                )
            })
            .when_some(self.error.clone(), |root, error| {
                root.child(
                    rsx! {
                        <div flex_none px={px(14.0)} py={px(6.0)} border_t_1
                            border_color={color(&t.border)} text_size={px(12.0)}
                            text_color={color(&t.error)}>
                            {format!("{}{error}", tr("保存失败：", "Save failed: "))}
                        </div>
                    },
                )
            })
            .child(
                rsx! {
                    <div flex_none flex items_center gap={px(6.0)} pl={px(6.0)} pr={px(10.0)}
                        py={px(8.0)} border_t_1 border_color={color(&t.border)} />
                }
                    .child(
                        self.button(
                            "tab-defaults".into(),
                            tr("恢复默认", "Restore defaults"),
                            editable,
                            t,
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.restore_defaults(window, cx);
                        })),
                    )
                    .child(
                        rsx! {
                            <div flex_1 min_w_0 flex items_center justify_end gap={px(5.0)}
                                text_size={px(11.0)} text_color={color(&t.text_muted)}
                                whitespace_nowrap />
                        }
                            .when(dirty && !self.saving, |status| {
                                status.child(
                                    div().size(px(6.0)).flex_none().rounded_full()
                                        .bg(color(&t.warning)),
                                )
                            })
                            .child(status),
                    )
                    .child(
                        self.button("tab-cancel".into(), tr("取消", "Cancel"), editable, t)
                            .h(px(28.0))
                            .px(px(12.0))
                            .border_1()
                            .border_color(color(&t.border_secondary))
                            .bg(color(&t.secondary))
                            .text_color(color(&t.text))
                            .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                    )
                    .child(
                        self.button(
                            "tab-save".into(),
                            if self.saving {
                                tr("保存中…", "Saving…")
                            } else {
                                tr("保存", "Save")
                            },
                            editable && (dirty || self.save_failed.is_some()),
                            t,
                        )
                        .h(px(28.0))
                        .px(px(14.0))
                        .bg(accent)
                        .text_color(gpui::white())
                        .font_weight(FontWeight::MEDIUM)
                        .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                    ),
            )
            .children(self.popup.as_ref().map(|session| session.popup.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accepted_draft(name: &str) -> FilterTabDraft {
        FilterTabDraft {
            filter_items: json!([{"id":"existing","displayName":name.trim(),"type":"text","itemType":2,"starType":0}]).to_string(),
            draft_filter_items: json!([{"id":"existing","displayName":name,"type":"text","itemType":2,"starType":0}]).to_string(),
            data_root: "isolated injected writer (no database)".into(),
        }
    }

    fn flush_test(registry: &FilterTabWrites) -> Result<(), String> {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(registry.flush())
    }

    #[test]
    fn accepted_filter_tab_save_is_registered_before_worker_and_survives_observer_drop() {
        let registry = Arc::new(FilterTabWrites::default());
        let calls = Arc::new(AtomicU64::new(0));
        let observed = calls.clone();
        let writer: FilterTabWriter = Arc::new(move |_| {
            observed.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        let (observer, start) = registry.enqueue(1, writer, accepted_draft("草稿")).unwrap();
        assert!(start);
        let mut exit = Box::pin(registry.flush());
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(std::future::Future::poll(exit.as_mut(), &mut context).is_pending());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        drop(observer);
        registry.run();
        assert!(matches!(
            std::future::Future::poll(exit.as_mut(), &mut context),
            std::task::Poll::Ready(Ok(()))
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn accepted_filter_tab_exit_waits_for_delayed_transaction_after_owner_drop() {
        let registry = Arc::new(FilterTabWrites::default());
        let (started, start) = std::sync::mpsc::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let wait = Mutex::new(wait);
        let writer: FilterTabWriter = Arc::new(move |_| {
            started.send(()).unwrap();
            wait.lock().unwrap().recv().unwrap();
            Err("disk failed".into())
        });
        let observer = registry
            .submit(1, writer, accepted_draft("  名称 😀  "))
            .unwrap();
        start
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        drop(observer);
        let mut exit = Box::pin(registry.flush());
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(std::future::Future::poll(exit.as_mut(), &mut context).is_pending());
        release.send(()).unwrap();
        let result = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(exit);
        assert!(result.unwrap_err().contains("disk failed"));
        let failures = registry.failures();
        assert_eq!(failures.len(), 1);
        let draft: Value = serde_json::from_str(&failures[0].draft_filter_items).unwrap();
        assert_eq!(draft[0]["displayName"], "  名称 😀  ");
        assert_eq!(draft[0]["id"], "existing");
        assert_eq!(draft[0]["itemType"], 2);
        assert!(registry.discard(failures[0].id).is_ok());
        assert_eq!(flush_test(&registry), Ok(()));
    }

    #[test]
    fn accepted_filter_tab_full_snapshots_write_in_ui_acceptance_order() {
        let registry = FilterTabWrites::default();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let observed = calls.clone();
        let writer: FilterTabWriter = Arc::new(move |value| {
            observed.lock().unwrap().push(value.to_string());
            Ok(())
        });
        let first = accepted_draft("first");
        let second = accepted_draft("second");
        let (one, _) = registry.enqueue(1, writer.clone(), first.clone()).unwrap();
        let (two, start) = registry.enqueue(2, writer, second.clone()).unwrap();
        assert!(!start);
        registry.run();
        assert_eq!(one.blocking_recv().unwrap(), Ok(()));
        assert_eq!(two.blocking_recv().unwrap(), Ok(()));
        assert_eq!(
            *calls.lock().unwrap(),
            [first.filter_items, second.filter_items]
        );
    }

    #[test]
    fn accepted_filter_tab_retry_keeps_original_snapshot_and_clears_only_its_failure() {
        let registry = Arc::new(FilterTabWrites::default());
        let calls = Arc::new(Mutex::new(Vec::new()));
        let successful = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = calls.clone();
        let enabled = successful.clone();
        let writer: FilterTabWriter = Arc::new(move |value| {
            observed.lock().unwrap().push(value.to_string());
            if enabled.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err("write failed".into())
            }
        });
        let draft = accepted_draft(" original ");
        let (one, _) = registry.enqueue(1, writer.clone(), draft.clone()).unwrap();
        let (two, _) = registry
            .enqueue(
                2,
                Arc::new(|_| Err("other failed".into())),
                accepted_draft("other"),
            )
            .unwrap();
        registry.run();
        assert!(one.blocking_recv().unwrap().is_err());
        assert!(two.blocking_recv().unwrap().is_err());
        let failures = registry.failures();
        successful.store(true, Ordering::SeqCst);
        assert_eq!(
            registry
                .retry(failures[0].id)
                .unwrap()
                .blocking_recv()
                .unwrap(),
            Ok(())
        );
        assert_eq!(
            *calls.lock().unwrap(),
            [draft.filter_items.clone(), draft.filter_items]
        );
        assert_eq!(registry.failures(), [failures[1].clone()]);
        assert!(flush_test(&registry).unwrap_err().contains("other failed"));
        registry.discard(failures[1].id).unwrap();
        assert_eq!(flush_test(&registry), Ok(()));
    }

    #[test]
    fn accepted_filter_tab_stale_recovery_cannot_drop_or_retry_replacement_failure() {
        let registry = Arc::new(FilterTabWrites::default());
        let writer: FilterTabWriter = Arc::new(|_| Err("failed".into()));
        let (one, _) = registry
            .enqueue(1, writer.clone(), accepted_draft("old"))
            .unwrap();
        registry.run();
        assert!(one.blocking_recv().unwrap().is_err());
        let old = registry.failures()[0].clone();
        let (two, _) = registry.enqueue(1, writer, accepted_draft("new")).unwrap();
        assert!(registry.discard(old.id).is_err()); // Cannot discard an accepted in-flight replacement.
        registry.run();
        assert!(two.blocking_recv().unwrap().is_err());
        let new = registry.failures()[0].clone();
        assert_ne!(old.id, new.id);
        assert!(registry.retry(old.id).is_err());
        assert!(registry.discard(old.id).is_err());
        assert_eq!(registry.failures(), [new]);
    }

    #[test]
    fn accepted_filter_tab_restored_editor_cannot_cancel_or_save_over_newer_failure() {
        let registry = Arc::new(FilterTabWrites::default());
        let writer: FilterTabWriter = Arc::new(|_| Err("failed".into()));
        let (one, _) = registry
            .enqueue(1, writer.clone(), accepted_draft("original"))
            .unwrap();
        registry.run();
        assert!(one.blocking_recv().unwrap().is_err());
        let restored = registry.failure(registry.failures()[0].id).unwrap();
        let (two, _) = registry
            .enqueue(1, writer.clone(), accepted_draft("replacement"))
            .unwrap();
        assert!(registry.failure(restored.snapshot.id).is_err()); // A pending owner cannot be restored.
        registry.run();
        assert!(two.blocking_recv().unwrap().is_err());
        let current = registry.failures();
        // These are the exact acceptance/discard paths used by the restored UI.
        assert!(registry
            .submit_recovery(
                restored.owner,
                Some(restored.snapshot.id),
                writer,
                accepted_draft("stale restored input")
            )
            .is_err());
        assert!(registry
            .discard_owner(restored.owner, Some(restored.snapshot.id))
            .is_err());
        assert_eq!(registry.failures(), current);
        registry
            .discard_owner(restored.owner, Some(current[0].id))
            .unwrap();
        assert_eq!(flush_test(&registry), Ok(()));
    }

    #[test]
    fn accepted_filter_tab_exit_gate_rejects_retained_callbacks_but_finishes_accepted_work() {
        let registry = Arc::new(FilterTabWrites::default());
        let calls = Arc::new(Mutex::new(Vec::new()));
        let observed = calls.clone();
        let writer: FilterTabWriter = Arc::new(move |value| {
            observed.lock().unwrap().push(value.to_string());
            Ok(())
        });
        let draft = accepted_draft("accepted before Exit");
        let (observer, _) = registry.enqueue(1, writer.clone(), draft.clone()).unwrap();
        registry.set_exit_pending(true);
        let late_draft = accepted_draft("retained Entity's late click");
        assert!(registry
            .submit_recovery(2, None, writer.clone(), late_draft.clone())
            .is_err());
        let mut barrier = registry.barrier();
        assert!(matches!(
            barrier.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        drop(observer);
        registry.run();
        assert_eq!(barrier.try_recv().unwrap(), Ok(()));
        assert_eq!(*calls.lock().unwrap(), [draft.filter_items]);
        registry.set_exit_pending(false);
        // Unfreezing does not replay the rejected click; explicit Save is required.
        assert_eq!(calls.lock().unwrap().len(), 1);
        let (_, observer) = registry
            .submit_recovery(2, None, writer, late_draft.clone())
            .unwrap();
        assert_eq!(observer.blocking_recv().unwrap(), Ok(()));
        assert_eq!(calls.lock().unwrap()[1], late_draft.filter_items);
        assert_eq!(flush_test(&registry), Ok(()));
    }

    #[test]
    fn accepted_filter_tab_worker_start_failure_settles_barrier_and_allows_recovery() {
        let registry = Arc::new(FilterTabWrites::default());
        let (observer, _) = registry
            .enqueue(1, Arc::new(|_| Ok(())), accepted_draft("keep"))
            .unwrap();
        let mut barrier = registry.barrier();
        registry.reject_start("worker could not start".into());
        assert_eq!(
            observer.blocking_recv().unwrap(),
            Err("worker could not start".into())
        );
        assert!(barrier
            .try_recv()
            .unwrap()
            .unwrap_err()
            .contains("worker could not start"));
        let failure = registry.failures()[0].clone();
        assert_eq!(
            registry.retry(failure.id).unwrap().blocking_recv().unwrap(),
            Ok(())
        );
        assert_eq!(flush_test(&registry), Ok(()));
    }

    #[test]
    fn accepted_filter_tab_panicked_writer_reports_failure_instead_of_hanging_exit() {
        let registry = FilterTabWrites::default();
        let (observer, _) = registry
            .enqueue(
                1,
                Arc::new(|_| panic!("controlled writer panic")),
                accepted_draft("keep"),
            )
            .unwrap();
        registry.run();
        assert!(observer.blocking_recv().unwrap().is_err());
        assert!(flush_test(&registry).is_err());
        let failure = registry.failures()[0].clone();
        registry.discard(failure.id).unwrap();
        assert_eq!(flush_test(&registry), Ok(()));
    }

    #[test]
    fn configured_names_are_not_promoted_to_builtin_labels() {
        let mut defaults = serde_json::to_value(clip_filter::create_default_tabs()).unwrap();
        for (row, name) in defaults
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .zip(["默认", "文本", "富文本", "图片", "文件", "网址"])
        {
            row["displayName"] = json!(name);
        }
        // Persisted default-looking records and a user-created identical set
        // are indistinguishable: neither has a recorded built-in origin.
        for language in [LanguageMode::Chinese, LanguageMode::English] {
            let tabs = build_tabs_for_language(&defaults, false, language);
            let labels: Vec<_> = tabs.iter().map(|tab| tab.label.as_str()).collect();
            assert_eq!(labels, ["默认", "文本", "富文本", "图片", "文件", "网址"]);
            for (tab, stored) in tabs.iter().zip(defaults.as_array().unwrap()) {
                assert_eq!(tab.filter_item.as_ref().unwrap(), stored);
                assert_eq!(tab.key, format!("id:{}", stored["id"].as_str().unwrap()));
            }
        }

        let mut custom = defaults.clone();
        custom[0]["displayName"] = json!("  Work 默认  ");
        let before = custom.clone();
        let tabs = build_tabs_for_language(&custom, true, LanguageMode::English);
        assert_eq!(tabs[0].label, "  Work 默认  ");
        assert_eq!(tabs[1].label, "Pinned");
        assert_eq!(tabs[2].label, "文本");
        assert_eq!(custom, before);

        let mut reordered = defaults.clone();
        reordered.as_array_mut().unwrap().swap(1, 2);
        assert_eq!(
            build_tabs_for_language(&reordered, false, LanguageMode::English)[1].label,
            "富文本"
        );

        let named_like_default = json!([{ "id": "custom", "displayName": "文本", "type": "text", "itemType": 2, "starType": 0 }]);
        assert_eq!(
            build_tabs_for_language(&named_like_default, false, LanguageMode::English)[0].label,
            "文本"
        );
    }

    #[test]
    fn known_builtin_labels_follow_language_without_rewriting_filter_names() {
        for (language, expected) in [
            (
                LanguageMode::Chinese,
                ["默认", "置顶", "文本", "富文本", "图片", "文件", "网址"],
            ),
            (
                LanguageMode::English,
                ["Default", "Pinned", "Text", "Rich text", "Images", "Files", "URLs"],
            ),
        ] {
            let tabs = build_tabs_for_language(&Value::Null, true, language);
            let labels: Vec<_> = tabs.iter().map(|tab| tab.label.as_str()).collect();
            assert_eq!(labels, expected);
            assert_eq!(tabs[0].filter_item.as_ref().unwrap()["displayName"], "");
            assert_eq!(tabs[2].filter_item.as_ref().unwrap()["displayName"], "");
        }
        assert_eq!(
            category_tab_label("文本", None, LanguageMode::English),
            "文本"
        );
        assert_eq!(
            category_tab_label("文本", Some(BuiltinTabLabel::Text), LanguageMode::English),
            "Text"
        );
    }

    #[test]
    fn persisted_automatic_names_follow_type_and_language_without_mutating_storage() {
        let rows: Vec<_> = TYPE_OPTIONS.iter().enumerate().map(|(index, (key, mask, _, _))| {
            json!({"id":format!("auto-{index}"),"displayName":"","type":key,"itemType":mask,"starType":0})
        }).collect();
        let stored = json!(rows);
        let round_trip = clip_filter::parse_form_value(&json!(clip_filter::serialize_form_value(
            &clip_filter::parse_form_value(&stored),
        )));
        let reloaded = serde_json::to_value(round_trip).unwrap();
        let before = reloaded.clone();
        for (language, expected) in [
            (
                LanguageMode::Chinese,
                ["默认", "文本", "纯文本", "富文本", "图片", "文件", "网址", "步骤"],
            ),
            (
                LanguageMode::English,
                [
                    "Default",
                    "Text",
                    "Plain text",
                    "Rich text",
                    "Images",
                    "Files",
                    "URLs",
                    "Steps",
                ],
            ),
        ] {
            let tabs = build_tabs_for_language(&reloaded, false, language);
            assert_eq!(
                tabs.iter()
                    .map(|tab| tab.label.as_str())
                    .collect::<Vec<_>>(),
                expected
            );
            for (index, tab) in tabs.iter().enumerate() {
                assert_eq!(tab.key, format!("id:auto-{index}"));
                assert_eq!(tab.filter_item.as_ref().unwrap()["displayName"], "");
            }
        }
        assert_eq!(reloaded, before);
    }

    #[test]
    fn custom_names_are_literal_even_when_matching_builtin_or_pinned_names() {
        let stored = json!([
            {"id":"a","displayName":"文本","type":"image","itemType":4,"starType":0},
            {"id":"b","displayName":"Images","type":"file","itemType":8,"starType":0},
            {"id":"c","displayName":"置顶","type":"text","itemType":2,"starType":0},
            {"id":"d","displayName":"网址","type":"url","itemType":128,"starType":0}
        ]);
        for language in [LanguageMode::Chinese, LanguageMode::English] {
            let tabs = build_tabs_for_language(&stored, false, language);
            assert_eq!(
                tabs.iter()
                    .map(|tab| tab.label.as_str())
                    .collect::<Vec<_>>(),
                ["文本", "Images", "置顶", "网址"]
            );
            assert_eq!(
                tabs.iter()
                    .map(|tab| tab.request_type.as_str())
                    .collect::<Vec<_>>(),
                ["image", "file", "text", "url"]
            );
        }
        let mut cleared = stored.clone();
        cleared[0]["displayName"] = json!("");
        assert_eq!(
            build_tabs_for_language(&cleared, false, LanguageMode::English)[0].label,
            "Images"
        );
    }

    #[test]
    fn default_tabs_keep_builtin_keys_and_optional_pinned_tab() {
        let tabs = build_tabs(&Value::Null, false);
        let keys: Vec<_> = tabs.iter().map(|tab| tab.key.as_str()).collect();
        assert_eq!(keys, ["all", "text", "richtext", "image", "file", "url"]);
        assert!(tabs
            .iter()
            .all(|tab| tab.filter_item.as_ref().unwrap()["starType"] == 0));
        let tabs = build_tabs(&json!([]), true);
        let keys: Vec<_> = tabs.iter().map(|tab| tab.key.as_str()).collect();
        assert_eq!(keys, ["all", "top", "text", "richtext", "image", "file", "url"]);
        // Separated pinned rows are excluded from ordinary tabs (web starType 2).
        assert_eq!(tabs[0].filter_item.as_ref().unwrap()["starType"], 2);
        assert_eq!(tabs[1].request_type, "star");
    }

    #[test]
    fn configured_tabs_map_types_icons_and_skip_legacy_star_rows() {
        let filters = json!([
            { "id": "a", "displayName": "全部", "type": "default", "itemType": 1, "starType": 0 },
            { "id": "b", "displayName": "富文本", "type": "richtext", "itemType": 64, "starType": 0 },
            { "id": "c", "displayName": "步骤", "type": "other", "itemType": 16, "starType": 0 },
            { "id": "url", "displayName": "", "type": "url", "itemType": 128, "starType": 0 },
            { "id": "d", "displayName": "星标", "type": "star", "itemType": 1, "starType": 1 },
            { "id": "b", "displayName": "重复", "type": "text", "itemType": 2, "starType": 0 }
        ]);
        let tabs = build_tabs(&filters, false);
        let summary: Vec<_> = tabs
            .iter()
            .map(|tab| (tab.key.as_str(), tab.request_type.as_str(), tab.icon))
            .collect();
        assert_eq!(
            summary,
            [
                ("id:a", "all", "all"),
                ("id:b", "richtext", "code"),
                ("id:c", "other", "list"),
                ("id:url", "url", "link")
            ]
        );
    }

    #[test]
    fn requests_carry_filter_item_and_trimmed_dates() {
        let tabs = build_tabs(&Value::Null, true);
        let advanced = AdvancedFilter {
            create_time_filter: -1,
            start: " 2026-10-01 ".into(),
            ..Default::default()
        };
        let params = tabs[1].request(&advanced).params("q", 0, 0, "s");
        assert_eq!(params["type"], "star");
        assert_eq!(params["filterItem"]["displayName"], "置顶");
        assert_eq!(params["take"], 100);
        assert_eq!(params["createTimeFilterStart"], "2026-10-01");
        assert_eq!(params["createTimeFilterEnd"], Value::Null);
        assert_eq!(ListRequest::category("top").fixture_category(), "top");
        assert_eq!(ListRequest::category("url").fixture_category(), "url");
        assert!(advanced.is_active());
        assert!(!AdvancedFilter::default().is_active());
    }

    #[test]
    fn process_constraint_composes_with_category_search_and_advanced_filters() {
        let tabs = build_tabs(&Value::Null, true);
        let advanced = AdvancedFilter {
            sort_type: 1,
            usage_filter: 2,
            create_time_filter: -2,
            date: " 2026-10-12 ".into(),
            ..Default::default()
        };
        let spec = json!({ "includeExeNames": ["Code.exe", "foreground"] });
        let request = tabs[1]
            .request(&advanced)
            .with_process_filter(Some(spec.clone()));
        let params = request.params("原始 query", 100, 25, "list-session");
        assert_eq!(params["processFilter"], spec);
        assert_eq!(params["type"], "star");
        assert_eq!(params["query"], "原始 query");
        assert_eq!(params["sortType"], 1);
        assert_eq!(params["usageFilter"], 2);
        assert_eq!(params["createTimeFilterDate"], "2026-10-12");
        assert_eq!(params["skip"], 100);
        assert_eq!(params["webSessionId"], "list-session");
        let decoded: cliphost_core::dto::ClipListRequest = serde_json::from_value(params).unwrap();
        assert_eq!(
            decoded.process_filter.unwrap().include_exe_names.unwrap(),
            ["Code.exe", "foreground"]
        );
        assert_eq!(
            ListRequest::category("all").params("", 0, 10, "")["processFilter"],
            Value::Null
        );
    }
}
