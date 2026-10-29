//! Slow companion classification belongs to the existing ordinary poll worker.
//! The input callback reads only identity-checked, short-lived facts under its
//! already-owned HostInner guard. Unknown facts never authorize dismissal.
use super::*;

const COMPANION_MAX_AGE: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct InputWindowIdentity {
    pub hwnd: isize,
    pub pid: u32,
    pub thread: u32,
}

#[derive(Clone, Copy)]
struct CompanionEntry {
    identity: InputWindowIdentity,
    epoch: u64,
    sampled_at: Instant,
    companion: bool,
}

#[derive(Default)]
pub(super) struct InputCompanionCache {
    entries: Vec<CompanionEntry>,
}

impl InputCompanionCache {
    fn publish(&mut self, epoch: u64, sampled_at: Instant, facts: &[(InputWindowIdentity, bool)]) {
        // At most the current pointer and foreground HWND are retained. No
        // input miss spawns work or queues a key for later classification.
        self.entries = facts
            .iter()
            .take(2)
            .map(|(identity, companion)| CompanionEntry {
                identity: *identity,
                epoch,
                sampled_at,
                companion: *companion,
            })
            .collect();
    }

    fn lookup(&self, identity: InputWindowIdentity, epoch: u64, now: Instant) -> Option<bool> {
        self.entries
            .iter()
            .find(|entry| {
                entry.identity == identity
                    && entry.epoch == epoch
                    && now.saturating_duration_since(entry.sampled_at) <= COMPANION_MAX_AGE
            })
            .map(|entry| entry.companion)
    }
}

#[cfg(test)]
thread_local! {
    static INPUT_TEST_WINDOW: std::cell::Cell<Option<InputWindowIdentity>> = const { std::cell::Cell::new(None) };
}

pub(super) fn input_window_identity(hwnd: isize) -> Option<InputWindowIdentity> {
    #[cfg(test)]
    if let Some(identity) = INPUT_TEST_WINDOW.with(|window| window.get()) {
        return (identity.hwnd == hwnd).then_some(identity);
    }
    #[cfg(windows)]
    unsafe {
        use windows::Win32::{Foundation::HWND, UI::WindowsAndMessaging::GetWindowThreadProcessId};
        let mut pid = 0;
        let thread = GetWindowThreadProcessId(HWND(hwnd as *mut _), Some(&mut pid));
        return (pid != 0 && thread != 0).then_some(InputWindowIdentity { hwnd, pid, thread });
    }
    #[cfg(not(windows))]
    {
        let _ = hwnd;
        None
    }
}

pub(super) fn input_window_from_point(x: i32, y: i32) -> isize {
    #[cfg(test)]
    if let Some(identity) = INPUT_TEST_WINDOW.with(|window| window.get()) {
        return identity.hwnd;
    }
    #[cfg(windows)]
    unsafe {
        use windows::Win32::{Foundation::POINT, UI::WindowsAndMessaging::WindowFromPoint};
        return WindowFromPoint(POINT { x, y }).0 as isize;
    }
    #[cfg(not(windows))]
    {
        let _ = (x, y);
        0
    }
}

impl HostState {
    pub(super) fn refresh_input_companions(&self) {
        if crate::clipboard::is_headless() {
            return;
        }
        #[cfg(windows)]
        {
            let foreground = crate::window_menu::foreground_hwnd();
            let pointer = cursor_screen_point()
                .map(|(x, y)| input_window_from_point(x, y))
                .unwrap_or(0);
            self.refresh_input_companions_with([foreground, pointer], |hwnd| {
                crate::host_hit_test::dismiss_companion(hwnd, std::process::id())
            });
        }
    }

    pub(super) fn refresh_input_companions_with(
        &self,
        windows: [isize; 2],
        classify: impl Fn(isize) -> bool,
    ) {
        debug_assert!(!crate::hooks::is_input_callback());
        let epoch = crate::active_window::raw_foreground_epoch();
        let sampled_at = Instant::now();
        let identities: Vec<_> = windows
            .into_iter()
            .filter_map(input_window_identity)
            .collect();
        if let Some(inner) = self.inner.try_lock() {
            if identities.iter().all(|identity| {
                inner.input_companions.entries.iter().any(|entry| {
                    entry.identity == *identity
                        && entry.epoch == epoch
                        && sampled_at.saturating_duration_since(entry.sampled_at)
                            < COMPANION_MAX_AGE / 2
                })
            }) {
                return;
            }
        } else {
            return;
        }
        let facts: Vec<_> = identities
            .into_iter()
            .map(|identity| identity.hwnd)
            .filter_map(|hwnd| {
                let identity = input_window_identity(hwnd)?;
                let companion = classify(hwnd);
                (input_window_identity(hwnd) == Some(identity)).then_some((identity, companion))
            })
            .collect();
        if sampled_at.elapsed() > COMPANION_MAX_AGE
            || epoch != crate::active_window::raw_foreground_epoch()
        {
            return;
        }
        // Slow HWND/title/process work is complete before taking HostInner.
        let Some(mut inner) = self.inner.try_lock() else {
            return;
        };
        if epoch == crate::active_window::raw_foreground_epoch() {
            inner.input_companions.publish(epoch, sampled_at, &facts);
        }
    }

    pub(super) fn input_dismiss_relation(
        &self,
        inner: &HostInner,
        host: isize,
        other: isize,
    ) -> Option<bool> {
        if super::input_routing::input_related(host, other) {
            return Some(true);
        }
        if other == 0 {
            return Some(false);
        }
        let Some(identity) = input_window_identity(other) else {
            // Existing headless fixtures use synthetic HWNDs with no native
            // identity. Explicit companion fixtures provide the identity seam.
            return crate::clipboard::is_headless().then_some(false);
        };
        inner.input_companions.lookup(
            identity,
            crate::active_window::raw_foreground_epoch(),
            Instant::now(),
        )
    }
}

#[cfg(test)]
pub(super) fn with_test_window<T>(identity: InputWindowIdentity, work: impl FnOnce() -> T) -> T {
    struct Restore(Option<InputWindowIdentity>);
    impl Drop for Restore {
        fn drop(&mut self) {
            INPUT_TEST_WINDOW.with(|window| window.set(self.0));
        }
    }
    let _restore = Restore(INPUT_TEST_WINDOW.with(|window| window.replace(Some(identity))));
    work()
}

#[cfg(test)]
#[path = "input_companion_cache_tests.rs"]
mod tests;
