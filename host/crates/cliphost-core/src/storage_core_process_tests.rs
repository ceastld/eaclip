//! Fault injection in explicit disposable copies. No real host is started.
use super::*;
use crate::storage_core::migrate::{build_candidate, MigrationOptions};
use std::process::{Command, Stdio};

const CHILD: &str = "host::storage_core_process_tests::core_migration_crash_child";
const MARKER: &[u8] = b"cliphost-core-own-crash-fixture-v1";

fn base() -> PathBuf {
    let root =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp/storage-core-process-tests");
    std::fs::create_dir_all(&root).unwrap();
    root.canonicalize().unwrap()
}

fn options(root: &Path) -> MigrationOptions {
    MigrationOptions {
        source_root: root.join("source"),
        candidate_root: root.join("candidate"),
    }
}

#[test]
#[ignore = "private subprocess helper entered only by its guarded parent test"]
fn core_migration_crash_child() {
    let root =
        PathBuf::from(std::env::var_os("CLIPHOST_CORE_CRASH_ROOT").expect("explicit fixture root"))
            .canonicalize()
            .unwrap();
    assert!(root.starts_with(base()));
    assert_eq!(std::fs::read(root.join("OWN_FIXTURE")).unwrap(), MARKER);
    let phase = std::env::var("CLIPHOST_CORE_CRASH_PHASE").unwrap();
    build_candidate(&options(&root), &mut |progress| {
        if progress.phase == phase {
            let mut ready = std::fs::File::create(root.join("child-ready")).unwrap();
            use std::io::Write;
            ready.write_all(phase.as_bytes()).unwrap();
            ready.sync_all().unwrap();
            loop {
                std::thread::park_timeout(Duration::from_secs(1));
            }
        }
        Ok(())
    })
    .unwrap();
    panic!("required crash phase was not reached");
}

#[test]
fn core_process_kill_before_publish_preserves_input_and_allows_clean_retry() {
    for phase in ["items", "copy_tree", "ready_to_publish"] {
        let directory = tempfile::tempdir_in(base()).unwrap();
        let root = directory.path().canonicalize().unwrap();
        std::fs::write(root.join("OWN_FIXTURE"), MARKER).unwrap();
        storage_core_host_tests::seed_documents(&root.join("source"));
        let before = storage_core_host_tests::tree_hashes(&root.join("source"));
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                CHILD,
                "--test-threads=1",
                "--nocapture",
            ])
            .env("CLIPHOST_CORE_CRASH_ROOT", &root)
            .env("CLIPHOST_CORE_CRASH_PHASE", phase)
            .env("CLIPHOST_TAURI_HEADLESS", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while !root.join("child-ready").exists() && Instant::now() < deadline {
            if let Some(status) = child.try_wait().unwrap() {
                panic!("child stopped before {phase}: {status}");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let ready = root.join("child-ready").exists();
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(ready, "child failed to reach {phase}");
        assert!(!root.join("candidate").exists());
        assert_eq!(
            before,
            storage_core_host_tests::tree_hashes(&root.join("source"))
        );
        build_candidate(&options(&root), &mut |_| Ok(())).unwrap();
        let store = AppStore::open_isolated_core(&root.join("candidate/userdata.db")).unwrap();
        assert_eq!(store.list_favors().unwrap().len(), 6);
        assert_eq!(
            before,
            storage_core_host_tests::tree_hashes(&root.join("source"))
        );
    }
}

#[test]
fn core_sqlite_capacity_failure_keeps_content_revisions_and_favorites() {
    let directory = tempfile::tempdir_in(base()).unwrap();
    let root = directory.path().canonicalize().unwrap();
    storage_core_host_tests::seed_documents(&root.join("source"));
    build_candidate(&options(&root), &mut |_| Ok(())).unwrap();
    let path = root.join("candidate/userdata.db");
    let store = AppStore::open_isolated_core(&path).unwrap();
    let old = store.get_clip("same").unwrap();
    let favorites = serde_json::to_string(&store.list_favors().unwrap()).unwrap();
    let pages: i64 = store
        .connection()
        .query_row("PRAGMA page_count", [], |row| row.get(0))
        .unwrap();
    store
        .connection()
        .execute_batch(&format!("PRAGMA max_page_count={pages};"))
        .unwrap();
    let mut changed = old.clone();
    changed.body_text = Some("capacity fixture\r\n".repeat(250_000));
    changed.content_hash = crate::model::hash_text(changed.body_text.as_deref().unwrap());
    let error = store.save_clip(&changed).unwrap_err().to_string();
    assert!(
        error
            .to_ascii_lowercase()
            .contains("database or disk is full"),
        "wrong injected failure: {error}"
    );
    let after = store.get_clip("same").unwrap();
    assert_eq!(old.body_text, after.body_text);
    assert_eq!(old.storage_revision, after.storage_revision);
    assert_eq!(
        favorites,
        serde_json::to_string(&store.list_favors().unwrap()).unwrap()
    );
    drop(store);
    let connection = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        connection
            .query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    drop(connection);
    assert_eq!(
        AppStore::open_isolated_core(&path)
            .unwrap()
            .get_clip("same")
            .unwrap()
            .body_text,
        old.body_text
    );
}
