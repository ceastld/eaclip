use super::*;
use crate::data_location::{with_confirmation, ApplyResult, DataLocation};
use std::{cell::Cell, fs};

fn fixture() -> (tempfile::TempDir, HostState, DataLocation) {
    crate::clipboard::force_headless(true);
    let dir = tempfile::tempdir().unwrap();
    let host = HostState::open(AppPaths::from_data_root(dir.path().join("active")), 28965).unwrap();
    let location = DataLocation {
        default_root: Some(dir.path().join("default")),
        program_directory: Some(dir.path().join("program")),
        environment_override: None,
        legacy_sources: Some(Default::default()),
        require_explicit_root: false,
    };
    host.inner.lock().data_location = location.clone();
    (dir, host, location)
}

#[test]
fn data_location_form_cancel_save_reset_and_next_start_preserve_original_data() {
    let (dir, host, location) = fixture();
    let original = host.inner.lock().paths.data_root.clone();
    host.add_text(&json!({"text":"original history"})).unwrap();
    let request =
        json!({"configId":"dataLocation","values":{"dataFolder":dir.path().join("new-data")}});
    let get = || {
        host.settings_form_get(&json!({"configId":"dataLocation"}))
            .unwrap()
    };
    assert_eq!(get()["fields"][0]["value"], "");
    let cancelled = with_confirmation(false, || host.settings_form_update(&request).unwrap());
    assert!(cancelled["messages"][0]["text"]
        .as_str()
        .unwrap()
        .contains("取消"));
    assert!(!location.sidecar_path().unwrap().exists());
    assert!(!dir.path().join("new-data").exists());

    let saved = with_confirmation(true, || host.settings_form_update(&request).unwrap());
    assert_eq!(saved["fields"][0]["value"], request["values"]["dataFolder"]);
    assert_eq!(
        host.data_paths()["sidecarOverridePath"],
        saved["fields"][0]["value"]
    );
    assert_eq!(host.inner.lock().paths.data_root, original);
    assert!(saved["messages"][0]["text"]
        .as_str()
        .unwrap()
        .contains("未执行重启"));
    let reopened =
        HostState::open(AppPaths::from_data_root(location.resolve().unwrap()), 28966).unwrap();
    assert!(reopened
        .inner
        .lock()
        .store
        .lock()
        .list_clips()
        .unwrap()
        .is_empty());
    assert_eq!(
        host.inner.lock().store.lock().list_clips().unwrap().len(),
        1
    );
    let same = with_confirmation(false, || host.settings_form_update(&request).unwrap());
    assert_eq!(same["messages"][0]["text"], "数据位置未更改。");
    let reset = with_confirmation(true, || {
        host.settings_form_update(&json!({
            "configId":"dataLocation","values":{"dataFolder":""}
        }))
        .unwrap()
    });
    assert_eq!(reset["fields"][0]["value"], "");
    assert_eq!(location.resolve().unwrap(), dir.path().join("default"));
    assert_eq!(host.inner.lock().paths.data_root, original);
    assert!(location.sidecar_path().unwrap().exists());
    assert!(location.override_path().unwrap().is_none());
}

#[test]
fn data_location_legacy_button_cancel_switch_and_restart_remember_original_directory() {
    let (dir, host, mut location) = fixture();
    let original = host.inner.lock().paths.data_root.clone();
    host.add_text(&json!({"text":"current history"})).unwrap();
    let legacy = dir
        .path()
        .join("RedirectedDocuments")
        .join("Quicker")
        .join("Ceastld");
    let old_host = HostState::open(AppPaths::from_data_root(legacy.clone()), 28966).unwrap();
    old_host
        .add_text(&json!({"text":"existing old history"}))
        .unwrap();
    drop(old_host);
    location.legacy_sources.as_mut().unwrap().documents_root = Some(legacy.clone());
    host.inner.lock().data_location = location.clone();
    let status = host.data_paths();
    assert_eq!(status["legacyDataRoot"], json!(legacy));
    assert!(status["legacyDataError"].is_null());
    assert_eq!(status["legacyDataRootActive"], false);
    assert!(
        !location.sidecar_path().unwrap().exists(),
        "discovery must be read-only"
    );

    let request = json!({"configId":"dataLocation","values":{"useLegacyDataRoot":true}});
    let cancelled = with_confirmation(false, || host.settings_form_update(&request).unwrap());
    assert!(cancelled["messages"][0]["text"]
        .as_str()
        .unwrap()
        .contains("取消"));
    assert!(!location.sidecar_path().unwrap().exists());
    let saved = with_confirmation(true, || host.settings_form_update(&request).unwrap());
    assert_eq!(saved["fields"][0]["value"], json!(legacy));
    assert_eq!(host.inner.lock().paths.data_root, original);
    assert_eq!(location.resolve().unwrap(), legacy);
    assert_eq!(
        host.inner.lock().store.lock().list_clips().unwrap().len(),
        1
    );

    let reopened =
        HostState::open(AppPaths::from_data_root(location.resolve().unwrap()), 28966).unwrap();
    reopened.inner.lock().data_location = location.clone();
    assert_eq!(
        reopened
            .inner
            .lock()
            .store
            .lock()
            .list_clips()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(reopened.data_paths()["legacyDataRootActive"], true);
    let before = fs::read(location.sidecar_path().unwrap()).unwrap();
    let same = with_confirmation(false, || reopened.settings_form_update(&request).unwrap());
    assert_eq!(same["messages"][0]["text"], "数据位置未更改。");
    assert_eq!(fs::read(location.sidecar_path().unwrap()).unwrap(), before);
    // A changed old candidate cannot change the remembered selection on the next startup.
    location.legacy_sources = Some(Default::default());
    assert_eq!(location.resolve().unwrap(), legacy);
}

#[test]
fn data_location_legacy_button_rechecks_missing_database_and_reports_detection_errors() {
    let (dir, host, mut location) = fixture();
    let request = json!({"configId":"dataLocation","values":{"useLegacyDataRoot":true}});
    let initial = host.data_paths();
    assert!(initial["legacyDataRoot"].is_null());
    assert!(initial["legacyDataError"].is_null());
    assert!(
        with_confirmation(true, || host.settings_form_update(&request))
            .unwrap_err()
            .contains("没有检测到")
    );
    assert!(!location.sidecar_path().unwrap().exists());

    let pointer = dir.path().join("bad-legacy-pointer.json");
    fs::write(&pointer, b"not JSON").unwrap();
    location.legacy_sources.as_mut().unwrap().csharp_sidecar = Some(pointer);
    host.inner.lock().data_location = location.clone();
    let failed = host.data_paths();
    assert!(failed["legacyDataRoot"].is_null());
    assert!(failed["legacyDataError"]
        .as_str()
        .unwrap()
        .contains("旧版数据位置配置"));
    assert_eq!(host.inner.lock().paths.data_root, dir.path().join("active"));
    assert!(!location.sidecar_path().unwrap().exists());

    let legacy = dir.path().join("Documents").join("Quicker").join("Ceastld");
    fs::create_dir_all(&legacy).unwrap();
    fs::write(legacy.join("userdata.db"), b"existing database").unwrap();
    location.legacy_sources = Some(crate::paths::data_bootstrap::LegacyDataSources {
        documents_root: Some(legacy.clone()),
        ..Default::default()
    });
    host.inner.lock().data_location = location.clone();
    assert_eq!(host.data_paths()["legacyDataRoot"], json!(legacy));
    // An explicit Rust override cannot be changed through this shortcut either.
    let mut locked = location.clone();
    locked.environment_override = Some(dir.path().join("active").display().to_string());
    assert!(locked
        .apply(
            &request["values"],
            || panic!("environment override prompted"),
            || panic!("restarted")
        )
        .unwrap_err()
        .contains("环境变量"));
    // The library can disappear after it was shown in the page, or during confirmation.
    fs::remove_file(legacy.join("userdata.db")).unwrap();
    assert!(
        with_confirmation(true, || host.settings_form_update(&request))
            .unwrap_err()
            .contains("没有检测到")
    );
    fs::write(legacy.join("userdata.db"), b"existing database").unwrap();
    assert!(location
        .apply(
            &request["values"],
            || {
                fs::remove_file(legacy.join("userdata.db")).unwrap();
                true
            },
            || panic!("missing library restarted")
        )
        .unwrap_err()
        .contains("没有 userdata.db"));
    assert!(!legacy.join("userdata.db").exists());
    assert!(!location.sidecar_path().unwrap().exists());
}

#[test]
fn data_location_rejects_relative_program_and_gpui_paths_before_confirmation() {
    let (dir, host, location) = fixture();
    let forbidden = [
        PathBuf::from("relative-data"),
        dir.path().join("program"),
        dir.path().join("program/subfolder"),
        dir.path().join("BetterClipRs"),
        dir.path().join("BetterClipRs/userdata"),
    ];
    for folder in forbidden {
        let prompted = Cell::new(false);
        assert!(location
            .apply(
                &json!({"dataFolder":folder}),
                || {
                    prompted.set(true);
                    true
                },
                || panic!("invalid path restarted")
            )
            .is_err());
        assert!(!prompted.get());
        assert!(!location.sidecar_path().unwrap().exists());
    }
    assert!(
        with_confirmation(true, || host.settings_form_update(&json!({
            "configId":"dataLocation","values":{"dataFolder":"relative-data"}
        })))
        .is_err()
    );
    // An explicit environment override fails instead of falling back into another database.
    let mut locked = location.clone();
    locked.environment_override = Some("relative-data".into());
    assert!(locked.resolve().is_err());
    assert!(locked
        .apply(
            &json!({"dataFolder":""}),
            || panic!("environment override prompted"),
            || Ok(true)
        )
        .is_err());
    host.inner.lock().data_location = locked;
    assert_eq!(host.data_paths()["envOverrideActive"], true);
}

#[test]
fn data_location_restart_failure_retains_saved_choice_and_retry_is_explicit() {
    let (dir, _host, location) = fixture();
    let destination = dir.path().join("new-data");
    let error = location
        .apply(
            &json!({"overrideDataRoot":destination}),
            || true,
            || Err("spawn rejected".into()),
        )
        .unwrap_err();
    assert!(error.contains("已保存"));
    assert!(error.contains("spawn rejected"));
    assert_eq!(location.resolve().unwrap(), destination);
    assert_eq!(
        location
            .apply(
                &json!({"DataRoot":destination}),
                || panic!("unchanged request prompted"),
                || panic!("unchanged request restarted")
            )
            .unwrap(),
        ApplyResult::Unchanged
    );
    assert_eq!(
        location
            .apply(&json!({}), || panic!("missing field prompted"), || Ok(true))
            .unwrap(),
        ApplyResult::Unchanged
    );
    let changed = dir.path().join("another-data");
    let result = location
        .apply(
            &json!({"dataFolder":changed}),
            || true,
            || {
                assert_eq!(
                    location.resolve().unwrap(),
                    changed,
                    "restart must see the saved choice"
                );
                Ok(true)
            },
        )
        .unwrap();
    assert_eq!(
        result,
        ApplyResult::Saved {
            restart_requested: true
        }
    );
}

#[test]
fn data_location_unwritable_destination_never_replaces_saved_choice_or_restarts() {
    let (dir, _host, location) = fixture();
    let old = dir.path().join("old-data");
    location.write_override(Some(&old)).unwrap();
    let sidecar = location.sidecar_path().unwrap();
    let before = fs::read(&sidecar).unwrap();
    let file = dir.path().join("a-file");
    fs::write(&file, "keep").unwrap();
    assert!(location
        .apply(
            &json!({"dataFolder":file}),
            || true,
            || panic!("failed write restarted")
        )
        .is_err());
    assert_eq!(fs::read(&sidecar).unwrap(), before);
    assert_eq!(fs::read_to_string(file).unwrap(), "keep");
}

#[test]
#[cfg(windows)]
fn data_location_locked_sidecar_reports_save_and_reset_failures_then_retries() {
    use std::os::windows::fs::OpenOptionsExt;
    let (dir, _host, location) = fixture();
    let old = dir.path().join("old-data");
    location.write_override(Some(&old)).unwrap();
    let sidecar = location.sidecar_path().unwrap();
    let before = fs::read(&sidecar).unwrap();
    let held = fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(&sidecar)
        .unwrap();
    assert!(location
        .apply(
            &json!({"dataFolder":dir.path().join("next-data")}),
            || true,
            || panic!("failed save restarted")
        )
        .is_err());
    assert!(location
        .apply(
            &json!({"dataFolder":""}),
            || true,
            || panic!("failed reset restarted")
        )
        .unwrap_err()
        .contains("恢复默认"));
    assert_eq!(fs::read(&sidecar).unwrap(), before);
    drop(held);
    assert_eq!(
        location
            .apply(&json!({"dataFolder":""}), || true, || Ok(true))
            .unwrap(),
        ApplyResult::Saved {
            restart_requested: true
        }
    );
    assert!(sidecar.exists());
    assert!(location.override_path().unwrap().is_none());
}
