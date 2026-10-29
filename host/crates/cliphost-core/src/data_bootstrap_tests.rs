use super::*;
use crate::data_location::DataLocation;
use serde_json::json;

fn fixture() -> (tempfile::TempDir, DataLocation, LegacyDataSources) {
    let dir = tempfile::tempdir().unwrap();
    let local = dir.path().join("LocalAppData");
    let sources = LegacyDataSources {
        environment_override: None,
        csharp_sidecar: Some(local.join("BetterClip/application-data-root.json")),
        documents_root: Some(dir.path().join("RedirectedDocuments/Quicker/Ceastld")),
        csharp_default: Some(local.join("BetterClip/userdata")),
        documents_from_system: false,
    };
    let location = DataLocation {
        default_root: Some(local.join("BetterClipTauri")),
        program_directory: Some(local.join("Programs/ClipHostRust")),
        environment_override: None,
        legacy_sources: Some(sources.clone()),
        require_explicit_root: false,
    };
    (dir, location, sources)
}

fn database(root: &Path, bytes: &[u8]) {
    fs::create_dir_all(root).unwrap();
    fs::write(root.join("userdata.db"), bytes).unwrap();
}

fn pointer(path: &Path, value: serde_json::Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, value.to_string()).unwrap();
}

#[test]
fn bootstrap_prefers_release_custom_root_and_preserves_existing_rust_data() {
    let (dir, location, sources) = fixture();
    let custom = dir.path().join("自定义-data");
    database(&custom, b"existing C# data");
    database(
        sources.documents_root.as_ref().unwrap(),
        b"older document data",
    );
    database(sources.csharp_default.as_ref().unwrap(), b"other C# data");
    database(
        location.default_root.as_ref().unwrap(),
        b"existing Rust data",
    );
    pointer(
        sources.csharp_sidecar.as_ref().unwrap(),
        json!({"DataRoot":custom}),
    );
    let pointer_path = sources.csharp_sidecar.as_ref().unwrap();
    let mut bom_pointer = vec![0xef, 0xbb, 0xbf];
    bom_pointer.extend(fs::read(pointer_path).unwrap());
    fs::write(pointer_path, &bom_pointer).unwrap();
    assert_eq!(location.resolve().unwrap(), custom);
    assert_eq!(location.override_path().unwrap(), Some(custom.clone()));
    assert_eq!(
        fs::read(custom.join("userdata.db")).unwrap(),
        b"existing C# data"
    );
    assert_eq!(
        fs::read(location.default_root.as_ref().unwrap().join("userdata.db")).unwrap(),
        b"existing Rust data"
    );
    assert_eq!(fs::read(pointer_path).unwrap(), bom_pointer);
    // A later pointer change never silently changes the already selected database.
    pointer(
        sources.csharp_sidecar.as_ref().unwrap(),
        json!({"dataRoot":sources.csharp_default}),
    );
    assert_eq!(location.resolve().unwrap(), custom);
}

#[test]
fn bootstrap_uses_redirected_documents_and_persists_choice_without_moving_files() {
    let (_dir, location, sources) = fixture();
    let documents = sources.documents_root.as_ref().unwrap();
    database(documents, b"document database");
    database(
        sources.csharp_default.as_ref().unwrap(),
        b"fallback database",
    );
    assert_eq!(location.resolve().unwrap(), *documents);
    let saved = fs::read(location.sidecar_path().unwrap()).unwrap();
    assert_eq!(
        fs::read(documents.join("userdata.db")).unwrap(),
        b"document database"
    );
    assert!(!location
        .default_root
        .as_ref()
        .unwrap()
        .join("userdata.db")
        .exists());
    fs::remove_file(documents.join("userdata.db")).unwrap();
    assert_eq!(location.resolve().unwrap(), *documents);
    assert_eq!(fs::read(location.sidecar_path().unwrap()).unwrap(), saved);
}

#[test]
fn bootstrap_only_selects_database_files_and_remembers_default_after_first_start() {
    let (_dir, location, sources) = fixture();
    fs::create_dir_all(sources.documents_root.as_ref().unwrap().join("userdata.db")).unwrap();
    database(
        sources.csharp_default.as_ref().unwrap(),
        b"C# default database",
    );
    assert_eq!(
        location.resolve().unwrap(),
        *sources.csharp_default.as_ref().unwrap()
    );
    let (_dir, location, sources) = fixture();
    let default = location.default_root.as_ref().unwrap();
    assert_eq!(location.resolve().unwrap(), *default);
    assert!(location.override_path().unwrap().is_none());
    database(
        sources.documents_root.as_ref().unwrap(),
        b"later legacy database",
    );
    assert_eq!(location.resolve().unwrap(), *default);
    assert!(!default.join("userdata.db").exists());
}

#[test]
fn bootstrap_explicit_rust_environment_and_pointer_aliases_win_before_legacy_probe() {
    let (dir, mut location, sources) = fixture();
    pointer(
        sources.csharp_sidecar.as_ref().unwrap(),
        json!({"broken":true}),
    );
    let explicit = dir.path().join("explicit-rust");
    location.environment_override = Some(explicit.to_string_lossy().into_owned());
    assert_eq!(location.resolve().unwrap(), explicit);
    assert!(!location.sidecar_path().unwrap().exists());
    location.environment_override = None;
    for alias in ["DataRoot", "dataRoot", "path"] {
        pointer(&location.sidecar_path().unwrap(), json!({alias:explicit}));
        let sidecar = location.sidecar_path().unwrap();
        let mut bom_pointer = vec![0xef, 0xbb, 0xbf];
        bom_pointer.extend(fs::read(&sidecar).unwrap());
        fs::write(&sidecar, &bom_pointer).unwrap();
        assert_eq!(location.resolve().unwrap(), explicit);
        assert_eq!(fs::read(sidecar).unwrap(), bom_pointer);
    }
}

#[test]
fn bootstrap_legacy_environment_has_priority_and_invalid_choice_never_falls_back() {
    let (dir, mut location, mut sources) = fixture();
    let legacy_environment = dir.path().join("legacy-environment");
    let custom = dir.path().join("custom");
    database(&legacy_environment, b"environment database");
    database(&custom, b"other old database");
    pointer(
        sources.csharp_sidecar.as_ref().unwrap(),
        json!({"dataRoot":custom}),
    );
    sources.environment_override = Some(legacy_environment.clone());
    location.legacy_sources = Some(sources.clone());
    assert_eq!(location.resolve().unwrap(), legacy_environment);
    sources.environment_override = Some(dir.path().join("missing"));
    location.legacy_sources = Some(sources.clone());
    // A saved Rust choice remains authoritative even when the old environment changes.
    assert_eq!(location.resolve().unwrap(), legacy_environment);
    fs::remove_file(location.sidecar_path().unwrap()).unwrap();
    let rust_environment = dir.path().join("explicit-rust");
    location.environment_override = Some(rust_environment.to_string_lossy().into_owned());
    assert_eq!(location.resolve().unwrap(), rust_environment);
    location.environment_override = None;
    for unavailable in [
        dir.path().join("missing"),
        PathBuf::from("relative-old-data"),
    ] {
        sources.environment_override = Some(unavailable);
        location.legacy_sources = Some(sources.clone());
        let error = location.resolve().unwrap_err();
        assert!(error.contains("BETTERCLIP_DATA_ROOT"), "{error}");
        assert!(error.contains("清除"), "{error}");
        assert!(!location.sidecar_path().unwrap().exists());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        sources.environment_override = Some(legacy_environment.clone());
        location.legacy_sources = Some(sources.clone());
        let held = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(legacy_environment.join("userdata.db"))
            .unwrap();
        assert!(location
            .resolve()
            .unwrap_err()
            .contains("BETTERCLIP_DATA_ROOT"));
        assert!(!location.sidecar_path().unwrap().exists());
        drop(held);
    }
    assert_eq!(
        fs::read(custom.join("userdata.db")).unwrap(),
        b"other old database"
    );
    assert_eq!(
        fs::read(legacy_environment.join("userdata.db")).unwrap(),
        b"environment database"
    );
}

#[test]
fn bootstrap_corrupt_rust_pointer_fails_instead_of_opening_another_database() {
    let (dir, location, sources) = fixture();
    database(
        sources.documents_root.as_ref().unwrap(),
        b"available legacy database",
    );
    fs::create_dir_all(location.default_root.as_ref().unwrap()).unwrap();
    for value in [
        "{broken".to_owned(),
        "{}".to_owned(),
        json!({"dataRoot":"relative"}).to_string(),
        json!({"dataRoot":null}).to_string(),
        json!({"dataRoot":dir.path().join("custom"),"useDefault":true}).to_string(),
    ] {
        let sidecar = location.sidecar_path().unwrap();
        fs::write(&sidecar, value.as_bytes()).unwrap();
        assert!(location.resolve().is_err(), "invalid pointer {value}");
        assert_eq!(fs::read(&sidecar).unwrap(), value.as_bytes());
    }
    assert!(!location
        .default_root
        .as_ref()
        .unwrap()
        .join("userdata.db")
        .exists());
}

#[test]
fn bootstrap_cannot_use_legacy_root_when_persisting_its_choice_fails() {
    let (_dir, location, sources) = fixture();
    database(
        sources.documents_root.as_ref().unwrap(),
        b"untouched source",
    );
    let default = location.default_root.as_ref().unwrap();
    fs::create_dir_all(default.parent().unwrap()).unwrap();
    fs::write(default, b"blocks sidecar directory").unwrap();
    assert!(location.resolve().is_err());
    assert_eq!(
        fs::read(sources.documents_root.as_ref().unwrap().join("userdata.db")).unwrap(),
        b"untouched source"
    );
    assert_eq!(fs::read(default).unwrap(), b"blocks sidecar directory");
}

#[test]
fn bootstrap_ignores_debug_pointer_and_does_not_create_old_data_directories() {
    let (dir, location, sources) = fixture();
    let debug_root = dir.path().join("debug-data");
    database(&debug_root, b"development database");
    let debug_pointer = sources
        .csharp_sidecar
        .as_ref()
        .unwrap()
        .with_file_name("application-data-root.debug.json");
    pointer(&debug_pointer, json!({"dataRoot":debug_root}));
    let original = fs::read(&debug_pointer).unwrap();
    assert_eq!(
        location.resolve().unwrap(),
        *location.default_root.as_ref().unwrap()
    );
    assert!(!sources.documents_root.as_ref().unwrap().exists());
    assert!(!sources.csharp_default.as_ref().unwrap().exists());
    assert_eq!(
        fs::read(debug_root.join("userdata.db")).unwrap(),
        b"development database"
    );
    assert_eq!(fs::read(debug_pointer).unwrap(), original);
}

#[test]
fn bootstrap_reset_keeps_explicit_default_and_acceptance_requires_environment_root() {
    let (dir, mut location, sources) = fixture();
    database(sources.documents_root.as_ref().unwrap(), b"legacy database");
    location.resolve().unwrap();
    location.write_override(None).unwrap();
    assert!(location.override_path().unwrap().is_none());
    assert_eq!(
        location.resolve().unwrap(),
        *location.default_root.as_ref().unwrap()
    );
    let saved = fs::read(location.sidecar_path().unwrap()).unwrap();
    location.require_explicit_root = true;
    assert!(location.resolve().unwrap_err().contains("隔离测试"));
    assert_eq!(fs::read(location.sidecar_path().unwrap()).unwrap(), saved);
    let isolated = dir.path().join("isolated");
    location.environment_override = Some(isolated.to_string_lossy().into_owned());
    assert_eq!(location.resolve().unwrap(), isolated);
    let unit_environment = DataLocation::from_environment();
    let disabled_sources = unit_environment.legacy_sources.unwrap();
    assert!(disabled_sources.environment_override.is_none());
    assert!(disabled_sources.csharp_sidecar.is_none());
    assert!(disabled_sources.documents_root.is_none());
    assert!(disabled_sources.csharp_default.is_none());
    assert!(!disabled_sources.documents_from_system);
}
