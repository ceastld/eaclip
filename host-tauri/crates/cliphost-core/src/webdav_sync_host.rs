//! Debug-only WebDAV orchestration. Network work never holds the UI/storage lock.
use super::*;
use crate::webdav_favorites::{self as adapter, ApplyPlan, Limits};
use cliphost_webdav::transport::{TransportConfig, WebDavStore};
use cliphost_webdav::{Engine, PreparedSync, RecoveryKey, Snapshot};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[path = "webdav_config.rs"]
mod config;
use config::DevConfig;

const RECEIPT: &str = "webdav.dev.applied-token";
const GLOBAL_FIELDS: &[&str] = &[
    "UiThemeMode",
    "UiLanguage",
    "WindowBackgroundColor",
    "FontForegroundColor",
];
const CLIP_FIELDS: &[&str] = &["ClipListAppearanceSlot", "ClipListCustomAppearance"];

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApplyJournal {
    token: String,
    expected_revision: String,
    settings_before: BTreeMap<String, Value>,
    settings_after: BTreeMap<String, Value>,
}

fn profile_root(paths: &AppPaths, config: &DevConfig) -> Result<PathBuf, String> {
    if config.profile_id.len() != 32 || !config.profile_id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("WebDAV 本机配置标识无效。".into());
    }
    Ok(paths.data_root.join("DevWebDav").join(&config.profile_id))
}
fn config_path(paths: &AppPaths) -> PathBuf {
    paths.configs.join("WebDavDev.protected.json")
}
fn report_path(paths: &AppPaths) -> PathBuf {
    paths.data_root.join("DevWebDav").join("status.json")
}
fn remote(config: &DevConfig) -> Result<WebDavStore, String> {
    WebDavStore::new(TransportConfig::new(
        &config.endpoint,
        &config.username,
        &config.password,
    ))
    .map_err(|e| e.to_string())
}
fn engine(paths: &AppPaths, config: &DevConfig) -> Result<Engine, String> {
    let key = RecoveryKey::from_hex(&config.recovery_key).map_err(|e| e.to_string())?;
    Engine::open(&profile_root(paths, config)?.join("engine"), key).map_err(|e| e.to_string())
}
fn read_json(path: &std::path::Path) -> Result<Value, String> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|_| "WebDAV 本机状态损坏。".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Value::Null),
        Err(_) => Err("无法读取 WebDAV 本机状态。".into()),
    }
}
fn settings_key(group: &str, field: &str) -> String {
    format!("setting/{group}/{field}")
}
fn settings_scope() -> Vec<String> {
    GLOBAL_FIELDS
        .iter()
        .map(|f| settings_key("global", f))
        .chain(CLIP_FIELDS.iter().map(|f| settings_key("clip", f)))
        .collect()
}

fn validate_merged_snapshot(snapshot: &Snapshot) -> Result<(), cliphost_webdav::SyncError> {
    let entries = snapshot
        .entries
        .iter()
        .filter(|(key, _)| key.starts_with("favor/"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<BTreeMap<_, _>>();
    let roots: Vec<String> = match snapshot.entries.get("roots") {
        Some(value) => serde_json::from_value(value.clone())
            .map_err(|_| cliphost_webdav::SyncError::InvalidData("favorite_roots"))?,
        None if entries.is_empty() => Vec::new(),
        None => {
            return Err(cliphost_webdav::SyncError::InvalidData(
                "missing_favorite_roots",
            ))
        }
    };
    adapter::validate_portable(&entries, &roots, &snapshot.blobs, &Limits::default())
        .map_err(|_| cliphost_webdav::SyncError::InvalidData("favorite_graph_or_resources"))
}

impl HostState {
    /// Contains only capability/state/counts. Never includes a password, key, or favorite body.
    pub fn webdav_dev_status(&self) -> Value {
        let paths = self.inner.lock().paths.clone();
        let config = config::load(&config_path(&paths));
        let last = read_json(&report_path(&paths)).unwrap_or(Value::Null);
        match config {
            Ok(config) => {
                json!({"devOnly":true,"configured":config.is_some(),"enabled":config.as_ref().is_some_and(|c|c.enabled),
                "syncSettings":config.as_ref().is_some_and(|c|c.sync_settings),"running":self.webdav_operation.is_locked(),
                "status":if self.webdav_operation.is_locked(){"正在同步"}else if config.is_none(){"尚未配置"}else if config.as_ref().is_some_and(|c|c.enabled){"后台同步已开启"}else{"已暂停"},
                "lastReport":last.get("report").cloned().unwrap_or(Value::Null),"error":last.get("error").cloned().unwrap_or(Value::Null),
                "issues":last.get("issues").cloned().unwrap_or(Value::Null)})
            }
            Err(error) => {
                json!({"devOnly":true,"configured":true,"enabled":false,"running":false,"status":"本机配置不可用","error":error})
            }
        }
    }

    pub fn webdav_dev_command(&self, request: &Value) -> Result<Value, String> {
        let command = request["command"].as_str().unwrap_or("status");
        if command == "status" {
            return Ok(self.webdav_dev_status());
        }
        let _operation = self
            .webdav_operation
            .try_lock()
            .ok_or("WebDAV 正在处理上一项操作，请稍后重试。")?;
        let paths = self.inner.lock().paths.clone();
        let old = config::load(&config_path(&paths))?;
        if command == "configure" {
            let endpoint = request["endpoint"]
                .as_str()
                .ok_or("请提供 WebDAV 专用目录地址。")?
                .to_owned();
            let username = request["username"]
                .as_str()
                .ok_or("请提供 WebDAV 账号。")?
                .to_owned();
            let password = request["password"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| old.as_ref().map(|c| c.password.clone()))
                .ok_or("请提供 WebDAV 应用密码。")?;
            let recovery_key = if let Some(old) = &old {
                if old.endpoint != endpoint {
                    return Err(
                        "更换 WebDAV 集合前请先停止并断开当前配置；旧同步状态会保留。".into(),
                    );
                }
                if request.get("recoveryKey").is_some() {
                    return Err("已有配置的恢复密钥不能直接替换，请先断开配置。".into());
                }
                old.recovery_key.clone()
            } else if let Some(key) = request["recoveryKey"].as_str() {
                RecoveryKey::from_hex(key)
                    .map_err(|e| e.to_string())?
                    .to_hex()
            } else {
                RecoveryKey::generate().map_err(|e| e.to_string())?.to_hex()
            };
            let next = DevConfig {
                profile_id: old
                    .as_ref()
                    .map(|c| c.profile_id.clone())
                    .unwrap_or_else(|| Uuid::new_v4().simple().to_string()),
                endpoint,
                username,
                password,
                recovery_key,
                enabled: false,
                sync_settings: request["syncSettings"].as_bool().unwrap_or(false),
                interval_seconds: 120,
            };
            let _validated = remote(&next)?;
            config::save(&config_path(&paths), &next)?;
            return Ok(
                json!({"ok":true,"configured":true,"enabled":false,"requiresInitialization":true}),
            );
        }
        let mut config = old.ok_or("请先配置用户自己的 WebDAV 地址与应用密码。")?;
        if command == "disable" {
            config.enabled = false;
            config::save(&config_path(&paths), &config)?;
            return Ok(json!({"ok":true,"enabled":false}));
        }
        if command == "disconnect" {
            config.enabled = false;
            config::save(&config_path(&paths), &config)?;
            let archive = profile_root(&paths, &config)?.join("disconnected.protected.json");
            config::save(&archive, &config)?;
            std::fs::remove_file(config_path(&paths))
                .map_err(|_| "无法移除当前配置，旧状态仍保留。")?;
            return Ok(json!({"ok":true,"configured":false,"localDataRetained":true}));
        }
        if command == "recoveryKey" {
            // This explicit action is used only by the helper's private recovery-file export.
            return Ok(json!({"recoveryKey":config.recovery_key}));
        }
        let mut engine = engine(&paths, &config)?;
        if command == "conflicts" {
            let pending = engine.pending().map_err(|e| e.to_string())?;
            return Ok(json!({"conflicts":pending.map(|p|p.conflicts).unwrap_or_default()}));
        }
        let remote = remote(&config)?;
        if command == "probe" {
            return remote
                .probe()
                .map(|report| json!({"ok":true,"probe":report}))
                .map_err(|e| e.to_string());
        }
        if command == "create" || command == "join" {
            if command == "create" && request["recoverySaved"] != true {
                return Err("创建前请先通过开发工具保存恢复密钥文件。".into());
            }
            remote.probe().map_err(|e| e.to_string())?;
            let id = if command == "create" {
                engine.create(&remote)
            } else {
                engine.join(&remote)
            }
            .map_err(|e| e.to_string())?;
            return Ok(json!({"ok":true,"collectionId":id,"enabled":false}));
        }
        if engine.collection_id().is_none() {
            return Err("请先新建集合，或使用恢复密钥加入已有集合。".into());
        }
        if command == "enable" {
            config.enabled = true;
            config::save(&config_path(&paths), &config)?;
            self.start_webdav_dev_worker();
            return Ok(json!({"ok":true,"enabled":true,"intervalSeconds":120}));
        }
        if command == "resolve" {
            if let Some(pending) = engine.pending().map_err(|e| e.to_string())? {
                if self
                    .store()
                    .lock()
                    .kv_get(RECEIPT)
                    .map_err(|e| e.to_string())?
                    .as_deref()
                    == Some(&pending.token)
                {
                    return Err("请先重试同步，完成已提交的本机应用。".into());
                }
                engine
                    .abandon_pending(&pending.token)
                    .map_err(|e| e.to_string())?;
            }
            let id = request["conflictId"].as_str().ok_or("缺少冲突标识。")?;
            let revision = request["revision"].as_str().ok_or("缺少选定版本。")?;
            let resolved = engine
                .resolve_validated(&remote, id, revision, validate_merged_snapshot)
                .map_err(|e| e.to_string())?;
            engine
                .abandon_pending(&resolved.token)
                .map_err(|e| e.to_string())?;
            let journal = profile_root(&paths, &config)?.join("apply.json");
            if journal.exists() {
                std::fs::remove_file(journal).map_err(|_| "无法更新本机应用状态。")?;
            }
        } else if command != "sync" {
            return Err("未知 WebDAV dev 操作。".into());
        }
        let result = self.webdav_sync_locked(&paths, &config, &remote, &mut engine);
        if let Err(error) = &result {
            let _ = crate::config_file::save(&report_path(&paths), &json!({"error":error}));
        }
        result
    }

    fn webdav_settings_snapshot(&self, enabled: bool) -> Result<BTreeMap<String, Value>, String> {
        if !enabled {
            return Ok(BTreeMap::new());
        }
        let inner = self.inner.lock();
        let global =
            serde_json::to_value(&inner.extras.global).map_err(|_| "无法读取外观设置。")?;
        let clip = persisted_clip_config(&inner.config)?;
        let mut values = BTreeMap::new();
        for (group, fields, value) in [
            ("global", GLOBAL_FIELDS, &global),
            ("clip", CLIP_FIELDS, &clip),
        ] {
            for &field in fields {
                if let Some(v) = value.as_object().and_then(|obj| {
                    obj.iter()
                        .find(|(key, _)| key.eq_ignore_ascii_case(field))
                        .map(|(_, v)| v)
                }) {
                    values.insert(settings_key(group, field), v.clone());
                }
            }
        }
        Ok(values)
    }

    #[cfg(test)]
    fn webdav_apply_settings(
        &self,
        before: &BTreeMap<String, Value>,
        after: &BTreeMap<String, Value>,
    ) -> Result<(), String> {
        let _settings = self.settings_operation.lock();
        self.webdav_apply_settings_locked(before, after)
    }

    /// Caller owns settings_operation. Applies appearance only, with no startup/hotkey side effects.
    fn webdav_apply_settings_locked(
        &self,
        before: &BTreeMap<String, Value>,
        after: &BTreeMap<String, Value>,
    ) -> Result<(), String> {
        if after.is_empty() {
            return Ok(());
        }
        let current = self.webdav_settings_snapshot(true)?;
        for (key, value) in after {
            if !settings_scope().contains(key) {
                return Err("同步版本包含未授权的设置字段。".into());
            }
            if current.get(key) != Some(value) && current.get(key) != before.get(key) {
                return Err("同步期间本机外观已修改，修改已保留。".into());
            }
        }
        let (paths, mut global, mut cfg) = {
            let inner = self.inner.lock();
            (
                inner.paths.clone(),
                inner.extras.global.clone(),
                inner.config.clone(),
            )
        };
        let mut global_values = serde_json::Map::new();
        let mut clip_values = serde_json::Map::new();
        for &field in GLOBAL_FIELDS {
            if let Some(value) = after.get(&settings_key("global", field)) {
                global_values.insert(field.into(), value.clone());
            }
        }
        for &field in CLIP_FIELDS {
            if let Some(value) = after.get(&settings_key("clip", field)) {
                clip_values.insert(field.into(), value.clone());
            }
        }
        crate::extra_settings::apply_global(&mut global, &Value::Object(global_values));
        // Applying a color in the UI selects Custom; a synchronized snapshot already
        // carries its explicit mode and must preserve a later choice of Dark/Light.
        if let Some(mode) = after
            .get("setting/global/UiThemeMode")
            .and_then(Value::as_str)
        {
            if !matches!(
                mode.to_ascii_lowercase().as_str(),
                "auto" | "light" | "dark" | "custom"
            ) {
                return Err("当前版本不支持该共享主题模式。".into());
            }
            global.ui_theme_mode = mode.to_owned();
        }
        apply_list_config(&mut cfg, &Value::Object(clip_values));
        crate::extra_settings::sync_clip_from_global(&mut cfg, &global);
        let global_json = serde_json::to_value(&global).map_err(|_| "无法保存同步外观。")?;
        let clip_json = persisted_clip_config(&cfg)?;
        for (key, value) in after {
            let (group, field) = key
                .strip_prefix("setting/")
                .and_then(|s| s.split_once('/'))
                .ok_or("无效设置标识。")?;
            let source = if group == "global" {
                &global_json
            } else {
                &clip_json
            };
            let actual = source.as_object().and_then(|obj| {
                obj.iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(field))
                    .map(|(_, v)| v)
            });
            if actual != Some(value) {
                return Err("当前版本无法完整应用该外观值，已保留原设置。".into());
            }
        }
        crate::config_file::save_values(&[
            (paths.configs.join("GlobalConfig.json"), global_json),
            (paths.configs.join("ClipConfig.json"), clip_json),
        ])?;
        let event = {
            let mut inner = self.inner.lock();
            inner.extras.global = global;
            inner.config = cfg;
            serde_json::to_value(&inner.config).map_err(|_| "无法刷新同步外观。")?
        };
        self.push_event("clipConfigChanged", event);
        Ok(())
    }

    fn webdav_sync_locked(
        &self,
        paths: &AppPaths,
        config: &DevConfig,
        remote: &impl cliphost_webdav::RemoteStore,
        engine: &mut Engine,
    ) -> Result<Value, String> {
        let limits = Limits::default();
        let journal_path = profile_root(paths, config)?.join("apply.json");
        // Recover any adapter cross-file transaction before inspecting its receipt.
        {
            let _settings = self.settings_operation.lock();
            let store = self.store();
            let db = store.lock();
            adapter::recover_apply(&db, paths)?;
        }
        if let Some(pending) = engine.pending().map_err(|e| e.to_string())? {
            if !pending.conflicts.is_empty() {
                return self.webdav_record_conflicts(paths, &pending);
            }
            let value = read_json(&journal_path)?;
            if !value.is_null() {
                let journal: ApplyJournal =
                    serde_json::from_value(value).map_err(|_| "WebDAV 本机应用日志损坏。")?;
                if journal.token != pending.token {
                    return Err("WebDAV 本机应用日志与待提交版本不一致。".into());
                }
                let receipt = self
                    .store()
                    .lock()
                    .kv_get(RECEIPT)
                    .map_err(|e| e.to_string())?;
                if receipt.as_deref() == Some(&pending.token) {
                    let _settings = self.settings_operation.lock();
                    let current = self.webdav_settings_snapshot(true)?;
                    let changed = journal.settings_after.iter().any(|(key, value)| {
                        current.get(key) != Some(value)
                            && current.get(key) != journal.settings_before.get(key)
                    });
                    if changed || (!config.sync_settings && !journal.settings_after.is_empty()) {
                        // The favorite receipt is durable. Keep newer local settings and
                        // rebase against the old shared baseline instead of acknowledging
                        // settings which were never applied or repeatedly overwriting them.
                        engine
                            .abandon_pending(&pending.token)
                            .map_err(|e| e.to_string())?;
                    } else {
                        self.webdav_apply_settings_locked(
                            &journal.settings_before,
                            &journal.settings_after,
                        )?;
                        engine
                            .acknowledge(&pending.token)
                            .map_err(|e| e.to_string())?;
                    }
                    std::fs::remove_file(&journal_path)
                        .map_err(|_| "无法完成 WebDAV 本机应用日志。")?;
                    self.push_event("favoritesChanged", json!({"op":"reload"}));
                } else {
                    // Unapplied published data is still remote; rebase fresh local edits instead of overwriting them.
                    engine
                        .abandon_pending(&pending.token)
                        .map_err(|e| e.to_string())?;
                    std::fs::remove_file(&journal_path)
                        .map_err(|_| "无法重新准备 WebDAV 本机应用。")?;
                }
            } else {
                engine
                    .abandon_pending(&pending.token)
                    .map_err(|e| e.to_string())?;
            }
        }
        let (bundle, settings_before) = {
            let _settings = self.settings_operation.lock();
            let store = self.store();
            let db = store.lock();
            let bundle = adapter::export_favorites(&db, paths, &limits)?;
            drop(db);
            (bundle, self.webdav_settings_snapshot(config.sync_settings)?)
        };
        if !bundle.issues.is_empty() {
            let issues: Vec<_> = bundle
                .issues
                .iter()
                .map(|i| json!({"itemId":i.item_id,"reason":i.reason}))
                .collect();
            let report = json!({"ok":false,"requiresResourceFix":true,"error":"部分收藏资源尚不能完整同步，已保留本机内容。","issues":issues});
            crate::config_file::save(&report_path(paths), &report)?;
            return Ok(report);
        }
        let mut snapshot = Snapshot {
            entries: bundle.entries.clone(),
            blobs: bundle.blobs.clone(),
        };
        if engine.has_acknowledged_baseline()
            || !bundle.entries.is_empty()
            || !bundle.roots.is_empty()
        {
            snapshot.entries.insert("roots".into(), json!(bundle.roots));
        }
        snapshot.entries.extend(settings_before.clone());
        let mut scope = vec!["favor/".to_string(), "roots".to_string()];
        if config.sync_settings {
            scope.extend(settings_scope());
        }
        let prepared = engine
            .prepare_scoped_validated(remote, &snapshot, &scope, validate_merged_snapshot)
            .map_err(|e| e.to_string())?;
        if !prepared.conflicts.is_empty() {
            return self.webdav_record_conflicts(paths, &prepared);
        }
        let subscribed_remote: BTreeMap<_, _> = prepared
            .snapshot
            .entries
            .iter()
            .filter(|(key, _)| {
                scope.iter().any(|s| {
                    if s.ends_with('/') {
                        key.starts_with(s)
                    } else {
                        *key == s
                    }
                })
            })
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        if subscribed_remote == snapshot.entries {
            // The selected snapshot is already durably present locally. Do not
            // rewrite every favorite/RowToken and invalidate open editor drafts.
            engine
                .acknowledge(&prepared.token)
                .map_err(|e| e.to_string())?;
            let report = json!({"ok":true,"report":prepared.report});
            crate::config_file::save(&report_path(paths), &report)?;
            return Ok(report);
        }
        let settings_after: BTreeMap<_, _> = prepared
            .snapshot
            .entries
            .iter()
            .filter(|(key, _)| settings_scope().contains(key))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let journal = ApplyJournal {
            token: prepared.token.clone(),
            expected_revision: bundle.revision.clone(),
            settings_before: settings_before.clone(),
            settings_after: if config.sync_settings {
                settings_after
            } else {
                BTreeMap::new()
            },
        };
        crate::config_file::save(&journal_path, &journal)?;
        {
            let _settings = self.settings_operation.lock();
            if self.webdav_settings_snapshot(config.sync_settings)? != settings_before {
                return Err("网络同步期间本机外观已修改，修改已保留，请重试。".into());
            }
            let roots = match prepared.snapshot.entries.get("roots") {
                Some(value) => {
                    serde_json::from_value(value.clone()).map_err(|_| "收藏根顺序无效。")?
                }
                None if !prepared
                    .snapshot
                    .entries
                    .keys()
                    .any(|key| key.starts_with("favor/")) =>
                {
                    Vec::new()
                }
                None => return Err("同步版本缺少收藏根顺序。".into()),
            };
            let entries = prepared
                .snapshot
                .entries
                .iter()
                .filter(|(key, _)| key.starts_with("favor/"))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            let plan = ApplyPlan {
                expected_revision: bundle.revision,
                entries,
                roots,
                blobs: prepared.snapshot.blobs.clone(),
                receipt: Some((RECEIPT.into(), prepared.token.clone())),
            };
            let store = self.store();
            let db = store.lock();
            adapter::apply_favorites(&db, paths, &plan, &limits)?;
            drop(db);
            self.webdav_apply_settings_locked(&journal.settings_before, &journal.settings_after)?;
        }
        engine
            .acknowledge(&prepared.token)
            .map_err(|e| e.to_string())?;
        std::fs::remove_file(journal_path).map_err(|_| "无法完成 WebDAV 本机应用日志。")?;
        {
            let mut inner = self.inner.lock();
            inner.extras.favor =
                crate::config_compat::load(&paths.configs.join("FavorConfig.json"))?;
        }
        let upsert_ids: Vec<_> = prepared
            .snapshot
            .entries
            .iter()
            .filter(|(key, value)| {
                key.starts_with("favor/") && bundle.entries.get(*key) != Some(*value)
            })
            .map(|(key, _)| key.trim_start_matches("favor/").to_owned())
            .collect();
        let remove_ids: Vec<_> = bundle
            .entries
            .keys()
            .filter(|key| !prepared.snapshot.entries.contains_key(*key))
            .map(|key| key.trim_start_matches("favor/").to_owned())
            .collect();
        self.emit_favorites_changed(&upsert_ids, &remove_ids);
        let report = json!({"ok":true,"report":prepared.report});
        crate::config_file::save(&report_path(paths), &report)?;
        Ok(report)
    }

    fn webdav_record_conflicts(
        &self,
        paths: &AppPaths,
        prepared: &PreparedSync,
    ) -> Result<Value, String> {
        let report = json!({"ok":false,"requiresConflictResolution":true,"report":prepared.report,"error":"收藏存在并发修改，双方版本均已保留。请通过开发工具导出冲突并选择保留版本。"});
        crate::config_file::save(&report_path(paths), &report)?;
        Ok(report)
    }

    pub fn start_webdav_dev_worker(&self) {
        let paths = self.inner.lock().paths.clone();
        if !config::load(&config_path(&paths))
            .ok()
            .flatten()
            .is_some_and(|config| config.enabled)
        {
            return;
        }
        if self.webdav_worker_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let host = self.clone();
        let started = self.webdav_worker_started.clone();
        let result = std::thread::Builder::new()
            .name("webdav-dev".into())
            .spawn(move || {
                let mut due = Instant::now() + Duration::from_millis(500);
                let mut failures = 0u32;
                loop {
                    if Arc::strong_count(&host.inner) <= 1 {
                        break;
                    }
                    if Instant::now() >= due {
                        let paths = host.inner.lock().paths.clone();
                        if let Ok(Some(config)) = config::load(&config_path(&paths)) {
                            if config.enabled {
                                match host.webdav_dev_command(&json!({"command":"sync"})) {
                                    Ok(_) => failures = 0,
                                    Err(error) => {
                                        failures = (failures + 1).min(6);
                                        if error.contains("authentication_failed") {
                                            // A user may have replaced the password just
                                            // after the request returned. Do not overwrite
                                            // newer protected settings with this old copy.
                                            let _operation = host.webdav_operation.lock();
                                            if let Ok(Some(mut latest)) =
                                                config::load(&config_path(&paths))
                                            {
                                                if latest.profile_id == config.profile_id
                                                    && latest.password == config.password
                                                    && latest.username == config.username
                                                {
                                                    latest.enabled = false;
                                                    let _ =
                                                        config::save(&config_path(&paths), &latest);
                                                }
                                            }
                                        }
                                    }
                                }
                                due = Instant::now()
                                    + Duration::from_secs(
                                        config
                                            .interval_seconds
                                            .saturating_mul(1 << failures)
                                            .min(3600),
                                    );
                            } else {
                                due = Instant::now() + Duration::from_secs(2);
                            }
                        } else {
                            due = Instant::now() + Duration::from_secs(2);
                        }
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
                started.store(false, Ordering::Release);
            });
        if result.is_err() {
            self.webdav_worker_started.store(false, Ordering::Release);
        }
    }
}

#[cfg(test)]
#[path = "webdav_sync_host_tests.rs"]
mod tests;
