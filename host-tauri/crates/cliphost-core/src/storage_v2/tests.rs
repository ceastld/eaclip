//! Candidate-coordinator safety tests. Every database and resource is synthetic
//! and lives under this repository's explicit `.tmp/storage-v2-tests` root.
//! The ignored child is only entered by the guarded subprocess tests below.

use super::*;
use crate::{
    model::{ClipKind, ClipRecord, FavorRecord},
    store::AppStore,
};
use std::{
    collections::BTreeMap,
    process::{Command, Output, Stdio},
    sync::mpsc,
    thread,
};

const CHILD_ROOT_ENV: &str = "CLIPHOST_STORAGE_V2_CRASH_FIXTURE_ROOT";
const CHILD_PHASE_ENV: &str = "CLIPHOST_STORAGE_V2_CRASH_FIXTURE_PHASE";
const CHILD_FIXTURE_MARKER: &[u8] = b"cliphost-storage-v2-synthetic-crash-fixture-v1";

fn fixture_parent() -> PathBuf {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .canonicalize()
        .expect("repository root");
    let directory = repository.join(".tmp").join("storage-v2-tests");
    fs::create_dir_all(&directory).expect("explicit repository test root");
    directory
}

struct Fixture {
    directory: tempfile::TempDir,
    source: PathBuf,
    target: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let directory = tempfile::Builder::new()
            .prefix(&format!("{label}-"))
            .tempdir_in(fixture_parent())
            .expect("isolated fixture directory");
        let source = directory.path().join("source");
        let target = directory.path().join("candidate");
        seed_native_source(&source);
        Self {
            directory,
            source,
            target,
        }
    }

    fn options(&self) -> MigrationOptions {
        MigrationOptions {
            source_root: self.source.clone(),
            candidate_root: self.target.clone(),
        }
    }

    fn archived_snapshot(&self, report: &CandidateReport) -> Connection {
        let encrypted = fs::read(self.target.join(&report.source_archive_path)).unwrap();
        let plaintext = protected_archive::unprotect(&encrypted).unwrap();
        assert_eq!(hash_bytes(&plaintext), report.source_snapshot_sha256);
        let restored = self
            .directory
            .path()
            .join("verified-source-snapshot.sqlite3");
        fs::write(&restored, plaintext).unwrap();
        open_readonly(&restored).unwrap()
    }
}

fn seed_native_source(source: &Path) {
    fs::create_dir_all(source.join("Configs")).unwrap();
    fs::create_dir_all(source.join("Image/Temp")).unwrap();
    fs::create_dir_all(source.join("WindowState")).unwrap();
    fs::write(
        source.join("Configs/GlobalConfig.json"),
        br#"{"UiThemeMode":"Dark","FuturePreference":{"flag":true}}"#,
    )
    .unwrap();
    fs::write(
        source.join("Configs/FavorConfig.json"),
        br#"{"RootChildren":["favorite-text"]}"#,
    )
    .unwrap();
    fs::write(
        source.join("WindowState/synthetic.json"),
        br#"{"Left":10,"Top":20}"#,
    )
    .unwrap();
    fs::write(
        source.join("clip-host-ui-preferences.json"),
        br#"{"showTrayIcon":true}"#,
    )
    .unwrap();
    let image_path = source.join("Image/Temp/synthetic.png");
    image::RgbaImage::from_pixel(2, 2, image::Rgba([20, 80, 140, 255]))
        .save(&image_path)
        .unwrap();
    let store = AppStore::open(&source.join("userdata.db")).unwrap();
    let mut text = ClipRecord::new_text("synthetic text\r\n保留原文", "migration-fixture");
    text.id = "history-text".into();
    text.create_time = chrono::DateTime::parse_from_rfc3339("2026-10-16T01:02:03.123456700Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    text.update_time = text.create_time;
    store.insert_clip(&text, true).unwrap();
    let mut picture = ClipRecord::new_text("", "migration-fixture");
    picture.id = "history-image".into();
    picture.kind = ClipKind::ImageClipItem;
    picture.image_path = Some(image_path.to_string_lossy().into_owned());
    store.insert_clip(&picture, true).unwrap();
    let mut favorite = FavorRecord::from_clip(&text, None);
    favorite.id = "favorite-text".into();
    store.insert_favor(&favorite).unwrap();
    drop(store);
    let connection = Connection::open(source.join("userdata.db")).unwrap();
    connection
        .execute(
            "INSERT INTO kv(key,value) VALUES('fixture-state','\"initial\"')",
            [],
        )
        .unwrap();
}

/// Read the exact synthetic subtree, including SQLite sidecars if present.
/// Never follow links or expand a test root outside the repository fixture dir.
fn tree_bytes(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn collect(root: &Path, directory: &Path, result: &mut BTreeMap<PathBuf, Vec<u8>>) {
        let mut entries: Vec<_> = fs::read_dir(directory)
            .unwrap()
            .map(Result::unwrap)
            .collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let kind = entry.file_type().unwrap();
            assert!(!kind.is_symlink(), "fixture must not contain links");
            if kind.is_dir() {
                collect(root, &path, result);
            } else {
                assert!(kind.is_file(), "fixture must contain only regular files");
                result.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    fs::read(path).unwrap(),
                );
            }
        }
    }
    let canonical = root.canonicalize().unwrap();
    assert!(canonical.starts_with(fixture_parent().canonicalize().unwrap()));
    let mut result = BTreeMap::new();
    collect(root, root, &mut result);
    result
}

fn assert_unchanged(root: &Path, expected: &BTreeMap<PathBuf, Vec<u8>>) {
    assert_eq!(
        &tree_bytes(root),
        expected,
        "migration changed synthetic source files"
    );
}

fn assert_unpublished(fixture: &Fixture) {
    assert!(
        !fixture.target.exists(),
        "a failed/cancelled build must not publish its final directory"
    );
}

fn build(options: &MigrationOptions) -> Result<CandidateReport, String> {
    build_candidate(options, &mut |_| Ok(()))
}

#[cfg(windows)]
fn run_crash_child(root: &Path, phase: &str) -> Output {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "storage_v2::tests::candidate_process_exit_child",
            "--ignored",
            "--nocapture",
        ])
        .env(CHILD_ROOT_ENV, root)
        .env(CHILD_PHASE_ENV, phase)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!(
                "fixture child timed out at {phase}:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn candidate_cancellation_at_every_observable_phase_keeps_source_and_target_absent() {
    for phase in [
        "preflight",
        "snapshot",
        "snapshot_complete",
        "convert",
        "assets",
        "assets_complete",
        "write",
        "before_commit",
        "after_commit",
        "verify",
        "before_publish",
    ] {
        let fixture = Fixture::new(&format!("cancel-{phase}"));
        let before = tree_bytes(&fixture.source);
        let mut observed = false;
        let sentinel = format!("injected cancellation at {phase}");
        let error = build_candidate(&fixture.options(), &mut |progress| {
            if progress.phase == phase {
                observed = true;
                return Err(sentinel.clone());
            }
            Ok(())
        })
        .unwrap_err();
        assert!(
            observed,
            "phase {phase} was not reached; actual error: {error}"
        );
        assert!(
            error.contains(&sentinel),
            "cancellation reason lost at {phase}: {error}"
        );
        assert_unchanged(&fixture.source, &before);
        assert_unpublished(&fixture);
        let stages = fs::read_dir(fixture.directory.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".cliphost-v2-candidate-")
            })
            .count();
        assert_eq!(
            stages, 0,
            "ordinary error must drop its private stage at {phase}"
        );
    }
}

#[test]
fn candidate_rejects_existing_and_overlapping_targets() {
    let fixture = Fixture::new("target-paths");
    let before = tree_bytes(&fixture.source);
    for target in [
        fixture.source.clone(),
        fixture.source.join("nested-candidate"),
        fixture.directory.path().to_path_buf(),
    ] {
        let error = build(&MigrationOptions {
            source_root: fixture.source.clone(),
            candidate_root: target,
        })
        .unwrap_err();
        assert!(!error.is_empty());
        assert_unchanged(&fixture.source, &before);
    }
    fs::create_dir(&fixture.target).unwrap();
    let sentinel = fixture.target.join("must-keep.txt");
    fs::write(&sentinel, b"existing target is user data").unwrap();
    assert!(build(&fixture.options()).unwrap_err().contains("已存在"));
    assert_eq!(
        fs::read(&sentinel).unwrap(),
        b"existing target is user data"
    );
    assert_unchanged(&fixture.source, &before);
    let existing_file = fixture.directory.path().join("existing-file");
    fs::write(&existing_file, b"keep file").unwrap();
    assert!(build(&MigrationOptions {
        source_root: fixture.source.clone(),
        candidate_root: existing_file.clone()
    })
    .is_err());
    assert_eq!(fs::read(existing_file).unwrap(), b"keep file");
}

#[cfg(windows)]
#[test]
fn candidate_target_lock_excludes_concurrent_builders() {
    let fixture = Fixture::new("target-lock");
    let before = tree_bytes(&fixture.source);
    let options = fixture.options();
    let first_options = options.clone();
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let worker = thread::spawn(move || {
        let mut held = false;
        build_candidate(&first_options, &mut |progress| {
            if progress.phase == "snapshot_complete" && !held {
                held = true;
                entered_tx.send(()).map_err(|error| error.to_string())?;
                release_rx
                    .recv_timeout(Duration::from_secs(15))
                    .map_err(|error| error.to_string())?;
            }
            Ok(())
        })
    });
    entered_rx
        .recv_timeout(Duration::from_secs(15))
        .expect("first migration reached its held lock");
    let second = build(&options);
    release_tx.send(()).unwrap();
    let first = worker.join().unwrap();
    let error = second.unwrap_err();
    assert!(
        error.contains("锁"),
        "second builder should fail on the OS lock: {error}"
    );
    assert!(first.unwrap().candidate_only);
    assert_unchanged(&fixture.source, &before);
}

#[cfg(windows)]
#[test]
fn candidate_source_guard_denies_callback_database_writes() {
    for phase in ["assets_complete", "before_publish"] {
        let fixture = Fixture::new(&format!("guard-db-{phase}"));
        let before = tree_bytes(&fixture.source);
        let mut attempted = false;
        let mut denied = false;
        let result = build_candidate(&fixture.options(), &mut |progress| {
            if progress.phase == phase && !attempted {
                attempted = true;
                denied = match Connection::open(fixture.source.join("userdata.db")) {
                    Err(_) => true,
                    Ok(writer) => writer
                        .execute(
                            "UPDATE kv SET value='\"external-change\"' WHERE key='fixture-state'",
                            [],
                        )
                        .is_err(),
                };
                if !denied {
                    return Err("test detected an unprotected source database write".into());
                }
            }
            Ok(())
        });
        assert!(
            attempted,
            "guard mutation phase {phase} was not reached: {result:?}"
        );
        assert!(denied, "OS source guard did not deny writing at {phase}");
        assert!(result.unwrap().candidate_only);
        assert_unchanged(&fixture.source, &before);
    }
}

#[cfg(windows)]
#[test]
fn candidate_rejects_live_database_writer_without_source_changes() {
    let fixture = Fixture::new("live-writer");
    let writer = Connection::open(fixture.source.join("userdata.db")).unwrap();
    writer.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; UPDATE kv SET value='\"live-writer\"' WHERE key='fixture-state';").unwrap();
    let before = tree_bytes(&fixture.source);
    let result = build(&fixture.options());
    assert!(
        result.is_err(),
        "migration must not pretend an active RW source is quiescent"
    );
    assert_unchanged(&fixture.source, &before);
    assert_unpublished(&fixture);
    assert_eq!(
        writer
            .query_row("SELECT count(*) FROM clip_item", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        2
    );
}

#[test]
fn candidate_rejects_config_changed_after_copy() {
    for phase in ["assets_complete", "before_publish"] {
        let fixture = Fixture::new(&format!("changed-config-{phase}"));
        let before = tree_bytes(&fixture.source);
        let config = fixture.source.join("Configs/GlobalConfig.json");
        let changed_bytes = br#"{"UiThemeMode":"Light","changedExternally":true}"#;
        let mut changed = false;
        let result = build_candidate(&fixture.options(), &mut |progress| {
            if progress.phase == phase && !changed {
                fs::write(&config, changed_bytes).map_err(|error| error.to_string())?;
                changed = true;
            }
            Ok(())
        });
        assert!(
            changed,
            "source mutation phase {phase} was not reached: {result:?}"
        );
        let error = result.unwrap_err();
        assert!(
            error.contains("配置") || error.contains("状态"),
            "wrong config-change error: {error}"
        );
        assert_unpublished(&fixture);
        let mut expected = before;
        expected.insert(
            PathBuf::from("Configs/GlobalConfig.json"),
            changed_bytes.to_vec(),
        );
        assert_unchanged(&fixture.source, &expected);
    }
}

#[test]
fn candidate_rejects_corrupt_json_and_invalid_favorite_graph() {
    for variant in [
        "config-json",
        "item-json",
        "missing-child",
        "self-cycle",
        "missing-owner",
    ] {
        let fixture = Fixture::new(&format!("bad-{variant}"));
        if variant == "config-json" {
            fs::write(
                fixture.source.join("Configs/GlobalConfig.json"),
                b"{invalid-json",
            )
            .unwrap();
        } else {
            let source = Connection::open(fixture.source.join("userdata.db")).unwrap();
            match variant {
                "item-json" => {
                    source
                        .execute(
                            "UPDATE clip_item SET extra_json='{bad' WHERE id='history-text'",
                            [],
                        )
                        .unwrap();
                }
                "missing-child" => {
                    source.execute("UPDATE favor_item SET is_folder=1,kind='FavoriteClipItem',children='[\"missing\"]' WHERE id='favorite-text'", []).unwrap();
                }
                "self-cycle" => {
                    source.execute("UPDATE favor_item SET is_folder=1,kind='FavoriteClipItem',children='[\"favorite-text\"]' WHERE id='favorite-text'", []).unwrap();
                }
                "missing-owner" => {
                    source
                        .execute(
                            "UPDATE favor_item SET parent_id='missing' WHERE id='favorite-text'",
                            [],
                        )
                        .unwrap();
                }
                _ => unreachable!(),
            }
        }
        let before = tree_bytes(&fixture.source);
        let error = build(&fixture.options()).unwrap_err();
        assert!(!error.is_empty(), "{variant} must report its rejection");
        assert_unchanged(&fixture.source, &before);
        assert_unpublished(&fixture);
    }
}

#[cfg(windows)]
#[test]
fn candidate_reads_committed_wal_snapshot_only() {
    let fixture = Fixture::new("wal-snapshot");
    fs::write(
        fixture
            .directory
            .path()
            .join("synthetic-crash-fixture.marker"),
        CHILD_FIXTURE_MARKER,
    )
    .unwrap();
    let output = run_crash_child(fixture.directory.path(), "wal_fixture");
    assert_eq!(
        output.status.code(),
        Some(77),
        "WAL fixture child failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(fixture.source.join("userdata.db-wal").exists());
    assert!(fixture.source.join("userdata.db-shm").exists());
    // The child intentionally bypassed SQLite close/rollback; the OS has now
    // released every writer and byte-range lock. This is an explicit offline
    // fixture containing committed WAL data and an interrupted large write.
    let before = tree_bytes(&fixture.source);
    let report = build(&fixture.options()).unwrap();
    assert!(report.candidate_only && !report.production_ready);
    assert_eq!(report.history_count, 2);
    let snapshot = fixture.archived_snapshot(&report);
    assert_eq!(
        snapshot
            .query_row(
                "SELECT value FROM kv WHERE key='wal-committed'",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
        "committed"
    );
    assert_eq!(
        snapshot
            .query_row(
                "SELECT count(*) FROM kv WHERE key='wal-uncommitted'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    assert_unchanged(&fixture.source, &before);
}

#[cfg(windows)]
#[test]
fn candidate_checkpointed_wal_header_without_sidecars_stays_unchanged() {
    let fixture = Fixture::new("checkpointed-wal-header");
    let writer = Connection::open(fixture.source.join("userdata.db")).unwrap();
    writer.execute_batch("PRAGMA journal_mode=WAL; INSERT INTO kv(key,value) VALUES('normal-close','checkpointed');").unwrap();
    drop(writer);
    assert!(!fixture.source.join("userdata.db-wal").exists());
    assert!(!fixture.source.join("userdata.db-shm").exists());
    let header = fs::read(fixture.source.join("userdata.db")).unwrap();
    assert_eq!(
        (header[18], header[19]),
        (2, 2),
        "fixture must retain WAL read/write header versions after normal close"
    );
    let before = tree_bytes(&fixture.source);
    let report = build(&fixture.options()).unwrap();
    assert!(report.candidate_only && !report.production_ready);
    let snapshot = fixture.archived_snapshot(&report);
    assert_eq!(
        snapshot
            .query_row("SELECT value FROM kv WHERE key='normal-close'", [], |row| {
                row.get::<_, String>(0)
            })
            .unwrap(),
        "checkpointed"
    );
    assert_unchanged(&fixture.source, &before);
}

#[test]
fn candidate_rejects_wal_without_shm_without_repairing_source() {
    let fixture = Fixture::new("wal-no-shm");
    fs::write(
        fixture.source.join("userdata.db-wal"),
        b"synthetic invalid WAL sentinel",
    )
    .unwrap();
    assert!(!fixture.source.join("userdata.db-shm").exists());
    let before = tree_bytes(&fixture.source);
    let error = build(&fixture.options()).unwrap_err();
    assert!(
        error.contains("WAL") && error.contains("SHM"),
        "preflight must reject missing SHM: {error}"
    );
    assert_unchanged(&fixture.source, &before);
    assert_unpublished(&fixture);
}

#[test]
fn snapshot_limit_uses_effective_pages_including_wal() {
    let fixture = Fixture::new("wal-page-limit");
    let writer = Connection::open(fixture.source.join("userdata.db")).unwrap();
    writer.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE large_test(value BLOB); INSERT INTO large_test VALUES(zeroblob(1048576));").unwrap();
    let physical_main = fs::metadata(fixture.source.join("userdata.db"))
        .unwrap()
        .len();
    let page_size: u64 = writer
        .query_row("PRAGMA page_size", [], |r| r.get(0))
        .unwrap();
    let page_count: u64 = writer
        .query_row("PRAGMA page_count", [], |r| r.get(0))
        .unwrap();
    assert!(page_count * page_size > physical_main + page_size);
    let before = tree_bytes(&fixture.source);
    let error = backup_snapshot_bounded(
        &writer,
        Instant::now(),
        &mut |_| Ok(()),
        physical_main + page_size,
    )
    .unwrap_err();
    assert!(error.contains("含 WAL"), "wrong limit error: {error}");
    assert_unchanged(&fixture.source, &before);
    assert!(!fixture.target.exists());
}

#[cfg(windows)]
#[test]
fn candidate_is_rejected_by_current_appstore_without_writes() {
    let fixture = Fixture::new("runtime-reject");
    let source_before = tree_bytes(&fixture.source);
    let report = build(&fixture.options()).unwrap();
    assert!(report.candidate_only);
    assert!(!report.production_ready);
    assert!(!report.blockers.is_empty());
    let candidate_before = tree_bytes(&fixture.target);
    let error = match AppStore::open(&fixture.target.join("userdata.db")) {
        Ok(_) => panic!("current AppStore must not initialize a v2 candidate"),
        Err(error) => error.to_string(),
    };
    assert!(
        error.contains("STORAGE_V2_RUNTIME_NOT_READY"),
        "wrong runtime gate error: {error}"
    );
    assert_unchanged(&fixture.target, &candidate_before);
    assert_unchanged(&fixture.source, &source_before);
}

#[cfg(windows)]
#[test]
fn candidate_complete_output_and_progress_remain_candidate_only() {
    let fixture = Fixture::new("candidate-output");
    let source_before = tree_bytes(&fixture.source);
    let mut progress = Vec::new();
    let report = build_candidate(&fixture.options(), &mut |event| {
        progress.push(event.clone());
        Ok(())
    })
    .unwrap();
    assert_eq!(
        (
            report.history_count,
            report.favorite_count,
            report.asset_count
        ),
        (2, 1, 1)
    );
    assert!(report.candidate_only && !report.production_ready);
    assert_eq!(report.missing_references, 0);
    assert_eq!(report.unresolved_references, 0);
    assert_eq!(
        hash_file(&fixture.target.join("userdata.db")).unwrap(),
        report.candidate_database_sha256
    );
    assert_eq!(
        hash_bytes(
            &protected_archive::unprotect(
                &fs::read(fixture.target.join(&report.source_archive_path)).unwrap()
            )
            .unwrap()
        ),
        report.source_snapshot_sha256
    );
    let saved: CandidateReport = serde_json::from_slice(
        &fs::read(fixture.target.join("migration/candidate-report.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(saved.operation_id, report.operation_id);
    assert_eq!(saved.dataset_id, report.dataset_id);
    assert!(!saved.production_ready);
    assert_eq!(progress.first().unwrap().phase, "preflight");
    assert!(progress.first().unwrap().total.is_none());
    assert_eq!(progress.last().unwrap().phase, "before_publish");
    for event in &progress {
        if let Some(total) = event.total {
            assert!(
                event.completed <= total,
                "invalid phase-local counter: {event:?}"
            );
        }
        assert!(!event.unit.contains('%'), "no invented overall percentage");
        let public = serde_json::to_string(event).unwrap();
        assert!(!public.contains("synthetic text") && !public.contains("保留原文"));
    }
    assert!(progress
        .windows(2)
        .all(|pair| pair[0].elapsed_ms <= pair[1].elapsed_ms));
    assert_unchanged(&fixture.source, &source_before);
}

#[cfg(windows)]
#[test]
fn candidate_protects_machine_credentials_and_does_not_import_them_as_settings() {
    let fixture = Fixture::new("protected-archives");
    fs::create_dir_all(fixture.source.join("Configs/LanShare")).unwrap();
    fs::create_dir_all(fixture.source.join("Configs/plugins")).unwrap();
    fs::create_dir_all(fixture.source.join("LanShare")).unwrap();
    let key = b"synthetic-private-key-do-not-log";
    let config = br#"{"AccessToken":"synthetic-account-token-do-not-log","Future":17}"#;
    fs::write(fixture.source.join("Configs/LanShare/key.der"), key).unwrap();
    fs::write(fixture.source.join("Configs/CloudConfig.json"), config).unwrap();
    fs::write(fixture.source.join("Configs/plugins/custom.json"), config).unwrap();
    fs::write(
        fixture.source.join("Configs/plugins/custom.ini"),
        b"opaque-plugin-secret",
    )
    .unwrap();
    fs::write(fixture.source.join("WindowState/custom.json"), config).unwrap();
    fs::write(
        fixture.source.join("LanShare/identity.pfx"),
        b"synthetic-pfx-material",
    )
    .unwrap();
    {
        let c = Connection::open(fixture.source.join("userdata.db")).unwrap();
        c.execute_batch("CREATE TABLE cliphost_lan_peer(id TEXT PRIMARY KEY,payload TEXT); INSERT INTO cliphost_lan_peer VALUES('synthetic-peer','{\"token\":\"synthetic-pairing-token\"}');").unwrap();
    }
    let before = tree_bytes(&fixture.source);
    let report = build(&fixture.options()).unwrap();
    assert!(report.source_archive_protected);
    assert!(!fixture.target.join("migration/source.sqlite3").exists());
    let protected = fs::read(fixture.target.join(&report.source_archive_path)).unwrap();
    let decoded = super::protected_archive::unprotect(&protected).unwrap();
    assert_eq!(hash_bytes(&decoded), report.source_snapshot_sha256);
    assert_eq!(hash_bytes(&protected), report.source_archive_sha256);
    for (relative, expected) in [
        ("Configs/LanShare/key.der", key.as_slice()),
        ("Configs/CloudConfig.json", config.as_slice()),
        ("Configs/plugins/custom.json", config.as_slice()),
        (
            "Configs/plugins/custom.ini",
            b"opaque-plugin-secret".as_slice(),
        ),
        ("WindowState/custom.json", config.as_slice()),
        (
            "LanShare/identity.pfx",
            b"synthetic-pfx-material".as_slice(),
        ),
    ] {
        assert!(!fixture
            .target
            .join("migration/source-files")
            .join(relative)
            .exists());
        let encrypted = fs::read(
            fixture
                .target
                .join("migration/source-files")
                .join(format!("{relative}.dpapi")),
        )
        .unwrap();
        assert_eq!(
            super::protected_archive::unprotect(&encrypted).unwrap(),
            expected
        );
    }
    let c = open_readonly(&fixture.target.join("userdata.db")).unwrap();
    assert!(!fixture.target.join("migration/source-rows.json").exists());
    let archived_rows = protected_archive::unprotect(
        &fs::read(fixture.target.join("migration/source-rows.json.dpapi")).unwrap(),
    )
    .unwrap();
    assert!(serde_json::from_slice::<Value>(&archived_rows).is_ok());
    assert_eq!(
        c.query_row(
            "SELECT count(*) FROM settings WHERE key='source/Configs/CloudConfig.json'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    assert_unchanged(&fixture.source, &before);
}

#[test]
#[ignore = "only run by subprocess tests with an explicit guarded repository fixture"]
fn candidate_process_exit_child() {
    let raw_root =
        std::env::var_os(CHILD_ROOT_ENV).expect("explicit crash fixture root is mandatory");
    let root = PathBuf::from(raw_root);
    assert!(root.is_absolute());
    let root = root.canonicalize().unwrap();
    let allowed = fixture_parent().canonicalize().unwrap();
    assert!(
        root != allowed && root.starts_with(&allowed),
        "child must stay inside explicit repository test root"
    );
    assert_eq!(
        fs::read(root.join("synthetic-crash-fixture.marker")).unwrap(),
        CHILD_FIXTURE_MARKER
    );
    let phase = std::env::var(CHILD_PHASE_ENV).expect("explicit crash phase is mandatory");
    assert!([
        "snapshot_complete",
        "before_commit",
        "after_commit",
        "before_publish",
        "wal_fixture",
        "rollback_fixture"
    ]
    .contains(&phase.as_str()));
    if phase == "wal_fixture" {
        let writer = Connection::open(root.join("source/userdata.db")).unwrap();
        writer.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; PRAGMA cache_size=5; INSERT INTO kv(key,value) VALUES('wal-committed','committed'); BEGIN IMMEDIATE; INSERT INTO kv(key,value) VALUES('wal-uncommitted',zeroblob(2000000));").unwrap();
        std::process::exit(77);
    }
    if phase == "rollback_fixture" {
        let writer = Connection::open(root.join("source/userdata.db")).unwrap();
        writer
            .execute_batch(
                "PRAGMA journal_mode=DELETE;
             PRAGMA synchronous=FULL;
             PRAGMA cache_size=5;
             PRAGMA cache_spill=ON;
             BEGIN IMMEDIATE;
             UPDATE kv SET value=zeroblob(2097152) WHERE key='rollback-spill-anchor';",
            )
            .unwrap();
        assert_eq!(
            writer.changes(),
            1,
            "the committed synthetic anchor must be overwritten"
        );
        let journal = root.join("source/userdata.db-journal");
        let journal_bytes = fs::read(&journal).unwrap();
        assert!(
            journal_bytes.len() > 512,
            "cache spill must create a real rollback journal"
        );
        assert_eq!(
            &journal_bytes[..8],
            &[0xd9, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7],
            "a synced hot-journal header is required for this crash fixture"
        );
        // Do not run Connection::drop: SQLite must leave the dirty main pages
        // and their recovery journal for the parent's read-only rejection test.
        std::process::exit(77);
    }
    let result = build_candidate(
        &MigrationOptions {
            source_root: root.join("source"),
            candidate_root: root.join("candidate"),
        },
        &mut |event| {
            if event.phase == phase {
                // Intentionally bypass all Drop handlers: the parent verifies OS
                // lock release, an unpublished final path, and a fresh retry.
                std::process::exit(77);
            }
            Ok(())
        },
    );
    panic!("child did not reach forced-exit phase {phase}: {result:?}");
}

#[cfg(windows)]
#[test]
fn candidate_abrupt_exit_leaves_no_final_and_retry_is_fresh() {
    for phase in [
        "snapshot_complete",
        "before_commit",
        "after_commit",
        "before_publish",
    ] {
        let fixture = Fixture::new(&format!("abrupt-{phase}"));
        fs::write(
            fixture
                .directory
                .path()
                .join("synthetic-crash-fixture.marker"),
            CHILD_FIXTURE_MARKER,
        )
        .unwrap();
        let source_before = tree_bytes(&fixture.source);
        let output = run_crash_child(fixture.directory.path(), phase);
        assert_eq!(
            output.status.code(),
            Some(77),
            "child failed before injected exit at {phase}:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_unpublished(&fixture);
        assert_unchanged(&fixture.source, &source_before);
        let abandoned: Vec<_> = fs::read_dir(fixture.directory.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".cliphost-v2-candidate-")
            })
            .map(|entry| entry.path())
            .collect();
        assert!(
            !abandoned.is_empty(),
            "abrupt exit should leave its private stage for recovery diagnostics"
        );
        for stage in &abandoned {
            assert!(
                tree_bytes(stage)
                    .keys()
                    .all(|path| { path.file_name().is_none_or(|name| name != "source.sqlite3") }),
                "abrupt exit at {phase} left a plaintext original SQLite snapshot"
            );
        }
        let retry = build(&fixture.options())
            .expect("OS lock must release and retry must generate a fresh candidate");
        assert!(retry.candidate_only && !retry.production_ready);
        assert_eq!((retry.history_count, retry.favorite_count), (2, 1));
        let candidate = open_readonly(&fixture.target.join("userdata.db")).unwrap();
        assert_eq!(
            candidate
                .query_row(
                    "SELECT count(*) FROM items WHERE system_role IS NULL",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            3
        );
        let operation: String = candidate
            .query_row(
                "SELECT operation_id FROM storage_v2_candidate WHERE singleton=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(operation, retry.operation_id);
        assert_unchanged(&fixture.source, &source_before);
    }
}

#[cfg(windows)]
#[test]
fn candidate_protected_favorite_config_keeps_root_order_without_plain_settings() {
    let fixture = Fixture::new("protected-favorite-order");
    {
        let store = AppStore::open(&fixture.source.join("userdata.db")).unwrap();
        store.delete_favor("favorite-text").unwrap();
        for id in ["Z", "A"] {
            let clip = ClipRecord::new_text("", "synthetic-folder");
            let mut folder = FavorRecord::from_clip(&clip, None);
            folder.id = id.into();
            folder.kind = ClipKind::FavoriteClipItem;
            folder.is_folder = true;
            folder.name = id.into();
            folder.source_clip_id = None;
            store.insert_favor(&folder).unwrap();
        }
    }
    // A future security-related extension protects the whole original file;
    // its known business fields must still be read in memory before archival.
    let original = br#"{"RootChildren":["Z","A"],"FutureToken":"synthetic-favorite-token"}"#;
    fs::write(fixture.source.join("Configs/FavorConfig.json"), original).unwrap();
    let before = tree_bytes(&fixture.source);
    let report = build(&fixture.options()).unwrap();
    assert!(report.candidate_only && !report.production_ready);
    let candidate = open_readonly(&fixture.target.join("userdata.db")).unwrap();
    let root_children: String = candidate
        .query_row(
            "SELECT i.children_json FROM items i JOIN store_info s
         ON i.scope=s.favorite_scope AND i.id=s.favorite_root_id WHERE s.singleton=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Vec<String>>(&root_children).unwrap(),
        vec!["Z", "A"]
    );
    assert_eq!(
        candidate
            .query_row(
                "SELECT count(*) FROM settings WHERE key='source/Configs/FavorConfig.json'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        0,
        "the complete protected document must not reappear as plain settings"
    );
    assert!(!fixture
        .target
        .join("migration/source-files/Configs/FavorConfig.json")
        .exists());
    let encrypted = fs::read(
        fixture
            .target
            .join("migration/source-files/Configs/FavorConfig.json.dpapi"),
    )
    .unwrap();
    assert_eq!(
        super::protected_archive::unprotect(&encrypted).unwrap(),
        original.as_slice()
    );
    assert_unchanged(&fixture.source, &before);
}

#[cfg(windows)]
#[test]
fn candidate_final_callback_target_tampering_is_rejected_before_publication() {
    for damaged_kind in ["asset", "protected_snapshot", "config_archive"] {
        let fixture = Fixture::new(&format!("tampered-final-{damaged_kind}"));
        let before = tree_bytes(&fixture.source);
        let mut changed = false;
        let result = build_candidate(&fixture.options(), &mut |progress| {
            if progress.phase != "before_publish" || changed {
                return Ok(());
            }
            // Discover only this explicitly owned fixture's private stage. No
            // search of another migration, user directory, or real data root.
            let stages: Vec<PathBuf> = fs::read_dir(fixture.directory.path())
                .unwrap()
                .map(Result::unwrap)
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".cliphost-v2-candidate-")
                })
                .map(|entry| entry.path())
                .collect();
            assert_eq!(
                stages.len(),
                1,
                "expected exactly the current synthetic stage"
            );
            let stage = &stages[0];
            let damaged_path = match damaged_kind {
                "asset" => {
                    let files = tree_bytes(stage);
                    let relative = files
                        .keys()
                        .find(|path| path.starts_with("assets"))
                        .expect("image fixture must produce a published immutable asset");
                    stage.join(relative)
                }
                "protected_snapshot" => stage.join("migration/source.sqlite3.dpapi"),
                _ => stage.join("migration/source-files/Configs/GlobalConfig.json"),
            };
            let mut bytes = fs::read(&damaged_path).unwrap();
            assert!(
                !bytes.is_empty(),
                "the selected verified fixture output must contain bytes"
            );
            bytes[0] ^= 0x40;
            fs::write(&damaged_path, &bytes).unwrap();
            changed = true;
            Ok(())
        });
        assert!(
            changed,
            "final callback was not reached for {damaged_kind}: {result:?}"
        );
        assert!(
            result.is_err(),
            "modified {damaged_kind} was published after its last verification"
        );
        assert_unpublished(&fixture);
        assert_unchanged(&fixture.source, &before);
        let stages = fs::read_dir(fixture.directory.path())
            .unwrap()
            .map(Result::unwrap)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".cliphost-v2-candidate-")
            })
            .count();
        assert_eq!(
            stages, 0,
            "failed final validation must discard its private stage"
        );
    }
}

#[test]
fn candidate_rejects_existing_rollback_journal_without_touching_source() {
    let fixture = Fixture::new("rollback-journal-sentinel");
    let journal = fixture.source.join("userdata.db-journal");
    fs::write(&journal, b"synthetic nonempty rollback journal sentinel").unwrap();
    let before = tree_bytes(&fixture.source);
    let error = build(&fixture.options()).unwrap_err();
    assert!(
        error.to_ascii_lowercase().contains("journal"),
        "missing recovery explanation: {error}"
    );
    assert_unchanged(&fixture.source, &before);
    assert_unpublished(&fixture);
}

#[cfg(windows)]
#[test]
fn candidate_rejects_real_spilled_hot_journal_without_recovering_dirty_source() {
    let fixture = Fixture::new("rollback-journal-crash");
    fs::write(
        fixture
            .directory
            .path()
            .join("synthetic-crash-fixture.marker"),
        CHILD_FIXTURE_MARKER,
    )
    .unwrap();
    {
        let writer = Connection::open(fixture.source.join("userdata.db")).unwrap();
        writer
            .execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;")
            .unwrap();
        // This committed allocation gives the child many existing main-file
        // pages to overwrite, rather than only appending uncommitted pages.
        writer
            .execute(
                "INSERT INTO kv(key,value) VALUES('rollback-spill-anchor',?1)",
                ["A".repeat(2 * 1024 * 1024)],
            )
            .unwrap();
    }
    let committed_main = fs::read(fixture.source.join("userdata.db")).unwrap();
    let output = run_crash_child(fixture.directory.path(), "rollback_fixture");
    assert_eq!(
        output.status.code(),
        Some(77),
        "child failed before producing the guarded hot journal:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let journal = fs::read(fixture.source.join("userdata.db-journal")).unwrap();
    assert!(journal.len() > 512);
    assert_eq!(
        &journal[..8],
        &[0xd9, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7]
    );
    assert_ne!(
        fs::read(fixture.source.join("userdata.db")).unwrap(),
        committed_main,
        "the controlled cache spill must actually dirty the main file"
    );
    // Do not open this source with SQLite in the parent: that could recover it.
    // Exact filesystem bytes after the child crash are the rejection baseline.
    let crashed_source = tree_bytes(&fixture.source);
    let error = build(&fixture.options()).unwrap_err();
    assert!(
        error.to_ascii_lowercase().contains("journal"),
        "wrong preflight rejection: {error}"
    );
    assert_unchanged(&fixture.source, &crashed_source);
    assert_unpublished(&fixture);
}

#[cfg(windows)]
#[test]
fn candidate_rejects_legal_sql_tampering_at_after_commit_callback() {
    for damaged_kind in ["dataset_id", "root_children", "settings"] {
        let fixture = Fixture::new(&format!("tampered-sql-after-commit-{damaged_kind}"));
        let source_before = tree_bytes(&fixture.source);
        let mut changed = false;
        let result = build_candidate(&fixture.options(), &mut |progress| {
            if progress.phase != "after_commit" || changed {
                return Ok(());
            }
            let stages: Vec<PathBuf> = fs::read_dir(fixture.directory.path())
                .unwrap()
                .map(Result::unwrap)
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".cliphost-v2-candidate-")
                })
                .map(|entry| entry.path())
                .collect();
            assert_eq!(
                stages.len(),
                1,
                "only the current synthetic candidate may be opened"
            );
            {
                let candidate = Connection::open(stages[0].join("userdata.db")).unwrap();
                candidate.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
                let sql = match damaged_kind {
                    "dataset_id" => "UPDATE store_info SET dataset_id='synthetic-tampered-dataset' WHERE singleton=1",
                    "root_children" => "UPDATE items SET children_json='[\"synthetic-missing-favorite\"]' WHERE system_role='favorite_root'",
                    _ => "UPDATE settings SET value_json='{\"UiThemeMode\":\"Light\",\"syntheticTamper\":true}' WHERE key='source/Configs/GlobalConfig.json'",
                };
                assert_eq!(
                    candidate.execute(sql, []).unwrap(),
                    1,
                    "the {damaged_kind} fixture must change exactly one existing row"
                );
                // All three writes satisfy SQLite's ordinary JSON, CHECK and
                // foreign-key constraints. The coordinator must validate its
                // expected committed content, rather than integrity alone.
                let integrity: String = candidate
                    .query_row("PRAGMA integrity_check", [], |row| row.get(0))
                    .unwrap();
                assert_eq!(integrity, "ok");
            }
            changed = true;
            Ok(())
        });
        assert!(
            changed,
            "after_commit was not reached for {damaged_kind}: {result:?}"
        );
        assert!(
            result.is_err(),
            "legally modified {damaged_kind} was accepted after commit"
        );
        assert_unpublished(&fixture);
        assert_unchanged(&fixture.source, &source_before);
    }
}
