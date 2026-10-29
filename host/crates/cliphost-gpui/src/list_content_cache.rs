//! Bounded, page-scoped resources for display-only list projections.
//!
//! Call `release_evicted` after every `advance`, `advance_retaining`, `reserve`, and `finish` on the
//! UI thread, including rejected completions. A rejected completion still frees
//! a physical worker slot, so the owner must schedule/notify after it. The byte
//! budget covers retained entry keys and the resources' conservative estimate;
//! at most four accepted workers and the short-lived release queue are separate.
//! Each entry also holds an explicit process-wide image lease. The App-side
//! retirement scheduler installed by native_content handles a final cache Drop
//! without App access; release_evicted remains required after normal mutations.
//! All default instances additionally share the process budget. Owners must
//! drain pressure on the UI pump even while hidden; no foreign thread can
//! mutate a cache or release GPUI assets on its behalf.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt::{self, Write},
    mem::size_of,
    sync::Arc,
    time::{Duration, Instant},
};

use gpui::{App, Image};

use super::{
    budget::{self, ContentBudget, ContentBudgetLease},
    ContentResources,
    image_holders::{self, ImageHolderLease},
    links::OpenTarget,
    model::{ContentMode, ContentProjection, MixedPart},
};

const MAX_PENDING: usize = 6;
const MAX_ENTRIES: usize = 128;
const MAX_RETAINED_BYTES: usize = 32 * 1024 * 1024;
// Backpressure protects against a caller forgetting UI-thread release. Already
// accepted workers can still finish, but no new work can grow this queue.
const MAX_QUEUED_IMAGES: usize = MAX_ENTRIES * 16;
const RETRY_DELAYS: [Duration; 3] = [
    Duration::from_millis(300),
    Duration::from_secs(1),
    Duration::from_secs(3),
];
// Hidden/inactive owners keep their last viewport warm for a quick reopen or
// tab switch, then return the remainder, and finally everything.
const IDLE_TRIM_AFTER: Duration = Duration::from_secs(30);
const IDLE_RELEASE_AFTER: Duration = Duration::from_secs(180);

/// Display state only. Scheduling and retry deadlines stay inside the cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentLoadStatus {
    Missing,
    Loading,
    Ready,
    Unavailable,
    RetryScheduled,
    RetryExhausted,
    MemoryLimited,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContentResourceKey {
    pub item_id: String,
    pub projection: ContentProjection,
    pub mode: ContentMode,
    pub foreground: String,
}

impl ContentResourceKey {
    fn matches(
        &self,
        item_id: &str,
        projection: &ContentProjection,
        mode: ContentMode,
        foreground: &str,
    ) -> bool {
        self.item_id == item_id
            && self.projection == *projection
            && self.mode == mode
            && self.foreground == foreground
    }

    fn retained_bytes(&self) -> usize {
        key_bytes(
            self.item_id.capacity(),
            &self.projection,
            self.foreground.capacity(),
        )
    }
}

#[derive(Clone, Debug)]
pub struct ContentLoadTicket {
    pub generation: u64,
    pub key: ContentResourceKey,
    // Never reused, including across page changes and duplicate completions.
    token: u64,
}

struct PendingLoad {
    generation: u64,
    key: ContentResourceKey,
    superseded: bool,
    retry_attempt: usize,
    // Recovered pressure fallbacks reserve their known cost against other
    // recovered workers in this cache, before they publish their resources.
    estimated_bytes: usize,
}

struct RetryState {
    attempt: usize,
    retry_at: Option<Instant>,
}

struct SuppressedResource {
    digest: [u8; 32],
    bytes: usize,
}

struct Entry {
    key: ContentResourceKey,
    resources: ContentResources,
    bytes: usize,
    _image_holder: ImageHolderLease,
    budget: ContentBudgetLease,
    retry: Option<RetryState>,
}

#[derive(Default)]
pub struct ListContentCache {
    budget: ContentBudget,
    generation: u64,
    next_token: u64,
    pending: HashMap<u64, PendingLoad>,
    entries: VecDeque<Entry>,
    retained_bytes: usize,
    evicted: HashMap<u64, Arc<Image>>,
    evicted_bytes: usize,
    // Keep the shared charge until release_evicted's deferred App barrier has
    // run after CPU/atlas cleanup. Merely requesting FIFO eviction is not free.
    evicted_budget: Vec<ContentBudgetLease>,
    // Ephemeral digests only: never retain another copy of a potentially large
    // display projection in the pressure fallback ledger. None preserves the
    // previous API for non-visual fixtures; real owners publish their viewport.
    visible_items: Option<HashSet<[u8; 32]>>,
    suppressed: HashMap<[u8; 32], SuppressedResource>,
    // Last non-empty viewport, kept through the first idle reclamation stage.
    last_visible: HashSet<[u8; 32]>,
    idle_since: Option<Instant>,
    idle_stage: u8,
}

pub type ContentResourceCache = ListContentCache;

impl ListContentCache {
    pub fn with_budget(budget: ContentBudget) -> Self {
        let mut cache = Self::default();
        cache.budget = budget;
        cache
    }

    /// Publish the active resource working set before scheduling loads. Hidden
    /// owners pass an empty iterator. A stable over-budget viewport settles on
    /// fallbacks instead of repeatedly loading pictures that it just evicted.
    /// The set is bounded by the local entry count; callers should use the same
    /// visible candidates (normally at most 32) as their load scheduler.
    pub fn set_visible_items<'a>(&mut self, items: impl IntoIterator<Item = &'a str>) {
        self.set_visible_items_at(items, Instant::now());
    }

    fn set_visible_items_at<'a>(
        &mut self,
        items: impl IntoIterator<Item = &'a str>,
        now: Instant,
    ) {
        let visible: HashSet<_> = items.into_iter().take(MAX_ENTRIES).map(item_digest).collect();
        if !visible.is_empty() {
            self.idle_since = None;
            self.idle_stage = 0;
            if self.last_visible != visible {
                self.last_visible = visible.clone();
            }
        }
        if self.visible_items.as_ref() != Some(&visible) {
            self.suppressed.clear();
            for entry in &mut self.entries {
                let item = item_digest(&entry.key.item_id);
                if visible.contains(&item)
                    && self.visible_items.as_ref().is_some_and(|old| !old.contains(&item))
                {
                    reset_retry(&mut entry.retry, now);
                }
            }
            // Capacity eviction pops the front: keep the working set at the
            // back so a long scroll retires rows that left it first, not
            // pictures still on screen or just above/below it.
            if !visible.is_empty() {
                let (kept, others): (VecDeque<_>, VecDeque<_>) = std::mem::take(&mut self.entries)
                    .into_iter()
                    .partition(|entry| visible.contains(&item_digest(&entry.key.item_id)));
                self.entries = others;
                self.entries.extend(kept);
            }
            self.visible_items = Some(visible);
        }
    }

    /// Consume only this owner's revoked entries. Returns true when its view
    /// should be notified to replace retired images and retry visible loads.
    /// Follow with release_evicted on the owning UI thread, also while hidden.
    pub fn apply_budget_pressure(&mut self) -> bool {
        let mut changed = false;
        let mut at = 0;
        while at < self.entries.len() {
            if self.entries[at].budget.revoked() {
                let entry = self.entries.remove(at).unwrap();
                self.evict_visible_entry(entry);
                changed = true;
            } else {
                at += 1;
            }
        }
        changed
    }

    pub fn advance(&mut self, generation: u64) {
        self.apply_budget_pressure();
        if self.generation == generation {
            return;
        }
        self.generation = generation;
        self.suppressed.clear();
        while let Some(entry) = self.entries.pop_front() {
            self.remove_entry(entry);
        }
        for load in self.pending.values_mut() {
            load.superseded = true;
        }
    }

    /// Advance the owner snapshot while retaining only completed resources the
    /// caller has confirmed still belong to unchanged rows. `keep` must verify
    /// the full source/render identity, not merely a projection or image path:
    /// a changed image can retain the same path. Use `advance` for an explicit
    /// reload that must discard such resources.
    ///
    /// Surviving entries move intact, preserving their resource Arcs, image
    /// leases, byte accounting and FIFO age. Every pending load is retired even
    /// if its key or numerical generation reappears; it still owns its physical
    /// slot until completion. Call `release_evicted` after this mutation.
    pub fn advance_retaining(
        &mut self,
        generation: u64,
        mut keep: impl FnMut(&ContentResourceKey) -> bool,
    ) {
        self.advance_retaining_at(generation, &mut keep, Instant::now());
    }

    fn advance_retaining_at(
        &mut self,
        generation: u64,
        mut keep: impl FnMut(&ContentResourceKey) -> bool,
        now: Instant,
    ) {
        self.apply_budget_pressure();
        let new_query = self.generation != generation;
        self.generation = generation;
        self.suppressed.clear();
        let entries = std::mem::take(&mut self.entries);
        for mut entry in entries {
            if keep(&entry.key) {
                if new_query {
                    reset_retry(&mut entry.retry, now);
                }
                self.entries.push_back(entry);
            } else {
                self.remove_entry(entry);
            }
        }
        for load in self.pending.values_mut() {
            load.superseded = true;
        }
    }

    /// Drop completed entries rejected by `keep` without changing the owner
    /// snapshot (e.g. a display setting changed). Pending loads are retired
    /// because their inputs may be stale. Call `release_evicted` afterwards.
    pub fn retain(&mut self, keep: impl FnMut(&ContentResourceKey) -> bool) {
        let generation = self.generation;
        self.advance_retaining(generation, keep);
    }

    /// Owner tick while hidden or inactive, after publishing an empty viewport.
    /// After a short grace only the last viewport stays warm; after a longer
    /// absence every entry (and its decoded/atlas memory) is released. Returns
    /// true when entries were removed; call `release_evicted` afterwards.
    pub fn reclaim_idle(&mut self) -> bool {
        self.reclaim_idle_at(Instant::now())
    }

    /// The owner's next one-shot reclamation. Empty/active caches never keep
    /// the UI pump alive, and the second stage is measured from the same hide.
    pub fn next_idle_deadline(&self) -> Option<Instant> {
        if self.entries.is_empty() || self.visible_items.as_ref().is_none_or(|items| !items.is_empty()) {
            return None;
        }
        let after = match self.idle_stage {
            0 => IDLE_TRIM_AFTER,
            1 => IDLE_RELEASE_AFTER,
            _ => return None,
        };
        self.idle_since.map(|since| since + after)
    }

    fn reclaim_idle_at(&mut self, now: Instant) -> bool {
        let idle = now.saturating_duration_since(*self.idle_since.get_or_insert(now));
        let stage = if idle >= IDLE_RELEASE_AFTER {
            2
        } else if idle >= IDLE_TRIM_AFTER {
            1
        } else {
            0
        };
        if stage <= self.idle_stage {
            return false;
        }
        self.idle_stage = stage;
        let before = self.entries.len();
        let keep = if stage == 1 { std::mem::take(&mut self.last_visible) } else { HashSet::new() };
        self.retain(|key| keep.contains(&item_digest(&key.item_id)));
        if stage == 1 {
            self.last_visible = keep;
        }
        self.entries.len() != before
    }

    pub fn get(
        &self,
        generation: u64,
        item_id: &str,
        projection: &ContentProjection,
        mode: ContentMode,
        foreground: &str,
    ) -> Option<&ContentResources> {
        if generation != self.generation {
            return None;
        }

        self.entries
            .iter()
            .find(|entry| {
                !entry.budget.revoked()
                    && entry.key.matches(item_id, projection, mode, foreground)
            })
            .map(|entry| &entry.resources)
    }

    pub fn reserve(
        &mut self,
        generation: u64,
        item_id: &str,
        projection: &ContentProjection,
        mode: ContentMode,
        foreground: &str,
    ) -> Option<ContentLoadTicket> {
        self.reserve_at(generation, item_id, projection, mode, foreground, Instant::now())
    }

    fn reserve_at(
        &mut self,
        generation: u64,
        item_id: &str,
        projection: &ContentProjection,
        mode: ContentMode,
        foreground: &str,
        now: Instant,
    ) -> Option<ContentLoadTicket> {
        self.apply_budget_pressure();
        if generation != self.generation {
            return None;
        }

        let item = item_digest(item_id);
        if self.visible_items.as_ref().is_some_and(|visible| !visible.contains(&item)) {
            return None;
        }
        let mut estimated_bytes = 0;
        if let Some(previous) = self.suppressed.get(&item) {
            if previous.digest == resource_digest(item_id, projection, mode, foreground) {
                let pending_bytes: usize = self.pending.values()
                    .filter(|load| !load.superseded)
                    .map(|load| load.estimated_bytes)
                    .sum();
                let shared = self.budget.snapshot();
                // Re-evaluate after actual cleanup, without evicting another
                // still-visible picture just to reload this one. A viewport
                // larger than its budget therefore remains stable.
                if self.entries.len() >= MAX_ENTRIES
                    || self.retained_bytes.saturating_add(pending_bytes)
                        .saturating_add(previous.bytes) > MAX_RETAINED_BYTES
                    || shared.retained_bytes.saturating_add(pending_bytes)
                        .saturating_add(previous.bytes) > shared.limit_bytes
                {
                    return None;
                }
                estimated_bytes = previous.bytes;
            }
        }

        // Invalidate before checking capacity: even a request that cannot start
        // must retire old work. K1 -> K2 -> K1 never revives the first K1 ticket.
        let mut at = 0;
        while at < self.entries.len() {
            let key = &self.entries[at].key;
            if key.item_id == item_id && !key.matches(item_id, projection, mode, foreground) {
                let entry = self.entries.remove(at).unwrap();
                self.remove_entry(entry);
            } else {
                at += 1;
            }
        }
        for load in self.pending.values_mut() {
            if load.key.item_id == item_id
                && (load.generation != generation
                    || !load.key.matches(item_id, projection, mode, foreground))
            {
                load.superseded = true;
            }
        }

        let mut retry_attempt = 0;
        let retry_entry = self.entries.iter().position(|entry| {
            !entry.budget.revoked() && entry.key.matches(item_id, projection, mode, foreground)
        });
        if let Some(index) = retry_entry {
            let Some(retry) = self.entries[index].retry.as_ref() else { return None; };
            if !retry.retry_at.is_some_and(|deadline| now >= deadline) {
                return None;
            }
            retry_attempt = retry.attempt + 1;
        }
        if self.pending.values().any(|load| {
                !load.superseded
                    && load.generation == generation
                    && load.key.matches(item_id, projection, mode, foreground)
            })
            || self.pending.len() >= MAX_PENDING
            || self.evicted.len() >= MAX_QUEUED_IMAGES
            || self.evicted_bytes >= MAX_RETAINED_BYTES
            // Do not continuously decode/reject while a different (possibly
            // hidden) owner is awaiting its UI cleanup barrier.
            || self.budget.pressure_pending()
        {
            return None;
        }

        // Check before cloning an arbitrarily large projection. Such a key
        // cannot retain even a negative result within the entry budget.
        let minimum_resources = ContentResources::default()
            .retained_bytes()
            .saturating_add(ImageHolderLease::estimate_retained_bytes(0));
        if key_bytes(item_id.len(), projection, foreground.len()).saturating_add(minimum_resources)
            > MAX_RETAINED_BYTES
        {
            return None;
        }
        let token = self.next_token.checked_add(1)?;
        let key = ContentResourceKey {
            item_id: item_id.to_owned(),
            projection: projection.clone(),
            mode,
            foreground: foreground.to_owned(),
        };
        if let Some(index) = retry_entry {
            let entry = self.entries.remove(index).unwrap();
            self.remove_entry(entry);
        }
        // A new source never inherits the old fallback. For the same source,
        // keep its pressure cost until a physical worker slot is accepted.
        self.suppressed.remove(&item);
        self.next_token = token;
        self.pending.insert(
            token,
            PendingLoad {
                generation,
                key: key.clone(),
                superseded: false,
                retry_attempt,
                estimated_bytes,
            },
        );
        Some(ContentLoadTicket {
            generation,
            key,
            token,
        })
    }

    /// Consume a physical reservation exactly once. `current` is the owner's
    /// additional session/row guard. Default resources are a completed negative
    /// result, preventing missing/invalid resources from loading every render.
    pub fn finish(
        &mut self,
        ticket: ContentLoadTicket,
        resources: ContentResources,
        current: bool,
    ) -> bool {
        self.finish_at(ticket, resources, current, false, Instant::now())
    }

    /// Explicit transient failure. At most three visible retries follow the
    /// initial load; permanent unavailable resources must keep using `finish`.
    /// Re-entering the viewport or a new query restarts the bounded sequence.
    pub fn finish_retryable(
        &mut self,
        ticket: ContentLoadTicket,
        resources: ContentResources,
        current: bool,
    ) -> bool {
        self.finish_at(ticket, resources, current, true, Instant::now())
    }

    fn finish_at(
        &mut self,
        ticket: ContentLoadTicket,
        mut resources: ContentResources,
        current: bool,
        retryable: bool,
        now: Instant,
    ) -> bool {
        self.apply_budget_pressure();
        let Some(load) = self.pending.remove(&ticket.token) else {
            return false;
        };
        if !current
            || load.superseded
            || load.generation != self.generation
            || load.generation != ticket.generation
            || load.key != ticket.key
            || self.visible_items.as_ref().is_some_and(|visible| {
                !visible.contains(&item_digest(&load.key.item_id))
            })
        {
            return false;
        }

        let key_bytes = load.key.retained_bytes();
        let resource_budget = MAX_RETAINED_BYTES.saturating_sub(key_bytes);
        let mut resource_bytes = resources.retained_bytes();
        if resource_bytes > resource_budget {
            resources.limit_retained_bytes(resource_budget);
            resource_bytes = resources.retained_bytes();
        }
        if resource_bytes > resource_budget {
            resources = ContentResources::default();
            resource_bytes = resources.retained_bytes();
        }
        // Claim incoming ids before retiring older entries, including equal ids
        // backed by independently allocated Arcs. Account for lease allocations.
        let mut image_holder = ImageHolderLease::new(resources.images_for_release());
        resource_bytes = resource_bytes.saturating_add(image_holder.metadata_bytes());
        if resource_bytes > resource_budget {
            resources.limit_retained_bytes(
                resource_budget.saturating_sub(image_holder.metadata_bytes()),
            );
            // Acquire the surviving claims before dropping the earlier lease.
            image_holder = ImageHolderLease::new(resources.images_for_release());
            resource_bytes = resources
                .retained_bytes()
                .saturating_add(image_holder.metadata_bytes());
        }
        if resource_bytes > resource_budget {
            resources = ContentResources::default();
            image_holder = ImageHolderLease::new(Vec::new());
            resource_bytes = resources.retained_bytes();
        }
        let bytes = key_bytes.saturating_add(resource_bytes);
        if bytes > MAX_RETAINED_BYTES {
            return false;
        }

        while self.entries.len() >= MAX_ENTRIES
            || self.retained_bytes.saturating_add(bytes) > MAX_RETAINED_BYTES
        {
            let Some(entry) = self.entries.pop_front() else {
                break;
            };
            self.evict_visible_entry(entry);
        }
        let Some(budget) = self.budget.try_acquire(bytes) else {
            // Do not decode the same rejected image on every maintenance
            // pass. Its known cost can be retried after real space returns.
            self.suppress_key(&load.key, bytes);
            self.apply_budget_pressure();
            return false;
        };
        self.retained_bytes = self.retained_bytes.saturating_add(bytes);
        self.entries.push_back(Entry {
            key: load.key,
            resources,
            bytes,
            _image_holder: image_holder,
            budget,
            retry: retryable.then(|| RetryState {
                attempt: load.retry_attempt,
                retry_at: RETRY_DELAYS.get(load.retry_attempt).map(|delay| now + *delay),
            }),
        });
        true
    }

    pub fn load_status(
        &self,
        generation: u64,
        item_id: &str,
        projection: &ContentProjection,
        mode: ContentMode,
        foreground: &str,
    ) -> ContentLoadStatus {
        if generation != self.generation {
            return ContentLoadStatus::Missing;
        }
        if self.pending.values().any(|load| {
            !load.superseded && load.generation == generation
                && load.key.matches(item_id, projection, mode, foreground)
        }) {
            return ContentLoadStatus::Loading;
        }
        if self.suppressed.get(&item_digest(item_id)).is_some_and(|entry| {
            entry.digest == resource_digest(item_id, projection, mode, foreground)
        }) {
            return ContentLoadStatus::MemoryLimited;
        }
        if let Some(entry) = self.entries.iter().find(|entry| {
            entry.key.matches(item_id, projection, mode, foreground)
        }) {
            if entry.budget.revoked() {
                return ContentLoadStatus::MemoryLimited;
            }
            if let Some(retry) = &entry.retry {
                return if retry.retry_at.is_some() {
                    ContentLoadStatus::RetryScheduled
                } else {
                    ContentLoadStatus::RetryExhausted
                };
            }
            return if entry.resources.images.values().any(Option::is_some)
                || entry.resources.icons.values().any(Option::is_some)
                || !entry.resources.paths.is_empty()
            {
                ContentLoadStatus::Ready
            } else {
                ContentLoadStatus::Unavailable
            };
        }
        ContentLoadStatus::Missing
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

    /// Release GPUI assets after mutations, including advance/invalidation.
    /// Image ids shared by any surviving cache holder stay live. Incoming worker results
    /// were never drawn, so rejecting/trimming them requires no asset lookup.
    pub fn release_evicted(&mut self, cx: &mut App) {
        self.apply_budget_pressure();
        budget::ensure_retirement_scheduler(cx);
        super::ensure_image_retirement_scheduler(cx);
        let mut images = self.take_unshared_evicted();
        images.extend(image_holders::take_retired_images());
        let mut seen = HashSet::new();
        for image in images {
            if seen.insert(image.id()) && !image_holders::image_has_holders(image.id()) {
                // native_content rechecks claims again inside its deferred
                // CPU/atlas cleanup, covering reacquisition after this drain.
                super::release_content_image(image, cx);
            }
        }
        let leases = std::mem::take(&mut self.evicted_budget);
        if !leases.is_empty() {
            // release_content_image enqueues its deferred cleanup first. Budget
            // capacity returns on the UI turn after those guards have run.
            cx.defer(move |_| drop(leases));
        }
    }

    fn remove_entry(&mut self, entry: Entry) {
        self.retained_bytes = self.retained_bytes.saturating_sub(entry.bytes);
        let resource_bytes = entry.bytes.saturating_sub(entry.key.retained_bytes());
        let mut queued = false;
        for image in entry.resources.images_for_release() {
            let id = image.id();
            if let std::collections::hash_map::Entry::Vacant(slot) = self.evicted.entry(id) {
                self.evicted_bytes = self
                    .evicted_bytes
                    .saturating_add(size_of::<Image>().saturating_add(6 * size_of::<usize>()));
                slot.insert(image);
                queued = true;
            }
        }
        if queued {
            // Count the removed entry's full resource estimate conservatively,
            // including decoded buffers, even when only some ids were new.
            self.evicted_bytes = self.evicted_bytes.saturating_add(resource_bytes);
        }
        self.evicted_budget.push(entry.budget);
    }

    fn evict_visible_entry(&mut self, entry: Entry) {
        self.suppress_key(&entry.key, entry.bytes);
        self.remove_entry(entry);
    }

    fn suppress_key(&mut self, key: &ContentResourceKey, bytes: usize) {
        let item = item_digest(&key.item_id);
        if self.visible_items.as_ref().is_some_and(|visible| visible.contains(&item)) {
            self.suppressed.insert(item, SuppressedResource {
                digest: resource_digest(&key.item_id, &key.projection, key.mode, &key.foreground),
                bytes,
            });
        }
    }

    fn take_unshared_evicted(&mut self) -> Vec<Arc<Image>> {
        let live: HashSet<_> = self
            .entries
            .iter()
            .flat_map(|entry| entry.resources.images_for_release())
            .map(|image| image.id())
            .collect();
        self.evicted_bytes = 0;
        std::mem::take(&mut self.evicted)
            .into_iter()
            .filter_map(|(id, image)| {
                (!live.contains(&id) && !image_holders::image_has_holders(id)).then_some(image)
            })
            .collect()
    }
}

/// Load order for a list viewport: the on-screen rows as given, then up to
/// one more screen (8..=16 rows) below and above, nearest first. A row should
/// be ready before a wheel glide brings it in, not a few frames after. Owners publish
/// the whole order as their visible set so prefetched entries are protected,
/// and reserve in this order so on-screen rows take worker slots first.
pub fn prefetch_order(on_screen: &[usize], len: usize) -> Vec<usize> {
    let (Some(&first), Some(&last)) = (on_screen.iter().min(), on_screen.iter().max()) else {
        return Vec::new();
    };
    let extra = on_screen.len().clamp(8, 16);
    let mut order = on_screen.to_vec();
    order.extend((last + 1..len).take(extra));
    order.extend((0..first).rev().take(extra));
    order
}

fn reset_retry(retry: &mut Option<RetryState>, now: Instant) {
    if let Some(retry) = retry {
        retry.attempt = 0;
        retry.retry_at = Some(now + RETRY_DELAYS[0]);
    }
}

fn item_digest(item_id: &str) -> [u8; 32] {
    *blake3::hash(item_id.as_bytes()).as_bytes()
}

fn resource_digest(
    item_id: &str,
    projection: &ContentProjection,
    mode: ContentMode,
    foreground: &str,
) -> [u8; 32] {
    struct DigestWriter(blake3::Hasher);
    impl Write for DigestWriter {
        fn write_str(&mut self, value: &str) -> fmt::Result {
            self.0.update(value.as_bytes());
            Ok(())
        }
    }
    let mut writer = DigestWriter(blake3::Hasher::new());
    // Debug is structural/escaped for these derived model types. Stream it
    // directly into BLAKE3: no large format! allocation or persistent plaintext.
    write!(writer, "{item_id:?}|{projection:?}|{mode:?}|{foreground:?}").unwrap();
    *writer.0.finalize().as_bytes()
}

impl Drop for ListContentCache {
    fn drop(&mut self) {
        while let Some(entry) = self.entries.pop_front() {
            self.remove_entry(entry);
        }
        let images = self.take_unshared_evicted();
        let leases = std::mem::take(&mut self.evicted_budget);
        budget::retire_dropped_cache(images, leases);
    }
}

// Inline structs are counted once by the key/entry header; these helpers count
// backing allocations, including links, highlights, and step/file strings.
fn key_bytes(
    item_id_bytes: usize,
    projection: &ContentProjection,
    foreground_bytes: usize,
) -> usize {
    size_of::<Entry>()
        .saturating_sub(size_of::<ContentResources>())
        .saturating_add(item_id_bytes)
        .saturating_add(foreground_bytes)
        .saturating_add(projection_heap_bytes(projection))
        .saturating_add(ContentBudgetLease::metadata_bytes())
}

fn projection_heap_bytes(projection: &ContentProjection) -> usize {
    match projection {
        ContentProjection::Text(text) => text_projection_heap_bytes(text),
        ContentProjection::Mixed(parts) => {
            let mut bytes = parts.capacity().saturating_mul(size_of::<MixedPart>());
            for part in parts {
                bytes = bytes.saturating_add(match part {
                    MixedPart::Text(text) | MixedPart::Placeholder(text) => text.capacity(),
                    MixedPart::Image { path } => path.as_ref().map_or(0, String::capacity),
                    MixedPart::File { name, path } => {
                        name.capacity().saturating_add(path.capacity())
                    }
                });
            }
            bytes
        }
        ContentProjection::Steps(steps) => {
            let mut bytes = steps
                .preview_text
                .capacity()
                .saturating_add(steps.stats.capacity())
                .saturating_add(
                    steps
                        .lines
                        .capacity()
                        .saturating_mul(size_of::<super::model::StepLine>()),
                );
            for line in &steps.lines {
                for text in [&line.title, &line.suffix, &line.display_text, &line.icon] {
                    bytes = bytes.saturating_add(text.capacity());
                }
            }
            bytes
        }
        ContentProjection::Files(files) => files
            .entries
            .capacity()
            .saturating_mul(size_of::<super::files::FileEntryProjection>())
            .saturating_add(
                files
                    .entries
                    .iter()
                    .map(|entry| entry.path.capacity().saturating_add(entry.name.capacity()))
                    .sum::<usize>(),
            )
            .saturating_add(text_projection_heap_bytes(&files.fallback)),
        ContentProjection::LocalPath(path) => path
            .path
            .capacity()
            .saturating_add(text_projection_heap_bytes(&path.text)),
    }
}

fn text_projection_heap_bytes(text: &super::model::TextProjection) -> usize {
    let mut bytes = text
        .text
        .capacity()
        .saturating_add(
            text.links
                .capacity()
                .saturating_mul(size_of::<super::links::InlineLink>()),
        )
        .saturating_add(
            text.hit_ranges
                .capacity()
                .saturating_mul(size_of::<std::ops::Range<usize>>()),
        );
    for link in &text.links {
        let target = match &link.target {
            OpenTarget::Http(target) | OpenTarget::ShellPath(target) => target,
        };
        bytes = bytes.saturating_add(target.capacity());
    }
    bytes
}

#[cfg(test)]
#[path = "list_content_budget_tests.rs"]
mod budget_tests;

#[cfg(test)]
mod tests {
    use super::super::{
        links::InlineLink,
        model::{StepLine, StepProjection, TextProjection},
    };
    use super::*;
    use gpui::ImageFormat;

    fn isolated_cache() -> ListContentCache {
        ListContentCache::with_budget(ContentBudget::new(
            super::super::budget::SHARED_CONTENT_RETAINED_BYTES,
        ))
    }

    fn text(value: &str) -> ContentProjection {
        ContentProjection::Text(TextProjection {
            text: value.to_owned(),
            links: Vec::new(),
            hit_ranges: Vec::new(),
        })
    }

    fn reserve(cache: &mut ListContentCache, id: &str, value: &str) -> ContentLoadTicket {
        cache
            .reserve(1, id, &text(value), ContentMode::Standard, "#222222")
            .unwrap()
    }

    fn insert(cache: &mut ListContentCache, id: &str, resources: ContentResources) {
        let ticket = reserve(cache, id, id);
        assert!(cache.finish(ticket, resources, true));
    }

    // Real ContentResources/Image objects, with in-memory SVG dimensions and
    // harmless padding. Resource budgeting reads headers; no decoder/UI/I/O.
    fn resource(
        seed: usize,
        encoded_bytes: usize,
        width: usize,
        height: usize,
    ) -> ContentResources {
        let header = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width}\" height=\"{height}\"><!--{seed}"
        );
        let padding_end = encoded_bytes.max(header.len());
        let mut bytes = Vec::with_capacity(padding_end + 9);
        bytes.extend_from_slice(header.as_bytes());
        bytes.resize(padding_end, b'x');
        bytes.extend_from_slice(b"--></svg>");
        let image = Arc::new(Image::from_bytes(ImageFormat::Svg, bytes));
        ContentResources {
            images: Default::default(),
            icons: [("icon".to_owned(), Some(image))].into_iter().collect(),
            paths: Default::default(),
        }
    }

    #[test]
    fn old_page_workers_keep_physical_slots_until_their_completion() {
        let mut cache = isolated_cache();
        cache.advance(1);
        let tickets: Vec<_> = (0..MAX_PENDING)
            .map(|i| reserve(&mut cache, &format!("old{i}"), "old"))
            .collect();
        assert!(
            cache
                .reserve(1, "blocked", &text("x"), ContentMode::Standard, "#222222")
                .is_none()
        );
        cache.advance(2);
        assert_eq!(cache.pending_len(), MAX_PENDING);
        assert!(
            cache
                .reserve(2, "new", &text("x"), ContentMode::Standard, "#222222")
                .is_none()
        );
        for ticket in tickets {
            assert!(!cache.finish(ticket, ContentResources::default(), true));
        }
        assert_eq!(cache.pending_len(), 0);
        let ticket = cache
            .reserve(2, "new", &text("x"), ContentMode::Standard, "#222222")
            .unwrap();
        assert!(cache.finish(ticket, ContentResources::default(), true));
        assert!(
            cache
                .get(1, "new", &text("x"), ContentMode::Standard, "#222222")
                .is_none()
        );
        assert!(
            cache
                .get(2, "new", &text("x"), ContentMode::Standard, "#222222")
                .is_some()
        );
        cache.advance(2);
        assert_eq!(cache.len(), 1);
        cache.advance(3);
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.retained_bytes(), 0);
    }

    #[test]
    fn retaining_advance_deletes_a_but_keeps_b_resources_identity_bytes_and_lease() {
        let mut cache = isolated_cache();
        cache.advance(1);
        let a = resource(unique_resource_seed(), 256, 1, 1);
        let a_image = a.icons["icon"].as_ref().unwrap().clone();
        let b = resource(unique_resource_seed(), 512, 1, 1);
        let b_image = b.icons["icon"].as_ref().unwrap().clone();
        insert(&mut cache, "a", a);
        insert(&mut cache, "b", b);
        let b_key = cache.entries[1].key.clone();
        let b_bytes = cache.entries[1].bytes;
        let old_total = cache.retained_bytes();

        cache.advance_retaining(2, |key| key == &b_key);

        assert_eq!(cache.len(), 1);
        assert_eq!(cache.retained_bytes(), b_bytes);
        assert!(cache.retained_bytes() < old_total);
        assert_eq!(cache.entries[0].key, b_key);
        let retained = cache
            .get(2, "b", &text("b"), ContentMode::Standard, "#222222")
            .unwrap();
        assert!(Arc::ptr_eq(
            retained.icons["icon"].as_ref().unwrap(),
            &b_image
        ));
        assert!(
            cache
                .get(2, "a", &text("a"), ContentMode::Standard, "#222222")
                .is_none()
        );
        assert!(
            cache
                .get(1, "b", &text("b"), ContentMode::Standard, "#222222")
                .is_none()
        );
        assert!(image_holders::image_has_holders(b_image.id()));
        assert!(!image_holders::image_has_holders(a_image.id()));
        let evicted = cache.take_unshared_evicted();
        assert_eq!(
            evicted.iter().map(|image| image.id()).collect::<Vec<_>>(),
            [a_image.id()]
        );
        image_holders::acknowledge_image_retirement(a_image.id());

        // The original API still provides a forced refresh for unchanged paths.
        cache.advance(3);
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.retained_bytes(), 0);
        assert_eq!(cache.take_unshared_evicted()[0].id(), b_image.id());
        image_holders::acknowledge_image_retirement(b_image.id());
    }

    #[test]
    fn retaining_advance_never_revives_pending_tickets_after_generation_or_key_aba() {
        let mut cache = isolated_cache();
        cache.advance(1);
        let first = reserve(&mut cache, "same", "K1");
        let duplicate = first.clone();
        cache.advance_retaining(2, |_| true);
        let second = cache
            .reserve(2, "same", &text("K2"), ContentMode::Standard, "#222222")
            .unwrap();
        cache.advance_retaining(1, |_| true);
        let third = reserve(&mut cache, "same", "K1");
        assert_eq!(cache.pending_len(), 3);
        assert!(!cache.finish(first, ContentResources::default(), true));
        assert!(!cache.finish(duplicate, ContentResources::default(), true));
        assert!(!cache.finish(second, ContentResources::default(), true));
        assert_eq!(cache.pending_len(), 1);
        assert!(cache.finish(third, ContentResources::default(), true));

        let same_generation = reserve(&mut cache, "pending", "same key");
        cache.advance_retaining(1, |_| true);
        assert!(!cache.finish(same_generation, ContentResources::default(), true));
        assert!(
            cache
                .get(1, "same", &text("K1"), ContentMode::Standard, "#222222")
                .is_some()
        );
        assert_eq!(cache.pending_len(), 0);
    }

    #[test]
    fn retaining_advance_evicts_removed_or_changed_full_keys_and_preserves_fifo_order() {
        let mut cache = isolated_cache();
        cache.advance(1);
        let mut images = Vec::new();
        for id in [
            "changed-projection",
            "changed-mode",
            "changed-color",
            "removed",
            "keep-first",
            "keep-last",
        ] {
            let resources = resource(unique_resource_seed(), 256, 1, 1);
            images.push(resources.icons["icon"].as_ref().unwrap().clone());
            insert(&mut cache, id, resources);
        }
        let mut wanted: Vec<_> = cache
            .entries
            .iter()
            .map(|entry| entry.key.clone())
            .collect();
        wanted[0].projection = text("different content");
        wanted[1].mode = ContentMode::Minimal;
        wanted[2].foreground = "#ffffff".into();
        wanted.remove(3);
        let retained_bytes = cache.entries[4].bytes + cache.entries[5].bytes;
        cache.advance_retaining(2, |key| wanted.contains(key));
        assert_eq!(
            cache
                .entries
                .iter()
                .map(|entry| entry.key.item_id.as_str())
                .collect::<Vec<_>>(),
            ["keep-first", "keep-last"]
        );
        assert_eq!(cache.retained_bytes(), retained_bytes);
        assert_eq!(cache.evicted.len(), 4);
        let evicted: HashSet<_> = cache
            .take_unshared_evicted()
            .iter()
            .map(|image| image.id())
            .collect();
        assert_eq!(
            evicted,
            images[..4].iter().map(|image| image.id()).collect()
        );
        for image in &images[..4] {
            image_holders::acknowledge_image_retirement(image.id());
        }
        for image in &images[4..] {
            assert!(image_holders::image_has_holders(image.id()));
        }
    }

    #[test]
    fn full_queue_aba_does_not_revive_a_superseded_ticket() {
        let mut cache = isolated_cache();
        cache.advance(1);
        let first = reserve(&mut cache, "same", "K1");
        let duplicate = first.clone();
        for id in (1..MAX_PENDING).map(|n| format!("other{n}")) {
            reserve(&mut cache, &id, &id);
        }
        assert!(
            cache
                .reserve(1, "same", &text("K2"), ContentMode::Standard, "#222222")
                .is_none()
        );
        assert!(
            cache
                .reserve(1, "same", &text("K1"), ContentMode::Standard, "#222222")
                .is_none()
        );
        assert!(!cache.finish(first, ContentResources::default(), true));
        assert_eq!(cache.pending_len(), MAX_PENDING - 1);
        let replacement = reserve(&mut cache, "same", "K1");
        assert!(!cache.finish(duplicate, ContentResources::default(), true));
        assert_eq!(cache.pending_len(), MAX_PENDING);
        assert!(cache.finish(replacement, ContentResources::default(), true));
        assert!(
            cache
                .get(1, "same", &text("K1"), ContentMode::Standard, "#222222")
                .is_some()
        );
    }

    #[test]
    fn current_guard_and_tampered_ticket_always_release_their_slot() {
        let mut cache = isolated_cache();
        cache.advance(1);
        let ticket = reserve(&mut cache, "guard", "text");
        assert!(!cache.finish(ticket, ContentResources::default(), false));
        assert_eq!(cache.pending_len(), 0);
        assert_eq!(cache.len(), 0);
        let mut ticket = reserve(&mut cache, "changed", "text");
        ticket.generation = 9;
        assert!(!cache.finish(ticket, ContentResources::default(), true));
        assert_eq!(cache.pending_len(), 0);
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn exact_projection_mode_and_foreground_are_part_of_identity() {
        let mut cache = isolated_cache();
        cache.advance(1);
        let first = reserve(&mut cache, "row", "old");
        assert!(
            cache
                .reserve(1, "row", &text("old"), ContentMode::Standard, "#222222")
                .is_none()
        );
        let second = cache
            .reserve(1, "row", &text("old"), ContentMode::Minimal, "#eeeeee")
            .unwrap();
        assert!(!cache.finish(first, ContentResources::default(), true));
        assert!(cache.finish(second, ContentResources::default(), true));
        assert!(
            cache
                .get(1, "row", &text("old"), ContentMode::Standard, "#eeeeee")
                .is_none()
        );
        assert!(
            cache
                .get(1, "row", &text("old"), ContentMode::Minimal, "#222222")
                .is_none()
        );
        assert!(
            cache
                .get(1, "row", &text("changed"), ContentMode::Minimal, "#eeeeee")
                .is_none()
        );
        let third = cache
            .reserve(1, "row", &text("changed"), ContentMode::Minimal, "#eeeeee")
            .unwrap();
        assert_eq!(cache.len(), 0);
        assert!(cache.finish(third, ContentResources::default(), true));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn capacity_eviction_retires_rows_outside_the_viewport_first() {
        let mut cache = isolated_cache();
        cache.advance(1);
        let ids: Vec<String> = (0..12).map(|i| i.to_string()).collect();
        cache.set_visible_items(ids[..9].iter().map(String::as_str));
        for (seed, id) in ids[..9].iter().enumerate() {
            insert(&mut cache, id, resource(seed, 3 * 1024 * 1024, 1, 1));
        }
        // Row 0 was loaded first but is still on screen; 1..8 scrolled away.
        cache.set_visible_items(["0", "9", "10", "11"]);
        for (seed, id) in ids[9..].iter().enumerate() {
            insert(&mut cache, id, resource(9 + seed, 3 * 1024 * 1024, 1, 1));
        }
        assert!(cache.retained_bytes() <= MAX_RETAINED_BYTES);
        assert!(cache.get(1, "0", &text("0"), ContentMode::Standard, "#222222").is_some());
        assert!(cache.get(1, "1", &text("1"), ContentMode::Standard, "#222222").is_none());
        assert!(cache.get(1, "11", &text("11"), ContentMode::Standard, "#222222").is_some());
    }

    #[test]
    fn prefetch_follows_the_viewport_nearest_first() {
        assert!(prefetch_order(&[], 10).is_empty());
        assert_eq!(prefetch_order(&[3, 4], 20), vec![3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 2, 1, 0]);
        assert_eq!(prefetch_order(&[0, 1], 3), vec![0, 1, 2]);
        let on_screen: Vec<_> = (10..22).collect();
        let order = prefetch_order(&on_screen, 100);
        assert_eq!(order.len(), 12 + 12 + 10);
        assert_eq!(&order[12..14], &[22, 23]);
        assert_eq!(&order[24..26], &[9, 8]);
    }

    #[test]
    fn idle_owner_keeps_last_viewport_then_releases_everything() {
        let mut cache = isolated_cache();
        cache.advance(1);
        insert(&mut cache, "a", ContentResources::default());
        insert(&mut cache, "b", ContentResources::default());
        cache.set_visible_items(["a"]);
        cache.set_visible_items(std::iter::empty::<&str>());
        let start = Instant::now();
        let has = |cache: &ListContentCache, id: &str| {
            cache.get(1, id, &text(id), ContentMode::Standard, "#222222").is_some()
        };

        assert!(!cache.reclaim_idle_at(start));
        assert_eq!(cache.next_idle_deadline(), Some(start + IDLE_TRIM_AFTER));
        assert!(!cache.reclaim_idle_at(start + IDLE_TRIM_AFTER - Duration::from_millis(1)));
        assert!(has(&cache, "a") && has(&cache, "b"));

        assert!(cache.reclaim_idle_at(start + IDLE_TRIM_AFTER));
        assert_eq!(cache.next_idle_deadline(), Some(start + IDLE_RELEASE_AFTER));
        assert!(has(&cache, "a") && !has(&cache, "b"));
        assert!(!cache.reclaim_idle_at(start + IDLE_TRIM_AFTER + Duration::from_secs(1)));

        assert!(cache.reclaim_idle_at(start + IDLE_RELEASE_AFTER));
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.retained_bytes(), 0);
        assert_eq!(cache.next_idle_deadline(), None);
    }

    #[test]
    fn showing_a_viewport_restarts_the_idle_grace() {
        let mut cache = isolated_cache();
        cache.advance(1);
        insert(&mut cache, "a", ContentResources::default());
        cache.set_visible_items(std::iter::empty::<&str>());
        let start = Instant::now();
        assert!(!cache.reclaim_idle_at(start));
        cache.set_visible_items(["a"]);
        assert_eq!(cache.next_idle_deadline(), None);
        cache.set_visible_items(std::iter::empty::<&str>());
        assert!(!cache.reclaim_idle_at(start + IDLE_TRIM_AFTER));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.next_idle_deadline(), Some(start + IDLE_TRIM_AFTER + IDLE_TRIM_AFTER));
    }

    #[test]
    fn hidden_late_worker_cannot_rearm_an_empty_cache_timer() {
        let mut cache = isolated_cache();
        cache.advance(1);
        cache.set_visible_items(["late"]);
        let ticket = cache.reserve(1, "late", &text("late"), ContentMode::Standard, "#222222").unwrap();
        cache.set_visible_items(std::iter::empty::<&str>());
        let start = Instant::now();
        assert!(!cache.reclaim_idle_at(start));
        assert_eq!(cache.next_idle_deadline(), None);
        assert!(!cache.finish_at(ticket, ContentResources::default(), true, false, start + IDLE_RELEASE_AFTER));
        assert_eq!(cache.next_idle_deadline(), None);
    }

    #[test]
    fn negative_result_is_cached_and_fifo_reads_do_not_refresh_age() {
        let mut cache = isolated_cache();
        cache.advance(1);
        for i in 0..MAX_ENTRIES {
            insert(&mut cache, &i.to_string(), ContentResources::default());
        }
        assert!(
            cache
                .get(1, "0", &text("0"), ContentMode::Standard, "#222222")
                .is_some()
        );
        assert!(
            cache
                .reserve(1, "0", &text("0"), ContentMode::Standard, "#222222")
                .is_none()
        );
        insert(&mut cache, "128", ContentResources::default());
        assert_eq!(cache.len(), MAX_ENTRIES);
        assert!(
            cache
                .get(1, "0", &text("0"), ContentMode::Standard, "#222222")
                .is_none()
        );
        assert!(
            cache
                .get(1, "1", &text("1"), ContentMode::Standard, "#222222")
                .is_some()
        );
        assert!(cache.retained_bytes() <= MAX_RETAINED_BYTES);
    }

    fn reserve_image_at(cache: &mut ListContentCache, now: Instant) -> Option<ContentLoadTicket> {
        cache.reserve_at(1, "image", &text("image"), ContentMode::Standard, "#222222", now)
    }

    fn image_status(cache: &ListContentCache) -> ContentLoadStatus {
        cache.load_status(1, "image", &text("image"), ContentMode::Standard, "#222222")
    }

    fn exhaust_image_retries(cache: &mut ListContentCache, mut now: Instant) -> Instant {
        let ticket = reserve_image_at(cache, now).unwrap();
        assert!(cache.finish_at(ticket, ContentResources::default(), true, true, now));
        for delay in RETRY_DELAYS {
            now += delay;
            let ticket = reserve_image_at(cache, now).unwrap();
            assert!(cache.finish_at(ticket, ContentResources::default(), true, true, now));
        }
        now
    }

    #[test]
    fn retryable_failures_wait_for_each_deadline_and_stop_after_three_retries() {
        let mut cache = isolated_cache();
        cache.advance(1);
        let mut now = Instant::now();
        cache.set_visible_items_at(["image"], now);
        assert_eq!(image_status(&cache), ContentLoadStatus::Missing);
        let ticket = reserve_image_at(&mut cache, now).unwrap();
        assert_eq!(image_status(&cache), ContentLoadStatus::Loading);
        assert!(cache.finish_at(ticket, ContentResources::default(), true, true, now));
        for (index, delay) in RETRY_DELAYS.into_iter().enumerate() {
            assert_eq!(image_status(&cache), ContentLoadStatus::RetryScheduled);
            let deadline = now + delay;
            for _ in 0..100 {
                assert!(reserve_image_at(&mut cache, deadline - Duration::from_millis(1)).is_none());
            }
            let ticket = reserve_image_at(&mut cache, deadline).unwrap();
            assert_eq!(cache.pending[&ticket.token].retry_attempt, index + 1);
            assert_eq!(image_status(&cache), ContentLoadStatus::Loading);
            assert!(cache.finish_at(ticket, ContentResources::default(), true, true, deadline));
            now = deadline;
        }
        assert_eq!(image_status(&cache), ContentLoadStatus::RetryExhausted);
        assert!(reserve_image_at(&mut cache, now + Duration::from_secs(3600)).is_none());
        assert_eq!(cache.pending_len(), 0);
    }

    #[test]
    fn retry_success_and_permanent_failure_end_the_retry_sequence() {
        for succeeds in [false, true] {
            let mut cache = isolated_cache();
            cache.advance(1);
            let now = Instant::now();
            cache.set_visible_items_at(["image"], now);
            let ticket = reserve_image_at(&mut cache, now).unwrap();
            assert!(cache.finish_at(ticket, ContentResources::default(), true, true, now));
            let ticket = reserve_image_at(&mut cache, now + RETRY_DELAYS[0]).unwrap();
            let resources = if succeeds {
                resource(unique_resource_seed(), 256, 1, 1)
            } else {
                ContentResources::default()
            };
            assert!(cache.finish(ticket, resources, true));
            assert_eq!(image_status(&cache), if succeeds {
                ContentLoadStatus::Ready
            } else {
                ContentLoadStatus::Unavailable
            });
            assert!(reserve_image_at(&mut cache, now + Duration::from_secs(3600)).is_none());
        }
    }

    #[test]
    fn exhausted_retry_resets_on_reentry_or_new_query_but_not_neighbour_changes() {
        let mut cache = isolated_cache();
        cache.advance(1);
        let began = Instant::now();
        cache.set_visible_items_at(["image"], began);
        let now = exhaust_image_retries(&mut cache, began) + Duration::from_secs(10);
        cache.set_visible_items_at(["image", "neighbour"], now);
        assert_eq!(image_status(&cache), ContentLoadStatus::RetryExhausted);
        assert!(reserve_image_at(&mut cache, now).is_none());
        cache.set_visible_items_at(["neighbour"], now);
        cache.set_visible_items_at(["image", "neighbour"], now);
        assert_eq!(image_status(&cache), ContentLoadStatus::RetryScheduled);
        assert!(reserve_image_at(&mut cache, now).is_none());
        let ticket = reserve_image_at(&mut cache, now + RETRY_DELAYS[0]).unwrap();
        assert!(cache.finish_at(ticket, ContentResources::default(), true, true, now));
        cache.advance_retaining_at(1, |_| true, now);
        assert_eq!(cache.entries[0].retry.as_ref().unwrap().attempt, 1);
        cache.advance_retaining_at(2, |_| true, now);
        assert_eq!(cache.entries[0].retry.as_ref().unwrap().attempt, 0);
        assert!(cache.reserve_at(2, "image", &text("image"), ContentMode::Standard, "#222222", now).is_none());
        assert!(cache.reserve_at(2, "image", &text("image"), ContentMode::Standard, "#222222", now + RETRY_DELAYS[0]).is_some());
    }

    #[test]
    fn stale_retry_completion_cannot_repopulate_a_new_query() {
        let mut cache = isolated_cache();
        cache.advance(1);
        let now = Instant::now();
        cache.set_visible_items_at(["image"], now);
        let ticket = reserve_image_at(&mut cache, now).unwrap();
        assert!(cache.finish_at(ticket, ContentResources::default(), true, true, now));
        let ticket = reserve_image_at(&mut cache, now + RETRY_DELAYS[0]).unwrap();
        cache.advance_retaining_at(2, |_| true, now);
        assert!(!cache.finish_retryable(ticket, ContentResources::default(), true));
        assert_eq!(cache.pending_len(), 0);
        assert_eq!(cache.load_status(2, "image", &text("image"), ContentMode::Standard, "#222222"), ContentLoadStatus::Missing);
    }

    #[test]
    fn visible_pressure_fallback_recovers_only_after_real_shared_space_returns() {
        let limit = 100_000;
        let budget = ContentBudget::new(limit);
        let mut cache = ListContentCache::with_budget(budget.clone());
        cache.advance(1);
        cache.set_visible_items(["image"]);
        let original = resource(unique_resource_seed(), 256, 64, 64);
        insert(&mut cache, "image", original.clone());
        let bytes = cache.retained_bytes();
        let other_owner = budget.try_acquire(limit - bytes).unwrap();
        assert!(budget.try_acquire(bytes).is_none());
        assert!(cache.apply_budget_pressure());
        assert_eq!(image_status(&cache), ContentLoadStatus::MemoryLimited);
        assert!(reserve_image_at(&mut cache, Instant::now()).is_none());

        // Mirror release_evicted's completed foreground cleanup without a UI.
        for image in cache.take_unshared_evicted() {
            image_holders::acknowledge_image_retirement(image.id());
        }
        cache.evicted_budget.clear();
        let competing_owner = budget.try_acquire(bytes).unwrap();
        for _ in 0..100 {
            assert!(reserve_image_at(&mut cache, Instant::now()).is_none());
        }
        assert_eq!(cache.pending_len(), 0);
        drop(competing_owner);
        let ticket = reserve_image_at(&mut cache, Instant::now()).unwrap();
        assert_eq!(cache.pending[&ticket.token].estimated_bytes, bytes);
        assert!(cache.finish(ticket, original, true));
        assert_eq!(image_status(&cache), ContentLoadStatus::Ready);
        assert!(budget.snapshot().retained_bytes <= limit);
        drop(other_owner);
    }

    #[test]
    fn rejected_resource_does_not_decode_again_until_budget_cleanup_finishes() {
        let budget = ContentBudget::new(100_000);
        let mut cache = ListContentCache::with_budget(budget.clone());
        cache.advance(1);
        cache.set_visible_items(["image"]);
        let busy = budget.try_acquire(100_000).unwrap();
        let ticket = reserve_image_at(&mut cache, Instant::now()).unwrap();
        let original = resource(unique_resource_seed(), 256, 64, 64);
        assert!(!cache.finish(ticket, original.clone(), true));
        assert_eq!(image_status(&cache), ContentLoadStatus::MemoryLimited);
        for _ in 0..100 {
            assert!(reserve_image_at(&mut cache, Instant::now()).is_none());
        }
        drop(busy);
        let ticket = reserve_image_at(&mut cache, Instant::now()).unwrap();
        assert!(cache.finish(ticket, original, true));
        assert_eq!(image_status(&cache), ContentLoadStatus::Ready);
    }

    #[test]
    fn recovering_workers_do_not_each_claim_the_same_available_space() {
        let mut cache = ListContentCache::with_budget(ContentBudget::new(100_000));
        cache.advance(1);
        cache.set_visible_items(["a", "b"]);
        for id in ["a", "b"] {
            cache.suppress_key(&ContentResourceKey {
                item_id: id.into(), projection: text(id),
                mode: ContentMode::Standard, foreground: "#222222".into(),
            }, 60_000);
        }
        let first = reserve(&mut cache, "a", "a");
        for _ in 0..100 {
            assert!(cache.reserve(1, "b", &text("b"), ContentMode::Standard, "#222222").is_none());
        }
        assert_eq!(cache.pending_len(), 1);
        assert!(!cache.finish(first, ContentResources::default(), false));
        assert!(cache.reserve(1, "b", &text("b"), ContentMode::Standard, "#222222").is_some());
    }

    #[test]
    fn byte_budget_evicts_before_count_limit_and_release_queue_has_backpressure() {
        let mut cache = isolated_cache();
        cache.advance(1);
        for i in 0..9 {
            insert(
                &mut cache,
                &i.to_string(),
                resource(i, 4 * 1024 * 1024, 1, 1),
            );
        }
        assert!(cache.len() < 9);
        assert!(cache.retained_bytes() <= MAX_RETAINED_BYTES);
        assert!(
            cache
                .get(1, "0", &text("0"), ContentMode::Standard, "#222222")
                .is_none()
        );
        assert!(
            cache
                .get(1, "8", &text("8"), ContentMode::Standard, "#222222")
                .is_some()
        );
        cache.advance(2);
        assert!(cache.evicted_bytes >= MAX_RETAINED_BYTES);
        assert!(
            cache
                .reserve(2, "blocked", &text("x"), ContentMode::Standard, "#222222")
                .is_none()
        );
        assert_eq!(cache.take_unshared_evicted().len(), 9);
        assert_eq!(cache.evicted_bytes, 0);
        assert!(
            cache
                .reserve(2, "unblocked", &text("x"), ContentMode::Standard, "#222222")
                .is_some()
        );
    }

    #[test]
    fn oversized_resources_become_completed_fallbacks_instead_of_reloading() {
        let mut cache = isolated_cache();
        cache.advance(1);
        insert(&mut cache, "oversized", resource(1, 256, 100_000, 100_000));
        let stored = cache
            .get(
                1,
                "oversized",
                &text("oversized"),
                ContentMode::Standard,
                "#222222",
            )
            .unwrap();
        assert!(stored.images_for_release().is_empty());
        assert!(
            cache
                .reserve(
                    1,
                    "oversized",
                    &text("oversized"),
                    ContentMode::Standard,
                    "#222222"
                )
                .is_none()
        );
        assert!(cache.retained_bytes() <= MAX_RETAINED_BYTES);
    }

    #[test]
    fn shared_image_ids_are_deduplicated_and_surviving_aliases_are_not_released() {
        let mut cache = isolated_cache();
        cache.advance(1);
        // Distinct Arcs with equal encoded bytes have the same GPUI image id.
        for id in ["a", "b", "c"] {
            insert(&mut cache, id, resource(1, 256, 1, 1));
        }
        for id in ["a", "b"] {
            let ticket = reserve(&mut cache, id, "new projection");
            assert!(!cache.finish(ticket, ContentResources::default(), false));
        }
        assert_eq!(cache.evicted.len(), 1);
        assert!(cache.take_unshared_evicted().is_empty());
        assert_eq!(cache.len(), 1);
        cache.advance(2);
        assert_eq!(cache.take_unshared_evicted().len(), 1);
        assert!(cache.evicted.is_empty());
    }

    fn unique_resource_seed() -> usize {
        static NEXT: std::sync::atomic::AtomicUsize =
            std::sync::atomic::AtomicUsize::new(1_000_000);
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    #[test]
    fn another_cache_protects_the_same_image_id_from_an_independent_arc() {
        let seed = unique_resource_seed();
        let a_resources = resource(seed, 512, 1, 1);
        let b_resources = resource(seed, 512, 1, 1);
        let a_image = a_resources.icons["icon"].as_ref().unwrap().clone();
        let b_image = b_resources.icons["icon"].as_ref().unwrap().clone();
        assert!(!Arc::ptr_eq(&a_image, &b_image));
        assert_eq!(a_image.id(), b_image.id());
        let mut a = isolated_cache();
        let mut b = isolated_cache();
        a.advance(1);
        b.advance(1);
        insert(&mut a, "a", a_resources);
        insert(&mut b, "b", b_resources);
        a.advance(2);
        assert!(a.take_unshared_evicted().is_empty());
        assert!(image_holders::image_has_holders(a_image.id()));
        drop(a);
        assert!(image_holders::image_has_holders(a_image.id()));
        b.advance(2);
        assert!(!image_holders::image_has_holders(a_image.id()));
        let retired = b.take_unshared_evicted();
        assert_eq!(retired.len(), 1);
        assert_eq!(retired[0].id(), a_image.id());
        image_holders::acknowledge_image_retirement(a_image.id());
    }

    #[test]
    fn queued_image_is_protected_when_a_new_cache_reacquires_it_and_drop_releases_the_claim() {
        let seed = unique_resource_seed();
        let resources = resource(seed, 512, 1, 1);
        let image = resources.icons["icon"].as_ref().unwrap().clone();
        let mut a = isolated_cache();
        a.advance(1);
        insert(&mut a, "a", resources);
        a.advance(2);
        assert!(!image_holders::image_has_holders(image.id()));
        let mut b = isolated_cache();
        b.advance(1);
        insert(&mut b, "b", resource(seed, 512, 1, 1));
        assert!(image_holders::image_has_holders(image.id()));
        assert!(a.take_unshared_evicted().is_empty());
        drop(b);
        assert!(!image_holders::image_has_holders(image.id()));
        image_holders::acknowledge_image_retirement(image.id());
    }

    #[test]
    fn holder_metadata_cannot_push_an_entry_over_the_retained_byte_budget() {
        let projection = text("boundary");
        let key = key_bytes("boundary".len(), &projection, "#222222".len());
        let mut metadata = ContentResources::default();
        metadata.icons.insert("icon".to_owned(), None);
        let base = metadata.retained_bytes();
        // The resource alone fits with only sixteen bytes left. Its explicit
        // holder/ledger cost must therefore force the completed fallback.
        let capacity = MAX_RETAINED_BYTES - key - base - 64 - 8 - 16;
        let resources = resource(unique_resource_seed(), capacity - 9, 1, 1);
        let image_id = resources.icons["icon"].as_ref().unwrap().id();
        assert!(resources.retained_bytes() + key <= MAX_RETAINED_BYTES);
        let mut cache = isolated_cache();
        cache.advance(1);
        insert(&mut cache, "boundary", resources);
        let stored = cache
            .get(1, "boundary", &projection, ContentMode::Standard, "#222222")
            .unwrap();
        assert!(stored.icons["icon"].is_none());
        assert!(cache.retained_bytes() <= MAX_RETAINED_BYTES);
        assert!(!image_holders::image_has_holders(image_id));
        image_holders::acknowledge_image_retirement(image_id);
    }

    #[test]
    fn projection_estimate_includes_nested_strings_vectors_and_key_overhead() {
        let projection = ContentProjection::Text(TextProjection {
            text: "label".to_owned(),
            links: vec![InlineLink {
                range: 0..5,
                target: OpenTarget::Http("https://example.test".to_owned()),
            }],
            hit_ranges: vec![0..5],
        });
        let expected = 5
            + size_of::<InlineLink>()
            + "https://example.test".len()
            + size_of::<std::ops::Range<usize>>();
        assert!(projection_heap_bytes(&projection) >= expected);
        let mixed = ContentProjection::Mixed(vec![
            MixedPart::File {
                name: "file".into(),
                path: "C:\\file".into(),
            },
            MixedPart::Image {
                path: Some("C:\\image.png".into()),
            },
        ]);
        assert!(projection_heap_bytes(&mixed) >= 2 * size_of::<MixedPart>() + 4 + 7 + 12);
        let steps = ContentProjection::Steps(StepProjection {
            lines: vec![StepLine {
                title: "title".into(),
                suffix: "suffix".into(),
                display_text: "display".into(),
                icon: "icon".into(),
                disabled: false,
            }],
            preview_text: "preview".into(),
            stats: "stats".into(),
        });
        assert!(projection_heap_bytes(&steps) >= size_of::<StepLine>() + 5 + 6 + 7 + 4 + 7 + 5);
        assert!(key_bytes(10, &projection, 7) > projection_heap_bytes(&projection) + 17);
        let huge = ContentProjection::Text(TextProjection {
            text: "x".repeat(MAX_RETAINED_BYTES),
            links: Vec::new(),
            hit_ranges: Vec::new(),
        });
        let mut cache = isolated_cache();
        assert!(
            cache
                .reserve(0, "huge", &huge, ContentMode::Standard, "#222222")
                .is_none()
        );
        assert_eq!(cache.pending_len(), 0);
    }
}
