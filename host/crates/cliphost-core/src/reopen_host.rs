use std::collections::HashSet;
#[cfg(test)]
use std::sync::atomic::Ordering;

use serde_json::{json, Map, Value};

use super::HostState;
use crate::extra_settings::UserSettings;
use crate::reopen_history::{directory_key, ReopenStoreSnapshot};

impl HostState {
    /// Native views call this on a background worker. Capture the live tab
    /// snapshot outside every HostInner/database lock; no path existence probe
    /// is needed to mark an entry as currently open.
    pub fn reopen_history_snapshot(&self) -> Result<Value, String> {
        let opened = crate::explorer::list_opened_folders_unprobed();
        let (settings, store) = {
            let inner = self.inner.lock();
            (inner.extras.user.clone(), inner.store.clone())
        };
        let snapshot = store
            .lock()
            .reopen_store_snapshot()
            .map_err(|e| e.to_string())?;
        Ok(build_snapshot(snapshot, &opened, &settings))
    }

    /// History mutation API shared by the dedicated quick window and the
    /// embedded manager. A `remove` request also implements clear-all: its
    /// entries are the fixed snapshot accepted by the confirmation dialog.
    pub fn reopen_history_apply(&self, params: &Value) -> Result<Value, String> {
        let action = params["action"].as_str().unwrap_or("");
        if action == "settings" {
            let mut values = Map::new();
            for (source, target) in [
                ("recording", "SaveReOpenHistory"),
                ("historyLimit", "ReOpenHistoryLimit"),
                ("menuLimit", "ReOpenMenuLimit"),
                ("showMenu", "ReOpenShowMenu"),
            ] {
                if let Some(value) = params.get(source) {
                    values.insert(target.into(), value.clone());
                }
            }
            self.settings_form_update(&json!({"configId":"user","values":values}))?;
        } else if !matches!(action, "remove" | "restore" | "exclude" | "unexclude") {
            return Err("不支持的重开历史操作。".into());
        }
        let opened = crate::explorer::list_opened_folders_unprobed();
        let (settings, store) = {
            let inner = self.inner.lock();
            (inner.extras.user.clone(), inner.store.clone())
        };
        let invalidate = json!({"action":"invalidateUndo"});
        let result = store
            .lock()
            .reopen_store_apply(if action == "settings" {
                &invalidate
            } else {
                params
            })
            .map_err(|error| {
                if action == "settings" {
                    format!("设置已保存，但重开历史刷新失败：{error}")
                } else {
                    error.to_string()
                }
            })?;
        let snapshot = build_snapshot(result.snapshot, &opened, &settings);
        self.inner.lock().reopen.menu_undo = None;
        self.push_event("reopenHistoryChanged", json!({
            "action":action, "changed":result.changed, "skipped":result.skipped, "ruleChanged":result.rule_changed,
        }));
        Ok(json!({
            "changed":result.changed, "skipped":result.skipped,
            "ruleChanged":result.rule_changed, "undo":result.undo, "snapshot":snapshot,
        }))
    }

    /// Compatibility is a one-time import, never a standing override. Native
    /// preference changes also set this persisted guard before a late action
    /// can deliver its old Quicker `show_menu` value.
    pub fn reopen_migrate_legacy_preference(&self, show: bool) -> Result<(), String> {
        let _operation = self.settings_operation.lock();
        let (mut settings, path) = {
            let inner = self.inner.lock();
            if inner.extras.user.re_open_legacy_preference_migrated {
                return Ok(());
            }
            (
                inner.extras.user.clone(),
                inner.paths.configs.join("UserConfig.json"),
            )
        };
        settings.re_open_show_menu = show;
        settings.re_open_legacy_preference_migrated = true;
        super::save_json(&path, &settings)?;
        self.inner.lock().extras.user = settings;
        drop(_operation);
        self.push_event("nativeSettingsChanged", json!({"configId":"user"}));
        Ok(())
    }

    pub fn reopen_prefers_quick_window(&self) -> bool {
        self.inner.lock().extras.user.re_open_show_menu
    }
}

fn build_snapshot(
    snapshot: ReopenStoreSnapshot,
    opened: &[crate::explorer::OpenedFolder],
    settings: &UserSettings,
) -> Value {
    let opened_keys: HashSet<String> = opened
        .iter()
        .map(|folder| directory_key(&folder.path))
        .collect();
    json!({
        "entries":snapshot.entries.into_iter().map(|entry| json!({
            "title":location_title(&entry.path, opened), "path":entry.path,
            "recordedAt":entry.recorded_at, "opened":opened_keys.contains(&directory_key(&entry.path)),
        })).collect::<Vec<_>>(),
        "opened":opened.iter().map(|folder| json!({
            "path":folder.path,"title":location_title(&folder.path, opened),"hwnd":folder.hwnd,
        })).collect::<Vec<_>>(),
        "exclusions":snapshot.exclusions,
        "recording":settings.save_re_open_history, "historyLimit":settings.re_open_history_limit,
        "menuLimit":settings.re_open_menu_limit, "showMenu":settings.re_open_show_menu,
    })
}

fn location_title(path: &str, opened: &[crate::explorer::OpenedFolder]) -> String {
    let key = directory_key(path);
    for (shell, title) in [
        (
            crate::explorer::THIS_PC_SHELL,
            crate::i18n::tr("此电脑", "This PC"),
        ),
        (
            crate::explorer::HOME_SHELL,
            crate::i18n::tr("主文件夹", "Home"),
        ),
        (
            crate::explorer::RECYCLE_BIN_SHELL,
            crate::i18n::tr("回收站", "Recycle Bin"),
        ),
        (
            crate::explorer::NETWORK_SHELL,
            crate::i18n::tr("网络", "Network"),
        ),
    ] {
        if key == directory_key(shell) {
            return title.to_string();
        }
    }
    if crate::reopen_history::is_shell_location(path) {
        return opened
            .iter()
            .find(|folder| directory_key(&folder.path) == key && !folder.title.is_empty())
            .map(|folder| folder.title.clone())
            .unwrap_or_else(|| path.to_string());
    }
    path.trim_end_matches(['\\', '/'])
        .rsplit(['\\', '/'])
        .next()
        .filter(|title| !title.is_empty())
        .unwrap_or(path)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::AppPaths;

    fn fixture() -> (tempfile::TempDir, AppPaths, HostState) {
        crate::clipboard::force_headless(true);
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
        std::fs::create_dir_all(&root).unwrap();
        let dir = tempfile::Builder::new()
            .prefix("reopen-host-")
            .tempdir_in(root)
            .unwrap();
        let paths = AppPaths::from_data_root(dir.path().join("data"));
        let host = HostState::open(paths.clone(), 29065).unwrap();
        (dir, paths, host)
    }

    #[test]
    fn reopen_management_reads_full_history_and_pause_or_lower_limits_preserves_existing_entries() {
        let (_dir, _paths, host) = fixture();
        for index in 0..13 {
            host.store()
                .lock()
                .upsert_explorer_record(&format!("shell:::fixture-{index}"))
                .unwrap();
        }
        let snapshot = host.reopen_history_snapshot().unwrap();
        assert_eq!(snapshot["entries"].as_array().unwrap().len(), 13);
        assert_eq!(snapshot["menuLimit"], 10);
        let changed = host.reopen_history_apply(&json!({"action":"settings","recording":false,"historyLimit":5,"menuLimit":10,"showMenu":true})).unwrap();
        assert_eq!(changed["snapshot"]["entries"].as_array().unwrap().len(), 13);
        assert_eq!(changed["snapshot"]["menuLimit"], 5);
        assert_eq!(changed["snapshot"]["recording"], false);
        let inner = host.inner.lock();
        assert!(!inner.save_reopen.load(Ordering::Relaxed));
        assert_eq!(inner.reopen_history_limit.load(Ordering::Relaxed), 5);
    }

    #[test]
    fn reopen_legacy_preference_migrates_once_and_native_setting_wins_across_restart() {
        let (_dir, paths, host) = fixture();
        host.reopen_migrate_legacy_preference(true).unwrap();
        assert!(host.reopen_prefers_quick_window());
        host.reopen_migrate_legacy_preference(false).unwrap();
        assert!(host.reopen_prefers_quick_window());
        host.reopen_history_apply(&json!({"action":"settings","showMenu":false}))
            .unwrap();
        host.reopen_migrate_legacy_preference(true).unwrap();
        assert!(!host.reopen_prefers_quick_window());
        drop(host);
        let host = HostState::open(paths, 29065).unwrap();
        host.reopen_migrate_legacy_preference(true).unwrap();
        assert!(!host.reopen_prefers_quick_window());
    }

    #[test]
    fn reopen_setting_save_failure_keeps_memory_and_legacy_migration_guard_unchanged() {
        let (_dir, paths, host) = fixture();
        let path = paths.configs.join("UserConfig.json");
        let prior = std::fs::read(&path).ok();
        if path.exists() {
            std::fs::remove_file(&path).unwrap();
        }
        std::fs::create_dir(&path).unwrap();
        assert!(host
            .reopen_history_apply(&json!({"action":"settings","showMenu":true,"recording":false}))
            .is_err());
        assert!(host.reopen_migrate_legacy_preference(true).is_err());
        let inner = host.inner.lock();
        assert!(inner.save_reopen.load(Ordering::Relaxed));
        assert!(!inner.extras.user.re_open_show_menu);
        assert!(!inner.extras.user.re_open_legacy_preference_migrated);
        drop(inner);
        std::fs::remove_dir(path.clone()).unwrap();
        if let Some(prior) = prior {
            std::fs::write(path, prior).unwrap();
        }
    }

    #[test]
    fn reopen_live_open_status_and_shell_titles_do_not_require_filesystem_probes() {
        let settings = UserSettings::default();
        let snapshot = ReopenStoreSnapshot {
            entries: vec![
                crate::reopen_history::ReopenHistoryEntry {
                    path: r"C:\Example\Docs".into(),
                    recorded_at: 7,
                },
                crate::reopen_history::ReopenHistoryEntry {
                    path: crate::explorer::THIS_PC_SHELL.into(),
                    recorded_at: 6,
                },
            ],
            exclusions: Vec::new(),
        };
        let value = build_snapshot(
            snapshot,
            &[crate::explorer::OpenedFolder {
                path: r"c:\example\docs\".into(),
                title: "Docs".into(),
                hwnd: 42,
            }],
            &settings,
        );
        assert_eq!(value["entries"][0]["opened"], true);
        assert_eq!(
            value["entries"][1]["title"],
            crate::i18n::tr("此电脑", "This PC")
        );
        assert_eq!(value["opened"][0]["hwnd"], 42);
    }
}
