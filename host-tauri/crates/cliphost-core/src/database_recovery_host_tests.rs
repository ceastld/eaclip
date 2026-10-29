use super::*;
use rusqlite::{params, Connection};

fn fixture(eager_projection: bool, pending_journal: bool) -> (tempfile::TempDir, AppPaths) {
    crate::clipboard::force_headless(true);
    let scratch = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
    std::fs::create_dir_all(&scratch).unwrap();
    let root = tempfile::Builder::new().prefix("database-recovery-host-").tempdir_in(scratch).unwrap();
    let paths = AppPaths::from_data_root(root.path().to_path_buf());
    paths.ensure().unwrap();
    let db = Connection::open(&paths.db).unwrap();
    db.execute_batch(
        "CREATE TABLE clip_item(Id TEXT PRIMARY KEY NOT NULL,Data TEXT,ObjectType TEXT,CreatedTime TEXT NOT NULL,UpdateTime TEXT NOT NULL);
         CREATE TABLE favor_item(Id TEXT PRIMARY KEY NOT NULL,Data TEXT,ObjectType TEXT,CreatedTime TEXT NOT NULL,UpdateTime TEXT NOT NULL);
         CREATE TABLE UsageInfo(Id TEXT PRIMARY KEY NOT NULL,Data TEXT,ObjectType TEXT,CreatedTime TEXT NOT NULL,UpdateTime TEXT NOT NULL);",
    ).unwrap();
    for id in ["history-a", "history-b"] {
        let document = json!({"Id":id,"CreateTime":"2026-01-02T03:04:05Z","Item":{"ObjType":1,"Text":id}});
        db.execute("INSERT INTO clip_item VALUES(?1,?2,NULL,'2026-01-02','2026-01-02')", params![id,document.to_string()]).unwrap();
    }
    let folder = json!({"Id":"visible-root","Title":"Visible folder","CreateTime":"2026-01-02T03:04:05Z","Item":{"ObjType":100},"Children":["isolated-root"]});
    db.execute("INSERT INTO favor_item VALUES('visible-root',?1,NULL,'2026-01-02','2026-01-02')", [folder.to_string()]).unwrap();
    db.execute("INSERT INTO favor_item VALUES('isolated-root','{broken',NULL,'2026-01-02','2026-01-02')", []).unwrap();
    std::fs::write(paths.configs.join("FavorConfig.json"), br#"{"RootChildren":["isolated-root","visible-root"],"FontSize":19,"FutureSetting":"keep"}"#).unwrap();
    if eager_projection {
        std::fs::write(paths.configs.join("ClipConfig.json"), br#"{"MaxItemSize":1}"#).unwrap();
    }
    if pending_journal {
        // Deliberately conflicting roots: normal recovery would refuse startup.
        std::fs::write(paths.data_root.join("clip-host-favorite-drag-journal.json"), br#"{"operation_id":"unfinished","before":["before"],"after":["after"]}"#).unwrap();
    }
    (root, paths)
}

fn open(paths: AppPaths) -> HostState {
    let host = HostState::open(paths.clone(), 29065).expect("isolated bad documents must not prevent host startup");
    host.inner.lock().data_location = crate::data_location::DataLocation {
        default_root: Some(paths.data_root.clone()),
        program_directory: None,
        environment_override: Some(paths.data_root.to_string_lossy().into_owned()),
        legacy_sources: Some(Default::default()),
        require_explicit_root: true,
    };
    host
}

fn recovery_toast(host: &HostState) -> Value {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(toast) = host.take_toast_push() {
            return toast;
        }
        assert!(Instant::now() < deadline, "deferred projection must publish its recovery notice");
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn database_recovery_host_starts_with_readable_rows_and_warning_for_eager_and_deferred_projection() {
    for eager in [true, false] {
        let (_root, paths) = fixture(eager, false);
        let host = open(paths.clone());
        let status = host.data_paths()["databaseRecovery"].clone();
        assert_eq!(status["readOnly"], true);
        assert_eq!(status["isolatedRows"], 1);
        assert_eq!(host.store().lock().list_clips().unwrap().len(), 2);
        assert_eq!(host.store().lock().list_favors().unwrap().len(), 1);
        let toast = recovery_toast(&host);
        assert_eq!(toast["kind"], "warning");
        let message = toast["message"].as_str().unwrap();
        assert!(message.contains("只读") || message.contains("read-only"));
        assert!(message.contains("保留") || message.contains("preserved"));
        assert!(message.contains("重启") || message.contains("restart"));
        assert!(host.add_text(&json!({"text":"must not save"})).unwrap_err().contains("DATABASE_RECOVERY_READ_ONLY"));
        host.flush_cleanup_now();
        assert_eq!(host.store().lock().list_clips().unwrap().len(), 2, "startup and exit retention must not prune a partial database");
        let db = Connection::open(&paths.db).unwrap();
        assert_eq!(db.query_row("SELECT Data FROM favor_item WHERE Id='isolated-root'", [], |row| row.get::<_,String>(0)).unwrap(), "{broken");
    }
}

#[test]
fn database_recovery_host_preserves_root_order_and_journal_and_blocks_config_only_structure_changes() {
    let (_root, paths) = fixture(false, true);
    let config_path = paths.configs.join("FavorConfig.json");
    let journal_path = paths.data_root.join("clip-host-favorite-drag-journal.json");
    let config_before = std::fs::read(&config_path).unwrap();
    let journal_before = std::fs::read(&journal_path).unwrap();
    let host = open(paths.clone());
    let tree = host.favor_tree().unwrap();
    assert_eq!(tree["childrenById"]["visible-root"], json!(["isolated-root"]));
    host.panel_catalog().unwrap();
    host.panel_layout().unwrap();
    assert!(host.try_reorder_root_children(&["visible-root".into()]).unwrap_err().contains("DATABASE_RECOVERY_READ_ONLY"));
    assert!(host.favor_remove_missing_reference(&json!({"parentId":"visible-root","childId":"isolated-root"})).unwrap_err().contains("DATABASE_RECOVERY_READ_ONLY"));
    assert_eq!(std::fs::read(&config_path).unwrap(), config_before, "displaying the partial graph must not normalize away isolated roots");
    assert_eq!(std::fs::read(&journal_path).unwrap(), journal_before, "pending journal must be preserved until the complete graph can be recovered");
}

#[cfg(windows)]
mod runtime_hot_journal {
    use super::*;
    use std::process::{Command, Stdio};

    const CHILD_ROOT_ENV: &str = "CLIPHOST_RUNTIME_HOT_JOURNAL_FIXTURE_ROOT";
    const MARKER: &[u8] = b"cliphost-runtime-synthetic-hot-journal-v1";
    const ANCHOR_BYTES: usize = 2 * 1024 * 1024;
    const SCHEMA_QUERY: &str = "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND lower(name) IN ('store_info','storage_v2_candidate'))";

    fn fixture_parent() -> PathBuf {
        let parent = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../..")
            .canonicalize().unwrap()
            .join(".tmp/runtime-hot-journal-tests");
        std::fs::create_dir_all(&parent).unwrap();
        parent
    }

    fn hot_fixture(unsupported: bool) -> (tempfile::TempDir, AppPaths) {
        crate::clipboard::force_headless(true);
        let root = tempfile::Builder::new().prefix("runtime-")
            .tempdir_in(fixture_parent()).unwrap();
        let paths = AppPaths::from_data_root(root.path().to_path_buf());
        paths.ensure().unwrap();
        std::fs::write(root.path().join("synthetic-crash-fixture.marker"), MARKER).unwrap();
        std::fs::write(paths.configs.join("ClipConfig.json"), br#"{"MaxItemSize":10000}"#).unwrap();
        // This old filter would normally receive a URL tab and migration marker.
        std::fs::write(paths.configs.join("ClipFilterConfig.json"),
            br#"{"FilterItems":[{"Id":"original-text","DisplayName":"original","Type":"text","ItemType":2,"StarType":0}],"FutureNumber":123456789012345678901234567890}"#).unwrap();
        let db = Connection::open(&paths.db).unwrap();
        db.execute_batch(
            "PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;
             CREATE TABLE clip_item(Id TEXT PRIMARY KEY NOT NULL,Data TEXT,ObjectType TEXT,CreatedTime TEXT NOT NULL,UpdateTime TEXT NOT NULL);
             CREATE TABLE favor_item(Id TEXT PRIMARY KEY NOT NULL,Data TEXT,ObjectType TEXT,CreatedTime TEXT NOT NULL,UpdateTime TEXT NOT NULL);
             CREATE TABLE UsageInfo(Id TEXT PRIMARY KEY NOT NULL,Data TEXT,ObjectType TEXT,CreatedTime TEXT NOT NULL,UpdateTime TEXT NOT NULL);
             CREATE TABLE recovery_anchor(id INTEGER PRIMARY KEY,payload TEXT NOT NULL);"
        ).unwrap();
        let document = json!({"Id":"committed-history","CreateTime":"2026-01-02T03:04:05Z","Item":{"ObjType":1,"Text":"committed text"}});
        db.execute("INSERT INTO clip_item VALUES('committed-history',?1,NULL,'2026-01-02','2026-01-02')", [document.to_string()]).unwrap();
        // Allocate committed main-file pages; overwriting them must spill a
        // genuine recovery journal, rather than only append uncommitted pages.
        db.execute("INSERT INTO recovery_anchor VALUES(1,?1)", ["A".repeat(ANCHOR_BYTES)]).unwrap();
        if unsupported {
            db.execute_batch("CREATE TABLE store_info(marker TEXT); INSERT INTO store_info VALUES('unsupported-runtime');").unwrap();
        }
        drop(db);
        (root, paths)
    }

    fn leave_hot_journal(root: &Path) {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "host::database_recovery_host_tests::runtime_hot_journal::runtime_hot_journal_crash_child", "--ignored", "--nocapture"])
            .env(CHILD_ROOT_ENV, root)
            .stdout(Stdio::piped()).stderr(Stdio::piped())
            .spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if child.try_wait().unwrap().is_some() {
                let output = child.wait_with_output().unwrap();
                assert_eq!(output.status.code(), Some(77), "crash fixture failed:\n{}\n{}",
                    String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
                break;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let output = child.wait_with_output().unwrap();
                panic!("crash fixture timed out:\n{}\n{}",
                    String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
            }
            thread::sleep(Duration::from_millis(20));
        }
        let journal = std::fs::read(root.join("userdata.db-journal")).unwrap();
        assert!(journal.len() > 512);
        assert_eq!(&journal[..8], &[0xd9, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7]);
    }

    fn assert_old_readonly_probe_fails(db_path: &Path) {
        let db = Connection::open_with_flags(db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX).unwrap();
        let error = db.query_row(SCHEMA_QUERY, [], |row| row.get::<_, bool>(0)).unwrap_err();
        match error {
            rusqlite::Error::SqliteFailure(code, message) => {
                assert_eq!(code.extended_code, 776, "fixture must need rollback, not a permissions approximation");
                assert_eq!(message.as_deref(), Some("attempt to write a readonly database"));
            }
            other => panic!("unexpected readonly probe error: {other:?}"),
        }
    }

    fn assert_committed_data(db: &Connection) {
        let anchor: String = db.query_row("SELECT payload FROM recovery_anchor WHERE id=1", [], |row| row.get(0)).unwrap();
        assert_eq!(anchor, "A".repeat(ANCHOR_BYTES), "uncommitted cache-spill bytes must be rolled back");
        let raw: String = db.query_row("SELECT Data FROM clip_item WHERE Id='committed-history'", [], |row| row.get(0)).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&raw).unwrap()["Item"]["Text"], "committed text");
        let integrity: String = db.query_row("PRAGMA integrity_check", [], |row| row.get(0)).unwrap();
        assert_eq!(integrity, "ok");
    }

    #[test]
    #[ignore = "only entered by the guarded synthetic hot-journal subprocess tests"]
    fn runtime_hot_journal_crash_child() {
        let root = PathBuf::from(std::env::var_os(CHILD_ROOT_ENV).expect("explicit synthetic fixture root is required"));
        assert!(root.is_absolute());
        let root = root.canonicalize().unwrap();
        let allowed = fixture_parent().canonicalize().unwrap();
        assert!(root != allowed && root.starts_with(&allowed), "child may only touch its repository .tmp fixture");
        assert_eq!(std::fs::read(root.join("synthetic-crash-fixture.marker")).unwrap(), MARKER);
        let db = Connection::open(root.join("userdata.db")).unwrap();
        db.execute_batch(
            "PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;
             PRAGMA cache_size=5; PRAGMA cache_spill=ON;
             BEGIN IMMEDIATE;
             UPDATE recovery_anchor SET payload=zeroblob(2097152) WHERE id=1;"
        ).unwrap();
        assert_eq!(db.changes(), 1);
        let journal = std::fs::read(root.join("userdata.db-journal")).unwrap();
        assert!(journal.len() > 512);
        assert_eq!(&journal[..8], &[0xd9, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7]);
        // Bypass Connection::drop so the parent receives dirty main pages and
        // their real SQLite rollback journal, with no live writer remaining.
        std::process::exit(77);
    }

    #[test]
    fn runtime_hot_journal_host_recovers_committed_data_before_schema_check() {
        let (root, paths) = hot_fixture(false);
        let committed_main = std::fs::read(&paths.db).unwrap();
        leave_hot_journal(root.path());
        assert_ne!(std::fs::read(&paths.db).unwrap(), committed_main, "fixture must dirty committed main-file pages");
        assert_old_readonly_probe_fails(&paths.db);
        let host = HostState::open(paths.clone(), 29065).expect("normal startup must permit SQLite rollback recovery");
        assert_eq!(host.store().lock().get_clip("committed-history").unwrap().body_text.as_deref(), Some("committed text"));
        assert_committed_data(&Connection::open(&paths.db).unwrap());
    }

    #[test]
    fn runtime_hot_journal_unsupported_store_still_rejects_before_business_migrations() {
        let (root, paths) = hot_fixture(true);
        let config_path = paths.configs.join("ClipFilterConfig.json");
        let config_before = std::fs::read(&config_path).unwrap();
        let db = Connection::open(&paths.db).unwrap();
        let schema_before: String = db.query_row("SELECT group_concat(sql, ';') FROM (SELECT sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY name)", [], |row| row.get(0)).unwrap();
        drop(db);
        leave_hot_journal(root.path());
        assert_old_readonly_probe_fails(&paths.db);
        let error = match HostState::open(paths.clone(), 29065) {
            Ok(_) => panic!("an unsupported store must remain rejected after SQLite recovery"),
            Err(error) => error,
        };
        assert!(error.contains("STORAGE_V2_RUNTIME_NOT_READY"), "{error}");
        assert_eq!(std::fs::read(&config_path).unwrap(), config_before, "format rejection must precede filter compatibility writes");
        let db = Connection::open_with_flags(&paths.db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let schema_after: String = db.query_row("SELECT group_concat(sql, ';') FROM (SELECT sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY name)", [], |row| row.get(0)).unwrap();
        assert_eq!(schema_after, schema_before, "format rejection must precede AppStore DDL and auxiliary tables");
        assert_committed_data(&db);
    }

    #[test]
    fn runtime_hot_journal_readonly_file_keeps_recovery_files_and_reports_context() {
        let (root, paths) = hot_fixture(false);
        leave_hot_journal(root.path());
        let journal_path = root.path().join("userdata.db-journal");
        let database_before = std::fs::read(&paths.db).unwrap();
        let journal_before = std::fs::read(&journal_path).unwrap();
        let original = std::fs::metadata(&paths.db).unwrap().permissions();
        let mut readonly = original.clone();
        readonly.set_readonly(true);
        std::fs::set_permissions(&paths.db, readonly).unwrap();
        let result = HostState::open(paths.clone(), 29065);
        std::fs::set_permissions(&paths.db, original).unwrap();
        let error = match result {
            Ok(_) => panic!("a write-protected hot journal database cannot be recovered"),
            Err(error) => error,
        };
        assert!(error.contains("extended_code=776"), "{error}");
        assert!(error.contains(paths.db.to_string_lossy().as_ref()), "{error}");
        assert!(error.contains("[schema]"), "{error}");
        assert_eq!(std::fs::read(&paths.db).unwrap(), database_before);
        assert_eq!(std::fs::read(&journal_path).unwrap(), journal_before);
    }

    #[test]
    fn runtime_hot_journal_clean_readonly_file_reports_initialize_context() {
        let (_root, paths) = hot_fixture(false);
        let database_before = std::fs::read(&paths.db).unwrap();
        let original = std::fs::metadata(&paths.db).unwrap().permissions();
        let mut readonly = original.clone();
        readonly.set_readonly(true);
        std::fs::set_permissions(&paths.db, readonly).unwrap();
        let result = HostState::open(paths.clone(), 29065);
        std::fs::set_permissions(&paths.db, original).unwrap();
        let error = match result {
            Ok(_) => panic!("missing auxiliary tables cannot be initialized in a write-protected database"),
            Err(error) => error,
        };
        assert!(error.contains("[initialize]"), "{error}");
        assert!(error.contains("extended_code=8"), "{error}");
        assert!(error.contains(paths.db.to_string_lossy().as_ref()), "{error}");
        assert_eq!(std::fs::read(&paths.db).unwrap(), database_before);
        assert_committed_data(&Connection::open(&paths.db).unwrap());
    }
}
