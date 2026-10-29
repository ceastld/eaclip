//! CeaBox owns its HWND, presentation and accepted execution independently of
//! the clipboard/panel surfaces. Queries and effects must run on a worker.

use super::*;
use crate::ceabox::{self, CeaBoxExecutionTarget, CeaBoxPage, CeaBoxSource, CeaBoxSourceStatus};
use crate::verb::{VerbAction, VerbRunContext};

#[path = "native_ceabox_search.rs"]
mod search;

const RETIRED: &str = "搜索操作已取消。";

#[derive(Default)]
pub(super) struct CeaBoxRuntime {
    pub(super) hwnd: isize,
    pub(super) visible: bool,
    pinned: bool,
    generation: u64,
    execution_generation: u64,
    execution_claimed: bool,
    executing: bool,
    source: CeaBoxSource,
    target_hwnd: isize,
    search_cache: Arc<Mutex<search::SearchCache>>,
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CeaBoxNativeSnapshot {
    pub visible: bool,
    pub pinned: bool,
    pub generation: u64,
    pub hwnd: isize,
    pub source: CeaBoxSource,
    pub executing: bool,
}

/// Opaque acceptance ticket: callers cannot substitute a replacement window,
/// presentation or external input receiver after a background wait.
#[derive(Debug, Clone)]
pub struct NativeCeaBoxExecuteRequest {
    owner: usize,
    source_hwnd: isize,
    generation: u64,
    execution_generation: u64,
    target: Option<InputTarget>,
}

#[derive(Clone, serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ActionSearchPage {
    #[serde(alias = "items")]
    items: Vec<ActionSearchItem>,
    #[serde(alias = "total")]
    total: usize,
    #[serde(alias = "hasMore")]
    has_more: bool,
}

#[derive(Clone, serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ActionSearchItem {
    #[serde(alias = "id")]
    id: String,
    #[serde(default, alias = "title")]
    title: String,
    #[serde(default, alias = "description")]
    description: String,
    #[serde(default, alias = "context")]
    context: String,
    #[serde(default, alias = "score")]
    score: Option<i32>,
    #[serde(default, alias = "lastEditTimeUtc")]
    last_edit_time_utc: Option<String>,
}

impl HostState {
    pub fn ceabox_native_snapshot(&self) -> CeaBoxNativeSnapshot {
        let inner = self.inner.lock();
        let state = &inner.ceabox;
        CeaBoxNativeSnapshot {
            visible: state.visible,
            pinned: state.pinned,
            generation: state.generation,
            hwnd: state.hwnd,
            source: state.source,
            executing: state.executing,
        }
    }

    pub fn ceabox_set_hwnd(&self, hwnd: isize) {
        let mut inner = self.inner.lock();
        if inner.ceabox.hwnd != hwnd {
            inner.ceabox.hwnd = hwnd;
            inner.ceabox.generation = inner.ceabox.generation.wrapping_add(1);
            retire_execution(&mut inner.ceabox);
            if hwnd == 0 {
                inner.ceabox.visible = false;
            }
        }
    }

    pub fn ceabox_set_pinned(&self, pinned: bool) {
        self.inner.lock().ceabox.pinned = pinned;
    }

    pub fn ceabox_set_source(&self, source: CeaBoxSource) {
        let mut inner = self.inner.lock();
        if inner.ceabox.source != source {
            inner.ceabox.source = source;
            retire_execution(&mut inner.ceabox);
        }
    }

    /// Text/selection changes invalidate a queued effect while preserving UI.
    pub fn ceabox_retire_execution(&self) {
        retire_execution(&mut self.inner.lock().ceabox);
    }

    pub fn ceabox_show(&self, source: CeaBoxSource, ctx: Option<&VerbRunContext>) {
        let foreground = native_paste_foreground();
        let mut inner = self.inner.lock();
        let requested = ctx.map_or(0, |context| context.target_window_handle as isize);
        let target = if requested != 0 && !foreground_is_own_window(&inner, requested) {
            requested
        } else if foreground != 0 && !foreground_is_own_window(&inner, foreground) {
            foreground
        } else {
            inner.last_non_host_fg
        };
        inner.ceabox.target_hwnd = if foreground_is_own_window(&inner, target) {
            0
        } else {
            target
        };
        inner.ceabox.visible = true;
        inner.ceabox.source = source;
        inner.ceabox.generation = inner.ceabox.generation.wrapping_add(1);
        retire_execution(&mut inner.ceabox);
        drop_ceabox_commands(&mut inner);
        push_chrome(&mut inner, ChromeCommand::ShowCeaBox);
    }

    pub fn ceabox_hide(&self) {
        let mut inner = self.inner.lock();
        hide_locked(&mut inner);
    }

    pub(super) fn execute_ceabox_verb(
        &self,
        action: VerbAction,
        source: String,
        ctx: Option<&VerbRunContext>,
    ) -> Result<Value, String> {
        let snapshot = self.ceabox_native_snapshot();
        if action == VerbAction::None {
            return Ok(crate::verb::verb_ok(json!({
                "action": action.as_str(), "source": snapshot.source.as_str(),
                "visible": snapshot.visible,
            })));
        }
        let visible = snapshot.visible;
        let show = match action {
            VerbAction::Hide | VerbAction::Stop | VerbAction::Close => false,
            VerbAction::Toggle | VerbAction::ShowOrHide | VerbAction::StartOrStop => !visible,
            _ => true,
        };
        if show {
            self.ceabox_show(CeaBoxSource::parse(&source), ctx);
        } else {
            self.ceabox_hide();
        }
        Ok(crate::verb::verb_ok(
            json!({ "action": action.as_str(), "source": source, "visible": show }),
        ))
    }

    pub fn capture_native_ceabox_execute(&self) -> Result<NativeCeaBoxExecuteRequest, String> {
        let foreground = native_paste_foreground();
        let (source_hwnd, generation, execution_generation, target_hwnd) = {
            let mut inner = self.inner.lock();
            if !inner.ceabox.visible || inner.ceabox.hwnd == 0 {
                return Err(RETIRED.into());
            }
            retire_execution(&mut inner.ceabox);
            let target = if foreground != 0 && !foreground_is_own_window(&inner, foreground) {
                foreground
            } else {
                inner.ceabox.target_hwnd
            };
            (
                inner.ceabox.hwnd,
                inner.ceabox.generation,
                inner.ceabox.execution_generation,
                target,
            )
        };
        let request = NativeCeaBoxExecuteRequest {
            owner: Arc::as_ptr(&self.inner) as usize,
            source_hwnd,
            generation,
            execution_generation,
            target: InputTarget::capture(target_hwnd).ok(),
        };
        self.verify_ceabox_request(&request)?;
        Ok(request)
    }

    fn verify_ceabox_request(&self, request: &NativeCeaBoxExecuteRequest) -> Result<(), String> {
        let inner = self.inner.lock();
        if request.owner != Arc::as_ptr(&self.inner) as usize
            || !request_matches(&inner.ceabox, request)
        {
            return Err(RETIRED.into());
        }
        Ok(())
    }

    pub fn execute_native_ceabox(
        &self,
        request: &NativeCeaBoxExecuteRequest,
        target: &CeaBoxExecutionTarget,
    ) -> Result<Value, String> {
        // Claim once before any I/O. A repeated key/click cannot replay a ticket,
        // nor can a failed duplicate clean up another worker's active effect.
        {
            let mut inner = self.inner.lock();
            if request.owner != Arc::as_ptr(&self.inner) as usize
                || !request_matches(&inner.ceabox, request)
                || inner.ceabox.execution_claimed
            {
                return Err(RETIRED.into());
            }
            inner.ceabox.execution_claimed = true;
        }
        let result = self.execute_native_ceabox_inner(request, target);
        let mut inner = self.inner.lock();
        if request.generation == inner.ceabox.generation
            && request.execution_generation == inner.ceabox.execution_generation
        {
            if result.is_ok() && !inner.ceabox.pinned {
                hide_locked(&mut inner);
            } else {
                retire_execution(&mut inner.ceabox);
            }
        }
        result
    }

    fn execute_native_ceabox_inner(
        &self,
        request: &NativeCeaBoxExecuteRequest,
        target: &CeaBoxExecutionTarget,
    ) -> Result<Value, String> {
        self.verify_ceabox_request(request)?;
        if target.id.trim().is_empty() || target.source == CeaBoxSource::All {
            return Err("搜索结果已失效。".into());
        }
        let host = self.clone();
        let accepted = request.clone();
        let guard: Arc<dyn Fn() -> Result<(), String> + Send + Sync> =
            Arc::new(move || host.verify_ceabox_request(&accepted));
        if target.source == CeaBoxSource::Action {
            self.begin_ceabox_effect(request)?;
            self.run_quicker_action_guarded(&target.id, "", guard)?;
            return Ok(json!({"ok": true}));
        }
        let store = self.store();
        let (clip, favorite) = match target.source {
            CeaBoxSource::Favorite => {
                let item = store.lock().get_favor(&target.id).map_err(item_not_found)?;
                self.verify_ceabox_request(request)?;
                if item.is_folder || item.kind == ClipKind::FavoriteClipItem {
                    self.begin_ceabox_effect(request)?;
                    self.panel_show_guarded(
                        &json!({ "favorId": item.id, "folderId": item.id }),
                        &|inner| {
                            if request.owner == Arc::as_ptr(&self.inner) as usize
                                && request_matches(&inner.ceabox, request)
                            {
                                Ok(())
                            } else {
                                Err(RETIRED.into())
                            }
                        },
                    )?;
                    self.push_event("hostShow", json!({ "tab": "panel", "id": item.id }));
                    return Ok(json!({"ok": true}));
                }
                let item = store
                    .lock()
                    .core_io_favor(&item)
                    .map_err(|error| error.to_string())?;
                (favor_as_clip(&item)?, Some(item))
            }
            CeaBoxSource::Clipboard => (
                store.lock().get_clip(&target.id).map_err(item_not_found)?,
                None,
            ),
            _ => unreachable!(),
        };
        self.verify_ceabox_request(request)?;
        if let Some(item) = &favorite {
            match item.kind {
                ClipKind::LinkClipItem => {
                    self.begin_ceabox_effect(request)?;
                    self.open_shell_target_guarded(
                        item.body_text.as_deref().unwrap_or("").trim(),
                        guard.as_ref(),
                    )?;
                    self.touch_favor_usage(&item.id);
                    return Ok(json!({"ok": true}));
                }
                ClipKind::FileClipItem => {
                    let paths = if item.file_paths.is_empty() {
                        item.body_text.iter().cloned().collect()
                    } else {
                        item.file_paths.clone()
                    };
                    // Re-resolve after record/path projection. The original
                    // receiver is a fallback while our own window owns focus.
                    let foreground = native_paste_foreground();
                    let own = foreground_is_own_window(&self.inner.lock(), foreground);
                    let file_target = (foreground != 0 && !own)
                        .then(|| InputTarget::capture(foreground).ok())
                        .flatten()
                        .or_else(|| {
                            request
                                .target
                                .filter(|target| target.verify_identity().is_ok())
                        });
                    self.begin_ceabox_effect(request)?;
                    self.execute_file_clip_paths_guarded(
                        &paths,
                        file_target.map_or(0, |target| target.hwnd()) as i64,
                        file_target,
                        guard.as_ref(),
                    )?;
                    self.touch_favor_usage(&item.id);
                    return Ok(json!({"ok": true}));
                }
                ClipKind::ActionClipItem => {
                    let action_id = crate::quicker_action::action_id_from_payload(
                        item.body_text.as_deref().unwrap_or(""),
                    )?;
                    self.begin_ceabox_effect(request)?;
                    self.run_quicker_action_guarded(&action_id, &item.execute_param, guard)?;
                    self.touch_favor_usage(&item.id);
                    return Ok(json!({"ok": true}));
                }
                _ => {}
            }
        }
        // History preserves its paste behavior for every kind, including links
        // and file drops. Favorite ordinary records share the same payload path.
        let receiver = request
            .target
            .or_else(|| {
                let foreground = native_paste_foreground();
                let own = foreground_is_own_window(&self.inner.lock(), foreground);
                (foreground != 0 && !own)
                    .then(|| InputTarget::capture(foreground).ok())
                    .flatten()
            })
            .ok_or("找不到可接收输入的目标窗口。")?;
        let clip = self.project_output_clip(&clip)?;
        let opts = self.paste_options();
        let copy_opts = opts.clone();
        self.begin_ceabox_effect(request)?;
        let activating = guard.clone();
        self.run_input(move || {
            activating()?;
            receiver.activate_for_paste()
        })?;
        self.paste_native_payload_after_activation(
            opts,
            receiver,
            guard,
            move || crate::paste::prepare_copy_record(&clip, &copy_opts),
            None,
        )?;
        if favorite.is_some() {
            self.touch_favor_usage(&target.id);
        } else {
            self.touch_clip_usage(&target.id);
        }
        Ok(json!({"ok": true}))
    }

    fn begin_ceabox_effect(&self, request: &NativeCeaBoxExecuteRequest) -> Result<(), String> {
        self.verify_ceabox_request(request)?;
        let mut inner = self.inner.lock();
        if request.generation != inner.ceabox.generation
            || request.execution_generation != inner.ceabox.execution_generation
        {
            return Err(RETIRED.into());
        }
        inner.ceabox.executing = true;
        Ok(())
    }
}

fn source_status(source: CeaBoxSource, message: Option<&str>) -> CeaBoxSourceStatus {
    CeaBoxSourceStatus {
        source,
        available: message.is_none(),
        message: message.map(str::to_string),
    }
}

fn parse_action_search_page(
    value: Value,
    skip: usize,
    take: usize,
    previous_total: Option<usize>,
) -> Result<ActionSearchPage, String> {
    let page: ActionSearchPage =
        serde_json::from_value(value).map_err(|_| "Invalid Quicker action search response.")?;
    let end = skip
        .checked_add(page.items.len())
        .ok_or("Invalid Quicker action search page bounds.")?;
    // The callback contract uses C# int totals and scores in the shared 0..1000
    // range. Never let malformed peer values overflow an aggregate or invent
    // missing pages. A catalog changed between pages is retried by a new query.
    if page.total > i32::MAX as usize
        || page.items.len() > take
        || page.total < end
        || page.has_more != (end < page.total)
        || previous_total.is_some_and(|total| total != page.total)
        || page
            .items
            .iter()
            .any(|item| item.score.is_some_and(|score| !(0..=1000).contains(&score)))
    {
        return Err("Invalid Quicker action search page bounds.".into());
    }
    Ok(page)
}

fn retire_execution(state: &mut CeaBoxRuntime) {
    state.execution_generation = state.execution_generation.wrapping_add(1);
    state.execution_claimed = false;
    state.executing = false;
}

fn request_matches(state: &CeaBoxRuntime, request: &NativeCeaBoxExecuteRequest) -> bool {
    state.visible
        && request.source_hwnd != 0
        && request.source_hwnd == state.hwnd
        && request.generation == state.generation
        && request.execution_generation == state.execution_generation
}

fn drop_ceabox_commands(inner: &mut HostInner) {
    inner.chrome.retain(|command| {
        !matches!(
            command,
            ChromeCommand::ShowCeaBox | ChromeCommand::HideCeaBox
        )
    });
}

fn hide_locked(inner: &mut HostInner) {
    inner.ceabox.visible = false;
    inner.ceabox.generation = inner.ceabox.generation.wrapping_add(1);
    retire_execution(&mut inner.ceabox);
    drop_ceabox_commands(inner);
    push_chrome(inner, ChromeCommand::HideCeaBox);
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        _root: tempfile::TempDir,
        host: HostState,
    }

    fn fixture() -> Fixture {
        crate::clipboard::force_headless(true);
        let parent = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
        std::fs::create_dir_all(&parent).unwrap();
        let root = tempfile::Builder::new()
            .prefix("ceabox-core-")
            .tempdir_in(parent)
            .unwrap();
        let host =
            HostState::open(AppPaths::from_data_root(root.path().to_path_buf()), 29065).unwrap();
        host.inner.lock().last_non_host_fg = 42;
        host.ceabox_set_hwnd(700);
        host.ceabox_show(CeaBoxSource::All, None);
        Fixture { _root: root, host }
    }

    fn insert_clip(host: &HostState, id: &str, title: &str, kind: ClipKind) -> ClipRecord {
        let mut item = ClipRecord::new_text(title, "sample.exe");
        item.id = id.into();
        item.title = Some(title.into());
        item.kind = kind;
        host.store().lock().insert_clips(&[item.clone()]).unwrap();
        item
    }

    fn target(source: CeaBoxSource, id: &str) -> CeaBoxExecutionTarget {
        CeaBoxExecutionTarget {
            source,
            id: id.into(),
        }
    }

    #[test]
    fn aggregate_queries_real_nested_favorites_and_history_without_entering_a_source() {
        let f = fixture();
        let clip = insert_clip(&f.host, "history-id", "项目", ClipKind::TextClipItem);
        let mut folder = FavorRecord::from_clip(&ClipRecord::new_text("工作", ""), None);
        folder.id = "folder-id".into();
        folder.name = "工作".into();
        folder.is_folder = true;
        let mut favorite = FavorRecord::from_clip(&clip, Some(folder.id.clone()));
        favorite.id = "favorite-id".into();
        favorite.name = "项目".into();
        f.host.store().lock().insert_favor(&folder).unwrap();
        f.host.store().lock().insert_favor(&favorite).unwrap();
        let page = f
            .host
            .ceabox_search("项目", CeaBoxSource::All, 0, 20)
            .unwrap();
        assert_eq!(
            page.items
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["favorite-id", "history-id"]
        );
        assert_eq!(page.items[0].context, "工作");
        assert_eq!(
            page.items[0].target,
            target(CeaBoxSource::Favorite, "favorite-id")
        );
        assert!(page
            .sources
            .iter()
            .any(|status| status.source == CeaBoxSource::Action && !status.available));
        assert!(page
            .sources
            .iter()
            .filter(|status| status.source != CeaBoxSource::Action)
            .all(|status| status.available));
        let scoped = f
            .host
            .ceabox_search("项目", CeaBoxSource::Clipboard, 0, 20)
            .unwrap();
        assert_eq!(scoped.total, 1);
        assert_eq!(scoped.items[0].id, "history-id");
    }

    #[test]
    fn full_history_search_does_not_lose_old_exact_hits_at_a_decode_page_boundary() {
        let f = fixture();
        let mut records = Vec::new();
        for i in 0..300 {
            let mut item = ClipRecord::new_text(format!("项目附件 {i}"), "sample.exe");
            item.id = format!("recent-{i}");
            item.create_time = Utc::now() + chrono::Duration::seconds(i);
            records.push(item);
        }
        let mut exact = ClipRecord::new_text("项目", "sample.exe");
        exact.id = "old-exact".into();
        exact.create_time = Utc::now() - chrono::Duration::days(90);
        records.push(exact);
        f.host.store().lock().insert_clips(&records).unwrap();
        let page = f
            .host
            .ceabox_search("项目", CeaBoxSource::Clipboard, 0, 2)
            .unwrap();
        assert_eq!(page.total, 301);
        assert_eq!(page.items[0].id, "old-exact");
        assert_eq!(page.items.len(), 2);
        assert!(page.has_more);
        assert_eq!(
            f.host
                .ceabox_search("xm fj", CeaBoxSource::Clipboard, 0, 2)
                .unwrap()
                .total,
            300
        );
    }

    #[test]
    fn progressive_history_publishes_a_small_first_batch_and_cancels_the_scan() {
        let f = fixture();
        let records: Vec<_> = (0..400)
            .map(|index| {
                let mut item = ClipRecord::new_text(format!("项目附件 {index}"), "sample.exe");
                item.id = format!("stream-{index}");
                item
            })
            .collect();
        f.host.store().lock().insert_clips(&records).unwrap();
        let cancelled = AtomicBool::new(false);
        let mut updates = Vec::new();
        let result = f
            .host
            .ceabox_search_stream(
                "项目",
                CeaBoxSource::Clipboard,
                0,
                20,
                || cancelled.load(Ordering::Acquire),
                |update| {
                    assert!(!update.complete);
                    assert_eq!(update.page.total, 32);
                    updates.push(update);
                    cancelled.store(true, Ordering::Release);
                },
            )
            .unwrap();
        assert!(result.is_none());
        assert_eq!(updates.len(), 1);
        let cache = f.host.inner.lock().ceabox.search_cache.clone();
        assert_eq!(cache.lock().document_count(), 32);
        assert!(cache.lock().page_count() == 0);
    }

    #[test]
    fn cached_ranking_serves_later_pages_without_decoding_or_matching_again() {
        let f = fixture();
        let records: Vec<_> = (0..300)
            .map(|index| {
                let mut item = ClipRecord::new_text(format!("项目附件 {index}"), "sample.exe");
                item.id = format!("page-{index}");
                item.create_time = Utc::now() + chrono::Duration::seconds(index);
                item
            })
            .collect();
        f.host.store().lock().insert_clips(&records).unwrap();
        let whole = f
            .host
            .ceabox_search("项目", CeaBoxSource::Clipboard, 0, 100)
            .unwrap();
        let cache = f.host.inner.lock().ceabox.search_cache.clone();
        {
            // Removing the optional document index makes rescans observable;
            // the independently retained complete ranking must serve this page.
            let mut cache = cache.lock();
            assert_eq!(cache.page_count(), 1);
            cache.clear_document_index();
        }
        let next = f
            .host
            .ceabox_search("项目", CeaBoxSource::Clipboard, 40, 20)
            .unwrap();
        assert_eq!(next.total, whole.total);
        assert_eq!(next.items, whole.items[40..60]);
        assert!(cache.lock().document_count() == 0);
        assert!(!cache.lock().has_keys());
    }

    #[test]
    fn changing_store_or_hidden_records_invalidates_a_cached_search_page() {
        let f = fixture();
        insert_clip(&f.host, "first", "项目", ClipKind::TextClipItem);
        assert_eq!(
            f.host
                .ceabox_search("项目", CeaBoxSource::Clipboard, 0, 20)
                .unwrap()
                .total,
            1
        );
        insert_clip(&f.host, "second", "项目", ClipKind::TextClipItem);
        assert_eq!(
            f.host
                .ceabox_search("项目", CeaBoxSource::Clipboard, 0, 20)
                .unwrap()
                .total,
            2
        );
        f.host
            .inner
            .lock()
            .deferred_hidden
            .lock()
            .insert("second".into());
        let page = f
            .host
            .ceabox_search("项目", CeaBoxSource::Clipboard, 0, 20)
            .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].id, "first");
    }

    #[test]
    fn obsolete_search_cancels_while_waiting_for_the_store_lock() {
        let f = fixture();
        let store = f.host.store();
        let locked = store.lock();
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel = cancelled.clone();
        let host = f.host.clone();
        let (sender, receiver) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            sender
                .send(host.ceabox_search_stream(
                    "project",
                    CeaBoxSource::Clipboard,
                    0,
                    20,
                    || cancel.load(Ordering::Acquire),
                    |_| panic!("cancelled search must not publish"),
                ))
                .unwrap();
        });
        cancelled.store(true, Ordering::Release);
        assert!(receiver
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap()
            .is_none());
        drop(locked);
        worker.join().unwrap();
    }

    #[test]
    fn no_match_publishes_only_a_final_empty_page_and_large_text_stays_complete() {
        let f = fixture();
        let text = format!("{}hidden-tail-needle", "ordinary words ".repeat(6_000));
        insert_clip(&f.host, "large", &text, ClipKind::TextClipItem);
        let mut updates = Vec::new();
        let page = f
            .host
            .ceabox_search_stream(
                "tail-needle",
                CeaBoxSource::Clipboard,
                0,
                20,
                || false,
                |update| updates.push(update),
            )
            .unwrap()
            .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].id, "large");
        assert!(updates.last().unwrap().complete);
        let cache = f.host.inner.lock().ceabox.search_cache.clone();
        assert!(!cache.lock().has_document("large"));
        updates.clear();
        let page = f
            .host
            .ceabox_search_stream(
                "unfindable-qzx",
                CeaBoxSource::Clipboard,
                0,
                20,
                || false,
                |update| updates.push(update),
            )
            .unwrap()
            .unwrap();
        assert_eq!(page.total, 0);
        assert_eq!(updates.len(), 1);
        assert!(updates[0].complete);
    }

    async fn assert_slow_action_source_streaming(
        cancel: bool,
        lock_local: bool,
        source: CeaBoxSource,
    ) {
        use crate::pipe_rpc::{encode_frame, PipeRpcMessage};
        use tokio::io::AsyncWriteExt;

        async fn receive(peer: &mut tokio::io::DuplexStream) -> PipeRpcMessage {
            tokio::time::timeout(
                Duration::from_secs(2),
                crate::quicker_rpc::read_message(peer),
            )
            .await
            .unwrap()
            .unwrap()
            .unwrap()
        }
        async fn reply(peer: &mut tokio::io::DuplexStream, id: i64, payload: Value) {
            peer.write_all(
                &encode_frame(&PipeRpcMessage {
                    version: 1,
                    type_name: "response".into(),
                    id,
                    method: None,
                    payload: Some(payload),
                    error_code: None,
                    error_message: None,
                })
                .unwrap(),
            )
            .await
            .unwrap();
        }

        let f = fixture();
        insert_clip(&f.host, "local-first", "项目", ClipKind::TextClipItem);
        let (server, mut peer) = tokio::io::duplex(4096);
        let serving = tokio::spawn(crate::quicker_rpc::serve(
            server,
            f.host.quicker_rpc(),
            "ceabox-slow-action-memory-only",
            Arc::new(crate::pipe_rpc::StopSignal::default()),
            Arc::new(|_| None),
        ));
        let ping = receive(&mut peer).await;
        assert_eq!(ping.method.as_deref(), Some("quicker.ping"));
        reply(&mut peer, ping.id, json!("pong")).await;
        tokio::time::timeout(Duration::from_secs(2), async {
            while f.host.quicker_rpc.select().is_none() {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancellation = cancelled.clone();
        let local_store = f.host.store();
        let mut local_lock = lock_local.then(|| local_store.lock());
        let host = f.host.clone();
        let (updates, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let search = tokio::task::spawn_blocking(move || {
            host.ceabox_search_stream(
                "项目",
                source,
                0,
                20,
                || cancellation.load(Ordering::Acquire),
                |update| {
                    updates.send(update).unwrap();
                },
            )
        });
        let action = receive(&mut peer).await;
        assert_eq!(action.method.as_deref(), Some("quicker.searchActions"));
        if lock_local {
            reply(
                &mut peer,
                action.id,
                json!({"Items":[{"Id":"real-action","Title":"项目","Score":1000}],
                "Total":1,"HasMore":false}),
            )
            .await;
            let first = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(first.page.items.iter().any(|item| item.id == "real-action"));
            if source == CeaBoxSource::Action {
                let page = tokio::time::timeout(Duration::from_secs(1), search)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap()
                    .unwrap();
                assert_eq!(page.total, 1);
            } else {
                assert!(!first.complete);
                assert!(!search.is_finished());
                drop(local_lock.take());
                let page = search.await.unwrap().unwrap().unwrap();
                assert_eq!(page.total, 2);
                assert!(page.items.iter().any(|item| item.id == "local-first"));
            }
            drop(local_lock.take());
            drop(peer);
            serving.await.unwrap().unwrap();
            return;
        }
        let first = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(!first.complete);
        assert!(first.page.items.iter().any(|item| item.id == "local-first"));
        assert!(
            !search.is_finished(),
            "local page must not wait for the action reply"
        );
        if cancel {
            cancelled.store(true, Ordering::Release);
            let frame = receive(&mut peer).await;
            assert_eq!(frame.type_name, "cancel");
            assert_eq!(frame.id, action.id);
            assert!(search.await.unwrap().unwrap().is_none());
            while let Ok(update) = receiver.try_recv() {
                assert!(!update.complete);
            }
            assert!(
                tokio::time::timeout(
                    Duration::from_millis(30),
                    crate::quicker_rpc::read_message(&mut peer)
                )
                .await
                .is_err(),
                "cancelled queries must not request another action page"
            );
        } else {
            reply(
                &mut peer,
                action.id,
                json!({"Items":[{"Id":"real-action","Title":"项目","Score":1000}],
                "Total":1,"HasMore":false}),
            )
            .await;
            let page = search.await.unwrap().unwrap().unwrap();
            assert_eq!(page.total, 2);
            assert!(page.items.iter().any(|item| item.id == "real-action"));
            let mut complete = false;
            while let Ok(update) = receiver.try_recv() {
                complete |= update.complete;
            }
            assert!(complete);
        }
        drop(peer);
        serving.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn local_partial_results_arrive_while_the_action_source_is_blocked() {
        assert_slow_action_source_streaming(false, false, CeaBoxSource::All).await;
    }

    #[tokio::test]
    async fn cancelling_the_slow_action_source_retires_its_rpc_and_final_snapshot() {
        assert_slow_action_source_streaming(true, false, CeaBoxSource::All).await;
    }

    #[tokio::test]
    async fn action_results_arrive_while_the_local_store_is_blocked() {
        assert_slow_action_source_streaming(false, true, CeaBoxSource::All).await;
    }

    #[tokio::test]
    async fn action_scope_finishes_without_waiting_for_the_local_store() {
        assert_slow_action_source_streaming(false, true, CeaBoxSource::Action).await;
    }

    #[test]
    fn empty_aggregate_uses_local_content_and_no_callback_requirement() {
        let f = fixture();
        insert_clip(&f.host, "recent", "最近记录", ClipKind::TextClipItem);
        let page = f.host.ceabox_search("", CeaBoxSource::All, 0, 20).unwrap();
        assert_eq!(page.items[0].id, "recent");
        assert_eq!(page.total, 1);
    }

    #[test]
    fn empty_overview_reserves_history_space_when_many_favorites_exist() {
        let f = fixture();
        let clip = insert_clip(&f.host, "recent", "最近记录", ClipKind::TextClipItem);
        for index in 0..20 {
            let mut item = FavorRecord::from_clip(&clip, None);
            item.id = format!("favorite-{index}");
            f.host.store().lock().insert_favor(&item).unwrap();
        }
        let page = f.host.ceabox_search("", CeaBoxSource::All, 0, 10).unwrap();
        assert_eq!(
            page.items
                .iter()
                .filter(|item| item.source == CeaBoxSource::Favorite)
                .count(),
            4
        );
        assert!(page.items.iter().any(|item| item.id == "recent"));
        assert_eq!(
            f.host
                .ceabox_search("", CeaBoxSource::Favorite, 0, 100)
                .unwrap()
                .total,
            20
        );
    }

    #[test]
    fn every_presentation_or_selection_retirement_rejects_a_queued_effect() {
        for retire in ["hide", "show", "hwnd", "selection", "new-ticket", "source"] {
            let f = fixture();
            let request = f.host.capture_native_ceabox_execute().unwrap();
            match retire {
                "hide" => f.host.ceabox_hide(),
                "show" => f.host.ceabox_show(CeaBoxSource::All, None),
                "hwnd" => f.host.ceabox_set_hwnd(701),
                "selection" => f.host.ceabox_retire_execution(),
                "new-ticket" => {
                    f.host.capture_native_ceabox_execute().unwrap();
                }
                "source" => f.host.ceabox_set_source(CeaBoxSource::Favorite),
                _ => unreachable!(),
            }
            assert_eq!(
                f.host
                    .execute_native_ceabox(&request, &target(CeaBoxSource::Action, "never-run"))
                    .unwrap_err(),
                RETIRED,
                "{retire}"
            );
        }
    }

    #[test]
    fn ticket_cannot_be_borrowed_by_another_host_and_new_show_drops_old_commands() {
        let f = fixture();
        let other = fixture();
        let request = f.host.capture_native_ceabox_execute().unwrap();
        assert_eq!(
            other
                .host
                .execute_native_ceabox(&request, &target(CeaBoxSource::Action, "never-run"))
                .unwrap_err(),
            RETIRED
        );
        f.host.ceabox_hide();
        f.host.ceabox_show(CeaBoxSource::Favorite, None);
        let commands: Vec<_> = std::iter::from_fn(|| f.host.take_chrome_command())
            .filter(|command| {
                matches!(
                    command,
                    ChromeCommand::ShowCeaBox | ChromeCommand::HideCeaBox
                )
            })
            .collect();
        assert_eq!(commands, [ChromeCommand::ShowCeaBox]);
        assert!(f.host.ceabox_native_snapshot().generation > request.generation);
    }

    #[test]
    fn ceabox_none_preserves_visible_and_hidden_state_without_retiring_a_ticket() {
        let f = fixture();
        f.host.ceabox_show(CeaBoxSource::Favorite, None);
        f.host.ceabox_set_pinned(true);
        let request = f.host.capture_native_ceabox_execute().unwrap();
        while f.host.take_chrome_command().is_some() {}
        let before = f.host.ceabox_native_snapshot();
        f.host.execute_ceabox_verb(VerbAction::None, "all".into(), None).unwrap();
        let after = f.host.ceabox_native_snapshot();
        assert_eq!(after.generation, before.generation);
        assert_eq!(after.source, CeaBoxSource::Favorite);
        assert_eq!(after.pinned, before.pinned);
        assert!(after.visible);
        f.host.verify_ceabox_request(&request).unwrap();
        assert!(f.host.take_chrome_command().is_none());

        f.host.ceabox_hide();
        while f.host.take_chrome_command().is_some() {}
        let before = f.host.ceabox_native_snapshot();
        f.host.execute_ceabox_verb(VerbAction::None, "action".into(), None).unwrap();
        let after = f.host.ceabox_native_snapshot();
        assert!(!after.visible);
        assert_eq!(after.generation, before.generation);
        assert_eq!(after.source, before.source);
        assert_eq!(after.pinned, before.pinned);
        assert!(f.host.take_chrome_command().is_none());
    }

    #[test]
    fn clipboard_links_paste_and_successful_pinned_execution_keeps_its_window() {
        let f = fixture();
        insert_clip(
            &f.host,
            "clip-link",
            "https://example.invalid/",
            ClipKind::LinkClipItem,
        );
        let shell_calls = Arc::new(AtomicUsize::new(0));
        let calls = shell_calls.clone();
        f.host.set_shell_open_runner(move |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        let writes = Arc::new(AtomicUsize::new(0));
        let count = writes.clone();
        f.host.set_clipboard_ui_runner(move |job| {
            job()?;
            count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        f.host.ceabox_set_pinned(true);
        let request = f.host.capture_native_ceabox_execute().unwrap();
        f.host
            .execute_native_ceabox(&request, &target(CeaBoxSource::Clipboard, "clip-link"))
            .unwrap();
        assert_eq!(shell_calls.load(Ordering::SeqCst), 0);
        assert_eq!(writes.load(Ordering::SeqCst), 1);
        assert!(f.host.ceabox_native_snapshot().visible);
        assert!(!f.host.ceabox_native_snapshot().executing);
        assert_eq!(
            f.host
                .execute_native_ceabox(&request, &target(CeaBoxSource::Clipboard, "clip-link"))
                .unwrap_err(),
            RETIRED
        );
        assert_eq!(
            writes.load(Ordering::SeqCst),
            1,
            "a pinned presentation cannot replay an accepted ticket"
        );
        f.host.ceabox_set_pinned(false);
        let request = f.host.capture_native_ceabox_execute().unwrap();
        f.host
            .execute_native_ceabox(&request, &target(CeaBoxSource::Clipboard, "clip-link"))
            .unwrap();
        assert!(!f.host.ceabox_native_snapshot().visible);
    }

    #[test]
    fn favorite_links_open_without_requesting_an_input_receiver() {
        let f = fixture();
        let clip = ClipRecord {
            kind: ClipKind::LinkClipItem,
            ..ClipRecord::new_text("https://example.invalid/", "")
        };
        let mut favor = FavorRecord::from_clip(&clip, None);
        favor.id = "favorite-link".into();
        f.host.store().lock().insert_favor(&favor).unwrap();
        f.host.inner.lock().ceabox.target_hwnd = 0;
        let opened = Arc::new(Mutex::new(Vec::new()));
        let calls = opened.clone();
        f.host.set_shell_open_runner(move |url| {
            calls.lock().push(url.to_owned());
            Ok(())
        });
        f.host
            .set_input_runner(|_| panic!("favorite link must not request input"));
        let request = f.host.capture_native_ceabox_execute().unwrap();
        f.host
            .execute_native_ceabox(&request, &target(CeaBoxSource::Favorite, "favorite-link"))
            .unwrap();
        assert_eq!(*opened.lock(), ["https://example.invalid/"]);
    }

    #[test]
    fn favorite_action_executes_its_original_action_id_and_parameters() {
        let f = fixture();
        let clip = ClipRecord {
            kind: ClipKind::ActionClipItem,
            execute_param: "first\nsecond".into(),
            ..ClipRecord::new_text("{\"Id\":\"stored-action-id\",\"Title\":\"项目动作\"}", "")
        };
        let mut favor = FavorRecord::from_clip(&clip, None);
        favor.id = "favorite-action".into();
        f.host.store().lock().insert_favor(&favor).unwrap();
        let opened = Arc::new(Mutex::new(Vec::new()));
        let calls = opened.clone();
        f.host.set_shell_open_runner(move |uri| {
            calls.lock().push(uri.to_owned());
            Ok(())
        });
        f.host
            .set_input_runner(|_| panic!("favorite action must not paste"));
        let request = f.host.capture_native_ceabox_execute().unwrap();
        f.host
            .execute_native_ceabox(&request, &target(CeaBoxSource::Favorite, "favorite-action"))
            .unwrap();
        assert_eq!(
            *opened.lock(),
            ["quicker:runaction:stored-action-id?first\nsecond"]
        );
    }

    #[test]
    fn retirement_on_the_clipboard_dispatcher_prevents_write_and_late_input() {
        let f = fixture();
        insert_clip(&f.host, "clip", "do not write", ClipKind::TextClipItem);
        let retire = f.host.clone();
        let writes = Arc::new(AtomicUsize::new(0));
        let count = writes.clone();
        f.host.set_clipboard_ui_runner(move |job| {
            retire.ceabox_hide();
            retire.ceabox_show(CeaBoxSource::All, None);
            job()?;
            count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        let request = f.host.capture_native_ceabox_execute().unwrap();
        assert_eq!(
            f.host
                .execute_native_ceabox(&request, &target(CeaBoxSource::Clipboard, "clip"))
                .unwrap_err(),
            RETIRED
        );
        assert_eq!(writes.load(Ordering::SeqCst), 0);
        assert!(
            f.host.ceabox_native_snapshot().visible,
            "failed old work must not close the new presentation"
        );
    }

    #[test]
    fn ceabox_is_an_own_window_and_cannot_replace_the_external_target() {
        let f = fixture();
        f.host.on_foreground_hwnd(700);
        assert_eq!(f.host.inner.lock().last_non_host_fg, 42);
        assert!(foreground_is_own_window(&f.host.inner.lock(), 700));
        f.host.ceabox_show(
            CeaBoxSource::All,
            Some(&VerbRunContext {
                target_window_handle: 700,
                ..Default::default()
            }),
        );
        assert_eq!(f.host.inner.lock().ceabox.target_hwnd, 42);
    }

    #[test]
    fn ceabox_action_retired_while_waiting_for_store_authority_never_launches() {
        let f = fixture();
        let launched = Arc::new(AtomicUsize::new(0));
        let count = launched.clone();
        f.host.set_shell_open_runner(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        let request = f.host.capture_native_ceabox_execute().unwrap();
        let store = f.host.store();
        let locked = store.lock();
        let host = f.host.clone();
        let worker = thread::spawn(move || {
            host.execute_native_ceabox(&request, &target(CeaBoxSource::Action, "must-not-run"))
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while !f.host.ceabox_native_snapshot().executing {
            assert!(
                Instant::now() < deadline,
                "worker did not reach its authority check"
            );
            thread::sleep(Duration::from_millis(1));
        }
        f.host.ceabox_hide();
        drop(locked);
        assert_eq!(worker.join().unwrap().unwrap_err(), RETIRED);
        assert_eq!(launched.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn ceabox_file_execution_rechecks_lifetime_between_each_shell_open() {
        let f = fixture();
        let mut clip = ClipRecord::new_text("", "");
        clip.kind = ClipKind::FileClipItem;
        clip.file_paths = vec!["first.fake".into(), "second.fake".into()];
        let mut favor = FavorRecord::from_clip(&clip, None);
        favor.id = "file-favorite".into();
        f.host.store().lock().insert_favor(&favor).unwrap();
        let launched = Arc::new(Mutex::new(Vec::new()));
        let calls = launched.clone();
        let retiring = f.host.clone();
        f.host.set_shell_open_runner(move |path| {
            calls.lock().push(path.to_owned());
            retiring.ceabox_hide();
            Ok(())
        });
        let request = f.host.capture_native_ceabox_execute().unwrap();
        let error = f
            .host
            .execute_native_ceabox(&request, &target(CeaBoxSource::Favorite, "file-favorite"))
            .unwrap_err();
        assert!(error.contains(RETIRED));
        assert_eq!(*launched.lock(), ["first.fake"]);
    }

    #[test]
    fn ceabox_peer_page_rejects_overflow_scores_missing_pages_and_catalog_changes() {
        let item = json!({"Id":"real-id", "Title":"项目", "Score":1000});
        assert!(parse_action_search_page(
            json!({"Items":[item],"Total":usize::MAX,"HasMore":true}),
            0,
            100,
            None
        )
        .is_err());
        assert!(parse_action_search_page(
            json!({"Items":[item],"Total":100,"HasMore":false}),
            0,
            100,
            None
        )
        .is_err());
        assert!(parse_action_search_page(
            json!({"Items":[item],"Total":1,"HasMore":true}),
            0,
            100,
            None
        )
        .is_err());
        assert!(parse_action_search_page(
            json!({"Items":[item],"Total":1,"HasMore":false}),
            0,
            100,
            Some(2)
        )
        .is_err());
        assert!(parse_action_search_page(
            json!({"Items":[{"Id":"real-id","Score":i32::MAX}],"Total":1,"HasMore":false}),
            0,
            100,
            None
        )
        .is_err());
        assert!(parse_action_search_page(
            json!({"Items":[item],"Total":1,"HasMore":false}),
            0,
            100,
            None
        )
        .is_ok());
        assert!(parse_action_search_page(
            json!({"Items":[],"Total":0,"HasMore":false}),
            0,
            100,
            None
        )
        .is_ok());
    }
}
