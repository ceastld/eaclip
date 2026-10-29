use super::*;
use crate::window_state::{self, SavedWindowState, MAIN_WINDOW};

fn fixture() -> (tempfile::TempDir, HostState) {
    crate::clipboard::force_headless(true);
    let dir = tempfile::tempdir().unwrap();
    let host = HostState::open(
        AppPaths::from_data_root(dir.path().join("BetterClipTauri")),
        28965,
    )
    .unwrap();
    (dir, host)
}

#[cfg(windows)]
#[test]
#[ignore = "Uses a hidden native HWND and a process-wide headless override; run alone"]
fn native_window_style_notifications_can_read_host_state() {
    use std::sync::atomic::AtomicUsize;
    use windows::core::w;
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::*;

    struct Probe { host: HostState, notifications: AtomicUsize, locked: AtomicUsize }
    unsafe extern "system" fn procedure(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        if msg == WM_STYLECHANGING {
            let data = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Probe;
            if let Some(probe) = data.as_ref() {
                probe.notifications.fetch_add(1, Ordering::SeqCst);
                // Real window callbacks read HostState. Detect the deadlock
                // without hanging this regression if the caller still owns it.
                if probe.host.inner.try_lock().is_none() {
                    probe.locked.fetch_add(1, Ordering::SeqCst);
                }
            }
        }
        DefWindowProcW(hwnd, msg, wp, lp)
    }
    let (_dir, host) = fixture();
    let probe = Box::new(Probe { host: host.clone(), notifications: AtomicUsize::new(0), locked: AtomicUsize::new(0) });
    unsafe {
        let instance = GetModuleHandleW(None).unwrap();
        let class = WNDCLASSW { lpfnWndProc: Some(procedure), hInstance: instance.into(),
            lpszClassName: w!("ClipHostStyleLockProbe"), ..Default::default() };
        assert_ne!(RegisterClassW(&class), 0);
        let hwnd = CreateWindowExW(WINDOW_EX_STYLE::default(), class.lpszClassName,
            w!("ClipHost hidden style probe"), WINDOW_STYLE::default(), 0, 0, 1, 1,
            HWND_MESSAGE, None, instance, None).unwrap();
        struct Cleanup(HWND, Option<String>);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                crate::clipboard::force_headless(true);
                if let Some(value) = &self.1 { std::env::set_var("CLIPHOST_TAURI_HEADLESS", value); }
                else { std::env::remove_var("CLIPHOST_TAURI_HEADLESS"); }
                unsafe { let _ = DestroyWindow(self.0); }
            }
        }
        let _cleanup = Cleanup(hwnd, std::env::var("CLIPHOST_TAURI_HEADLESS").ok());
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, &*probe as *const Probe as isize);
        std::env::set_var("CLIPHOST_TAURI_HEADLESS", "0");
        crate::clipboard::force_headless(false);
        host.set_host_hwnd(hwnd.0 as isize);
        assert_eq!(probe.locked.load(Ordering::SeqCst), 0,
            "SetWindowLongW synchronously called back while HostInner was locked");
        assert_eq!(probe.notifications.load(Ordering::SeqCst), 0,
            "state mutation must defer native styles to the window loop");
        let apply = |expected: bool| {
            assert_eq!(host.take_clipboard_no_activate_update(), Some(expected));
            crate::window_menu::set_no_activate(hwnd.0 as i64, expected).unwrap();
            assert_eq!(GetWindowLongW(hwnd, GWL_EXSTYLE) & 0x0800_0000 != 0, expected);
            assert_eq!(probe.locked.load(Ordering::SeqCst), 0);
            assert_eq!(host.take_clipboard_no_activate_update(), None);
        };
        apply(host.clipboard_no_activate());
        assert!(probe.notifications.load(Ordering::SeqCst) > 0);
        host.exit_no_focus_session_for_keyboard();
        apply(false);
        host.config_update(&json!({"enableNoFocusMode": false})).unwrap();
        host.config_update(&json!({"enableNoFocusMode": true})).unwrap();
        // Multiple pending changes coalesce to the latest effective style.
        apply(true);
        host.window_hide();
        apply(false);
    }
}

#[test]
fn window_persistence_saves_the_last_event_after_the_quiet_period() {
    let (_dir, host) = fixture();
    let start = Instant::now();
    host.window_states.remember(
        SavedWindowState::new(MAIN_WINDOW, 1.0, 2.0, 500.0, 500.0),
        start,
    );
    host.window_states.remember(
        SavedWindowState::new(MAIN_WINDOW, 99.0, 100.0, 700.0, 650.0),
        start + Duration::from_millis(800),
    );
    host.window_states
        .flush_with(start + Duration::from_millis(1100), false, |_| {
            panic!("saved before drag settled")
        })
        .unwrap();
    let mut saved = Vec::new();
    host.window_states
        .flush_with(start + Duration::from_millis(1900), false, |state| {
            saved.push(state.clone());
            Ok(())
        })
        .unwrap();
    assert_eq!(saved.len(), 1);
    assert_eq!(
        (saved[0].left, saved[0].width, saved[0].height),
        (99.0, 700.0, 650.0)
    );
    assert_eq!(host.window_states.latest(MAIN_WINDOW).as_ref(), Some(&saved[0]));
    assert!(!host.window_states.has_due(start + Duration::from_millis(1900)));
}

#[test]
fn window_persistence_slow_write_keeps_window_events_responsive_and_preserves_newer_state() {
    let (_dir, host) = fixture();
    let root = host.inner.lock().paths.data_root.clone();
    host.remember_host_window_rect("main", 10.0, 20.0, 500.0, 550.0);
    let writer = host.window_states.clone();
    let writing_root = root.clone();
    let (entered, ready) = mpsc::channel();
    let (release, wait) = mpsc::channel();
    let writing = thread::spawn(move || {
        writer.flush_with(Instant::now(), true, |state| {
            entered.send(()).unwrap();
            wait.recv_timeout(Duration::from_secs(10)).unwrap();
            window_state::save(&writing_root, state)
        })
    });
    ready
        .recv_timeout(Duration::from_secs(3))
        .expect("writer started");
    let changing = host.clone();
    let (finished, response) = mpsc::channel();
    let events = thread::spawn(move || {
        for step in 0..20 {
            changing.remember_host_window_rect("main", 100.0 + step as f64, 200.0, 750.0, 650.0);
        }
        finished.send(changing.window_hide()).unwrap();
    });
    let responsive = response.recv_timeout(Duration::from_secs(3));
    release.send(()).unwrap();
    writing.join().unwrap().unwrap();
    events.join().unwrap();
    assert_eq!(
        responsive.expect("window events waited for disk I/O")["ok"],
        true
    );
    assert_eq!(host.window_states.latest(MAIN_WINDOW).unwrap().left, 119.0);
    assert_eq!(window_state::load(&root, MAIN_WINDOW).unwrap().left, 10.0);
    host.flush_for_shutdown();
    let restored = window_state::load(&root, MAIN_WINDOW).unwrap();
    assert_eq!(
        (restored.left, restored.width, restored.height),
        (119.0, 750.0, 650.0)
    );
}

#[test]
fn native_window_geometry_persists_physical_position_with_logical_size_and_pin() {
    let (_dir, host) = fixture();
    let root = host.inner.lock().paths.data_root.clone();
    host.apply_window_action_for("setPinned", true, "main");
    host.remember_physical_host_window_rect("main", 2680, 100, 950, 725, 1.25);
    let pending = host.window_states.latest(MAIN_WINDOW).unwrap();
    assert_eq!(pending.physical_position, Some((2680, 100)));
    assert_eq!((pending.left, pending.top, pending.width, pending.height), (2144.0, 80.0, 760.0, 580.0));
    assert_eq!(window_state::property_bool(&pending, "IsPinned"), Some(true));
    assert!(!root.join("WindowState").exists(), "native window events must not write synchronously");
    for scale in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        host.remember_physical_host_window_rect("main", 0, 0, 100, 100, scale);
    }
    host.remember_physical_host_window_rect("main", -32000, -32000, 0, 0, 1.25);
    assert_eq!(host.window_states.latest(MAIN_WINDOW).unwrap(), pending);
    host.flush_for_shutdown();
    assert_eq!(window_state::load(&root, MAIN_WINDOW).unwrap(), pending);
}

#[test]
fn window_persistence_shutdown_flushes_without_waiting_and_ignores_minimized_geometry() {
    let (_dir, host) = fixture();
    let root = host.inner.lock().paths.data_root.clone();
    host.remember_host_window_rect("main", 10.0, 20.0, 500.0, 550.0);
    host.remember_host_window_rect("main", 40.0, 50.0, 800.0, 700.0);
    host.remember_host_window_rect("main", -32000.0, -32000.0, 0.0, 0.0);
    assert!(
        !root.join("WindowState").exists(),
        "window events wrote files synchronously"
    );
    host.flush_for_shutdown();
    let saved = window_state::load(&root, MAIN_WINDOW).unwrap();
    assert_eq!(
        (saved.left, saved.top, saved.width, saved.height),
        (40.0, 50.0, 800.0, 700.0)
    );
    assert_eq!(host.window_states.latest(MAIN_WINDOW), Some(saved));
    assert!(!host.window_states.has_due(Instant::now() + Duration::from_secs(2)));
}

#[test]
#[cfg(windows)]
fn window_persistence_failed_replace_preserves_previous_file_and_retries_latest_state() {
    use std::os::windows::fs::OpenOptionsExt;
    let (_dir, host) = fixture();
    let root = host.inner.lock().paths.data_root.clone();
    host.remember_host_window_rect("main", 10.0, 20.0, 500.0, 550.0);
    host.flush_for_shutdown();
    let path = root.join("WindowState").join(format!("{MAIN_WINDOW}.json"));
    let before = std::fs::read(&path).unwrap();
    let held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(&path)
        .unwrap();
    host.remember_host_window_rect("main", 30.0, 40.0, 800.0, 700.0);
    assert!(host.window_states.flush(&root, true).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(host.window_states.latest(MAIN_WINDOW).unwrap().left, 30.0);
    drop(held);
    host.flush_for_shutdown();
    assert_eq!(window_state::load(&root, MAIN_WINDOW).unwrap().left, 30.0);
}
