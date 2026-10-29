use super::*;
use std::{sync::{Arc, mpsc}, thread};

#[test]
fn clipboard_monitor_stop_interrupts_quiet_wait_and_is_not_lost_before_wait() {
    let stop = Arc::new(StopSignal::new());
    let worker_stop = stop.clone();
    let (ready, receive_ready) = mpsc::sync_channel(0);
    let (done, receive_done) = mpsc::sync_channel(0);
    let worker = thread::spawn(move || {
        ready.send(()).unwrap();
        worker_stop.wait_timeout(Duration::from_secs(30));
        done.send(()).unwrap();
    });
    receive_ready.recv_timeout(Duration::from_secs(2)).unwrap();
    stop.signal();
    receive_done.recv_timeout(Duration::from_secs(1)).expect("Stop did not interrupt the quiet wait");
    worker.join().unwrap();
    let started = Instant::now();
    stop.wait_timeout(Duration::from_secs(30));
    assert!(started.elapsed() < Duration::from_millis(100), "A stop before wait was lost");
}

#[test]
fn clipboard_monitor_fallback_remains_interruptible() {
    let stop = StopSignal::new();
    let mut wait = MonitorWait { #[cfg(windows)] listener: None };
    stop.signal();
    assert!(!wait.wait(&stop, false));
}

#[cfg(windows)]
#[path = "clipboard_private_station_test.rs"]
mod private_station;

#[cfg(windows)]
#[test]
#[ignore = "requires an authorized isolated process with permission to create a new non-visible private window station"]
fn clipboard_monitor_private_station_notifications() {
    use super::super::{clipboard_sequence, consume_monitored_capture, ClipboardReadFailure};
    assert_eq!(std::env::var("CLIPHOST_RUN_PRIVATE_CLIPBOARD_TESTS").as_deref(), Ok("1"),
        "Run this exact ignored test alone in an explicitly isolated test process");
    let station = private_station::PrivateStation::create()
        .expect("Private clipboard unavailable; refusing any clipboard access");
    station.verify_current().unwrap();
    let stop = Arc::new(StopSignal::new());
    assert_ne!(stop.event, 0, "Native fixture needs a real stop event");
    let mut wait = MonitorWait::new(&stop);
    assert!(wait.listener.is_some(), "Registration fallback is not native notification acceptance");

    // No periodic notification may wake an otherwise idle worker. Drain any
    // setup messages using the real queue before checking idle blocking.
    let listener = wait.listener.as_ref().unwrap();
    assert_eq!(listener.wait(&stop, Duration::from_millis(40)).unwrap(), native::Wake::TimedOut);
    let initial = clipboard_sequence();
    let started = Instant::now();
    thread::scope(|scope| {
        let station = &station;
        scope.spawn(move || {
            private_station::attach(station.desktop).unwrap();
            station.verify_current().unwrap();
            let owner = PrivateOwner::new();
            thread::sleep(Duration::from_millis(40));
            owner.publish(station, "first notification");
        });
        assert_eq!(listener.wait(&stop, Duration::from_secs(2)).unwrap(), native::Wake::Changed);
    });
    let first_wake_ms = started.elapsed().as_millis();
    assert!(first_wake_ms < 1000, "First publication was not delivered promptly");
    assert_ne!(clipboard_sequence(), initial);

    // A burst can coalesce to one message. The worker must still observe the
    // final sequence itself, and no listener may mark it consumed prematurely.
    let owner = PrivateOwner::new();
    let mut consumed_sequence = clipboard_sequence();
    for text in ["burst one", "burst two", "burst final"] { owner.publish(&station, text); }
    let final_sequence = clipboard_sequence();
    assert_ne!(final_sequence, consumed_sequence);
    assert_eq!(wait.listener.as_ref().unwrap().wait(&stop, Duration::from_secs(1)).unwrap(), native::Wake::Changed);
    assert_ne!(consumed_sequence, final_sequence);

    // The read failed after that notification was consumed. No later write or
    // message is required for the pending copy's next attempt.
    assert!(consume_monitored_capture(&mut consumed_sequence, Err(ClipboardReadFailure::Unavailable)).is_none());
    assert_ne!(consumed_sequence, final_sequence);
    let started = Instant::now();
    assert!(wait.wait(&stop, true));
    let retry_ms = started.elapsed().as_millis();
    assert!(retry_ms >= 60 && retry_ms < 1000, "Pending-copy retry did not use its 80 ms deadline: {retry_ms} ms");
    assert_eq!(clipboard_sequence(), final_sequence, "Retry unexpectedly changed the clipboard");
    assert!(consume_monitored_capture(&mut consumed_sequence, Ok((final_sequence, None))).is_none());
    assert_eq!(consumed_sequence, final_sequence);

    let started = Instant::now();
    thread::scope(|scope| {
        let signal = stop.clone();
        scope.spawn(move || { thread::sleep(Duration::from_millis(40)); signal.signal(); });
        assert!(!wait.wait(&stop, false), "Stop must release the native 30-second idle wait");
    });
    let stop_wake_ms = started.elapsed().as_millis();
    assert!(stop_wake_ms < 1000);

    // The fixture owns all content and its non-interactive station. No system
    // clipboard restoration/readback or production database access occurs.
    let receipt_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../.tmp/clipboard-monitor-notifications");
    std::fs::create_dir_all(&receipt_root).unwrap();
    std::fs::write(receipt_root.join("result.json"), serde_json::to_vec_pretty(&serde_json::json!({
        "privateStation": station.name, "firstNotificationMs": first_wake_ms,
        "retryWithoutNotificationMs": retry_ms, "idleStopMs": stop_wake_ms,
        "burstFinalSequenceConsumedOnlyByWorker": true,
        "idleWaitTimedOutWithoutClipboardEvents": true,
    })).unwrap()).unwrap();
}

#[cfg(windows)]
struct PrivateOwner(windows::Win32::Foundation::HWND);

#[cfg(windows)]
impl PrivateOwner {
    fn new() -> Self {
        use windows::{core::w, Win32::UI::WindowsAndMessaging::*};
        Self(unsafe { CreateWindowExW(WINDOW_EX_STYLE::default(), w!("STATIC"),
            w!("owned clipboard notification fixture"), WINDOW_STYLE::default(),
            0, 0, 0, 0, HWND_MESSAGE, None, None, None).unwrap() })
    }

    fn publish(&self, station: &private_station::PrivateStation, text: &str) {
        use windows::Win32::{Foundation::HANDLE,
            System::{DataExchange::{OpenClipboard, EmptyClipboard, CloseClipboard, SetClipboardData},
                Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE}}};
        station.verify_current().unwrap();
        let bytes: Vec<_> = text.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect();
        unsafe {
            OpenClipboard(self.0).unwrap();
            struct Close;
            impl Drop for Close { fn drop(&mut self) { unsafe { let _ = CloseClipboard(); } } }
            let _close = Close;
            EmptyClipboard().unwrap();
            let memory = GlobalAlloc(GMEM_MOVEABLE, bytes.len()).unwrap();
            let pointer = GlobalLock(memory);
            assert!(!pointer.is_null());
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer.cast(), bytes.len());
            let _ = GlobalUnlock(memory);
            SetClipboardData(13, HANDLE(memory.0)).unwrap();
        }
    }
}

#[cfg(windows)]
impl Drop for PrivateOwner {
    fn drop(&mut self) { unsafe { let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(self.0); } }
}
