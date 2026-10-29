use super::*;
use crate::file_history::{
    local_presence, record_paths, MissingObservation, Presence, MISSING_KEY_PREFIX,
};
use crate::store::FileHistoryImageReplacement;

/// These unique new copies have never been referenced by a committed record.
/// Existing/source images never enter this guard. Release committed paths.
#[derive(Default)]
struct UncommittedImages(Vec<PathBuf>);
impl Drop for UncommittedImages {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl HostState {
    /// Memory-only report of intersecting rows; safe on the window thread.
    pub fn report_visible_file_history(&self, ids: &[String]) {
        let queue = self.inner.lock().file_history.clone();
        let mut queue = queue.lock();
        let now = Instant::now();
        let pending = queue.next_delay(now).is_some();
        queue.report(ids, now);
        if pending != queue.next_delay(now).is_some() {
            self.maintenance_wakeup.notify_one();
        }
    }

    pub(super) fn poll_visible_file_history(&self) {
        let queue = self.inner.lock().file_history.clone();
        if queue.lock().next_delay(Instant::now()) != Some(Duration::ZERO) { return; }
        self.poll_visible_file_history_with(
            Instant::now(),
            Utc::now().timestamp_millis(),
            local_presence,
        );
    }

    fn poll_visible_file_history_with(
        &self,
        now: Instant,
        wall_ms: i64,
        mut probe: impl FnMut(&Path) -> Presence,
    ) {
        let (queue, store, root) = {
            let inner = self.inner.lock();
            if !inner.window_visible || inner.is_collapsed || inner.pending_clear.is_some() {
                return;
            }
            (
                inner.file_history.clone(),
                inner.store.clone(),
                inner.paths.data_root.clone(),
            )
        };
        let ids = queue.lock().claim(now);
        if ids.is_empty() {
            return;
        }
        let mut uncommitted = UncommittedImages::default();
        let result = (|| -> Result<_, StoreError> {
            let candidates = store.lock().file_history_by_ids(&ids)?;
            let mut observed = HashMap::new();
            let mut replacements = Vec::new();
            let mut removals = Vec::new();
            let mut snapshot_attempted = false;
            for item in candidates {
                if !queue.lock().contains(&item.id) {
                    continue;
                }
                let key = format!("{MISSING_KEY_PREFIX}{}", item.id);
                let snapshot_candidate = crate::volatile_images::is_volatile_image_record(&item);
                if snapshot_candidate && snapshot_attempted {
                    queue.lock().defer(&item.id);
                    continue;
                }
                if item.is_top && !snapshot_candidate {
                    store.lock().kv_delete(&key)?;
                    continue;
                }
                let paths = record_paths(&item);
                // Bound work even for a record with a huge multi-file selection.
                if paths.is_empty() || paths.len() > 64 {
                    continue;
                }
                let mut all_missing = true;
                let mut first_present = false;
                let mut budget_exhausted = false;
                for (index, path) in paths.iter().enumerate() {
                    let presence = if let Some(presence) = observed.get(path.as_str()) {
                        *presence
                    } else if observed.len() < 64 {
                        let presence = probe(Path::new(path));
                        observed.insert(path.clone(), presence);
                        presence
                    } else {
                        budget_exhausted = true;
                        Presence::Unknown
                    };
                    if index == 0 {
                        first_present = presence == Presence::Present;
                    }
                    if presence != Presence::Missing {
                        all_missing = false;
                        // A single available/uncertain member is enough to
                        // retain a multi-file record; do not probe the rest.
                        break;
                    }
                }
                if budget_exhausted {
                    queue.lock().defer(&item.id);
                    continue;
                }
                // At most one bounded image copy/decode per maintenance tick.
                if snapshot_candidate && first_present && !snapshot_attempted {
                    snapshot_attempted = true;
                    match crate::volatile_images::snapshot_volatile_image(&item, &root) {
                        Ok(Some(image)) => {
                            uncommitted.0.push(PathBuf::from(image.image_path.as_ref().unwrap()));
                            replacements.push(FileHistoryImageReplacement {
                                snapshot: item, image_path: image.image_path.unwrap(),
                                content_hash: image.content_hash, perceptual_hash: image.perceptual_hash,
                            });
                            continue;
                        }
                        Ok(None) => {}
                        Err(reason) => tracing::debug!(reason, "Visible temporary image could not be preserved; keeping original record"),
                    }
                }
                if item.is_top || !all_missing {
                    // Restoration, unknown storage, or partial loss resets grace.
                    store.lock().kv_delete(&key)?;
                    continue;
                }
                let previous = store.lock().kv_get(&key)?;
                let previous = previous
                    .as_deref()
                    .and_then(|raw| serde_json::from_str::<MissingObservation>(raw).ok());
                if previous
                    .as_ref()
                    .is_some_and(|entry| entry.confirmed(&item, wall_ms))
                {
                    removals.push(item);
                } else if previous
                    .as_ref()
                    .is_none_or(|entry| !entry.same_payload(&item) || entry.clock_reversed(wall_ms))
                {
                    store.lock().kv_set(
                        &key,
                        &serde_json::to_string(&MissingObservation::first(&item, wall_ms))?,
                    )?;
                }
            }
            // Recheck final deletion candidates without any business lock.
            let mut confirmed = Vec::new();
            for item in removals {
                if !queue.lock().contains(&item.id) {
                    continue;
                }
                if record_paths(&item)
                    .iter()
                    .all(|path| probe(Path::new(path)) == Presence::Missing)
                {
                    confirmed.push(item);
                } else {
                    store
                        .lock()
                        .kv_delete(&format!("{MISSING_KEY_PREFIX}{}", item.id))?;
                }
            }
            let removals = confirmed;
            if replacements.is_empty() && removals.is_empty() {
                return Ok(None);
            }
            let operation = self.history_operation.lock();
            {
                let inner = self.inner.lock();
                if !inner.window_visible || inner.is_collapsed || inner.pending_clear.is_some() {
                    return Ok(None);
                }
            }
            let delta = store
                .lock()
                .apply_file_history_maintenance(&replacements, &removals)?;
            drop(operation);
            uncommitted.0.retain(|path| {
                !delta.changed.iter().any(|item| {
                    item.image_path
                        .as_deref()
                        .is_some_and(|saved| Path::new(saved) == path)
                })
            });
            Ok(Some(delta))
        })();
        match result {
            Ok(Some(delta)) if !delta.changed.is_empty() || !delta.removed.is_empty() => {
                tracing::info!(
                    preserved = delta.changed.len(),
                    removed = delta.removed.len(),
                    "Visible file history maintenance committed"
                );
                self.emit_clip_changed(&delta.changed, &delta.removed);
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(%error, "Visible file history maintenance failed; records retained")
            }
        }
        drop(uncommitted);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_history::MISSING_GRACE_MS;
    fn setup() -> (tempfile::TempDir, HostState, ClipRecord) {
        let dir = tempfile::tempdir().unwrap();
        let host =
            HostState::open(AppPaths::from_data_root(dir.path().join("data")), 28965).unwrap();
        let mut item = ClipRecord::new_text("", "test");
        item.kind = ClipKind::FileClipItem;
        item.file_paths = vec![dir
            .path()
            .join("missing.txt")
            .to_string_lossy()
            .into_owned()];
        host.store().lock().insert_clip(&item, true).unwrap();
        host.inner.lock().window_visible = true;
        (dir, host, item)
    }
    fn enqueue(host: &HostState, item: &ClipRecord, now: Instant) {
        host.inner
            .lock()
            .file_history
            .lock()
            .report(&[item.id.clone()], now);
    }
    #[test]
    fn invisible_and_unrequested_records_never_probe_or_create_observations() {
        let (_dir, host, item) = setup();
        let now = Instant::now();
        host.poll_visible_file_history_with(now, 1000, |_| panic!("no viewport request"));
        enqueue(&host, &item, now);
        host.inner.lock().window_visible = false;
        host.poll_visible_file_history_with(now, 1000, |_| panic!("hidden window"));
        assert!(host
            .store()
            .lock()
            .kv_get(&format!("{MISSING_KEY_PREFIX}{}", item.id))
            .unwrap()
            .is_none());
    }
    #[test]
    fn confirmed_missing_is_removed_after_24h_and_emits_delta() {
        let (_dir, host, item) = setup();
        let now = Instant::now();
        enqueue(&host, &item, now);
        host.poll_visible_file_history_with(now, 1000, |_| Presence::Missing);
        assert!(host.store().lock().get_clip(&item.id).is_ok());
        *host.inner.lock().file_history.lock() = Default::default();
        enqueue(&host, &item, now);
        host.poll_visible_file_history_with(now, 1000 + MISSING_GRACE_MS, |_| Presence::Missing);
        assert!(matches!(
            host.store().lock().get_clip(&item.id),
            Err(StoreError::NotFound(_))
        ));
        assert!(host
            .drain_push_events()
            .iter()
            .any(|(name, _)| name == "clipChanged"));
    }
    #[test]
    fn partial_loss_and_unknown_interrupt_grace_and_pins_are_protected() {
        let (_dir, host, mut item) = setup();
        item.file_paths.push("second-file".into());
        host.store().lock().save_clip(&item).unwrap();
        let key = format!("{MISSING_KEY_PREFIX}{}", item.id);
        host.store()
            .lock()
            .kv_set(
                &key,
                &serde_json::to_string(&MissingObservation::first(&item, 0)).unwrap(),
            )
            .unwrap();
        let now = Instant::now();
        enqueue(&host, &item, now);
        host.poll_visible_file_history_with(now, MISSING_GRACE_MS, |path| {
            if path == Path::new("second-file") {
                Presence::Unknown
            } else {
                Presence::Missing
            }
        });
        assert!(host.store().lock().get_clip(&item.id).is_ok());
        assert!(host.store().lock().kv_get(&key).unwrap().is_none());
        item.is_top = true;
        host.store().lock().save_clip(&item).unwrap();
        *host.inner.lock().file_history.lock() = Default::default();
        enqueue(&host, &item, now);
        host.poll_visible_file_history_with(now, 2 * MISSING_GRACE_MS, |_| {
            panic!("pinned ordinary file needs no probe")
        });
        assert!(host.store().lock().get_clip(&item.id).unwrap().is_top);
    }
    #[test]
    fn filesystem_work_releases_business_locks_and_stale_edit_survives() {
        let (_dir, host, item) = setup();
        let key = format!("{MISSING_KEY_PREFIX}{}", item.id);
        host.store()
            .lock()
            .kv_set(
                &key,
                &serde_json::to_string(&MissingObservation::first(&item, 0)).unwrap(),
            )
            .unwrap();
        let now = Instant::now();
        enqueue(&host, &item, now);
        let mut changed = false;
        host.poll_visible_file_history_with(now, MISSING_GRACE_MS, |_| {
            assert!(host.history_operation.try_lock().is_some());
            let store = host.store();
            let db = store.try_lock().expect("probe must not hold DB lock");
            if !changed {
                let mut edited = db.get_clip(&item.id).unwrap();
                edited.file_paths = vec!["replacement.txt".into()];
                db.save_clip(&edited).unwrap();
                changed = true;
            }
            Presence::Missing
        });
        assert_eq!(
            host.store().lock().get_clip(&item.id).unwrap().file_paths,
            vec!["replacement.txt"]
        );
    }

    #[test]
    fn visible_temporary_image_is_preserved_and_uncommitted_copy_is_reclaimed() {
        for conflict in [false, true] {
            let (dir, host, mut item) = setup();
            let cache = dir.path().join("PixPin/Temp");
            std::fs::create_dir_all(&cache).unwrap();
            let source = cache.join("shot.png");
            image::RgbaImage::from_pixel(2, 2, image::Rgba([8, 16, 32, 255]))
                .save(&source)
                .unwrap();
            item.file_paths = vec![source.to_string_lossy().into_owned()];
            host.store().lock().save_clip(&item).unwrap();
            let item = host.store().lock().get_clip(&item.id).unwrap();
            let now = Instant::now();
            enqueue(&host, &item, now);
            host.poll_visible_file_history_with(now, 1000, |_| {
                if conflict {
                    let mut edited = host.store().lock().get_clip(&item.id).unwrap();
                    edited.title = Some("edited during check".into());
                    host.store().lock().save_clip(&edited).unwrap();
                }
                Presence::Present
            });
            let stored = host.store().lock().get_clip(&item.id).unwrap();
            let temp = dir.path().join("data/Image/Temp");
            let copies = std::fs::read_dir(temp).unwrap().count();
            if conflict {
                assert_eq!(stored.kind, ClipKind::FileClipItem);
                assert_eq!(copies, 0, "only this attempt's uncommitted copy is removed");
                assert!(source.is_file());
            } else {
                assert_eq!(stored.kind, ClipKind::ImageClipItem);
                assert_eq!(copies, 1);
                std::fs::remove_file(source).unwrap();
                assert!(Path::new(stored.image_path.as_deref().unwrap()).is_file());
            }
        }
    }
}
