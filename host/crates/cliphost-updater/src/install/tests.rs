use super::*;
use crate::tests::{make_package, temporary};

fn plan(root: &Path, stage: &Path) -> Plan {
    Plan {
        format: 1,
        token: uuid::Uuid::new_v4().to_string(),
        stage: stage.into(),
        install_root: root.into(),
        executable: root.join(EXE),
        test_fixture: true,
        data_root: root.join("user data"),
        api_port: 29099,
        parent_pid: std::process::id(),
        parent_created: 1,
        parent_sha256: hash_file(&root.join(EXE)).unwrap(),
        old_manifest_sha256: hash_file(&root.join(MANIFEST)).unwrap(),
        new_manifest_sha256: hash_file(&stage.join("payload").join(MANIFEST)).unwrap(),
        old_version: "1.0.0.0".into(),
        new_version: "2.0.0.0".into(),
    }
}

#[test]
fn authorized_replacement_can_restore_every_file_and_preserves_user_data() {
    let temp = temporary();
    let root = temp.path().join("installation");
    let old = make_package(&root, "1.0.0.0", 1);
    let stage = root
        .join(".cliphost-updates")
        .join(uuid::Uuid::new_v4().to_string());
    fs::create_dir_all(&stage).unwrap();
    let new = make_package(&stage.join("payload"), "2.0.0.0", 2);
    fs::write(root.join("userdata.db"), b"original database").unwrap();
    fs::write(root.join("application-data-root.json"), b"original pointer").unwrap();
    let plan = plan(&root, &stage);
    let changes = replace_files(&plan, &old, &new).unwrap();
    assert_eq!(read_manifest(&root).unwrap().version, "2.0.0.0");
    rollback(&plan, &changes).unwrap();
    assert_eq!(read_manifest(&root).unwrap().version, "1.0.0.0");
    assert_eq!(hash_file(&root.join(EXE)).unwrap(), plan.parent_sha256);
    assert_eq!(
        fs::read(root.join("userdata.db")).unwrap(),
        b"original database"
    );
    assert_eq!(
        fs::read(root.join("application-data-root.json")).unwrap(),
        b"original pointer"
    );
}

#[test]
fn mid_install_failure_rolls_back_earlier_replacements() {
    let temp = temporary();
    let root = temp.path().join("installation");
    let old = make_package(&root, "1.0.0.0", 1);
    let stage = root
        .join(".cliphost-updates")
        .join(uuid::Uuid::new_v4().to_string());
    fs::create_dir_all(&stage).unwrap();
    let new = make_package(&stage.join("payload"), "2.0.0.0", 2);
    let plan = plan(&root, &stage);
    // README sorts before executable/manifest; deleting the final runtime
    // forces a real failure after several original files have been renamed.
    fs::remove_file(stage.join("payload/vcruntime140.dll")).unwrap();
    assert!(replace_files(&plan, &old, &new).is_err());
    package::validate_installed(&root.join(EXE), "1.0.0.0").unwrap();
    assert_eq!(
        hash_file(&root.join(MANIFEST)).unwrap(),
        plan.old_manifest_sha256
    );
}

#[test]
fn cancellation_and_missing_authorization_never_accept_process_exit() {
    let temp = temporary();
    let root = temp.path().join("installation");
    make_package(&root, "1.0.0.0", 1);
    let stage = root
        .join(".cliphost-updates")
        .join(uuid::Uuid::new_v4().to_string());
    fs::create_dir_all(&stage).unwrap();
    make_package(&stage.join("payload"), "2.0.0.0", 2);
    let plan = plan(&root, &stage);
    let a = uuid::Uuid::new_v4().to_string();
    let b = uuid::Uuid::new_v4().to_string();
    assert!(wait_for_exit(&plan, &a, || Ok(true), Duration::from_millis(1)).is_err());
    write_new(&marker(&stage, "authorize", &a), plan.token.as_bytes()).unwrap();
    assert!(wait_for_exit(&plan, &a, || Ok(false), Duration::from_millis(1)).is_err());
    assert!(wait_for_exit(&plan, &a, || Ok(true), Duration::from_millis(1)).is_ok());
    write_new(&marker(&stage, "cancel", &a), plan.token.as_bytes()).unwrap();
    assert!(wait_for_exit(&plan, &a, || Ok(true), Duration::from_millis(1)).is_err());
    // A later session cannot inherit the old session's authorization.
    assert!(wait_for_exit(&plan, &b, || Ok(true), Duration::from_millis(1)).is_err());
    assert_eq!(hash_file(&root.join(EXE)).unwrap(), plan.parent_sha256);
}

#[test]
fn launcher_floor_and_original_build_metadata_survive_local_supplement() {
    let temp = temporary();
    let root = temp.path().join("old");
    let payload = temp.path().join("new");
    make_package(&root, "1.0.0.0", 1);
    make_package(&payload, "2.0.0.0", 2);
    let mut old: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join(MANIFEST)).unwrap()).unwrap();
    old["launcherVersion"] = "0.31.27.1".into();
    old["packageName"] = "cea.cliphost.rust".into();
    fs::write(root.join(MANIFEST), serde_json::to_vec(&old).unwrap()).unwrap();
    let mut new: serde_json::Value =
        serde_json::from_slice(&fs::read(payload.join(MANIFEST)).unwrap()).unwrap();
    new["executableBuild"] = serde_json::json!({"sourceCommit":"fixture", "optLevel":3});
    fs::write(payload.join(MANIFEST), serde_json::to_vec(&new).unwrap()).unwrap();
    let before = hash_file(&payload.join(EXE)).unwrap();
    supplement_launcher_contract(&root, &payload).unwrap();
    let result: serde_json::Value =
        serde_json::from_slice(&fs::read(payload.join(MANIFEST)).unwrap()).unwrap();
    assert_eq!(result["launcherVersion"], "0.31.27.1");
    assert_eq!(result["executableBuild"], new["executableBuild"]);
    assert_eq!(result["files"], new["files"]);
    assert_eq!(before, hash_file(&payload.join(EXE)).unwrap());
}

#[test]
fn process_identity_is_pinned_to_path_and_creation_time() {
    let current = std::env::current_exe().unwrap();
    let process = platform::Process::open(std::process::id(), &current, None).unwrap();
    let creation = process.creation_time().unwrap();
    assert!(!process.exited().unwrap());
    assert!(platform::Process::open(std::process::id(), &current, Some(creation + 1)).is_err());
}
