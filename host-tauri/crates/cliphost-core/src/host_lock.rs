//! The `HostInner` mutex, recording who holds it. The input hook only
//! try-locks HostInner, so any hold can make a key pass through unseen; the
//! gap report names the holder's call site. Acquiring costs a clock read and
//! two atomic stores, releasing a clock read and one store.
use std::ops::{Deref, DerefMut};
use std::panic::Location;
use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

type Site = &'static Location<'static>;

/// Holds at least this long are reported as slow.
const SLOW_HOLD_US: u64 = 4_000;

fn now_us() -> u64 {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    ORIGIN.get_or_init(Instant::now).elapsed().as_micros() as u64
}

fn site_ptr(site: Site) -> *mut Location<'static> {
    site as *const Location<'static> as *mut Location<'static>
}

fn load_site(site: &AtomicPtr<Location<'static>>) -> Option<Site> {
    // Only `Location::caller()` statics are ever stored.
    unsafe { site.load(Ordering::Acquire).as_ref() }
}

pub(crate) struct HostLock<T> {
    mutex: parking_lot::Mutex<T>,
    holder: AtomicPtr<Location<'static>>,
    held_since_us: AtomicU64,
    // Written only while the mutex is held (in guard drop), so holders never race.
    slow_holds: AtomicU64,
    slowest_us: AtomicU64,
    slowest_ended_us: AtomicU64,
    slowest_site: AtomicPtr<Location<'static>>,
}

/// Slow holds since the previous `take_slow_holds`.
pub(crate) struct SlowHolds {
    pub count: u64,
    pub slowest: Duration,
    /// How long ago the slowest hold ended.
    pub slowest_age: Duration,
    pub site: Option<Site>,
}

impl<T> HostLock<T> {
    pub(crate) fn new(value: T) -> Self {
        Self {
            mutex: parking_lot::Mutex::new(value),
            holder: AtomicPtr::new(std::ptr::null_mut()),
            held_since_us: AtomicU64::new(0),
            slow_holds: AtomicU64::new(0),
            slowest_us: AtomicU64::new(0),
            slowest_ended_us: AtomicU64::new(0),
            slowest_site: AtomicPtr::new(std::ptr::null_mut()),
        }
    }

    #[track_caller]
    pub(crate) fn lock(&self) -> HostGuard<'_, T> {
        let site = Location::caller();
        let guard = self.mutex.lock();
        self.enter(guard, site)
    }

    #[track_caller]
    pub(crate) fn try_lock(&self) -> Option<HostGuard<'_, T>> {
        let site = Location::caller();
        let guard = self.mutex.try_lock()?;
        Some(self.enter(guard, site))
    }

    fn enter<'a>(&'a self, guard: parking_lot::MutexGuard<'a, T>, site: Site) -> HostGuard<'a, T> {
        let acquired_us = now_us();
        self.held_since_us.store(acquired_us, Ordering::Relaxed);
        self.holder.store(site_ptr(site), Ordering::Release);
        HostGuard { guard, lock: self, site, acquired_us }
    }

    /// The current holder and how long it has held the lock. Racy by design:
    /// a gap reads it right after its own try-lock failed.
    pub(crate) fn holder(&self) -> Option<(Site, Duration)> {
        let site = load_site(&self.holder)?;
        let since = self.held_since_us.load(Ordering::Relaxed);
        Some((site, Duration::from_micros(now_us().saturating_sub(since))))
    }

    pub(crate) fn take_slow_holds(&self) -> SlowHolds {
        let count = self.slow_holds.swap(0, Ordering::Relaxed);
        let site = load_site(&self.slowest_site);
        let slowest = Duration::from_micros(self.slowest_us.swap(0, Ordering::Relaxed));
        let ended = self.slowest_ended_us.load(Ordering::Relaxed);
        let slowest_age = Duration::from_micros(now_us().saturating_sub(ended));
        SlowHolds { count, slowest, slowest_age, site: site.filter(|_| count != 0) }
    }
}

#[must_use = "if unused the lock will immediately unlock"]
pub(crate) struct HostGuard<'a, T> {
    guard: parking_lot::MutexGuard<'a, T>,
    lock: &'a HostLock<T>,
    site: Site,
    acquired_us: u64,
}

impl<T> Deref for HostGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T { &self.guard }
}

impl<T> DerefMut for HostGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T { &mut self.guard }
}

impl<T> Drop for HostGuard<'_, T> {
    fn drop(&mut self) {
        // Runs before the inner guard unlocks.
        let lock = self.lock;
        lock.holder.store(std::ptr::null_mut(), Ordering::Release);
        let released_us = now_us();
        let held = released_us.saturating_sub(self.acquired_us);
        if held >= SLOW_HOLD_US {
            lock.slow_holds.fetch_add(1, Ordering::Relaxed);
            if held > lock.slowest_us.load(Ordering::Relaxed) {
                lock.slowest_us.store(held, Ordering::Relaxed);
                lock.slowest_ended_us.store(released_us, Ordering::Relaxed);
                lock.slowest_site.store(site_ptr(self.site), Ordering::Release);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holder_names_the_locking_call_site_until_release() {
        let lock = HostLock::new(1);
        assert!(lock.holder().is_none());
        let guard = lock.lock();
        let (site, _) = lock.holder().expect("held lock reports its holder");
        assert_eq!(site.file(), file!());
        assert!(lock.try_lock().is_none());
        drop(guard);
        assert!(lock.holder().is_none());
        assert_eq!(*lock.try_lock().unwrap(), 1);
    }

    #[test]
    fn slow_holds_are_counted_once_and_reset() {
        let lock = HostLock::new(());
        drop(lock.lock());
        assert_eq!(lock.take_slow_holds().count, 0);
        let guard = lock.lock();
        std::thread::sleep(Duration::from_micros(SLOW_HOLD_US + 2_000));
        drop(guard);
        let slow = lock.take_slow_holds();
        assert_eq!(slow.count, 1);
        assert!(slow.slowest >= Duration::from_micros(SLOW_HOLD_US));
        assert_eq!(slow.site.map(|site| site.file()), Some(file!()));
        assert_eq!(lock.take_slow_holds().count, 0);
    }
}
