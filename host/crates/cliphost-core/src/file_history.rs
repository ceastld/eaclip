//! Demand-driven file validation: only IDs reported by an actual viewport are
//! queued. No database enumeration, directory walk or periodic history scan.
use crate::model::{ClipKind, ClipRecord};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};

pub(crate) const MISSING_GRACE_MS: i64 = 24 * 60 * 60 * 1000;
pub(crate) const MISSING_KEY_PREFIX: &str = "file-history-missing-v1:";
const RECHECK: Duration = Duration::from_secs(5 * 60);
const MAX_VISIBLE: usize = 32;
pub(crate) const MAX_BATCH: usize = 8;
const MAX_CACHE: usize = 256;

#[derive(Default)]
pub(crate) struct VisibleFileHistory {
    visible: HashSet<String>,
    pending: VecDeque<String>,
    checked: HashMap<String, Instant>,
    last_claim: Option<Instant>,
}

impl VisibleFileHistory {
    pub(crate) fn contains(&self, id: &str) -> bool {
        self.visible.contains(id)
    }

    /// Keep the existing bounded half-second batch cadence only while actual
    /// viewport work remains; an empty queue has no maintenance deadline.
    pub(crate) fn next_delay(&self, now: Instant) -> Option<Duration> {
        (!self.pending.is_empty()).then(|| self.last_claim.map_or(Duration::ZERO, |last| {
            (last + Duration::from_millis(500)).saturating_duration_since(now)
        }))
    }

    pub(crate) fn defer(&mut self, id: &str) {
        self.checked.remove(id);
        if self.visible.contains(id) && !self.pending.iter().any(|pending| pending == id) {
            self.pending.push_back(id.to_string());
        }
    }
    /// Memory-only UI notification; never opens the database or filesystem.
    pub(crate) fn report(&mut self, ids: &[String], now: Instant) {
        self.visible = ids.iter().take(MAX_VISIBLE).cloned().collect();
        self.pending.retain(|id| self.visible.contains(id));
        for id in ids.iter().take(MAX_VISIBLE) {
            if !self
                .checked
                .get(id)
                .is_some_and(|at| now.saturating_duration_since(*at) < RECHECK)
                && !self.pending.contains(id)
            {
                self.pending.push_back(id.clone());
            }
        }
    }

    pub(crate) fn claim(&mut self, now: Instant) -> Vec<String> {
        let mut ids = Vec::new();
        while ids.len() < MAX_BATCH {
            let Some(id) = self.pending.pop_front() else {
                break;
            };
            if !self.visible.contains(&id) {
                continue;
            }
            if self.checked.len() >= MAX_CACHE && !self.checked.contains_key(&id) {
                if let Some(oldest) = self
                    .checked
                    .iter()
                    .min_by_key(|(_, at)| **at)
                    .map(|(id, _)| id.clone())
                {
                    self.checked.remove(&oldest);
                }
            }
            self.checked.insert(id.clone(), now);
            ids.push(id);
        }
        if !ids.is_empty() { self.last_claim = Some(now); }
        ids
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Presence {
    Present,
    Missing,
    Unknown,
}

/// Metadata only, without following junctions or opening cloud placeholders.
/// Only an online fixed local volume may produce a destructive Missing result.
pub(crate) fn local_presence(path: &Path) -> Presence {
    if !path.is_absolute() || path.as_os_str().len() > 32760 {
        return Presence::Unknown;
    }
    for part in path.components() {
        match part {
            Component::ParentDir | Component::CurDir => return Presence::Unknown,
            Component::Normal(name) => {
                let name = name.to_string_lossy().to_ascii_lowercase();
                if name.starts_with("onedrive")
                    || name == "dropbox"
                    || name.starts_with("icloud")
                    || name.contains(':')
                    || name.ends_with(['.', ' '])
                {
                    return Presence::Unknown;
                }
            }
            _ => {}
        }
    }
    #[cfg(windows)]
    {
        use std::{os::windows::ffi::OsStrExt, path::Prefix};
        let Some(Component::Prefix(prefix)) = path.components().next() else {
            return Presence::Unknown;
        };
        let drive = match prefix.kind() {
            Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => drive,
            _ => return Presence::Unknown,
        };
        #[link(name = "kernel32")]
        extern "system" {
            fn GetDriveTypeW(root: *const u16) -> u32;
        }
        let root: Vec<u16> = std::ffi::OsStr::new(&format!("{}:\\", drive as char))
            .encode_wide()
            .chain(Some(0))
            .collect();
        if unsafe { GetDriveTypeW(root.as_ptr()) } != 3 {
            return Presence::Unknown;
        }
    }
    let mut current = PathBuf::new();
    for part in path.components() {
        current.push(part.as_os_str());
        if matches!(part, Component::Prefix(_)) {
            continue;
        }
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Presence::Unknown;
                }
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    if metadata.file_attributes() & (0x400 | 0x1000 | 0x40000 | 0x400000) != 0 {
                        return Presence::Unknown;
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return if current.parent().is_some() {
                    Presence::Missing
                } else {
                    Presence::Unknown
                };
            }
            Err(_) => return Presence::Unknown,
        }
    }
    Presence::Present
}

pub(crate) fn record_paths(item: &ClipRecord) -> &[String] {
    match item.kind {
        ClipKind::FileClipItem => &item.file_paths,
        ClipKind::ImageClipItem => item.image_path.as_slice(),
        _ => &[],
    }
}

/// The ledger is one small KV value per observed missing record. Neither
/// visible checks nor restarts load or serialize a history-sized ledger.
#[derive(Serialize, Deserialize)]
pub(crate) struct MissingObservation {
    fingerprint: String,
    first_missing_ms: i64,
}

impl MissingObservation {
    pub(crate) fn fingerprint(item: &ClipRecord) -> String {
        crate::model::sha256_hex(
            serde_json::to_string(&(item.kind, &item.file_paths, &item.image_path))
                .unwrap()
                .as_bytes(),
        )
    }
    pub(crate) fn first(item: &ClipRecord, now_ms: i64) -> Self {
        Self {
            fingerprint: Self::fingerprint(item),
            first_missing_ms: now_ms,
        }
    }
    pub(crate) fn same_payload(&self, item: &ClipRecord) -> bool {
        self.fingerprint == Self::fingerprint(item)
    }
    pub(crate) fn confirmed(&self, item: &ClipRecord, now_ms: i64) -> bool {
        self.same_payload(item) && now_ms.saturating_sub(self.first_missing_ms) >= MISSING_GRACE_MS
    }
    pub(crate) fn clock_reversed(&self, now_ms: i64) -> bool {
        now_ms < self.first_missing_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn viewport_deadlines_exist_only_for_pending_batches() {
        let now = Instant::now();
        let mut state = VisibleFileHistory::default();
        assert_eq!(state.next_delay(now), None);
        let ids = (0..MAX_BATCH + 1).map(|i| i.to_string()).collect::<Vec<_>>();
        state.report(&ids, now);
        assert_eq!(state.next_delay(now), Some(Duration::ZERO));
        assert_eq!(state.claim(now).len(), MAX_BATCH);
        assert_eq!(state.next_delay(now), Some(Duration::from_millis(500)));
        let due = now + Duration::from_millis(500);
        assert_eq!(state.claim(due).len(), 1);
        assert_eq!(state.next_delay(due), None);
        state.defer(&ids[0]);
        assert_eq!(state.next_delay(due), Some(Duration::from_millis(500)));
        state.report(&[], due);
        assert_eq!(state.next_delay(due), None);
    }

    #[test]
    fn viewport_queue_is_bounded_coalesced_and_retires_hidden_rows() {
        let now = Instant::now();
        let ids: Vec<_> = (0..10000).map(|i| i.to_string()).collect();
        let mut state = VisibleFileHistory::default();
        assert!(state.claim(now).is_empty());
        state.report(&ids, now);
        state.report(&ids, now);
        assert_eq!(state.pending.len(), MAX_VISIBLE);
        assert_eq!(state.claim(now).len(), MAX_BATCH);
        state.report(&[], now);
        assert!(state.claim(now).is_empty());
        state.report(&ids[..1], now);
        assert!(state.claim(now).is_empty());
        state.report(&ids[..1], now + RECHECK);
        assert_eq!(state.claim(now + RECHECK), vec!["0"]);
    }
    #[test]
    fn missing_grace_survives_restart_but_not_a_changed_path() {
        let mut item = ClipRecord::new_text("", "test");
        item.kind = ClipKind::FileClipItem;
        item.file_paths = vec!["C:\\old.png".into()];
        let first = MissingObservation::first(&item, 1_000);
        let restored: MissingObservation =
            serde_json::from_str(&serde_json::to_string(&first).unwrap()).unwrap();
        assert!(!restored.confirmed(&item, 1_000 + MISSING_GRACE_MS - 1));
        assert!(restored.confirmed(&item, 1_000 + MISSING_GRACE_MS));
        assert!(restored.clock_reversed(999));
        item.file_paths[0] = "C:\\new.png".into();
        assert!(!restored.confirmed(&item, 1_000 + MISSING_GRACE_MS));
    }
    #[test]
    fn metadata_probe_preserves_unknown_paths_and_detects_local_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            local_presence(&dir.path().join("missing.png")),
            Presence::Missing
        );
        assert_eq!(local_presence(dir.path()), Presence::Present);
        assert_eq!(local_presence(Path::new("relative.png")), Presence::Unknown);
        assert_eq!(
            local_presence(&dir.path().join("OneDrive/absent.png")),
            Presence::Unknown
        );
        #[cfg(windows)]
        assert_eq!(
            local_presence(Path::new(r"\\server\share\missing.png")),
            Presence::Unknown
        );
    }
}
