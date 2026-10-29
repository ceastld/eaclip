//! Process-local PNG cache; Shell extraction is supplied by file_icon.
use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

const TTL: Duration = Duration::from_secs(300);
const MAX_ENTRIES: usize = 512;
const MAX_BYTES: usize = 4 * 1024 * 1024;

struct Entry {
    png: Vec<u8>,
    at: Instant,
    used: u64,
}

#[derive(Default)]
struct Cache {
    entries: HashMap<String, Entry>,
    bytes: usize,
    clock: u64,
}

impl Cache {
    fn get(&mut self, key: &str, now: Instant) -> Option<Vec<u8>> {
        if self
            .entries
            .get(key)
            .is_some_and(|entry| now.saturating_duration_since(entry.at) >= TTL)
        {
            self.remove(key);
            return None;
        }
        let entry = self.entries.get_mut(key)?;
        self.clock += 1;
        entry.used = self.clock;
        Some(entry.png.clone())
    }

    fn insert(&mut self, key: String, png: Vec<u8>, now: Instant) {
        self.remove(&key);
        let bytes = &mut self.bytes;
        self.entries.retain(|_, entry| {
            let fresh = now.saturating_duration_since(entry.at) < TTL;
            if !fresh {
                *bytes -= entry.png.len();
            }
            fresh
        });
        // Return a valid oversized result to its caller, without retaining it.
        if png.len() > MAX_BYTES {
            return;
        }
        while self.entries.len() >= MAX_ENTRIES || self.bytes + png.len() > MAX_BYTES {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.used)
                .map(|(key, _)| key.clone());
            let Some(oldest) = oldest else {
                break;
            };
            self.remove(&oldest);
        }
        self.clock += 1;
        self.bytes += png.len();
        self.entries.insert(
            key,
            Entry {
                png,
                at: now,
                used: self.clock,
            },
        );
    }

    fn remove(&mut self, key: &str) {
        if let Some(old) = self.entries.remove(key) {
            self.bytes -= old.png.len();
        }
    }
}

#[derive(Default)]
pub(crate) struct State {
    cache: Mutex<Cache>,
    loading: Mutex<HashMap<String, Weak<Mutex<()>>>>,
}

impl State {
    pub(crate) fn get_or_load(
        &self,
        key: String,
        load: impl FnOnce() -> Result<Vec<u8>, String>,
    ) -> Result<Vec<u8>, String> {
        if let Some(png) = self.cache.lock().get(&key, Instant::now()) {
            return Ok(png);
        }
        #[cfg(test)]
        BEFORE_LOAD.with(|slot| {
            let hook = slot.borrow_mut().take();
            if let Some(hook) = hook {
                hook();
            }
        });
        // Only equal cache keys wait on one another. No cache/map mutex is held
        // during Shell work. Weak gates cannot accumulate completed paths.
        let gate = {
            let mut loading = self.loading.lock();
            loading.retain(|_, gate| gate.strong_count() > 0);
            match loading.get(&key).and_then(Weak::upgrade) {
                Some(gate) => gate,
                None => {
                    let gate = Arc::new(Mutex::new(()));
                    loading.insert(key.clone(), Arc::downgrade(&gate));
                    gate
                }
            }
        };
        let _loading = gate.lock();
        if let Some(png) = self.cache.lock().get(&key, Instant::now()) {
            return Ok(png);
        }
        let png = load()?;
        self.cache.lock().insert(key, png.clone(), Instant::now());
        Ok(png)
    }
}

#[cfg(test)]
thread_local! {
    // Force all workers to observe the same cold miss before extraction. This
    // makes the duplicate-load regression independent of scheduler timing.
    static BEFORE_LOAD: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn set_miss_hook(hook: impl FnOnce() + 'static) {
    BEFORE_LOAD.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Arc, Barrier,
    };

    #[test]
    fn icon_cache_bounds_memory_and_retains_recently_used_icons() {
        let now = Instant::now();
        let mut cache = Cache::default();
        for id in 0..MAX_ENTRIES {
            cache.insert(id.to_string(), vec![id as u8], now);
        }
        assert_eq!(cache.get("0", now), Some(vec![0]));
        cache.insert("new".into(), vec![99], now);
        assert!(
            cache.entries.len() <= MAX_ENTRIES,
            "icon cache grows without an entry limit"
        );
        assert_eq!(cache.get("0", now), Some(vec![0]));
        assert!(
            cache.get("1", now).is_none(),
            "oldest unused icon must be evicted"
        );

        let mut cache = Cache::default();
        for id in 0..4 {
            cache.insert(id.to_string(), vec![7; MAX_BYTES / 3], now);
        }
        assert!(
            cache.bytes <= MAX_BYTES,
            "icon cache grows without a byte limit"
        );
        assert!(cache.get("0", now).is_none());
        cache.insert("oversized".into(), vec![0; MAX_BYTES + 1], now);
        assert!(cache.bytes <= MAX_BYTES);
        assert!(cache.get("oversized", now).is_none());
    }

    #[test]
    fn icon_cache_expires_without_renewing_ttl_and_removes_dead_entries() {
        let start = Instant::now();
        let mut cache = Cache::default();
        cache.insert("read".into(), vec![1], start);
        cache.insert("unread".into(), vec![2], start);
        assert_eq!(
            cache.get("read", start + TTL - Duration::from_secs(1)),
            Some(vec![1])
        );
        assert!(cache.get("read", start + TTL).is_none());
        assert!(
            !cache.entries.contains_key("read"),
            "an expired hit must release its PNG"
        );
        cache.insert("fresh".into(), vec![3], start + TTL);
        assert_eq!(
            cache.entries.len(),
            1,
            "cold loads must release other expired icons"
        );
        assert_eq!(cache.bytes, 1);
    }

    #[test]
    fn icon_cache_parallel_cold_requests_extract_once() {
        let state = Arc::new(State::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let misses = Arc::new(Barrier::new(8));
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let state = state.clone();
                let calls = calls.clone();
                let misses = misses.clone();
                std::thread::spawn(move || {
                    BEFORE_LOAD.with(|slot| {
                        *slot.borrow_mut() = Some(Box::new(move || {
                            misses.wait();
                        }))
                    });
                    state
                        .get_or_load("same.exe".into(), || {
                            calls.fetch_add(1, Ordering::SeqCst);
                            Ok(vec![1, 2, 3])
                        })
                        .unwrap()
                })
            })
            .collect();
        for worker in workers {
            assert_eq!(worker.join().unwrap(), vec![1, 2, 3]);
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "parallel misses duplicate Shell extraction"
        );
        assert_eq!(
            state
                .get_or_load("same.exe".into(), || panic!("cached hit must not extract"))
                .unwrap(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn icon_cache_failed_load_can_retry_and_does_not_block_another_path() {
        let state = Arc::new(State::default());
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let worker_state = state.clone();
        let worker = std::thread::spawn(move || {
            worker_state.get_or_load("blocked.exe".into(), || {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                Err("Shell fixture failure".into())
            })
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let other_state = state.clone();
        let (done_tx, done_rx) = mpsc::channel();
        let other = std::thread::spawn(move || {
            let result = other_state.get_or_load("other.exe".into(), || Ok(vec![9]));
            done_tx.send(result).unwrap();
        });
        let result = done_rx.recv_timeout(Duration::from_secs(2));
        release_tx.send(()).unwrap();
        other.join().unwrap();
        assert_eq!(worker.join().unwrap().unwrap_err(), "Shell fixture failure");
        assert_eq!(
            result
                .expect("independent path blocked by cache lock")
                .unwrap(),
            vec![9]
        );
        assert_eq!(
            state
                .get_or_load("blocked.exe".into(), || Ok(vec![4]))
                .unwrap(),
            vec![4]
        );
        assert_eq!(
            state
                .get_or_load("blocked.exe".into(), || panic!(
                    "successful retry must be cached"
                ))
                .unwrap(),
            vec![4]
        );
        for id in 0..MAX_ENTRIES * 2 {
            state
                .get_or_load(format!("unique{id}.exe"), || Ok(vec![5]))
                .unwrap();
        }
        assert_eq!(
            state.loading.lock().len(),
            1,
            "completed paths must not leak load gates"
        );
        assert!(state.cache.lock().entries.len() <= MAX_ENTRIES);
    }
}
