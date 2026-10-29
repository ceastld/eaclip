//! Process-wide ownership of GPUI image assets by prepared resource holders.
//!
//! Claims use image.id(), so independently allocated Images with identical bytes
//! protect the same GPUI asset. One lease claims each distinct id once; cloned
//! leases share that claim until their final Drop. Acquire before publishing a
//! resource snapshot, and recheck `image_has_holders` inside deferred cleanup.
//!
//! The pending index keeps only Weak<Image>. To guarantee cleanup after a holder
//! drops without an App, the App must install a retirement scheduler before any
//! such resources paint. The scheduler receives strong image packets outside
//! the ledger lock and must retain them until its App-side deferred guard/cleanup
//! completes. Without that scheduler, an expired weak record cannot remove an
//! already decoded GPUI asset. No arbitrary thread may access App from Drop.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, OnceLock, Weak},
};

use gpui::Image;

pub type RetirementScheduler = Arc<dyn Fn(Vec<Arc<Image>>) + Send + Sync + 'static>;

#[derive(Default)]
struct Ledger {
    holders: HashMap<u64, usize>,
    pending: HashMap<u64, Weak<Image>>,
    scheduler: Option<RetirementScheduler>,
}

type SharedLedger = Arc<Mutex<Ledger>>;

fn ledger() -> &'static SharedLedger {
    static LEDGER: OnceLock<SharedLedger> = OnceLock::new();
    LEDGER.get_or_init(|| Arc::new(Mutex::new(Ledger::default())))
}

/// An explicit resource holder, independent of Arc<Image>'s incidental clones.
#[derive(Clone)]
pub struct ImageHolderLease {
    inner: Option<Arc<LeaseInner>>,
}

struct LeaseInner {
    ledger: SharedLedger,
    images: Vec<Arc<Image>>,
}

impl ImageHolderLease {
    pub fn new(images: Vec<Arc<Image>>) -> Self {
        if images.is_empty() {
            return Self { inner: None };
        }
        Self::with_ledger(images, ledger().clone())
    }

    fn with_ledger(images: Vec<Arc<Image>>, ledger: SharedLedger) -> Self {
        let mut seen = HashSet::new();
        let mut images: Vec<_> = images
            .into_iter()
            .filter(|image| seen.insert(image.id()))
            .collect();
        if images.is_empty() {
            return Self { inner: None };
        }
        images.shrink_to_fit();
        {
            let mut state = ledger.lock().unwrap();
            for image in &images {
                *state.holders.entry(image.id()).or_default() += 1;
                // Reacquiring an id invalidates its earlier queued retirement.
                state.pending.remove(&image.id());
            }
        }
        Self {
            inner: Some(Arc::new(LeaseInner { ledger, images })),
        }
    }

    /// Conservative per-holder estimate includes its Arc/Vec and amortized
    /// process ledger headers/buckets. Identical ids shared by holders may be
    /// overcounted deliberately; the estimate never borrows another cache's
    /// budget. Zero-image negative entries allocate no lease or ledger backing.
    pub fn estimate_retained_bytes(images_count: usize) -> usize {
        if images_count == 0 {
            return 0;
        }
        holder_header_bytes()
            .saturating_add(
                images_count
                    .saturating_mul(2)
                    .max(4)
                    .saturating_mul(std::mem::size_of::<Arc<Image>>()),
            )
            // Allow both active-count and retirement-weak tables, including
            // low-load-factor allocation, hash control bytes and bucket slack.
            .saturating_add(images_count.saturating_mul(256))
    }

    pub(super) fn metadata_bytes(&self) -> usize {
        self.inner.as_ref().map_or(0, |inner| {
            Self::estimate_retained_bytes(inner.images.len()).max(
                holder_header_bytes()
                    .saturating_add(
                        inner
                            .images
                            .capacity()
                            .saturating_mul(std::mem::size_of::<Arc<Image>>()),
                    )
                    .saturating_add(inner.images.len().saturating_mul(256)),
            )
        })
    }
}

fn holder_header_bytes() -> usize {
    std::mem::size_of::<LeaseInner>()
        .saturating_add(std::mem::size_of::<Mutex<Ledger>>())
        .saturating_add(4 * std::mem::size_of::<usize>())
}

impl Drop for LeaseInner {
    fn drop(&mut self) {
        let (retired, scheduler) = {
            let mut state = self.ledger.lock().unwrap();
            let mut retired = Vec::new();
            for image in &self.images {
                let id = image.id();
                let Some(count) = state.holders.get_mut(&id) else {
                    continue;
                };
                if *count > 1 {
                    *count -= 1;
                } else {
                    state.holders.remove(&id);
                    state.pending.insert(id, Arc::downgrade(image));
                    // Capture the last holder's source before its own strong
                    // references vanish. Scheduler handoff guarantees lifetime.
                    retired.push(image.clone());
                }
            }
            (retired, state.scheduler.clone())
        };
        if !retired.is_empty() {
            if let Some(schedule) = scheduler {
                // May enqueue on a foreground task, but must not borrow App or
                // perform GPUI cleanup on this possibly non-UI dropping thread.
                schedule(retired);
            }
        }
    }
}

pub fn image_has_holders(id: u64) -> bool {
    ledger().lock().unwrap().holders.contains_key(&id)
}

/// Install an App-owned foreground handoff, typically a Tokio unbounded Sender
/// captured by this Send + Sync closure. Return the previous scheduler so an App
/// can preserve/restore its registration rather than resetting live claims.
pub fn set_retirement_scheduler(scheduler: RetirementScheduler) -> Option<RetirementScheduler> {
    ledger().lock().unwrap().scheduler.replace(scheduler)
}

/// Drain zero-holder weak records for immediate App-side cleanup. The caller
/// must still recheck holders when its deferred closure actually runs. A stale
/// scheduler packet is equally harmless only when it performs that same guard.
pub fn take_retired_images() -> Vec<Arc<Image>> {
    take_retired_images_from(ledger())
}

fn take_retired_images_from(ledger: &SharedLedger) -> Vec<Arc<Image>> {
    let mut state = ledger.lock().unwrap();
    let ids: Vec<_> = state.pending.keys().copied().collect();
    let mut images = Vec::new();
    for id in ids {
        if state.holders.contains_key(&id) {
            continue;
        }
        if let Some(image) = state.pending.remove(&id).and_then(|image| image.upgrade()) {
            images.push(image);
        }
    }
    shrink_idle_tables(&mut state);
    images
}

/// Call after actual deferred CPU/GPU retirement to remove its weak index. A
/// live claim protects a reacquired image even from an old scheduler packet.
pub fn acknowledge_image_retirement(id: u64) {
    let mut state = ledger().lock().unwrap();
    if !state.holders.contains_key(&id) {
        state.pending.remove(&id);
    }
    shrink_idle_tables(&mut state);
}

fn shrink_idle_tables(state: &mut Ledger) {
    if state.holders.is_empty() && state.pending.is_empty() {
        state.holders.shrink_to_fit();
        state.pending.shrink_to_fit();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::ImageFormat;

    fn image() -> Arc<Image> {
        Arc::new(Image::from_bytes(
            ImageFormat::Svg,
            b"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"1\" height=\"1\"/>".to_vec(),
        ))
    }

    fn local_ledger() -> SharedLedger {
        Arc::new(Mutex::new(Ledger::default()))
    }

    #[test]
    fn independent_arcs_share_id_and_each_lease_claims_duplicates_once() {
        let ledger = local_ledger();
        let first = image();
        let second = image();
        assert!(!Arc::ptr_eq(&first, &second));
        assert_eq!(first.id(), second.id());
        let a = ImageHolderLease::with_ledger(vec![first.clone(), first.clone()], ledger.clone());
        let b = ImageHolderLease::with_ledger(vec![second.clone()], ledger.clone());
        assert_eq!(ledger.lock().unwrap().holders[&first.id()], 2);
        drop(a);
        assert_eq!(ledger.lock().unwrap().holders[&first.id()], 1);
        assert!(take_retired_images_from(&ledger).is_empty());
        drop(b);
        assert!(!ledger.lock().unwrap().holders.contains_key(&first.id()));
        assert_eq!(take_retired_images_from(&ledger).len(), 1);
    }

    #[test]
    fn lease_clone_keeps_its_claim_until_the_final_drop() {
        let ledger = local_ledger();
        let image = image();
        let lease = ImageHolderLease::with_ledger(vec![image.clone()], ledger.clone());
        let clone = lease.clone();
        assert_eq!(ledger.lock().unwrap().holders[&image.id()], 1);
        drop(lease);
        assert_eq!(ledger.lock().unwrap().holders[&image.id()], 1);
        drop(clone);
        assert_eq!(take_retired_images_from(&ledger).len(), 1);
    }

    #[test]
    fn queued_retirement_is_invalidated_when_the_id_is_reacquired() {
        let ledger = local_ledger();
        let first = image();
        let a = ImageHolderLease::with_ledger(vec![first.clone()], ledger.clone());
        drop(a);
        assert!(ledger.lock().unwrap().pending.contains_key(&first.id()));
        let second = image();
        let b = ImageHolderLease::with_ledger(vec![second.clone()], ledger.clone());
        assert!(ledger.lock().unwrap().pending.is_empty());
        assert!(take_retired_images_from(&ledger).is_empty());
        drop(b);
        let retired = take_retired_images_from(&ledger);
        assert_eq!(retired.len(), 1);
        assert!(Arc::ptr_eq(&retired[0], &second));
    }

    #[test]
    fn final_holder_drop_hands_off_a_strong_packet_outside_the_lock() {
        let ledger = local_ledger();
        let (sender, receiver) = std::sync::mpsc::channel();
        let for_callback = ledger.clone();
        ledger.lock().unwrap().scheduler = Some(Arc::new(move |images| {
            // This would deadlock if the scheduler ran under the ledger lock.
            assert!(for_callback.lock().unwrap().holders.is_empty());
            sender.send(images).unwrap();
        }));
        let source = image();
        let weak = Arc::downgrade(&source);
        let lease = ImageHolderLease::with_ledger(vec![source], ledger.clone());
        drop(lease);
        let packet = receiver.recv().unwrap();
        assert!(weak.upgrade().is_some());
        let retired = take_retired_images_from(&ledger);
        assert_eq!(retired.len(), 1);
        drop(retired);
        drop(packet);
        assert!(weak.upgrade().is_none());
        assert!(ledger.lock().unwrap().pending.is_empty());
        // Avoid a callback -> ledger cycle in this isolated test fixture.
        ledger.lock().unwrap().scheduler = None;
    }

    #[test]
    fn an_unpainted_expired_weak_record_does_not_retain_encoded_bytes() {
        let ledger = local_ledger();
        let source = image();
        let weak = Arc::downgrade(&source);
        let lease = ImageHolderLease::with_ledger(vec![source], ledger.clone());
        drop(lease);
        assert!(weak.upgrade().is_none());
        assert!(take_retired_images_from(&ledger).is_empty());
        assert!(ledger.lock().unwrap().pending.is_empty());
    }

    #[test]
    fn retained_estimate_covers_lease_backing_and_global_metadata() {
        assert_eq!(ImageHolderLease::estimate_retained_bytes(0), 0);
        let ledger = local_ledger();
        let lease = ImageHolderLease::with_ledger(vec![image()], ledger);
        let inner = lease.inner.as_ref().unwrap();
        let direct_backing = std::mem::size_of::<LeaseInner>()
            + 2 * std::mem::size_of::<usize>()
            + inner.images.capacity() * std::mem::size_of::<Arc<Image>>();
        assert!(lease.metadata_bytes() >= direct_backing + 256);
        assert!(
            ImageHolderLease::estimate_retained_bytes(2)
                > ImageHolderLease::estimate_retained_bytes(1)
        );
        assert_eq!(ImageHolderLease::new(Vec::new()).metadata_bytes(), 0);
    }
}
