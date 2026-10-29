use super::*;
use std::{collections::BTreeMap, fs, path::Path};

fn fixture() -> (tempfile::TempDir, HostState) {
    crate::clipboard::force_headless(true);
    let dir = tempfile::tempdir().unwrap();
    let host = HostState::open(
        AppPaths::from_data_root(dir.path().join("BetterClipTauri")),
        28965,
    )
    .unwrap();
    host.config_update(&json!({})).unwrap();
    (dir, host)
}

fn memory(host: &HostState) -> Value {
    let inner = host.inner.lock();
    json!({"clip":inner.config,"filters":inner.filters,"global":inner.extras.global,
        "quickPaste":inner.qp_config,"hotkeys":inner.hotkeys,"favor":inner.extras.favor,
        "monitor":inner.monitor_on.load(Ordering::Relaxed),"duplicate":inner.allow_dup.load(Ordering::Relaxed),
        "expression":*inner.text_process_expr.lock(),"blacklist":*inner.blacklist.lock()})
}

fn files(directory: &Path) -> BTreeMap<String, Vec<u8>> {
    fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_file())
        .map(|path| {
            (
                path.file_name().unwrap().to_string_lossy().into_owned(),
                fs::read(path).unwrap(),
            )
        })
        .collect()
}

fn while_blocked(path: &Path, work: impl FnOnce()) {
    let backup = path.with_extension("fixture-backup");
    let existed = path.exists();
    if existed {
        fs::rename(path, &backup).unwrap();
    }
    fs::create_dir(path).unwrap();
    work();
    fs::remove_dir(path).unwrap();
    if existed {
        fs::rename(backup, path).unwrap();
    }
}

#[test]
fn auto_scroll_to_top_list_presentation_requires_an_expanded_visible_window() {
    let (_dir, host) = fixture();
    for (visible, collapsed, edge_tucked, expected) in [
        (true, false, false, true),
        (false, false, false, false),
        (true, true, false, false),
        (true, false, true, false),
    ] {
        {
            let mut inner = host.inner.lock();
            inner.window_visible = visible;
            inner.is_collapsed = collapsed;
            inner.edge_tucked = edge_tucked;
        }
        assert_eq!(host.native_clipboard_list_presented(), expected);
    }
}

#[test]
fn auto_scroll_to_top_defaults_and_pascal_case_compatibility() {
    let old: ClipConfigDto = serde_json::from_value(json!({})).unwrap();
    assert!(!old.auto_scroll_to_top);
    assert_eq!(old.auto_scroll_to_top_idle_seconds, 60);
    let enabled: ClipConfigDto = serde_json::from_value(json!({
        "AutoScrollToTop": true, "AutoScrollToTopIdleSeconds": 125
    })).unwrap();
    assert!(enabled.auto_scroll_to_top);
    assert_eq!(enabled.auto_scroll_to_top_idle_seconds, 125);
    let wire = serde_json::to_value(enabled).unwrap();
    assert_eq!(wire["autoScrollToTop"], true);
    assert_eq!(wire["autoScrollToTopIdleSeconds"], 125);
}

#[test]
fn auto_scroll_to_top_load_normalizes_idle_time_and_keeps_old_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let paths = AppPaths::from_data_root(dir.path().join("data"));
    fs::create_dir_all(&paths.configs).unwrap();
    let path = paths.configs.join("ClipConfig.json");
    fs::write(&path, br#"{"MaxItemSize":100}"#).unwrap();
    let old = load_config(&paths).unwrap();
    assert!(!old.auto_scroll_to_top);
    assert_eq!(old.auto_scroll_to_top_idle_seconds, 60);
    for (stored, expected) in [(0, 1), (86_401, 86_400), (125, 125)] {
        fs::write(&path, json!({"MaxItemSize":100, "AutoScrollToTop":true,
            "AutoScrollToTopIdleSeconds":stored}).to_string()).unwrap();
        let loaded = load_config(&paths).unwrap();
        assert!(loaded.auto_scroll_to_top);
        assert_eq!(loaded.auto_scroll_to_top_idle_seconds, expected);
    }
}

#[test]
fn auto_scroll_to_top_list_and_settings_updates_persist_and_survive_reopen() {
    let (_dir, host) = fixture();
    host.drain_push_events();
    host.config_update(&json!({"autoScrollToTop":true, "autoScrollToTopIdleSeconds":125})).unwrap();
    let event = host.drain_push_events().into_iter().find(|(name, _)| name == "clipConfigChanged").unwrap().1;
    assert_eq!(event["autoScrollToTop"], true);
    assert_eq!(event["autoScrollToTopIdleSeconds"], 125);
    let paths = host.inner.lock().paths.clone();
    let saved: Value = serde_json::from_slice(&fs::read(paths.configs.join("ClipConfig.json")).unwrap()).unwrap();
    assert_eq!(saved["autoScrollToTop"], true);
    assert_eq!(saved["autoScrollToTopIdleSeconds"], 125);
    drop(host);
    let reopened = HostState::open(paths.clone(), 28965).unwrap();
    assert_eq!(reopened.config_get()["autoScrollToTop"], true);
    assert_eq!(reopened.config_get()["autoScrollToTopIdleSeconds"], 125);
    reopened.settings_form_update(&json!({"configId":"clip", "values":{
        "AutoScrollToTop":false, "AutoScrollToTopIdleSeconds":0
    }})).unwrap();
    assert_eq!(reopened.config_get()["autoScrollToTop"], false);
    assert_eq!(reopened.config_get()["autoScrollToTopIdleSeconds"], 1);
    reopened.config_update(&json!({"AutoScrollToTop":true, "AutoScrollToTopIdleSeconds":u64::MAX})).unwrap();
    assert_eq!(reopened.config_get()["autoScrollToTopIdleSeconds"], 86_400);
    reopened.config_update(&json!({"autoScrollToTopIdleSeconds":-5})).unwrap();
    drop(reopened);
    let reopened = HostState::open(paths, 28965).unwrap();
    assert_eq!(reopened.config_get()["autoScrollToTop"], true);
    assert_eq!(reopened.config_get()["autoScrollToTopIdleSeconds"], 1);
}

#[test]
fn clip_filter_optional_names_survive_settings_save_and_host_reopen() {
    crate::clipboard::force_headless(true);
    let temporary_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
    fs::create_dir_all(&temporary_root).unwrap();
    let dir = tempfile::Builder::new()
        .prefix("clip-filter-names-")
        .tempdir_in(temporary_root)
        .unwrap();
    let paths = AppPaths::from_data_root(dir.path().join("data"));
    let host = HostState::open(paths.clone(), 28965).unwrap();
    assert!(host
        .filters_get()
        .as_array()
        .unwrap()
        .iter()
        .all(|tab| tab["displayName"] == ""));
    let rows = json!([
        {"id":"auto", "type":"image", "itemType":4, "displayName":""},
        {"id":"old", "type":"all", "itemType":1, "displayName":"默认"},
        {"id":"custom", "type":"file", "itemType":8, "displayName":"图片"},
        {"id":"named-pinned", "type":"text", "itemType":2, "displayName":"置顶"}
    ]);
    host.settings_form_update(&json!({
        "configId":"clip", "values":{"FilterItems":rows.to_string()}
    }))
    .unwrap();
    let saved: Value =
        serde_json::from_slice(&fs::read(paths.configs.join("ClipFilterConfig.json")).unwrap())
            .unwrap();
    let expected = host.filters_get();
    assert_eq!(saved["FilterItems"], expected);
    assert_eq!(expected[0]["displayName"], "");
    assert_eq!(expected[1]["displayName"], "默认");
    assert_eq!(expected[2]["displayName"], "图片");
    assert_eq!(expected[2]["type"], "file");
    assert_eq!(expected[3]["displayName"], "置顶");
    drop(host);

    let reopened = HostState::open(paths.clone(), 28965).unwrap();
    assert_eq!(reopened.filters_get(), expected);
    let form = reopened
        .settings_form_get(&json!({"configId":"clip"}))
        .unwrap();
    let field = form["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|field| field["key"] == "FilterItems")
        .unwrap();
    let form_rows = crate::clip_filter::parse_form_value(&field["value"]);
    assert_eq!(serde_json::to_value(form_rows).unwrap(), expected);

    let mut changed = expected;
    changed[0]["type"] = json!("richtext");
    changed[1]["type"] = json!("image");
    changed[2]["displayName"] = json!("");
    reopened
        .settings_form_update(&json!({
            "configId":"clip", "values":{"FilterItems":changed}
        }))
        .unwrap();
    drop(reopened);
    let reopened = HostState::open(paths, 28965).unwrap();
    let reloaded = reopened.filters_get();
    assert_eq!(reloaded[0]["displayName"], "");
    assert_eq!(reloaded[0]["type"], "richtext");
    assert_eq!(reloaded[0]["itemType"], 64);
    assert_eq!(reloaded[1]["displayName"], "默认");
    assert_eq!(reloaded[1]["type"], "image");
    assert_eq!(reloaded[2]["displayName"], "");
    assert_eq!(reloaded[2]["type"], "file");
}

#[test]
fn persisted_clip_config_pushes_current_values_and_failed_save_does_not_publish() {
    let (_dir, host) = fixture();
    host.drain_push_events();
    host.config_update(&json!({"usingNumberKeyToPaste": false})).unwrap();
    let events = host.drain_push_events();
    let updates: Vec<_> = events.iter().filter(|(name, _)| name == "clipConfigChanged").collect();
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].1["usingNumberKeyToPaste"], false);
    let path = host.inner.lock().paths.configs.join("ClipConfig.json");
    while_blocked(&path, || {
        assert!(host.config_update(&json!({"usingNumberKeyToPaste": true})).is_err());
        assert!(!host.drain_push_events().iter().any(|(name, _)| name == "clipConfigChanged"));
        assert_eq!(host.config_get()["usingNumberKeyToPaste"], false);
    });
    host.config_update(&json!({"usingNumberKeyToPaste": true})).unwrap();
    let events = host.drain_push_events();
    let update = events.iter().find(|(name, _)| name == "clipConfigChanged").unwrap();
    assert_eq!(update.1["usingNumberKeyToPaste"], true);
    host.settings_form_update(&json!({
        "configId": "clip", "values": {"UsingNumberKeyToPaste": false}
    })).unwrap();
    let events = host.drain_push_events();
    let update = events.iter().find(|(name, _)| name == "clipConfigChanged").unwrap();
    assert_eq!(update.1["usingNumberKeyToPaste"], false);
}

#[test]
fn execution_alignment_global_appearance_publishes_only_committed_config() {
    let (_dir, host) = fixture();
    let configs = host.inner.lock().paths.configs.clone();
    for (id, image) in [("global", "first.png"), ("homeGlobal", "second.png")] {
        host.drain_push_events();
        host.settings_form_update(&json!({"configId": id, "values": {
            "UiThemeMode": "Custom", "WindowBackground": "#FF204060",
            "FontForeground": "#FFE0E8F0", "WindowBackgroundImage": image,
            "WindowBackgroundBlend": 72, "WindowBackgroundFrostedGlass": false
        }})).unwrap();
        let current = serde_json::to_value(&host.inner.lock().config).unwrap();
        let events = host.drain_push_events();
        let updates = events.iter().filter(|(name, _)| name == "clipConfigChanged").collect::<Vec<_>>();
        assert_eq!(updates.len(), 1, "{id}");
        assert_eq!(updates[0].1, current, "{id}: all windows receive the full committed config");
        assert_eq!(current["uiThemeMode"], "custom");
        assert_eq!(current["windowBackground"], "#FF204060");
        assert_eq!(current["fontForeground"], "#FFE0E8F0");
        assert_eq!(current["windowBackgroundImage"], image);
        assert_eq!(current["windowBackgroundBlend"], 72);
        assert_eq!(current["windowBackgroundFrostedGlass"], false);
        let persisted: Value = serde_json::from_slice(&fs::read(configs.join("ClipConfig.json")).unwrap()).unwrap();
        for key in ["uiThemeMode", "windowBackground", "fontForeground", "windowBackgroundImage", "windowBackgroundBlend", "windowBackgroundFrostedGlass"] {
            assert_eq!(persisted[key], current[key], "{id}: {key}");
        }
        let before = memory(&host);
        let disk = files(&configs);
        while_blocked(&configs.join("ClipConfig.json"), || {
            assert!(host.settings_form_update(&json!({"configId": id, "values": {
                "UiThemeMode": "Light", "WindowBackgroundImage": "not-saved.png"
            }})).is_err());
            assert_eq!(memory(&host), before, "{id}: failed persistence keeps live config");
            assert!(host.drain_push_events().is_empty(), "{id}: failed persistence cannot repaint windows");
        });
        assert_eq!(files(&configs), disk, "{id}: failed transaction keeps both config files");
    }
}

#[cfg(windows)]
#[test]
fn ui_preferences_failure_preserves_saved_behavior_and_publishes_correction_then_retries() {
    use std::os::windows::fs::OpenOptionsExt;
    let (_dir, host) = fixture();
    let paths = host.inner.lock().paths.clone();
    let file = paths.data_root.join("clip-host-ui-preferences.json");
    let baseline = json!({"kind":"mainHostCommand","command":"setShowTrayIcon","show":true});
    assert_eq!(host.chrome_web_action(&baseline)["ok"], true);
    let before = fs::read(&file).unwrap();
    let baseline_state = host.main_window_state_dto();
    let locked = fs::OpenOptions::new().read(true).share_mode(1).open(&file).unwrap();
    for request in [
        json!({"kind":"mainHostCommand","command":"setShowTrayIcon","show":false}),
        json!({"kind":"mainHostCommand","command":"setCloseBehavior","behavior":"exitApplication"}),
    ] {
        while host.take_chrome_command().is_some() {}
        host.take_toast_push();
        let result = host.chrome_web_action(&request);
        assert_eq!(result["ok"], false);
        assert!(result["error"].as_str().unwrap().contains("保存窗口设置失败"));
        assert_eq!(host.main_window_state_dto(), baseline_state);
        assert_eq!(fs::read(&file).unwrap(), before);
        let commands = std::iter::from_fn(|| host.take_chrome_command()).collect::<Vec<_>>();
        assert!(commands.iter().any(|c| matches!(c, ChromeCommand::PushWindowState)));
        assert!(commands.iter().any(|c| matches!(c, ChromeCommand::ShowToast)));
        let toast = host.take_toast_push().expect("save error must reach the visible warning surface");
        assert!(toast.to_string().contains("保存窗口设置失败"));
    }
    drop(locked);
    assert_eq!(host.chrome_web_action(&json!({"kind":"mainHostCommand","command":"setShowTrayIcon","show":false}))["ok"], true);
    assert!(!host.show_tray_icon());
    drop(host);
    let reopened = HostState::open(paths, 28965).unwrap();
    assert!(!reopened.show_tray_icon());
    assert_eq!(reopened.close_behavior(), "MinimizeToTray");
}

#[test]
fn ui_preferences_concurrent_patches_survive_reopen_without_losing_either_choice() {
    let (_dir, host) = fixture();
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let workers = [
        json!({"kind":"mainHostCommand","command":"setShowTrayIcon","show":false}),
        json!({"kind":"mainHostCommand","command":"setCloseBehavior","behavior":"exitApplication"}),
    ].into_iter().map(|request| {
        let host = host.clone();
        let barrier = barrier.clone();
        thread::spawn(move || {
            barrier.wait();
            assert_eq!(host.chrome_web_action(&request)["ok"], true);
        })
    }).collect::<Vec<_>>();
    barrier.wait();
    for worker in workers { worker.join().unwrap(); }
    assert_eq!(host.close_behavior(), "ExitApplication");
    assert!(!host.show_tray_icon());
    let paths = host.inner.lock().paths.clone();
    drop(host);
    let reopened = HostState::open(paths, 28965).unwrap();
    assert_eq!(reopened.close_behavior(), "ExitApplication");
    assert!(!reopened.show_tray_icon());
}

#[test]
fn config_persistence_failures_leave_runtime_flags_and_related_files_unchanged() {
    let (_dir, host) = fixture();
    let configs = host.inner.lock().paths.configs.clone();
    let before = memory(&host);
    let disk = files(&configs);
    for (blocked, request) in [
        (
            "ClipConfig.json",
            json!({"configId":"clip", "values":{"AllowDuplicateCopy":true,"ClipMonitorOn":false,"TextProcessExpression":"changed","BlackList":"changed","MaxItemSize":1}}),
        ),
        (
            "ClipConfig.json",
            json!({"configId":"global", "values":{"UiThemeMode":"Dark","RunAsAdministrator":true}}),
        ),
        (
            "GlobalHotkeys.json",
            json!({"configId":"trigger", "values":{"AllowDuplicateCopy":true,"EnableNoFocusMode":false,"HotkeyShowClipboard":"Ctrl+Alt+9"}}),
        ),
        (
            "ClipFilterConfig.json",
            json!({"configId":"clip", "values":{"AllowDuplicateCopy":true,"FilterItems":[]}}),
        ),
    ] {
        host.drain_push_events();
        while_blocked(&configs.join(blocked), || {
            assert!(host.settings_form_update(&request).is_err(), "{blocked}");
            assert_eq!(memory(&host), before, "{blocked}");
            assert!(host.drain_push_events().is_empty());
        });
        assert_eq!(files(&configs), disk, "{blocked}");
    }
    while_blocked(&configs.join("GlobalConfig.json"), || {
        assert!(host
            .config_update(&json!({"uiThemeMode":"dark","allowDuplicateCopy":true}))
            .is_err());
        assert_eq!(memory(&host), before);
    });
    assert_eq!(files(&configs), disk);
}

#[test]
fn config_persistence_root_reorder_fails_visibly_then_survives_reopen() {
    let (_dir, host) = fixture();
    let a = host.favor_folder_create(&json!({"title":"A"})).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let b = host.favor_folder_create(&json!({"title":"B"})).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let paths = host.inner.lock().paths.clone();
    let before = memory(&host);
    host.drain_push_events();
    let request = json!({"folderId":"root","orderedChildIds":[b,a]});
    while_blocked(&paths.configs.join("FavorConfig.json"), || {
        assert!(host.panel_reorder(&request).is_err());
        assert_eq!(memory(&host), before);
        assert!(host.drain_push_events().is_empty());
        assert!(
            host.panel_catalog().is_ok(),
            "read-only configuration must not hide saved favorites"
        );
    });
    host.panel_reorder(&request).unwrap();
    assert_eq!(
        host.inner.lock().extras.favor.root_children,
        [b.clone(), a.clone()]
    );
    drop(host);
    let reopened = HostState::open(paths, 28965).unwrap();
    assert_eq!(reopened.inner.lock().extras.favor.root_children, [b, a]);
}

#[test]
fn config_persistence_concurrent_patches_preserve_each_change_on_disk_and_reopen() {
    let (_dir, host) = fixture();
    let barrier = Arc::new(std::sync::Barrier::new(4));
    let workers = [
        json!({"allowDuplicateCopy":true}),
        json!({"showCopyMessage":false}),
        json!({"clipListExecuteMode":3}),
    ]
    .into_iter()
    .map(|params| {
        let testing = host.clone();
        let barrier = barrier.clone();
        thread::spawn(move || {
            barrier.wait();
            testing.config_update(&params).unwrap();
        })
    })
    .collect::<Vec<_>>();
    barrier.wait();
    for worker in workers {
        worker.join().unwrap();
    }
    let cfg = host.inner.lock().config.clone();
    assert!(cfg.allow_duplicate_copy);
    assert!(!cfg.show_copy_message);
    assert_eq!(cfg.clip_list_execute_mode, 3);
    let paths = host.inner.lock().paths.clone();
    drop(host);
    let reopened = HostState::open(paths, 28965).unwrap();
    let saved = reopened.inner.lock().config.clone();
    assert!(saved.allow_duplicate_copy);
    assert!(!saved.show_copy_message);
    assert_eq!(saved.clip_list_execute_mode, 3);
}

#[test]
fn recent_colors_old_file_without_field_loads_empty_and_keeps_other_fields() {
    crate::clipboard::force_headless(true);
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("BetterClipTauri");
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("clip-host-ui-preferences.json"),
        r#"{"closeBehavior":"ExitApplication","showTrayIcon":false}"#,
    )
    .unwrap();
    let host = HostState::open(AppPaths::from_data_root(root.clone()), 28965).unwrap();
    assert!(host.recent_colors().is_empty());
    host.push_recent_color("#ff0078d4").unwrap();
    let saved: Value =
        serde_json::from_slice(&fs::read(root.join("clip-host-ui-preferences.json")).unwrap()).unwrap();
    assert_eq!(saved["closeBehavior"], "ExitApplication");
    assert_eq!(saved["showTrayIcon"], false);
    assert_eq!(saved["recentColors"], json!(["#FF0078D4"]));
}

#[test]
fn recent_colors_dedupe_normalize_cap_and_ignore_invalid() {
    let (_dir, host) = fixture();
    host.push_recent_color("#112233").unwrap(); // six digits become opaque
    host.push_recent_color("not a color").unwrap();
    host.push_recent_color("#12345").unwrap();
    assert_eq!(host.recent_colors(), vec!["#FF112233"]);
    host.push_recent_color("#80ABCDEF").unwrap();
    host.push_recent_color("#ff112233").unwrap(); // duplicate moves to the front
    assert_eq!(host.recent_colors(), vec!["#FF112233", "#80ABCDEF"]);
    for index in 0..12u32 {
        host.push_recent_color(&format!("#FF{index:06X}")).unwrap();
    }
    let recent = host.recent_colors();
    assert_eq!(recent.len(), 8);
    assert_eq!(recent[0], "#FF00000B");
    assert_eq!(recent[7], "#FF000004");
    let paths = host.inner.lock().paths.clone();
    drop(host);
    let reopened = HostState::open(paths, 28965).unwrap();
    assert_eq!(reopened.recent_colors(), recent);
}

#[test]
fn recent_colors_and_close_behavior_writes_do_not_overwrite_each_other() {
    let (_dir, host) = fixture();
    host.push_recent_color("#FF0078D4").unwrap();
    assert_eq!(
        host.chrome_web_action(&json!({"kind":"mainHostCommand","command":"setCloseBehavior","behavior":"exitApplication"}))["ok"],
        true
    );
    assert_eq!(host.recent_colors(), vec!["#FF0078D4"]);
    host.push_recent_color("#FFD83B01").unwrap();
    assert_eq!(host.close_behavior(), "ExitApplication");
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let workers = [0, 1]
        .into_iter()
        .map(|which| {
            let host = host.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                if which == 0 {
                    host.push_recent_color("#FF107C10").unwrap();
                } else {
                    host.chrome_web_action(&json!({"kind":"mainHostCommand","command":"setShowTrayIcon","show":false}));
                }
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    for worker in workers {
        worker.join().unwrap();
    }
    let paths = host.inner.lock().paths.clone();
    drop(host);
    let reopened = HostState::open(paths, 28965).unwrap();
    assert_eq!(reopened.close_behavior(), "ExitApplication");
    assert!(!reopened.show_tray_icon());
    assert_eq!(
        reopened.recent_colors(),
        vec!["#FF107C10", "#FFD83B01", "#FF0078D4"]
    );
}

#[test]
fn super_menu_hotkey_save_clear_and_reopen_keep_both_config_files_in_sync() {
    crate::clipboard::force_headless(true);
    let temporary_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
    fs::create_dir_all(&temporary_root).unwrap();
    let dir = tempfile::Builder::new().prefix("super-menu-hotkey-").tempdir_in(temporary_root).unwrap();
    let paths = AppPaths::from_data_root(dir.path().join("data"));
    let host = HostState::open(paths.clone(), 28965).unwrap();
    host.config_update(&json!({})).unwrap();
    fs::write(paths.configs.join("GlobalConfig.json"), br#"{"FutureGlobal":true}"#).unwrap();
    for (input, expected) in [("Alt+F10", "Alt+F10"), ("", "")] {
        host.settings_form_update(&json!({"configId":"trigger","values":{"HotkeySuperMenu":input}})).unwrap();
        for file in ["GlobalHotkeys.json", "GlobalConfig.json"] {
            let saved: Value = serde_json::from_slice(&fs::read(paths.configs.join(file)).unwrap()).unwrap();
            assert_eq!(saved["HotkeySuperMenu"], expected, "{file}");
            if file == "GlobalConfig.json" {
                assert_eq!(saved["FutureGlobal"], true);
            }
        }
        let reopened = HostState::open(paths.clone(), 28965).unwrap();
        assert_eq!(reopened.inner.lock().hotkeys.hotkey_super_menu, expected);
        if !expected.is_empty() {
            let updated = reopened.settings_form_update(&json!({
                "configId":"trigger", "values":{"HotkeySuperMenu":"Alt+Oem3"}
            })).unwrap();
            assert!(updated["messages"].as_array().unwrap().iter().any(|message| {
                message["fieldKey"] == "HotkeySuperMenu" && message["level"] == "warning"
            }));
            assert_eq!(reopened.hotkey_binding_statuses()[2]["state"], "Registered");
            assert_eq!(reopened.hotkey_binding_statuses()[4]["state"], "Failed");
        }
    }
}

#[test]
fn explorer_history_settings_apply_live_keep_records_and_survive_reopen() {
    crate::clipboard::force_headless(true);
    let temporary_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
    fs::create_dir_all(&temporary_root).unwrap();
    let dir = tempfile::Builder::new().prefix("explorer-history-settings-").tempdir_in(temporary_root).unwrap();
    let paths = AppPaths::from_data_root(dir.path().join("data"));
    fs::create_dir_all(&paths.configs).unwrap();
    fs::write(paths.configs.join("UserConfig.json"), br#"{"SaveReOpenHistory":true,"FutureUserSetting":42}"#).unwrap();
    let host = HostState::open(paths.clone(), 28965).unwrap();
    assert_eq!(host.inner.lock().extras.user.re_open_history_limit, 200);
    assert_eq!(host.inner.lock().extras.user.re_open_menu_limit, 10);
    let store = host.inner.lock().store.clone();
    for index in 0..12 {
        store.lock().upsert_explorer_record(&format!("shell:::fixture-{index}")).unwrap();
    }
    host.settings_form_update(&json!({"configId":"user","values":{
        "SaveReOpenHistory":false,"ReOpenHistoryLimit":3,"ReOpenMenuLimit":2
    }})).unwrap();
    {
        let inner = host.inner.lock();
        assert!(!inner.save_reopen.load(Ordering::Relaxed));
        assert_eq!(inner.reopen_history_limit.load(Ordering::Relaxed), 3);
        assert_eq!(inner.extras.user.re_open_menu_limit, 2);
    }
    assert_eq!(store.lock().list_explorer_records().unwrap().len(), 12, "saving lower limits must not delete existing records");
    let config_path = paths.configs.join("UserConfig.json");
    let saved: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    assert_eq!(saved["FutureUserSetting"], 42);
    assert_eq!(saved["ReOpenHistoryLimit"], 3);
    assert_eq!(saved["ReOpenMenuLimit"], 2);
    while_blocked(&config_path, || {
        assert!(host.settings_form_update(&json!({"configId":"user","values":{
            "SaveReOpenHistory":true,"ReOpenHistoryLimit":40,"ReOpenMenuLimit":20
        }})).is_err());
        let inner = host.inner.lock();
        assert!(!inner.save_reopen.load(Ordering::Relaxed));
        assert_eq!(inner.reopen_history_limit.load(Ordering::Relaxed), 3);
        assert_eq!(inner.extras.user.re_open_menu_limit, 2);
    });
    drop(host);
    let reopened = HostState::open(paths.clone(), 28965).unwrap();
    {
        let inner = reopened.inner.lock();
        assert!(!inner.save_reopen.load(Ordering::Relaxed));
        assert_eq!(inner.reopen_history_limit.load(Ordering::Relaxed), 3);
        assert_eq!(inner.extras.user.re_open_menu_limit, 2);
    }
    assert_eq!(store.lock().list_explorer_records().unwrap().len(), 12, "opening the host must not trim saved history");
    drop(reopened);
    fs::write(config_path, br#"{"SaveReOpenHistory":true,"ReOpenHistoryLimit":0,"ReOpenMenuLimit":900}"#).unwrap();
    let reopened = HostState::open(paths, 28965).unwrap();
    let inner = reopened.inner.lock();
    assert_eq!(inner.reopen_history_limit.load(Ordering::Relaxed), 1);
    assert_eq!(inner.extras.user.re_open_menu_limit, 1);
}

#[test]
fn explorer_history_menu_uses_saved_limit_after_excluding_opened_folders() {
    crate::clipboard::force_headless(true);
    let temporary_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
    fs::create_dir_all(&temporary_root).unwrap();
    let dir = tempfile::Builder::new().prefix("explorer-history-menu-").tempdir_in(temporary_root).unwrap();
    let host = HostState::open(AppPaths::from_data_root(dir.path().join("data")), 28965).unwrap();
    let store = host.inner.lock().store.clone();
    for index in 0..6 {
        store.lock().upsert_explorer_record(&format!("shell:::fixture-{index}")).unwrap();
    }
    let records = store.lock().list_explorer_records().unwrap();
    host.settings_form_update(&json!({"configId":"user","values":{
        "SaveReOpenHistory":false,"ReOpenHistoryLimit":6,"ReOpenMenuLimit":2
    }})).unwrap();
    for action in ["reopen", "file", "favor", "super"] {
        let ctx = host.menu_build_context_with(action, "", |ctx| {
            ctx.opened = vec![crate::explorer::OpenedFolder {
                hwnd: 42, path: records[0].clone(), title: "fixture".into(),
            }];
        });
        assert_eq!(ctx.reopen_paths, records[1..3], "{action}: the display limit applies after excluding opened folders, including when recording is off");
    }
}
