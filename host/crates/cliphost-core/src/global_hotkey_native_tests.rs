use super::*;
use std::time::Instant;
use windows::{core::w, Win32::{Foundation::{LPARAM, WPARAM},
    UI::WindowsAndMessaging::{FindWindowExW, GetWindowThreadProcessId, PostMessageW, HWND_MESSAGE}}};

#[path = "clipboard_private_station_test.rs"]
mod private_station;

#[test]
#[ignore = "requires an authorized isolated process with permission to create a new non-visible private window station"]
fn global_hotkey_private_station_event_pump() {
    assert_eq!(std::env::var("CLIPHOST_RUN_PRIVATE_HOTKEY_TESTS").as_deref(), Ok("1"),
        "Run this exact ignored test alone in an explicitly isolated test process");
    let station = Arc::new(private_station::PrivateStation::create()
        .expect("Private station unavailable; refusing to register a hotkey on the user desktop"));
    station.verify_current().unwrap();
    let (tx, rx) = mpsc::channel();
    let wake = Arc::new(CommandSignal::new().unwrap());
    let listener = GlobalHotkeyListener { tx, wake: wake.clone() };
    let (initial_reply, initial_results) = mpsc::channel();
    // Queue and signal before the worker even exists. The initialization race
    // must not drop either the command or the wakeup.
    listener.tx.send(ListenerCmd::Replace {
        bindings: vec![(HOTKEY_ID_CLIP_TOGGLE, MOD_NOREPEAT, 0x87)], // F24 on the private station only.
        reply: initial_reply,
    }).unwrap();
    wake.signal();
    let (hotkey, received_hotkey) = mpsc::channel();
    let (done, stopped) = mpsc::channel();
    let worker_station = station.clone();
    let worker = thread::spawn(move || {
        private_station::attach(worker_station.desktop).unwrap();
        worker_station.verify_current().unwrap();
        unsafe { pump(rx, wake, Arc::new(move |id| { let _ = hotkey.send(id); })); }
        done.send(()).unwrap();
    });
    assert_eq!(initial_results.recv_timeout(Duration::from_secs(2)).unwrap(), vec![(HOTKEY_ID_CLIP_TOGGLE, 0)]);
    assert!(received_hotkey.recv_timeout(Duration::from_millis(60)).is_err());

    // Replacing bindings must wake a completely idle message pump immediately.
    let started = Instant::now();
    assert_eq!(listener.replace_bindings(vec![(HOTKEY_ID_FAVOR_TOGGLE, MOD_NOREPEAT, 0x86)]),
        vec![(HOTKEY_ID_FAVOR_TOGGLE, 0)]); // F23 on the private station.
    let replace_ms = started.elapsed().as_millis();
    assert!(replace_ms < 1000);
    station.verify_current().unwrap();
    let hwnd = unsafe { FindWindowExW(HWND_MESSAGE, None, w!("ClipHostTauriGlobalHotkey"), None).unwrap() };
    let mut owner_pid = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut owner_pid)); }
    assert_eq!(owner_pid, std::process::id(), "Fixture may only address its own listener");
    let started = Instant::now();
    // Address the owned hidden window directly; no keyboard input is injected.
    unsafe { PostMessageW(hwnd, WM_HOTKEY, WPARAM(HOTKEY_ID_FAVOR_TOGGLE as usize), LPARAM(0)).unwrap(); }
    assert_eq!(received_hotkey.recv_timeout(Duration::from_secs(1)).unwrap(), HOTKEY_ID_FAVOR_TOGGLE);
    let hotkey_ms = started.elapsed().as_millis();
    assert!(received_hotkey.recv_timeout(Duration::from_millis(60)).is_err());

    let started = Instant::now();
    drop(listener);
    stopped.recv_timeout(Duration::from_secs(1)).expect("Idle hotkey listener did not stop");
    worker.join().unwrap();
    let stop_ms = started.elapsed().as_millis();
    let receipt_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../.tmp/global-hotkey-event-pump");
    std::fs::create_dir_all(&receipt_root).unwrap();
    std::fs::write(receipt_root.join("result.json"), serde_json::to_vec_pretty(&serde_json::json!({
        "privateStation": station.name, "queuedBeforeStartupRegistered": true,
        "idleReplaceMs": replace_ms, "ownedHotkeyMessageMs": hotkey_ms, "idleStopMs": stop_ms,
    })).unwrap()).unwrap();
}
