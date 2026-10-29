//! Cancellable aggregate search with versioned, payload-free in-memory indexes.

use super::*;
use crate::ceabox::CeaBoxSearchUpdate;
use crate::clip_fuzzy::ceabox_matcher::{PreparedClip, PreparedFavor, PreparedQuery};
use crate::store::{ClipListKey, StoreError};

const INDEX_DOCUMENT_LIMIT: usize = 10_000;
// This bounds retained source text, not process RAM: prepared Unicode and pinyin
// representations are lazy and can expand the original UTF-8 several times.
const INDEX_TEXT_BUDGET: usize = 8 * 1024 * 1024;
const INDEX_SINGLE_TEXT_LIMIT: usize = 64 * 1024;
const PAGE_CACHE_LIMIT: usize = 2;
const ACTION_CACHE_LIFETIME: Duration = Duration::from_millis(750);

struct ClipDocument {
    search: PreparedClip,
    item: ceabox::CeaBoxItem,
    time: i64,
}

struct FavorDocument {
    search: PreparedFavor,
    item: ceabox::CeaBoxItem,
    time: i64,
}

struct LocalPage {
    query: String,
    source: CeaBoxSource,
    hidden: HashSet<String>,
    collector: Arc<ceabox::Collector>,
    sources: Vec<CeaBoxSourceStatus>,
}

struct ActionPage {
    query: String,
    session: usize,
    at: Instant,
    items: Vec<ActionSearchItem>,
    total: usize,
}

type ActionResult = Result<Option<(Vec<ActionSearchItem>, usize)>, String>;

struct ActionStream {
    receiver: Option<mpsc::Receiver<ActionResult>>,
    collector: ceabox::Collector,
    status: Option<CeaBoxSourceStatus>,
}

impl ActionStream {
    fn poll(&mut self, query: &str) -> bool {
        let Some(receiver) = self.receiver.as_ref() else {
            return false;
        };
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return false,
            Err(mpsc::TryRecvError::Disconnected) => Err("Quicker action worker stopped.".into()),
        };
        self.receiver = None;
        match result {
            Ok(Some((items, total))) => {
                let materialized = items.len();
                for action in items {
                    let source = CeaBoxSource::Action;
                    let score =
                        action.score.unwrap_or_else(|| {
                            ceabox::field_score(&action.title, query, true)
                                .max(ceabox::field_score(&action.description, query, false))
                        });
                    let time = action
                        .last_edit_time_utc
                        .as_deref()
                        .and_then(|time| DateTime::parse_from_rfc3339(time).ok())
                        .map_or(0, |time| time.timestamp_millis());
                    self.collector.add_with_score(
                        ceabox::CeaBoxItem {
                            id: action.id.clone(),
                            source,
                            kind: "ActionClipItem".into(),
                            title: action.title,
                            context: action.context,
                            time: action.last_edit_time_utc.unwrap_or_default(),
                            is_folder: false,
                            target: CeaBoxExecutionTarget {
                                source,
                                id: action.id,
                            },
                        },
                        score,
                        time,
                    );
                }
                self.collector
                    .add_unmaterialized(total.saturating_sub(materialized));
                self.status = Some(source_status(CeaBoxSource::Action, None));
            }
            Ok(None) => {}
            Err(error) => {
                tracing::debug!(%error, "CeaBox action source unavailable");
                let message = if error.contains("method_not_found") {
                    "Quicker 插件需要更新后搜索动作"
                } else if error == "Quicker is not connected." {
                    "Quicker 未连接"
                } else {
                    "Quicker 动作搜索暂不可用"
                };
                self.status = Some(source_status(CeaBoxSource::Action, Some(message)));
            }
        }
        true
    }

    fn sources(&self, locals: &[CeaBoxSourceStatus]) -> Vec<CeaBoxSourceStatus> {
        let mut sources = locals.to_vec();
        sources.extend(self.status.clone());
        sources
    }
}

/// Owned by one HostState. Locks cover cache metadata only, never database,
/// text matching, callback waits or UI publication.
#[derive(Default)]
pub(super) struct SearchCache {
    stamp: Option<(u64, i64)>,
    roots: Vec<String>,
    favorites: Option<Arc<Vec<Arc<FavorDocument>>>>,
    keys: Option<Arc<Vec<ClipListKey>>>,
    clips: HashMap<String, Arc<ClipDocument>>,
    text_bytes: usize,
    favorite_text_bytes: usize,
    pages: VecDeque<LocalPage>,
    actions: VecDeque<ActionPage>,
}

impl SearchCache {
    fn favorite_documents(
        &self,
        stamp: (u64, i64),
        roots: &[String],
    ) -> Option<Arc<Vec<Arc<FavorDocument>>>> {
        (self.stamp == Some(stamp) && self.roots == roots)
            .then(|| self.favorites.clone())
            .flatten()
    }

    fn clip_keys(&self, stamp: (u64, i64)) -> Option<Arc<Vec<ClipListKey>>> {
        (self.stamp == Some(stamp))
            .then(|| self.keys.clone())
            .flatten()
    }

    fn clip_document(&self, stamp: (u64, i64), id: &str) -> Option<Arc<ClipDocument>> {
        (self.stamp == Some(stamp))
            .then(|| self.clips.get(id).cloned())
            .flatten()
    }

    #[cfg(test)]
    pub(super) fn document_count(&self) -> usize {
        self.clips.len()
    }

    #[cfg(test)]
    pub(super) fn page_count(&self) -> usize {
        self.pages.len()
    }

    #[cfg(test)]
    pub(super) fn has_document(&self, id: &str) -> bool {
        self.clips.contains_key(id)
    }

    #[cfg(test)]
    pub(super) fn has_keys(&self) -> bool {
        self.keys.is_some()
    }

    #[cfg(test)]
    pub(super) fn clear_document_index(&mut self) {
        self.clips.clear();
        self.keys = None;
    }

    fn validate(&mut self, stamp: (u64, i64), roots: &[String]) {
        if self.stamp != Some(stamp) {
            self.stamp = Some(stamp);
            self.favorites = None;
            self.keys = None;
            self.clips.clear();
            self.text_bytes = 0;
            self.favorite_text_bytes = 0;
            self.pages.clear();
        }
        if self.roots != roots {
            self.roots = roots.to_vec();
            self.favorites = None;
            self.text_bytes = self.text_bytes.saturating_sub(self.favorite_text_bytes);
            self.favorite_text_bytes = 0;
            self.pages.clear();
        }
    }
}

/// Waiting for the store is also cancellable: stale keystrokes must not queue
/// behind a store writer and then perform a full scan after becoming obsolete.
fn read_store<T>(
    store: &Arc<Mutex<AppStore>>,
    cancelled: &dyn Fn() -> bool,
    read: impl FnOnce(&AppStore) -> Result<T, StoreError>,
) -> Result<Option<T>, String> {
    read_store_progress(store, cancelled, read, || {})
}

fn read_store_progress<T>(
    store: &Arc<Mutex<AppStore>>,
    cancelled: &dyn Fn() -> bool,
    read: impl FnOnce(&AppStore) -> Result<T, StoreError>,
    mut progress: impl FnMut(),
) -> Result<Option<T>, String> {
    loop {
        if cancelled() {
            return Ok(None);
        }
        if let Some(store) = store.try_lock_for(Duration::from_millis(5)) {
            if cancelled() {
                return Ok(None);
            }
            return read(&store).map(Some).map_err(|error| error.to_string());
        }
        progress();
    }
}

impl HostState {
    /// Compatibility entry point; native typing uses the progressive variant.
    pub fn ceabox_search(
        &self,
        query: &str,
        source: CeaBoxSource,
        skip: usize,
        take: usize,
    ) -> Result<CeaBoxPage, String> {
        self.ceabox_search_stream(query, source, skip, take, || false, |_| {})?
            .ok_or_else(|| RETIRED.into())
    }

    /// Each update replaces the requested bounded page. Local results appear
    /// without waiting for the optional Quicker source; no callback runs under
    /// a host/store/cache lock. Cancellation returns `Ok(None)` and emits no
    /// final snapshot. The caller still owns query-generation publication guards.
    pub fn ceabox_search_stream(
        &self,
        query: &str,
        source: CeaBoxSource,
        skip: usize,
        take: usize,
        is_cancelled: impl Fn() -> bool + Sync,
        mut emit: impl FnMut(CeaBoxSearchUpdate),
    ) -> Result<Option<CeaBoxPage>, String> {
        let started = Instant::now();
        let query = normalize_query(query);
        let prepared = PreparedQuery::new(&query);
        let mut collector = ceabox::Collector::cached(&query, skip, take)?;
        if is_cancelled() {
            return Ok(None);
        }
        let (store, roots, hidden, cache) = {
            let inner = self.inner.lock();
            let mut hidden = inner.deferred_hidden.lock().clone();
            if let Some(clear) = &inner.pending_clear {
                hidden.extend(clear.ids.iter().cloned());
            }
            (
                inner.store.clone(),
                inner.extras.favor.root_children.clone(),
                hidden,
                inner.ceabox.search_cache.clone(),
            )
        };
        thread::scope(|scope| {
            let mut actions = ActionStream {
                receiver: None,
                collector: ceabox::Collector::cached(&query, 0, ceabox::MAX_PAGE_SIZE)?,
                status: None,
            };
            if source.includes(CeaBoxSource::Action) {
                if query.is_empty() && source == CeaBoxSource::All {
                    actions.status = Some(source_status(
                        CeaBoxSource::Action,
                        self.quicker_rpc
                            .select()
                            .is_none()
                            .then_some("Quicker 未连接"),
                    ));
                } else {
                    let (sender, receiver) = mpsc::sync_channel(1);
                    actions.receiver = Some(receiver);
                    let query_ref = query.as_str();
                    let cancel_ref = &is_cancelled;
                    let cache_ref = &cache;
                    let needed = collector.prefix_len();
                    scope.spawn(move || {
                        let result =
                            self.ceabox_action_prefix(query_ref, needed, cache_ref, cancel_ref);
                        let _ = sender.send(result);
                    });
                }
            }
            let mut sources = Vec::new();
            let mut last_update = started
                .checked_sub(Duration::from_secs(1))
                .unwrap_or(started);
            let mut publish = |collector: &ceabox::Collector,
                               sources: &[CeaBoxSourceStatus],
                               force: bool| {
                let action_changed = actions.poll(&query);
                if !is_cancelled()
                    && (collector.has_matches() || actions.collector.has_matches())
                    && (force
                        || action_changed
                        || last_update.elapsed() >= Duration::from_millis(16))
                {
                    emit(CeaBoxSearchUpdate {
                        page: collector
                            .snapshot_merged(Some(&actions.collector), actions.sources(sources)),
                        complete: false,
                        elapsed_ms: started.elapsed().as_millis().min(u64::MAX as u128) as u64,
                    });
                    last_update = Instant::now();
                }
            };
            // Validate the version bucket under the same short store authority
            // boundary as stamp acquisition, so an old query cannot reset a newer
            // index after pausing between these two operations.
            let (stamp, cached_page) = if source == CeaBoxSource::Action {
                ((0, 0), None)
            } else {
                let Some(result) = read_store_progress(
                    &store,
                    &is_cancelled,
                    |store| {
                        let stamp = store.change_stamp()?;
                        let mut cache = cache.lock();
                        cache.validate(stamp, &roots);
                        let page = cache
                            .pages
                            .iter()
                            .find(|page| {
                                page.query == query
                                    && page.source == source
                                    && page.hidden == hidden
                            })
                            .map(|page| (page.collector.clone(), page.sources.clone()));
                        Ok((stamp, page))
                    },
                    || publish(&collector, &sources, false),
                )?
                else {
                    return Ok(None);
                };
                result
            };
            if let Some((cached, cached_sources)) = cached_page {
                collector = cached.for_page(skip, take);
                sources = cached_sources;
                publish(&collector, &sources, true);
            } else {
                if source.includes(CeaBoxSource::Favorite) {
                    let result = (|| -> Result<bool, String> {
                        let overview = query.is_empty() && source == CeaBoxSource::All;
                        let mut overview_count = 0;
                        let mut collect = |index: usize, document: &FavorDocument| {
                            if prepared.matches_favor(&document.search)
                                && (!overview || (!document.item.is_folder && overview_count < 4))
                            {
                                overview_count += 1;
                                collector.add_with_score(
                                    document.item.clone(),
                                    prepared.score_favor(&document.search),
                                    document.time,
                                );
                            }
                            if index % 32 == 31 {
                                publish(&collector, &sources, false);
                            }
                        };
                        let cached = cache.lock().favorite_documents(stamp, &roots);
                        match cached {
                            Some(documents) => {
                                for (index, document) in documents.iter().enumerate() {
                                    if is_cancelled() {
                                        return Ok(false);
                                    }
                                    collect(index, document);
                                }
                            }
                            None => {
                                let Some(mut items) =
                                    read_store(&store, &is_cancelled, AppStore::list_favors)?
                                else {
                                    return Ok(false);
                                };
                                items.sort_by(|a, b| {
                                    b.update_time
                                        .cmp(&a.update_time)
                                        .then(a.sub_index.cmp(&b.sub_index))
                                        .then(a.id.cmp(&b.id))
                                });
                                let contexts = ceabox::favorite_contexts(&items, &roots);
                                let mut documents = Vec::with_capacity(items.len());
                                let mut text_bytes = 0usize;
                                let mut cacheable = true;
                                for (index, item) in items.iter().enumerate() {
                                    if is_cancelled() {
                                        return Ok(false);
                                    }
                                    let item_bytes = item
                                        .name
                                        .len()
                                        .saturating_add(
                                            item.body_text.as_ref().map_or(0, String::len),
                                        )
                                        .saturating_add(
                                            item.file_paths.iter().map(String::len).sum::<usize>(),
                                        );
                                    text_bytes = text_bytes.saturating_add(item_bytes);
                                    if cacheable
                                        && (item_bytes > INDEX_SINGLE_TEXT_LIMIT
                                            || index >= INDEX_DOCUMENT_LIMIT
                                            || text_bytes > INDEX_TEXT_BUDGET)
                                    {
                                        cacheable = false;
                                        documents.clear();
                                    }
                                    let context = contexts
                                        .get(&item.id)
                                        .cloned()
                                        .filter(|path| !path.is_empty())
                                        .unwrap_or_else(|| "收藏".into());
                                    let document = Arc::new(FavorDocument {
                                        search: PreparedFavor::new(item),
                                        item: ceabox::favor_item(item, context),
                                        time: item.update_time.timestamp_millis(),
                                    });
                                    // Match and publish while constructing the cold
                                    // index instead of waiting for every favorite.
                                    collect(index, &document);
                                    if cacheable {
                                        documents.push(document);
                                    }
                                }
                                let mut cache = cache.lock();
                                if cacheable
                                    && cache.stamp == Some(stamp)
                                    && cache.roots == roots
                                    && cache.favorites.is_none()
                                    && documents.len() <= INDEX_DOCUMENT_LIMIT
                                    && cache.text_bytes.saturating_add(text_bytes)
                                        <= INDEX_TEXT_BUDGET
                                {
                                    cache.favorites = Some(Arc::new(documents));
                                    cache.text_bytes += text_bytes;
                                    cache.favorite_text_bytes = text_bytes;
                                }
                            }
                        }
                        Ok(true)
                    })();
                    match result {
                        Ok(false) => return Ok(None),
                        Ok(true) => sources.push(source_status(CeaBoxSource::Favorite, None)),
                        Err(error) => {
                            tracing::warn!(%error, "CeaBox favorite query failed");
                            sources
                                .push(source_status(CeaBoxSource::Favorite, Some("收藏暂不可用")));
                        }
                    }
                    publish(&collector, &sources, true);
                }
                if source.includes(CeaBoxSource::Clipboard) {
                    let result = (|| -> Result<bool, String> {
                        let cached_keys = cache.lock().clip_keys(stamp);
                        let keys = match cached_keys {
                            Some(keys) => keys,
                            None => {
                                let Some(mut keys) = read_store_progress(
                                    &store,
                                    &is_cancelled,
                                    AppStore::list_clip_keys,
                                    || publish(&collector, &sources, false),
                                )?
                                else {
                                    return Ok(false);
                                };
                                keys.sort_by(|a, b| b.create_ms.cmp(&a.create_ms));
                                let keys = Arc::new(keys);
                                let mut cache = cache.lock();
                                if cache.stamp == Some(stamp) {
                                    cache.keys = Some(keys.clone());
                                }
                                keys
                            }
                        };
                        let keys: Vec<_> = keys
                            .iter()
                            .filter(|key| !hidden.contains(&key.id))
                            .collect();
                        let mut decode_count = keys.len();
                        // Retain enough recent rows for every legal page in the
                        // overview, including all rows at its timestamp boundary.
                        let retained = ceabox::MAX_PAGE_OFFSET + ceabox::MAX_PAGE_SIZE;
                        if query.is_empty() && keys.len() > retained {
                            decode_count = retained;
                            let boundary = keys[decode_count - 1].create_ms;
                            while decode_count < keys.len()
                                && keys[decode_count].create_ms == boundary
                            {
                                decode_count += 1;
                            }
                        }
                        // A small first decode batch makes the first visible page
                        // available before the larger throughput-oriented batches.
                        let mut offset = 0;
                        while offset < decode_count {
                            if is_cancelled() {
                                return Ok(false);
                            }
                            let end =
                                (offset + if offset == 0 { 32 } else { 128 }).min(decode_count);
                            let block = &keys[offset..end];
                            let (mut documents, missing) = {
                                let cache = cache.lock();
                                let mut documents = HashMap::new();
                                let mut missing = Vec::new();
                                for key in block {
                                    if let Some(document) = cache.clip_document(stamp, &key.id) {
                                        documents.insert(key.id.as_str(), document);
                                    } else {
                                        missing.push(key.id.as_str());
                                    }
                                }
                                (documents, missing)
                            };
                            if !missing.is_empty() {
                                let Some(records) = read_store_progress(
                                    &store,
                                    &is_cancelled,
                                    |store| store.list_clips_by_ids(&missing),
                                    || publish(&collector, &sources, false),
                                )?
                                else {
                                    return Ok(false);
                                };
                                for key in block {
                                    if is_cancelled() {
                                        return Ok(false);
                                    }
                                    let Some(item) = records.get(&key.id) else {
                                        continue;
                                    };
                                    let source_bytes = item
                                        .body_text
                                        .as_ref()
                                        .map_or(0, String::len)
                                        .saturating_add(item.title.as_ref().map_or(0, String::len))
                                        .saturating_add(item.description.len())
                                        .saturating_add(
                                            item.file_paths.iter().map(String::len).sum::<usize>(),
                                        );
                                    let document = Arc::new(ClipDocument {
                                        search: PreparedClip::new(item),
                                        item: ceabox::clip_item(item),
                                        time: item.create_time.timestamp_millis(),
                                    });
                                    documents.insert(key.id.as_str(), document.clone());
                                    let mut cache = cache.lock();
                                    if cache.stamp == Some(stamp)
                                        && cache.clips.len() < INDEX_DOCUMENT_LIMIT
                                        && source_bytes <= INDEX_SINGLE_TEXT_LIMIT
                                        && cache.text_bytes.saturating_add(source_bytes)
                                            <= INDEX_TEXT_BUDGET
                                    {
                                        if cache.clips.insert(key.id.clone(), document).is_none() {
                                            cache.text_bytes += source_bytes;
                                        }
                                    }
                                }
                            }
                            for key in block {
                                if is_cancelled() {
                                    return Ok(false);
                                }
                                let Some(document) = documents.get(key.id.as_str()) else {
                                    continue;
                                };
                                if prepared.matches_clip(&document.search) {
                                    collector.add_with_score(
                                        document.item.clone(),
                                        prepared.score_clip(&document.search),
                                        document.time,
                                    );
                                }
                            }
                            offset = end;
                            publish(&collector, &sources, offset == 32);
                        }
                        collector.add_unmaterialized(keys.len() - decode_count);
                        Ok(true)
                    })();
                    match result {
                        Ok(false) => return Ok(None),
                        Ok(true) => sources.push(source_status(CeaBoxSource::Clipboard, None)),
                        Err(error) => {
                            tracing::warn!(%error, "CeaBox history query failed");
                            sources
                                .push(source_status(CeaBoxSource::Clipboard, Some("历史暂不可用")));
                        }
                    }
                    publish(&collector, &sources, true);
                }
                if is_cancelled() {
                    return Ok(None);
                }
                // Recheck the authoritative store stamp before retaining a ranking
                // that was built outside the store lock from several decode batches.
                if source != CeaBoxSource::Action
                    && read_store_progress(&store, &is_cancelled, AppStore::change_stamp, || {
                        publish(&collector, &sources, false)
                    })? == Some(stamp)
                    && sources.iter().all(|source| source.available)
                {
                    let mut cache = cache.lock();
                    if cache.stamp == Some(stamp) && cache.roots == roots {
                        cache
                            .pages
                            .retain(|page| !(page.query == query && page.source == source));
                        cache.pages.push_front(LocalPage {
                            query: query.clone(),
                            source,
                            hidden: hidden.clone(),
                            collector: Arc::new(collector.clone()),
                            sources: sources.clone(),
                        });
                        cache.pages.truncate(PAGE_CACHE_LIMIT);
                    }
                }
            }

            if is_cancelled() {
                return Ok(None);
            }
            drop(publish);
            while actions.receiver.is_some() {
                if is_cancelled() {
                    return Ok(None);
                }
                if !actions.poll(&query) {
                    thread::sleep(Duration::from_millis(5));
                }
            }
            if is_cancelled() {
                return Ok(None);
            }
            let page =
                collector.snapshot_merged(Some(&actions.collector), actions.sources(&sources));
            emit(CeaBoxSearchUpdate {
                page: page.clone(),
                complete: true,
                elapsed_ms: started.elapsed().as_millis().min(u64::MAX as u128) as u64,
            });
            Ok(Some(page))
        })
    }

    fn ceabox_action_prefix(
        &self,
        query: &str,
        needed: usize,
        cache: &Arc<Mutex<SearchCache>>,
        is_cancelled: &dyn Fn() -> bool,
    ) -> Result<Option<(Vec<ActionSearchItem>, usize)>, String> {
        let session = self
            .quicker_rpc
            .select()
            .ok_or("Quicker is not connected.")?;
        let session_key = Arc::as_ptr(&session) as usize;
        let mut items = Vec::new();
        let mut total = 0;
        {
            let mut cache = cache.lock();
            cache.actions.retain(|page| {
                page.at.elapsed() < ACTION_CACHE_LIFETIME && page.session == session_key
            });
            if let Some(page) = cache.actions.iter().find(|page| page.query == query) {
                total = page.total;
                items = page.items.clone();
                if items.len() >= needed || items.len() >= total {
                    items.truncate(needed);
                    return Ok(Some((items, total)));
                }
            }
        }
        let deadline = Instant::now() + crate::quicker_rpc::INTERACTIVE_TIMEOUT;
        let mut seen: HashSet<_> = items.iter().map(|item| item.id.clone()).collect();
        while items.len() < needed {
            if is_cancelled() {
                return Ok(None);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("Quicker action search timed out.".into());
            }
            let take = (needed - items.len()).min(500);
            let value = session.call_cancellable(
                "quicker.searchActions",
                json!({ "Query": query, "Skip": items.len(), "Take": take }),
                remaining,
                is_cancelled,
            )?;
            if is_cancelled() {
                return Ok(None);
            }
            let page = parse_action_search_page(
                value,
                items.len(),
                take,
                (!items.is_empty()).then_some(total),
            )?;
            total = page.total;
            let count = page.items.len();
            for item in page.items {
                if item.id.trim().is_empty() || !seen.insert(item.id.clone()) {
                    return Err("Invalid Quicker action search identifiers.".into());
                }
                items.push(item);
            }
            if !page.has_more || items.len() >= total {
                break;
            }
            if count == 0 {
                return Err("Quicker action search did not advance.".into());
            }
        }
        if is_cancelled() {
            return Ok(None);
        }
        let mut cache = cache.lock();
        cache
            .actions
            .retain(|page| !(page.query == query && page.session == session_key));
        cache.actions.push_front(ActionPage {
            query: query.into(),
            session: session_key,
            at: Instant::now(),
            items: items.clone(),
            total,
        });
        cache.actions.truncate(PAGE_CACHE_LIMIT);
        Ok(Some((items, total)))
    }
}

#[cfg(test)]
mod cache_version_tests {
    use super::*;

    #[test]
    fn overlapping_versions_cannot_borrow_each_others_search_documents() {
        let mut cache = SearchCache::default();
        let old = (1, 1);
        let new = (2, 1);
        let old_roots = vec!["old-root".to_string()];
        let new_roots = vec!["new-root".to_string()];
        cache.validate(old, &old_roots);
        let mut item = ClipRecord::new_text("new revision", "");
        item.id = "same-id".into();
        cache.validate(new, &new_roots);
        cache.clips.insert(
            item.id.clone(),
            Arc::new(ClipDocument {
                search: PreparedClip::new(&item),
                item: ceabox::clip_item(&item),
                time: 0,
            }),
        );
        cache.keys = Some(Arc::new(Vec::new()));
        cache.favorites = Some(Arc::new(Vec::new()));
        assert!(cache.clip_document(old, &item.id).is_none());
        assert!(cache.clip_keys(old).is_none());
        assert!(cache.favorite_documents(old, &old_roots).is_none());
        assert!(cache.favorite_documents(new, &old_roots).is_none());
        assert_eq!(
            cache.clip_document(new, &item.id).unwrap().item.title,
            "new revision"
        );
        assert!(cache.clip_keys(new).is_some());
        assert!(cache.favorite_documents(new, &new_roots).is_some());
        assert_eq!(cache.stamp, Some(new));
    }

    #[test]
    fn stamp_read_and_cache_validation_cannot_be_reordered_by_a_new_writer() {
        let parent = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
        std::fs::create_dir_all(&parent).unwrap();
        let root = tempfile::Builder::new()
            .prefix("ceabox-version-")
            .tempdir_in(parent)
            .unwrap();
        let store = Arc::new(Mutex::new(
            AppStore::open(&root.path().join("test.db")).unwrap(),
        ));
        let cache = Arc::new(Mutex::new(SearchCache::default()));
        let (old_read, observed_old) = mpsc::sync_channel(1);
        let (release, resume_old) = mpsc::sync_channel(1);
        let old_store = store.clone();
        let old_cache = cache.clone();
        let older = thread::spawn(move || {
            read_store(&old_store, &|| false, |store| {
                let stamp = store.change_stamp()?;
                old_read.send(stamp).unwrap();
                resume_old.recv_timeout(Duration::from_secs(2)).unwrap();
                old_cache.lock().validate(stamp, &[]);
                Ok(stamp)
            })
            .unwrap()
            .unwrap()
        });
        let old_stamp = observed_old.recv_timeout(Duration::from_secs(2)).unwrap();
        let (attempting, writer_started) = mpsc::sync_channel(1);
        let new_store = store.clone();
        let new_cache = cache.clone();
        let newer = thread::spawn(move || {
            attempting.send(()).unwrap();
            read_store(&new_store, &|| false, |store| {
                store.insert_clips(&[ClipRecord::new_text("new revision", "")])?;
                let stamp = store.change_stamp()?;
                new_cache.lock().validate(stamp, &[]);
                Ok(stamp)
            })
            .unwrap()
            .unwrap()
        });
        writer_started.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(!newer.is_finished());
        release.send(()).unwrap();
        assert_eq!(older.join().unwrap(), old_stamp);
        let new_stamp = newer.join().unwrap();
        assert_ne!(new_stamp, old_stamp);
        assert_eq!(cache.lock().stamp, Some(new_stamp));
    }
}
