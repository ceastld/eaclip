//! Input gaps: hook events passed to the foreground application unseen
//! because a lock the hook only try-locks was busy. The hook thread records
//! them with atomics; the keyboard worker logs a rate-limited summary, since
//! a hook callback must not wait on the log file.
use std::panic::Location;
use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
#[cfg(debug_assertions)]
use std::sync::atomic::{AtomicU32, AtomicUsize};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

type Site = &'static Location<'static>;

/// The first gap is logged at once; later ones are summarized at this interval.
const REPORT_INTERVAL: Duration = Duration::from_secs(10);
#[cfg(debug_assertions)]
const RECENT_KEYS: usize = 8;

#[derive(Clone, Copy)]
pub(crate) enum GapInput {
    Key { vk: u32, down: bool },
    Mouse,
    Foreground,
    /// The UI thread's re-check before delivering queued clipboard list keys.
    ListInputCheck,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Totals {
    host_keys: u64,
    host_mouse: u64,
    host_foreground: u64,
    host_list_check: u64,
    chrome_keys: u64,
    chrome_mouse: u64,
    registry: u64,
    qp_kept: u64,
    qp_canceled: u64,
    qp_commands_dropped: u64,
}

impl Totals {
    fn gaps(&self) -> u64 {
        self.host_keys + self.host_mouse + self.host_foreground + self.host_list_check
            + self.chrome_keys + self.chrome_mouse + self.registry
    }

    fn since(&self, earlier: &Self) -> Self {
        Self {
            host_keys: self.host_keys - earlier.host_keys,
            host_mouse: self.host_mouse - earlier.host_mouse,
            host_foreground: self.host_foreground - earlier.host_foreground,
            host_list_check: self.host_list_check - earlier.host_list_check,
            chrome_keys: self.chrome_keys - earlier.chrome_keys,
            chrome_mouse: self.chrome_mouse - earlier.chrome_mouse,
            registry: self.registry - earlier.registry,
            qp_kept: self.qp_kept - earlier.qp_kept,
            qp_canceled: self.qp_canceled - earlier.qp_canceled,
            qp_commands_dropped: self.qp_commands_dropped - earlier.qp_commands_dropped,
        }
    }
}

#[derive(Default)]
pub(crate) struct InputGapStats {
    host_keys: AtomicU64,
    host_mouse: AtomicU64,
    host_foreground: AtomicU64,
    host_list_check: AtomicU64,
    chrome_keys: AtomicU64,
    chrome_mouse: AtomicU64,
    qp_kept: AtomicU64,
    qp_canceled: AtomicU64,
    qp_commands_dropped: AtomicU64,
    /// Missed keys, `vk | down << 16`, newest at `recent_next - 1`.
    #[cfg(debug_assertions)]
    recent_keys: [AtomicU32; RECENT_KEYS],
    #[cfg(debug_assertions)]
    recent_next: AtomicUsize,
    /// The longest-held HostInner holder seen by a gap since the last report.
    holder_site: AtomicPtr<Location<'static>>,
    holder_held_us: AtomicU64,
    /// Registry gaps are counted process-wide in `hooks`.
    registry_base: OnceLock<u64>,
    report: parking_lot::Mutex<ReportState>,
}

#[derive(Default)]
struct ReportState {
    last: Option<Instant>,
    reported: Totals,
    #[cfg(debug_assertions)]
    reported_keys: usize,
}

fn site_ptr(site: Site) -> *mut Location<'static> {
    site as *const Location<'static> as *mut Location<'static>
}

fn format_site(site: Site) -> String {
    let file = site.file().rsplit(['/', '\\']).next().unwrap_or(site.file());
    format!("{file}:{}", site.line())
}

#[cfg(debug_assertions)]
fn format_key(code: u32) -> String {
    format!("{:02X}{}", code & 0xFFFF, if code >> 16 != 0 { "↓" } else { "↑" })
}

impl InputGapStats {
    pub(crate) fn new() -> Self {
        let stats = Self::default();
        let _ = stats.registry_base.set(crate::hooks::registry_gap_count());
        stats
    }

    fn registry(&self) -> u64 {
        crate::hooks::registry_gap_count()
            .saturating_sub(*self.registry_base.get().unwrap_or(&0))
    }

    fn totals(&self) -> Totals {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        Totals {
            host_keys: load(&self.host_keys),
            host_mouse: load(&self.host_mouse),
            host_foreground: load(&self.host_foreground),
            host_list_check: load(&self.host_list_check),
            chrome_keys: load(&self.chrome_keys),
            chrome_mouse: load(&self.chrome_mouse),
            registry: self.registry(),
            qp_kept: load(&self.qp_kept),
            qp_canceled: load(&self.qp_canceled),
            qp_commands_dropped: load(&self.qp_commands_dropped),
        }
    }

    #[cfg(debug_assertions)]
    fn record_key(&self, input: GapInput) {
        if let GapInput::Key { vk, down } = input {
            let slot = self.recent_next.fetch_add(1, Ordering::Relaxed) % RECENT_KEYS;
            self.recent_keys[slot].store(vk & 0xFFFF | (down as u32) << 16, Ordering::Relaxed);
        }
    }

    /// Hook thread: HostInner was held by `holder` when `input` arrived.
    pub(crate) fn record_host_busy(&self, input: GapInput, holder: Option<(Site, Duration)>) {
        let counter = match input {
            GapInput::Key { .. } => &self.host_keys,
            GapInput::Mouse => &self.host_mouse,
            GapInput::Foreground => &self.host_foreground,
            GapInput::ListInputCheck => &self.host_list_check,
        };
        counter.fetch_add(1, Ordering::Relaxed);
        #[cfg(debug_assertions)]
        self.record_key(input);
        if let Some((site, held)) = holder {
            let held = held.as_micros() as u64;
            if held >= self.holder_held_us.load(Ordering::Relaxed) {
                self.holder_held_us.store(held, Ordering::Relaxed);
                self.holder_site.store(site_ptr(site), Ordering::Release);
            }
        }
    }

    /// Hook thread: the window-chrome registry was busy.
    pub(crate) fn record_chrome_busy(&self, input: GapInput) {
        match input {
            GapInput::Mouse => &self.chrome_mouse,
            _ => &self.chrome_keys,
        }
        .fetch_add(1, Ordering::Relaxed);
        #[cfg(debug_assertions)]
        self.record_key(input);
    }

    /// A gap was reconciled while a quick paste session or its queued work existed.
    pub(crate) fn record_quick_paste(&self, kept: bool, dropped_commands: usize) {
        if kept { &self.qp_kept } else { &self.qp_canceled }.fetch_add(1, Ordering::Relaxed);
        self.qp_commands_dropped.fetch_add(dropped_commands as u64, Ordering::Relaxed);
    }

    /// Cheap check for the keyboard worker's poll decision.
    pub(crate) fn report_due(&self) -> bool {
        let Some(report) = self.report.try_lock() else { return false; };
        self.totals() != report.reported
            && report.last.is_none_or(|last| last.elapsed() >= REPORT_INTERVAL)
    }

    pub(crate) fn report_delay(&self) -> Option<Duration> {
        let Some(report) = self.report.try_lock() else { return Some(Duration::from_millis(50)); };
        (self.totals() != report.reported).then(|| {
            report.last.map_or(Duration::ZERO, |last| REPORT_INTERVAL.saturating_sub(last.elapsed()))
        })
    }

    /// Keyboard worker: log what changed since the previous report.
    pub(crate) fn report(&self, slow: Option<crate::host::host_lock::SlowHolds>) {
        let mut report = self.report.lock();
        if report.last.is_some_and(|last| last.elapsed() < REPORT_INTERVAL) { return; }
        let totals = self.totals();
        if totals == report.reported { return; }
        let delta = totals.since(&report.reported);
        #[cfg(debug_assertions)]
        let (next, missed_keys) = {
            let next = self.recent_next.load(Ordering::Relaxed);
            let missed = next - report.reported_keys;
            let keys: Vec<String> = (next - missed.min(RECENT_KEYS)..next)
                .map(|index| format_key(self.recent_keys[index % RECENT_KEYS].load(Ordering::Relaxed)))
                .collect();
            (next, keys.join(" "))
        };
        let holder_us = self.holder_held_us.swap(0, Ordering::Relaxed);
        // Only `Location::caller()` statics are ever stored.
        let holder = unsafe { self.holder_site.load(Ordering::Acquire).as_ref() }
            .filter(|_| holder_us != 0)
            .map(|site| format!("{} for {:.1} ms", format_site(site), holder_us as f64 / 1000.0))
            .unwrap_or_default();
        let slow_holds = slow.filter(|slow| slow.count != 0).map(|slow| {
            format!("{}x >=4 ms, slowest {} {:.1} ms ({:.0} s ago)", slow.count,
                slow.site.map(format_site).unwrap_or_default(),
                slow.slowest.as_secs_f64() * 1000.0, slow.slowest_age.as_secs_f64())
        }).unwrap_or_default();
        macro_rules! report_gaps {
            ($($debug_fields:tt)*) => {
                tracing::warn!(
                    gaps = delta.gaps(),
                    host_keys = delta.host_keys,
                    host_mouse = delta.host_mouse,
                    host_foreground = delta.host_foreground,
                    host_list_check = delta.host_list_check,
                    chrome_keys = delta.chrome_keys,
                    chrome_mouse = delta.chrome_mouse,
                    registry = delta.registry,
                    qp_kept = delta.qp_kept,
                    qp_canceled = delta.qp_canceled,
                    qp_commands_dropped = delta.qp_commands_dropped,
                    $($debug_fields)*
                    %holder,
                    %slow_holds,
                    "Input hook passed events through while host state was busy"
                );
            };
        }
        #[cfg(debug_assertions)]
        report_gaps!(%missed_keys,);
        #[cfg(not(debug_assertions))]
        report_gaps!();
        report.last = Some(Instant::now());
        report.reported = totals;
        #[cfg(debug_assertions)]
        { report.reported_keys = next; }
    }

    /// Cumulative counters for the debug quick paste status.
    pub(crate) fn snapshot(&self) -> Value {
        let totals = self.totals();
        json!({
            "gaps": totals.gaps(),
            "hostInnerKeys": totals.host_keys,
            "hostInnerMouse": totals.host_mouse,
            "hostInnerForeground": totals.host_foreground,
            "hostInnerListInputCheck": totals.host_list_check,
            "chromeKeys": totals.chrome_keys,
            "chromeMouse": totals.chrome_mouse,
            "registry": totals.registry,
            "quickPasteKept": totals.qp_kept,
            "quickPasteCanceled": totals.qp_canceled,
            "quickPasteCommandsDropped": totals.qp_commands_dropped,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_each_gate_and_reports_only_changes() {
        // Registry gaps are process-wide; other tests may add some meanwhile.
        let stats = InputGapStats::new();
        stats.record_host_busy(GapInput::Key { vk: 0xC0, down: true }, Some((Location::caller(), Duration::from_millis(3))));
        stats.record_host_busy(GapInput::Mouse, None);
        stats.record_chrome_busy(GapInput::Key { vk: 0xC0, down: false });
        stats.record_quick_paste(true, 0);
        stats.record_quick_paste(false, 2);
        let snapshot = stats.snapshot();
        assert_eq!(snapshot["gaps"].as_u64().unwrap() - snapshot["registry"].as_u64().unwrap(), 3);
        assert_eq!(snapshot["hostInnerKeys"], 1);
        assert_eq!(snapshot["chromeKeys"], 1);
        assert_eq!(snapshot["quickPasteKept"], 1);
        assert_eq!(snapshot["quickPasteCanceled"], 1);
        assert_eq!(snapshot["quickPasteCommandsDropped"], 2);
        assert!(stats.report_due(), "the first gap reports at once");
        stats.report(None);
        assert!(!stats.report_due(), "reported counters do not report again");
        stats.record_host_busy(GapInput::Foreground, None);
        assert!(!stats.report_due(), "later gaps wait for the report interval");
    }

    #[test]
    fn idle_reports_have_no_timer_and_pending_reports_use_their_deadline() {
        let _input = crate::hooks::input_globals_test::exclusive();
        let stats = InputGapStats::new();
        assert_eq!(stats.report_delay(), None);
        stats.record_host_busy(GapInput::Mouse, None);
        assert_eq!(stats.report_delay(), Some(Duration::ZERO));
        stats.report(None);
        assert_eq!(stats.report_delay(), None);
        stats.record_host_busy(GapInput::Mouse, None);
        let delay = stats.report_delay().unwrap();
        assert!(delay > Duration::ZERO && delay <= REPORT_INTERVAL);
        stats.report.lock().last = Some(Instant::now() - REPORT_INTERVAL);
        assert_eq!(stats.report_delay(), Some(Duration::ZERO));
        assert!(stats.report_due());
    }

    #[test]
    fn formats_sites_and_debug_key_codes() {
        #[cfg(debug_assertions)]
        {
            assert_eq!(format_key(0xC0 | 1 << 16), "C0↓");
            assert_eq!(format_key(0x12), "12↑");
        }
        assert!(format_site(Location::caller()).starts_with("input_gaps.rs:"));
    }

    #[test]
    fn report_includes_key_sequence_only_in_debug_builds() {
        use std::collections::BTreeMap;
        use std::sync::{Arc, Mutex};
        use tracing::field::{Field, Visit};
        use tracing::span::{Attributes, Id, Record};
        use tracing::{Event, Metadata, Subscriber};

        #[derive(Clone, Default)]
        struct Capture(Arc<Mutex<BTreeMap<String, String>>>);
        impl Visit for Capture {
            fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                self.0.lock().unwrap().insert(field.name().into(), format!("{value:?}"));
            }
        }
        impl Subscriber for Capture {
            fn enabled(&self, _: &Metadata<'_>) -> bool { true }
            fn new_span(&self, _: &Attributes<'_>) -> Id { Id::from_u64(1) }
            fn record(&self, _: &Id, _: &Record<'_>) {}
            fn record_follows_from(&self, _: &Id, _: &Id) {}
            fn event(&self, event: &Event<'_>) { event.record(&mut self.clone()); }
            fn enter(&self, _: &Id) {}
            fn exit(&self, _: &Id) {}
        }

        let capture = Capture::default();
        tracing::subscriber::with_default(capture.clone(), || {
            let stats = InputGapStats::new();
            stats.record_host_busy(
                GapInput::Key { vk: 0xC0, down: true },
                Some((Location::caller(), Duration::from_millis(3))),
            );
            stats.record_chrome_busy(GapInput::Key { vk: 0x12, down: false });
            stats.report(None);
        });
        let fields = capture.0.lock().unwrap();
        assert_eq!(fields.get("host_keys").map(String::as_str), Some("1"));
        assert_eq!(fields.get("chrome_keys").map(String::as_str), Some("1"));
        assert!(fields.get("holder").unwrap().contains("input_gaps.rs:"));
        #[cfg(debug_assertions)]
        assert_eq!(fields.get("missed_keys").map(String::as_str), Some("C0↓ 12↑"));
        #[cfg(not(debug_assertions))]
        assert!(!fields.contains_key("missed_keys"), "Release events must not contain key sequences");
    }
}
