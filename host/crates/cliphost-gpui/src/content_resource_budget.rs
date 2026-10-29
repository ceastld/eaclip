//! Shared admission for retained managed-content entries, not a second image cache.
//!
//! Only integer accounting and revocation flags cross owners/threads. Pressure
//! never borrows another cache or App. The owning UI pump must call its cache's
//! `apply_budget_pressure` and `release_evicted`, including while hidden. A
//! revoked reservation is still charged until its last lease actually drops;
//! requesting eviction cannot create fictitious free memory.

use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock,
    },
};

use gpui::{App, Image};

/// All ListContentCache instances in a process share this retained estimate.
/// Worker decode/input allocations and external previews have separate limits.
pub const SHARED_CONTENT_RETAINED_BYTES: usize = 64 * 1024 * 1024;

pub type ContentBudgetPressureNotifier = Arc<dyn Fn() + Send + Sync + 'static>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ContentBudgetSnapshot {
    pub limit_bytes: usize,
    pub retained_bytes: usize,
    pub entries: usize,
    pub pending_evictions: usize,
    pub pending_eviction_bytes: usize,
    pub pressure_notification_pending: bool,
}

#[derive(Clone)]
pub struct ContentBudget {
    ledger: Arc<Mutex<Ledger>>,
}

struct Ledger {
    limit: usize,
    used: usize,
    next_id: u64,
    entries: BTreeMap<u64, Reservation>,
    notifier: Option<ContentBudgetPressureNotifier>,
    notification_pending: bool,
}

struct Reservation {
    bytes: usize,
    revoked: Arc<AtomicBool>,
}

/// Clones share one accounting charge. Drop has no App/GPU operations.
#[derive(Clone)]
pub(super) struct ContentBudgetLease(Arc<LeaseInner>);

struct LeaseInner {
    ledger: Arc<Mutex<Ledger>>,
    id: u64,
    revoked: Arc<AtomicBool>,
}

struct RetirementBatch {
    images: Vec<Arc<Image>>,
    leases: Vec<ContentBudgetLease>,
}

type RetirementScheduler = Arc<dyn Fn(RetirementBatch) + Send + Sync + 'static>;

fn retirement_scheduler() -> &'static Mutex<Option<RetirementScheduler>> {
    static SCHEDULER: OnceLock<Mutex<Option<RetirementScheduler>>> = OnceLock::new();
    SCHEDULER.get_or_init(|| Mutex::new(None))
}

struct BudgetRetirementPump {
    _task: gpui::Task<()>,
}
impl gpui::Global for BudgetRetirementPump {}

/// Retired cache drops can occur without an App borrow. Keep their shared
/// charges with the images until the owning App has run the existing guarded
/// CPU/atlas retirement. This mailbox is bounded by still-charged reservations,
/// not an independent image cache or a source of new decode work.
pub(super) fn ensure_retirement_scheduler(cx: &mut App) {
    if cx.has_global::<BudgetRetirementPump>() {
        return;
    }
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<RetirementBatch>();
    *retirement_scheduler().lock().unwrap() = Some(Arc::new(move |batch| {
        // If App shutdown closed the receiver, no live GPU owner remains.
        let _ = sender.send(batch);
    }));
    let task = cx.spawn(async move |cx| {
        while let Some(batch) = receiver.recv().await {
            if cx.update(move |cx| {
                for image in batch.images {
                    super::release_content_image(image, cx);
                }
                cx.defer(move |_| drop(batch.leases));
            }).is_err() {
                break;
            }
        }
    });
    cx.set_global(BudgetRetirementPump { _task: task });
}

pub(super) fn retire_dropped_cache(
    images: Vec<Arc<Image>>,
    leases: Vec<ContentBudgetLease>,
) {
    if leases.is_empty() {
        return;
    }
    let batch = RetirementBatch { images, leases };
    let scheduler = retirement_scheduler().lock().unwrap().clone();
    dispatch_retirement(batch, scheduler);
}

fn dispatch_retirement(batch: RetirementBatch, scheduler: Option<RetirementScheduler>) {
    if let Some(schedule) = scheduler {
        schedule(batch);
    }
    // With no installed scheduler, these cache images have never been painted:
    // release_evicted installs it before a managed result may be rendered.
}

impl Default for ContentBudget {
    fn default() -> Self {
        process_budget().clone()
    }
}

fn process_budget() -> &'static ContentBudget {
    static BUDGET: OnceLock<ContentBudget> = OnceLock::new();
    BUDGET.get_or_init(|| ContentBudget::new(SHARED_CONTENT_RETAINED_BYTES))
}

pub fn shared_content_budget_snapshot() -> ContentBudgetSnapshot {
    process_budget().snapshot()
}

/// App-global owners register one foreground handoff, never one callback per
/// cache. Callbacks run outside the ledger mutex and may inspect this budget.
pub fn set_shared_content_budget_pressure_notifier(
    notifier: ContentBudgetPressureNotifier,
) -> Option<ContentBudgetPressureNotifier> {
    process_budget().set_pressure_notifier(notifier)
}

/// Consume the event before sweeping every live owner on the UI thread. New
/// revocations after this acknowledgement may schedule the next merged event.
pub fn acknowledge_shared_content_budget_pressure() {
    process_budget().acknowledge_pressure();
}

impl ContentBudget {
    /// An explicitly isolated budget is useful for deterministic fixtures. All
    /// normal caches use Default so they share the process-wide ledger.
    pub fn new(limit_bytes: usize) -> Self {
        Self {
            ledger: Arc::new(Mutex::new(Ledger {
                limit: limit_bytes,
                used: 0,
                next_id: 0,
                entries: BTreeMap::new(),
                notifier: None,
                notification_pending: false,
            })),
        }
    }

    pub fn snapshot(&self) -> ContentBudgetSnapshot {
        let state = self.ledger.lock().unwrap();
        let mut snapshot = ContentBudgetSnapshot {
            limit_bytes: state.limit,
            retained_bytes: state.used,
            entries: state.entries.len(),
            pressure_notification_pending: state.notification_pending,
            ..ContentBudgetSnapshot::default()
        };
        for entry in state.entries.values() {
            if entry.revoked.load(Ordering::Acquire) {
                snapshot.pending_evictions += 1;
                snapshot.pending_eviction_bytes += entry.bytes;
            }
        }
        snapshot
    }

    pub(super) fn pressure_pending(&self) -> bool {
        self.ledger
            .lock()
            .unwrap()
            .entries
            .values()
            .any(|entry| entry.revoked.load(Ordering::Acquire))
    }

    fn set_pressure_notifier(
        &self,
        notifier: ContentBudgetPressureNotifier,
    ) -> Option<ContentBudgetPressureNotifier> {
        let (previous, notify) = {
            let mut state = self.ledger.lock().unwrap();
            let previous = state.notifier.replace(notifier.clone());
            (previous, state.notification_pending)
        };
        if notify {
            notifier();
        }
        previous
    }

    fn acknowledge_pressure(&self) {
        self.ledger.lock().unwrap().notification_pending = false;
    }

    /// Global FIFO: reads never refresh age. (Each local cache additionally
    /// keeps its published viewport at the back of its own eviction order.)
    /// An unsatisfied request marks only enough old entries to cover the
    /// shortage, then fails. The caller retries after owners complete cleanup.
    /// Existing requests awaiting cleanup count toward the same shortage so
    /// repeated attempts cannot revoke the whole process cache unnecessarily.
    pub(super) fn try_acquire(&self, bytes: usize) -> Option<ContentBudgetLease> {
        let mut state = self.ledger.lock().unwrap();
        if bytes > state.limit {
            return None;
        }
        let available = state.limit.saturating_sub(state.used);
        if bytes > available {
            let shortage = bytes - available;
            let mut requested: usize = state
                .entries
                .values()
                .filter(|entry| entry.revoked.load(Ordering::Acquire))
                .map(|entry| entry.bytes)
                .sum();
            let mut newly_revoked = false;
            for entry in state.entries.values() {
                if requested >= shortage {
                    break;
                }
                if !entry.revoked.swap(true, Ordering::AcqRel) {
                    requested += entry.bytes;
                    newly_revoked = true;
                }
            }
            let notify = if newly_revoked && !state.notification_pending {
                state.notification_pending = true;
                state.notifier.clone()
            } else {
                None
            };
            drop(state);
            if let Some(notify) = notify {
                notify();
            }
            return None;
        }
        let id = state.next_id.checked_add(1)?;
        let revoked = Arc::new(AtomicBool::new(false));
        state.next_id = id;
        state.used += bytes;
        state.entries.insert(id, Reservation { bytes, revoked: revoked.clone() });
        Some(ContentBudgetLease(Arc::new(LeaseInner {
            ledger: self.ledger.clone(),
            id,
            revoked,
        })))
    }
}

impl ContentBudgetLease {
    pub(super) fn revoked(&self) -> bool {
        self.0.revoked.load(Ordering::Acquire)
    }

    /// Includes lease/flag Arcs and amortized ledger tree allocation. This is
    /// added to the ordinary per-entry estimate before either limit is tested.
    pub(super) const fn metadata_bytes() -> usize {
        256
    }
}

impl Drop for LeaseInner {
    fn drop(&mut self) {
        let mut state = self.ledger.lock().unwrap();
        if let Some(entry) = state.entries.remove(&self.id) {
            state.used -= entry.bytes;
        }
    }
}

#[cfg(test)]
#[path = "content_resource_budget_tests.rs"]
mod tests;
