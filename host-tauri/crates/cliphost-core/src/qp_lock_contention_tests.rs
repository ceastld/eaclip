//! Quick paste state publishing while the input hook only try-locks
//! `HostInner`, plus a measurement of how often that hook would find it busy:
//! rapid Alt+` sessions publish state on the keyboard worker while the native
//! overlay polls `qp_state_detail`. The measurement is timing-dependent, so it
//! only runs explicitly:
//!
//! ```text
//! cargo test -p cliphost-core --lib qp_lock_contention -- --ignored --nocapture --test-threads=1
//! ```
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

const PHASE: Duration = Duration::from_secs(4);
const TAP_INTERVAL: Duration = Duration::from_millis(80);
const POLL_INTERVAL: Duration = Duration::from_millis(16);
const SAMPLE_INTERVAL: Duration = Duration::from_micros(250);

fn words(seed: usize, chars: usize) -> String {
    const WORDS: [&str; 12] = ["clipboard", "粘贴", "history", "快速", "session", "overlay",
        "数据", "native", "selection", "预览", "quick", "文本"];
    let mut text = String::with_capacity(chars + 16);
    let mut index = seed;
    while text.chars().count() < chars {
        text.push_str(WORDS[index % WORDS.len()]);
        text.push(if index % 9 == 0 { '\n' } else { ' ' });
        index = index.wrapping_mul(31).wrapping_add(7);
    }
    text
}

/// 25 recent rows of mixed kinds and sizes plus 15 pinned rows.
fn realistic_items(root: &std::path::Path) -> Vec<ClipRecord> {
    let now = Utc::now();
    let mut items = Vec::new();
    let sizes = [80, 200, 500, 1_200, 3_000, 8_000, 20_000, 60_000];
    for index in 0..25usize {
        let mut item = ClipRecord::new_text(words(index, sizes[index % sizes.len()]), "notepad.exe");
        match index % 5 {
            1 => {
                item.kind = ClipKind::HtmlClipItem;
                item.html_data = Some(format!("<html><body>{}</body></html>",
                    "<p style=\"margin:0;color:#333\">quick paste html row</p>".repeat(3_000)));
                item.source_process_name = "msedge.exe".into();
            }
            3 if index < 20 => {
                let path = root.join(format!("image-{index}.png"));
                std::fs::write(&path, vec![0u8; 48 * 1024]).unwrap();
                item.kind = ClipKind::ImageClipItem;
                item.body_text = None;
                item.image_path = Some(path.to_string_lossy().into_owned());
                item.content_hash = format!("image-{index}");
            }
            4 if index < 10 => {
                item.kind = ClipKind::FileClipItem;
                item.body_text = None;
                item.file_paths = (0..3).map(|n| format!("C:\\Data\\report-{index}-{n}.docx")).collect();
                item.content_hash = format!("files-{index}");
            }
            _ => {}
        }
        item.create_time = now - chrono::Duration::seconds(index as i64 + 100);
        item.update_time = item.create_time;
        items.push(item);
    }
    for index in 0..15usize {
        let mut item = ClipRecord::new_text(words(index + 100, 500 + index * 170), "code.exe");
        item.is_top = true;
        item.create_time = now - chrono::Duration::days(index as i64 + 1);
        item.update_time = item.create_time;
        items.push(item);
    }
    items
}

fn open_session(host: &HostState) {
    host.qp_debug_begin();
    assert!(host.inner.lock().qp.overlay_visible, "quick paste session did not open");
}

fn session_host(rows: usize) -> (tempfile::TempDir, HostState) {
    crate::clipboard::force_headless(true);
    let root = tempfile::tempdir().unwrap();
    let host = HostState::open(AppPaths::from_data_root(root.path().to_path_buf()), 28965).unwrap();
    host.set_window_runtime_ready(true);
    for index in 0..rows {
        host.add_text(&json!({ "text": format!("quick paste row {index}") })).unwrap();
    }
    open_session(&host);
    (root, host)
}

fn shared_rows(host: &HostState) -> Option<*const Value> {
    host.inner.lock().qp.items_json.as_ref().map(|json| Arc::as_ptr(&json.rows))
}

fn pushed_states(host: &HostState) -> Vec<Value> {
    host.drain_push_events().into_iter()
        .filter(|(name, _)| name == "quickPasteState")
        .map(|(_, state)| state)
        .collect()
}

fn key(vk: u32, down: bool) -> KeyEvent {
    KeyEvent { vk, down, alt: true, ctrl: false, shift: false, win: false, now_ms: 1 }
}

fn gap_count(host: &HostState, field: &str) -> u64 {
    host.qp_debug_status()["inputGaps"][field].as_u64().unwrap()
}

#[test]
fn selection_moves_publish_rows_built_at_load() {
    let (_root, host) = session_host(3);
    let loaded = shared_rows(&host).expect("opening installs prebuilt rows");
    let before = host.qp_state_detail()["activeIndex"].as_u64().unwrap();
    pushed_states(&host);
    host.qp_move(1);
    assert_eq!(shared_rows(&host), Some(loaded), "a move must not rebuild rows");
    let states = pushed_states(&host);
    assert_eq!(states.len(), 1);
    assert_eq!(states[0]["activeIndex"].as_u64(), Some((before + 1) % 3));
    let detail = host.qp_state_detail();
    assert_eq!(detail["items"], states[0]["items"]);
    assert_eq!(detail["items"].as_array().map(Vec::len), Some(3));
}

#[test]
fn replaced_items_publish_their_own_rows() {
    let (_root, host) = session_host(3);
    let kept = host.inner.lock().qp.items[2].clone();
    host.inner.lock().qp.items = vec![kept.clone()];
    let rows = host.qp_state_detail()["items"].clone();
    assert_eq!(rows.as_array().map(Vec::len), Some(1));
    assert_eq!(rows[0]["id"].as_str(), Some(kept.id.as_str()));
    host.qp_debug_end();
    assert_eq!(host.qp_state_detail()["items"], json!([]));
}

#[test]
fn concurrent_publishes_keep_revision_order() {
    let (_root, host) = session_host(6);
    pushed_states(&host);
    let movers: Vec<_> = (0..4).map(|_| {
        let host = host.clone();
        std::thread::spawn(move || for _ in 0..40 { host.qp_move(1); })
    }).collect();
    for mover in movers { mover.join().unwrap(); }
    let revisions: Vec<i64> = pushed_states(&host).iter()
        .map(|state| state["stateRevision"].as_i64().unwrap()).collect();
    assert_eq!(revisions.len(), 160);
    assert!(revisions.windows(2).all(|pair| pair[0] < pair[1]), "out of order: {revisions:?}");
}

#[test]
fn host_inner_gap_is_counted_with_its_quick_paste_outcome() {
    let (_root, host) = session_host(2);
    let held = host.inner.lock();
    assert!(!host.try_dispatch_input_key_for_foreground(key(0xC0, true), 0));
    drop(held);
    assert_eq!(gap_count(&host, "hostInnerKeys"), 1);
    assert!(host.input_gaps.report_due(), "the first gap is reported at once");
    // The next key reconciles the gap; a headless chord is never physically held.
    host.try_dispatch_input_key_for_foreground(key(0x87, false), 0);
    assert_eq!(gap_count(&host, "quickPasteCanceled"), 1);
    assert_eq!(gap_count(&host, "quickPasteKept"), 0);
    assert!(!host.inner.lock().qp.overlay_visible);
    // The hook and chrome registries are process-wide; holding them here would
    // turn other tests' input into gaps. `input_gaps` tests cover their counts.
}

/// Keyboard worker: Alt held with ` tapped six times, then released.
fn spawn_tapping(host: &HostState, stop: &Arc<AtomicBool>) -> std::thread::JoinHandle<u64> {
    let host = host.clone();
    let stop = stop.clone();
    std::thread::spawn(move || {
        let mut publishes = 0u64;
        while !stop.load(Ordering::Acquire) {
            open_session(&host);
            publishes += 1;
            for _ in 0..6 {
                std::thread::sleep(TAP_INTERVAL);
                host.qp_move(1);
                publishes += 1;
            }
            host.qp_debug_end();
        }
        publishes
    })
}

/// Native overlay maintenance and its guards read the full state each frame.
fn spawn_polling(host: &HostState, stop: &Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    let host = host.clone();
    let stop = stop.clone();
    std::thread::spawn(move || {
        while !stop.load(Ordering::Acquire) {
            let _ = host.qp_state_detail();
            let _ = host.qp_state_detail();
            std::thread::sleep(POLL_INTERVAL);
        }
    })
}

fn spin_until(at: Instant) {
    while Instant::now() < at { std::hint::spin_loop(); }
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() { return Duration::ZERO; }
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn ms(duration: Duration) -> f64 { duration.as_secs_f64() * 1000.0 }

#[test]
#[ignore = "timing measurement; run explicitly with --ignored --nocapture"]
fn qp_lock_contention_measurement() {
    crate::clipboard::force_headless(true);
    let root = tempfile::tempdir().unwrap();
    let host = HostState::open(AppPaths::from_data_root(root.path().to_path_buf()), 28965).unwrap();
    host.set_window_runtime_ready(true);
    host.store().lock().insert_clips(&realistic_items(root.path())).unwrap();

    // Single-threaded cost of each publish path.
    open_session(&host);
    let rows = host.inner.lock().qp.items.len();
    let timed = |work: &dyn Fn()| {
        let mut runs: Vec<Duration> = (0..30).map(|_| {
            let started = Instant::now();
            work();
            started.elapsed()
        }).collect();
        runs.sort();
        runs
    };
    let moves = timed(&|| host.qp_move(1));
    let details = timed(&|| { let _ = host.qp_state_detail(); });
    host.qp_debug_end();
    let opens = timed(&|| { open_session(&host); host.qp_debug_end(); });
    println!("rows={rows}");
    for (name, runs) in [("qp_move", &moves), ("qp_state_detail", &details), ("open+end", &opens)] {
        println!("{name}: median {:.3} ms, p90 {:.3} ms, max {:.3} ms",
            ms(percentile(runs, 0.5)), ms(percentile(runs, 0.9)), ms(percentile(runs, 1.0)));
    }

    // Phase A: hook-style try_lock through the real dispatch path.
    let stop = Arc::new(AtomicBool::new(false));
    let tapping = spawn_tapping(&host, &stop);
    let polling = spawn_polling(&host, &stop);
    let (mut samples, mut gaps) = (0u64, 0u64);
    let ended = Instant::now() + PHASE;
    let mut next = Instant::now();
    while Instant::now() < ended {
        next += SAMPLE_INTERVAL;
        spin_until(next);
        // F24 release: routed like any key, never changes quick paste state.
        host.try_dispatch_input_key_for_foreground(KeyEvent {
            vk: 0x87, down: false, alt: true, ctrl: false, shift: false, win: false, now_ms: 0,
        }, 0);
        samples += 1;
        // Clear the gap ourselves; reconciling it would retire the session.
        if host.input_was_busy.swap(false, Ordering::AcqRel) { gaps += 1; }
    }
    stop.store(true, Ordering::Release);
    let publishes = tapping.join().unwrap();
    polling.join().unwrap();
    println!("try_lock: {gaps} gaps / {samples} hook samples = {:.2}% ({publishes} publishes)",
        gaps as f64 * 100.0 / samples.max(1) as f64);

    // Phase B: the remaining hold an event arriving at each sample time would
    // face, i.e. what a hook waiting up to N ms (`try_lock_for`) would cost.
    if host.inner.lock().qp.overlay_visible { host.qp_debug_end(); }
    let stop = Arc::new(AtomicBool::new(false));
    let tapping = spawn_tapping(&host, &stop);
    let polling = spawn_polling(&host, &stop);
    let mut remaining = Vec::new();
    let ended = Instant::now() + PHASE;
    let mut next = Instant::now();
    while Instant::now() < ended {
        next += SAMPLE_INTERVAL;
        spin_until(next);
        if let Some(guard) = host.inner.try_lock() {
            drop(guard);
            remaining.push(Duration::ZERO);
            continue;
        }
        // Spin rather than park, so the probe sees the release itself.
        let started = Instant::now();
        let released = loop {
            if let Some(guard) = host.inner.try_lock() { drop(guard); break Instant::now(); }
            std::hint::spin_loop();
        };
        remaining.push(released - started);
        // Sample times that fell inside this wait would have seen its rest.
        while next + SAMPLE_INTERVAL < released {
            next += SAMPLE_INTERVAL;
            remaining.push(released - next);
        }
    }
    stop.store(true, Ordering::Release);
    tapping.join().unwrap();
    polling.join().unwrap();
    let samples = remaining.len().max(1) as f64;
    let mut busy: Vec<Duration> = remaining.iter().copied().filter(|wait| !wait.is_zero()).collect();
    busy.sort();
    println!("busy at {:.2}% of {} sample times; remaining hold median {:.3} ms, p90 {:.3} ms, max {:.3} ms",
        busy.len() as f64 * 100.0 / samples, remaining.len(),
        ms(percentile(&busy, 0.5)), ms(percentile(&busy, 0.9)), ms(percentile(&busy, 1.0)));
    for limit in [0.5, 1.0, 2.0, 4.0, 8.0] {
        let bound = Duration::from_secs_f64(limit / 1000.0);
        let missed = remaining.iter().filter(|wait| **wait > bound).count();
        let waited: Duration = remaining.iter().map(|wait| (*wait).min(bound)).sum();
        println!("  try_lock_for({limit} ms): {:.2}% still missed, mean added hook latency {:.3} ms, max {limit} ms",
            missed as f64 * 100.0 / samples, ms(waited) / samples);
    }
}
