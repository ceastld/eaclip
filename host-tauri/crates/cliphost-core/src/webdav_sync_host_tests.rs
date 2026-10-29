//! Host glue checks use an explicit repository fixture, no listeners, no
//! clipboard capture, and no production-root discovery.
use super::*;

fn host_fixture() -> (tempfile::TempDir, HostState) {
    let repository = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .unwrap();
    let base = repository.join(".tmp").join("webdav-host-tests");
    std::fs::create_dir_all(&base).unwrap();
    let fixture = tempfile::Builder::new()
        .prefix("isolated-")
        .tempdir_in(&base)
        .unwrap();
    let paths = AppPaths::from_data_root(fixture.path().join("data"));
    let host = open_fixture_host(paths);
    (fixture, host)
}

fn open_fixture_host(paths: AppPaths) -> HostState {
    // The QA validator inspects every existing root ancestor before the host
    // opens storage. The dedicated empty fixture leaf must already exist.
    std::fs::create_dir_all(&paths.data_root).unwrap();
    let host = if crate::clipboard::is_headless() {
        HostState::open_isolated_visual(paths, 29065)
    } else {
        HostState::open_keyboard_input_qa(paths, 29065)
    }
    .unwrap();
    host
}

#[derive(Default)]
struct MemoryRemote {
    state: std::sync::Mutex<MemoryRemoteState>,
}

#[derive(Default)]
struct MemoryRemoteState {
    files: BTreeMap<String, cliphost_webdav::RemoteFile>,
    next: usize,
}

impl cliphost_webdav::RemoteStore for MemoryRemote {
    fn get(
        &self,
        path: &str,
    ) -> Result<Option<cliphost_webdav::RemoteFile>, cliphost_webdav::SyncError> {
        Ok(self.state.lock().unwrap().files.get(path).cloned())
    }

    fn create(&self, path: &str, bytes: &[u8]) -> Result<bool, cliphost_webdav::SyncError> {
        let mut state = self.state.lock().unwrap();
        if state.files.contains_key(path) {
            return Ok(false);
        }
        state.next += 1;
        let etag = format!("\"v{}\"", state.next);
        state.files.insert(
            path.into(),
            cliphost_webdav::RemoteFile {
                bytes: bytes.to_vec(),
                etag,
            },
        );
        Ok(true)
    }

    fn replace(
        &self,
        path: &str,
        etag: &str,
        bytes: &[u8],
    ) -> Result<bool, cliphost_webdav::SyncError> {
        let mut state = self.state.lock().unwrap();
        if state.files.get(path).map(|file| file.etag.as_str()) != Some(etag) {
            return Ok(false);
        }
        state.next += 1;
        let etag = format!("\"v{}\"", state.next);
        state.files.insert(
            path.into(),
            cliphost_webdav::RemoteFile {
                bytes: bytes.to_vec(),
                etag,
            },
        );
        Ok(true)
    }
}

fn dev_config(key: &RecoveryKey, sync_settings: bool) -> DevConfig {
    DevConfig {
        profile_id: uuid::Uuid::new_v4().simple().to_string(),
        endpoint: "https://webdav.example.invalid/fixture-only/".into(),
        username: "synthetic-user".into(),
        password: "synthetic-password".into(),
        recovery_key: key.to_hex(),
        enabled: false,
        sync_settings,
        interval_seconds: 120,
    }
}

#[test]
fn webdav_host_settings_snapshot_contains_only_portable_whitelisted_values() {
    let (_fixture, host) = host_fixture();
    {
        let mut inner = host.inner.lock();
        inner.extras.global.window_background_image_path =
            r"C:\synthetic-private-user\private-background.png".into();
        inner.extras.global.launch_at_startup = true;
        inner.extras.global.run_as_administrator = true;
    }
    assert!(host.webdav_settings_snapshot(false).unwrap().is_empty());
    let snapshot = host.webdav_settings_snapshot(true).unwrap();
    let mut actual: Vec<_> = snapshot.keys().cloned().collect();
    let mut expected = settings_scope();
    actual.sort();
    expected.sort();
    assert_eq!(actual, expected);
    let encoded = serde_json::to_string(&snapshot).unwrap();
    assert!(!encoded.contains("synthetic-private-user"));
    assert!(!encoded.contains("LaunchAtStartup"));
    assert!(!encoded.contains("RunAsAdministrator"));
    assert!(!encoded.contains("password"));
    assert!(!encoded.contains("recoveryKey"));
}

#[test]
fn webdav_host_settings_apply_preserves_explicit_dark_mode_when_colors_change() {
    let (_fixture, host) = host_fixture();
    let before = host.webdav_settings_snapshot(true).unwrap();
    let mut after = before.clone();
    after.insert("setting/global/UiThemeMode".into(), json!("Dark"));
    after.insert(
        "setting/global/WindowBackgroundColor".into(),
        json!("#FF102030"),
    );
    after.insert(
        "setting/global/FontForegroundColor".into(),
        json!("#FFEEDDCC"),
    );
    let machine_settings = {
        let inner = host.inner.lock();
        (
            inner.extras.global.launch_at_startup,
            inner.extras.global.run_as_administrator,
            inner.extras.global.window_background_image_path.clone(),
        )
    };
    host.webdav_apply_settings(&before, &after).unwrap();
    assert_eq!(host.webdav_settings_snapshot(true).unwrap(), after);
    let inner = host.inner.lock();
    assert_eq!(inner.extras.global.ui_theme_mode, "Dark");
    assert_eq!(inner.extras.global.window_background_color, "#FF102030");
    assert_eq!(inner.extras.global.font_foreground_color, "#FFEEDDCC");
    assert_eq!(
        (
            inner.extras.global.launch_at_startup,
            inner.extras.global.run_as_administrator,
            inner.extras.global.window_background_image_path.clone(),
        ),
        machine_settings
    );
    let stored: Value = serde_json::from_slice(
        &std::fs::read(inner.paths.configs.join("GlobalConfig.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(stored["UiThemeMode"], "Dark");
    assert_eq!(
        stored
            .get("WindowBackground")
            .or_else(|| stored.get("WindowBackgroundColor")),
        Some(&json!("#FF102030"))
    );
}

#[test]
fn webdav_host_settings_apply_rejects_a_third_local_value_without_writing() {
    let (_fixture, host) = host_fixture();
    let before = host.webdav_settings_snapshot(true).unwrap();
    let mut after = before.clone();
    after.insert("setting/global/UiThemeMode".into(), json!("Dark"));
    let global_path = {
        let mut inner = host.inner.lock();
        // Simulates a committed local edit after the network snapshot.
        inner.extras.global.ui_theme_mode = "Light".into();
        inner.paths.configs.join("GlobalConfig.json")
    };
    let file_before = std::fs::read(&global_path).ok();
    assert!(host.webdav_apply_settings(&before, &after).is_err());
    assert_eq!(host.inner.lock().extras.global.ui_theme_mode, "Light");
    assert_eq!(std::fs::read(global_path).ok(), file_before);
}

#[test]
fn webdav_host_settings_apply_accepts_lowercase_theme_values_written_by_the_ui() {
    let (_fixture, host) = host_fixture();
    let before = host.webdav_settings_snapshot(true).unwrap();
    let mut after = before.clone();
    after.insert("setting/global/UiThemeMode".into(), json!("dark"));
    after.insert(
        "setting/global/WindowBackgroundColor".into(),
        json!("#FF123456"),
    );
    host.webdav_apply_settings(&before, &after).unwrap();
    assert_eq!(host.webdav_settings_snapshot(true).unwrap(), after);
}

#[test]
fn webdav_host_settings_apply_rejects_machine_integration_fields() {
    let (_fixture, host) = host_fixture();
    let before = host.webdav_settings_snapshot(true).unwrap();
    let initial_startup = host.inner.lock().extras.global.launch_at_startup;
    let mut after = before.clone();
    after.insert(
        "setting/global/LaunchAtStartup".into(),
        json!(!initial_startup),
    );
    assert!(host.webdav_apply_settings(&before, &after).is_err());
    assert_eq!(
        host.inner.lock().extras.global.launch_at_startup,
        initial_startup
    );
    assert_eq!(host.webdav_settings_snapshot(true).unwrap(), before);
}

#[cfg(windows)]
#[test]
fn webdav_host_configuration_and_status_do_not_disclose_credentials() {
    let (_fixture, host) = host_fixture();
    let endpoint = "https://webdav.example.invalid/synthetic-private-vault/";
    let username = "synthetic-webdav-user";
    let password = "synthetic-webdav-password";
    let key = "a1".repeat(32);
    let configured = host
        .webdav_dev_command(&json!({
            "command":"configure", "endpoint":endpoint, "username":username,
            "password":password, "recoveryKey":key,
        }))
        .unwrap();
    assert_eq!(configured["configured"], true);
    assert_eq!(configured["enabled"], false);
    let status = host.webdav_dev_status();
    assert_eq!(status["configured"], true);
    assert_eq!(status["enabled"], false);
    let paths = host.inner.lock().paths.clone();
    let protected = std::fs::read_to_string(config_path(&paths)).unwrap();
    for secret in [endpoint, username, password, key.as_str()] {
        assert!(!serde_json::to_string(&configured).unwrap().contains(secret));
        assert!(!serde_json::to_string(&status).unwrap().contains(secret));
        assert!(!protected.contains(secret));
    }
    // Only the explicit protected-file-export command returns recovery material.
    let exported = host
        .webdav_dev_command(&json!({"command":"recoveryKey"}))
        .unwrap();
    assert_eq!(exported["recoveryKey"], key);
    let exported_text = serde_json::to_string(&exported).unwrap();
    assert!(!exported_text.contains(endpoint));
    assert!(!exported_text.contains(username));
    assert!(!exported_text.contains(password));
    let disconnected = host
        .webdav_dev_command(&json!({"command":"disconnect"}))
        .unwrap();
    assert_eq!(disconnected["configured"], false);
    assert_eq!(disconnected["localDataRetained"], true);
    assert_eq!(host.webdav_dev_status()["configured"], false);
}

#[test]
fn webdav_host_empty_creation_then_fresh_join_applies_nonempty_favorites_and_roots() {
    let (_source_fixture, source) = host_fixture();
    let source_paths = source.inner.lock().paths.clone();
    let key = RecoveryKey::generate().unwrap();
    let source_config = dev_config(&key, false);
    let remote = MemoryRemote::default();
    let mut source_engine = engine(&source_paths, &source_config).unwrap();
    source_engine.create(&remote).unwrap();

    // A newly created empty collection need not invent a roots entry merely to
    // prove it is empty; the host must nevertheless complete its local apply.
    let empty = source
        .webdav_sync_locked(&source_paths, &source_config, &remote, &mut source_engine)
        .unwrap();
    assert_eq!(empty["ok"], true);
    assert_eq!(empty["report"]["entries"], 0);
    assert!(source_engine.has_acknowledged_baseline());
    assert!(!source_engine.has_pending_apply());

    let mut favorite = crate::model::FavorRecord::from_clip(
        &crate::model::ClipRecord::new_text("完整的收藏正文", "webdav-host-fixture"),
        None,
    );
    let mut folder = crate::model::FavorRecord::from_clip(
        &crate::model::ClipRecord::new_text("同步目录", "webdav-host-fixture"),
        None,
    );
    folder.is_folder = true;
    folder.kind = crate::model::ClipKind::FavoriteClipItem;
    folder.children = vec![favorite.id.clone()];
    favorite.parent_id = Some(folder.id.clone());
    source
        .store()
        .lock()
        .replace_favors(&[folder.clone(), favorite.clone()])
        .unwrap();
    crate::config_file::save(
        &source_paths.configs.join("FavorConfig.json"),
        &json!({"RootChildren":[folder.id.clone()]}),
    )
    .unwrap();
    let sent = source
        .webdav_sync_locked(&source_paths, &source_config, &remote, &mut source_engine)
        .unwrap();
    assert_eq!(sent["ok"], true);

    let (_target_fixture, target) = host_fixture();
    let target_paths = target.inner.lock().paths.clone();
    let target_config = dev_config(&key, false);
    let mut target_engine = engine(&target_paths, &target_config).unwrap();
    target_engine.join(&remote).unwrap();
    assert!(!target_engine.has_acknowledged_baseline());
    let joined = target
        .webdav_sync_locked(&target_paths, &target_config, &remote, &mut target_engine)
        .unwrap();
    assert_eq!(joined["ok"], true);
    assert_eq!(joined["report"]["conflicts"], 0);
    let received = target.store().lock().get_favor(&favorite.id).unwrap();
    assert_eq!(received.body_text.as_deref(), Some("完整的收藏正文"));
    assert_eq!(received.parent_id.as_deref(), Some(folder.id.as_str()));
    assert_eq!(
        target
            .store()
            .lock()
            .get_favor(&folder.id)
            .unwrap()
            .children,
        vec![favorite.id]
    );
    let bundle =
        adapter::export_favorites(&target.store().lock(), &target_paths, &Limits::default())
            .unwrap();
    // RootChildren is the ordering of root FOLDERS; leaf ownership remains on
    // the record and its ordered occurrence is in the folder's Children.
    assert_eq!(bundle.roots, vec![folder.id]);
    assert!(!target_engine.has_pending_apply());
    let again = target
        .webdav_sync_locked(&target_paths, &target_config, &remote, &mut target_engine)
        .unwrap();
    assert_eq!(again["ok"], true);
    assert_eq!(again["report"]["published"], false);
}

#[test]
fn webdav_host_committed_favorite_receipt_rebases_new_settings_after_restart() {
    let (_fixture, host) = host_fixture();
    let paths = host.inner.lock().paths.clone();
    let key = RecoveryKey::generate().unwrap();
    let config = dev_config(&key, true);
    let remote = MemoryRemote::default();
    let mut local_engine = engine(&paths, &config).unwrap();
    local_engine.create(&remote).unwrap();
    assert_eq!(
        host.webdav_sync_locked(&paths, &config, &remote, &mut local_engine)
            .unwrap()["ok"],
        true
    );
    let before = host.webdav_settings_snapshot(true).unwrap();

    // A second replica changes the shared theme while this host is offline.
    let mut peer = Engine::open(&paths.data_root.join("peer-engine-fixture"), key).unwrap();
    peer.join(&remote).unwrap();
    let initial = peer.prepare(&remote, &Snapshot::default()).unwrap();
    peer.acknowledge(&initial.token).unwrap();
    let mut remote_edit = initial.snapshot;
    remote_edit
        .entries
        .insert("setting/global/UiThemeMode".into(), json!("Dark"));
    let remote_change = peer.prepare(&remote, &remote_edit).unwrap();
    peer.acknowledge(&remote_change.token).unwrap();

    // Reproduce the crash boundary after favorites and their receipt commit,
    // but before the separately recoverable appearance files are applied.
    let bundle =
        adapter::export_favorites(&host.store().lock(), &paths, &Limits::default()).unwrap();
    let mut input = Snapshot {
        entries: bundle.entries.clone(),
        blobs: bundle.blobs.clone(),
    };
    input.entries.insert("roots".into(), json!(bundle.roots));
    input.entries.extend(before.clone());
    let mut scopes = vec!["favor/".into(), "roots".into()];
    scopes.extend(settings_scope());
    let pending = local_engine
        .prepare_scoped(&remote, &input, &scopes)
        .unwrap();
    assert!(pending.conflicts.is_empty());
    let after: BTreeMap<_, _> = pending
        .snapshot
        .entries
        .iter()
        .filter(|(key, _)| settings_scope().contains(key))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    assert_eq!(after["setting/global/UiThemeMode"], "Dark");
    let journal_path = profile_root(&paths, &config).unwrap().join("apply.json");
    crate::config_file::save(
        &journal_path,
        &ApplyJournal {
            token: pending.token.clone(),
            expected_revision: bundle.revision.clone(),
            settings_before: before,
            settings_after: after,
        },
    )
    .unwrap();
    let roots = pending
        .snapshot
        .entries
        .get("roots")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .unwrap()
        .unwrap_or_default();
    adapter::apply_favorites(
        &host.store().lock(),
        &paths,
        &ApplyPlan {
            expected_revision: bundle.revision,
            entries: pending
                .snapshot
                .entries
                .iter()
                .filter(|(key, _)| key.starts_with("favor/"))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            roots,
            blobs: pending.snapshot.blobs.clone(),
            receipt: Some((RECEIPT.into(), pending.token.clone())),
        },
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(
        host.store().lock().kv_get(RECEIPT).unwrap().as_deref(),
        Some(pending.token.as_str())
    );

    // The user changes the local theme before restarting. Preserve that third
    // value on disk instead of repeatedly retrying the abandoned appearance.
    let global = {
        let mut inner = host.inner.lock();
        inner.extras.global.ui_theme_mode = "Light".into();
        inner.extras.global.clone()
    };
    crate::config_file::save(&paths.configs.join("GlobalConfig.json"), &global).unwrap();
    drop(local_engine);
    drop(host);
    let host = open_fixture_host(paths.clone());
    let mut local_engine = engine(&paths, &config).unwrap();
    assert!(local_engine.has_pending_apply());
    let rebased = host
        .webdav_sync_locked(&paths, &config, &remote, &mut local_engine)
        .unwrap();
    assert_eq!(rebased["requiresConflictResolution"], true);
    assert_eq!(host.inner.lock().extras.global.ui_theme_mode, "Light");
    assert!(!journal_path.exists());
    let conflict_pending = local_engine.pending().unwrap().unwrap();
    assert_eq!(conflict_pending.conflicts.len(), 1);
    let conflict = &conflict_pending.conflicts[0];
    assert_eq!(conflict.key, "setting/global/UiThemeMode");
    let variants: std::collections::BTreeSet<_> = [&conflict.left_value, &conflict.right_value]
        .into_iter()
        .map(|value| value.as_ref().unwrap().as_str().unwrap())
        .collect();
    assert_eq!(
        variants,
        std::collections::BTreeSet::from(["Dark", "Light"])
    );

    // Explicitly keeping the new local value unblocks subsequent sync; it must
    // not return to the old DB-receipt recovery loop.
    let chosen = if conflict.left_value == Some(json!("Light")) {
        &conflict.left_revision
    } else {
        &conflict.right_revision
    };
    local_engine
        .abandon_pending(&conflict_pending.token)
        .unwrap();
    let resolved = local_engine.resolve(&remote, &conflict.id, chosen).unwrap();
    local_engine.abandon_pending(&resolved.token).unwrap();
    let completed = host
        .webdav_sync_locked(&paths, &config, &remote, &mut local_engine)
        .unwrap();
    assert_eq!(completed["ok"], true);
    assert_eq!(host.inner.lock().extras.global.ui_theme_mode, "Light");
    assert!(!local_engine.has_pending_apply());
}
