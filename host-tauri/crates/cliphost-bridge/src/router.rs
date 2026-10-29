use cliphost_core::HostState;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::methods::HostBridgeMethods;

#[derive(Debug, Clone, Deserialize)]
pub struct HostRequest {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub method: String,
    pub params: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HostError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct HostResponse {
    pub id: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<HostError>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostPushEnvelope {
    #[serde(rename = "type")]
    pub kind: String,
    pub event_name: String,
    pub data: Value,
}

impl HostPushEnvelope {
    pub fn event(event_name: impl Into<String>, data: Value) -> Self {
        Self {
            kind: "event".into(),
            event_name: event_name.into(),
            data,
        }
    }
}

fn bad_request(id: impl Into<String>, message: impl Into<String>) -> HostResponse {
    HostResponse {
        id: id.into(),
        ok: false,
        result: None,
        error: Some(HostError {
            code: "BAD_REQUEST".into(),
            message: message.into(),
        }),
    }
}

pub fn handle_request(state: &HostState, raw: &str) -> HostResponse {
    // C# `ClipHostBridgeRouter.HandleRequestAsync`: parse exception → ex.Message;
    // null / missing / whitespace id|method → "Invalid RPC request."
    let req = match serde_json::from_str::<Option<HostRequest>>(raw) {
        Ok(Some(req)) => req,
        Ok(None) => return bad_request(String::new(), "Invalid RPC request."),
        Err(err) => return bad_request(String::new(), err.to_string()),
    };
    if req.id.trim().is_empty() || req.method.trim().is_empty() {
        return bad_request(req.id, "Invalid RPC request.");
    }
    let params = req.params.clone().unwrap_or(Value::Null);
    match dispatch(state, &req.method, &params) {
        Ok(result) => HostResponse {
            id: req.id,
            ok: true,
            result: Some(result),
            error: None,
        },
        Err(err) => {
            let code = classify_host_error(&err);
            HostResponse {
                id: req.id,
                ok: false,
                result: None,
                error: Some(HostError {
                    code: code.into(),
                    message: err.strip_prefix("WRITE_CONFLICT:").unwrap_or(&err).to_string(),
                }),
            }
        }
    }
}

fn dispatch(state: &HostState, method: &str, params: &Value) -> Result<Value, String> {
    match method {
        HostBridgeMethods::PING => Ok(state.ping()),
        HostBridgeMethods::CLIP_WEB_SESSION_CREATE => Ok(state.create_session()),
        HostBridgeMethods::CLIP_WEB_SESSION_DELETE => state.delete_session(params),
        HostBridgeMethods::CLIP_ITEMS_SEARCH => state.search_items(params),
        HostBridgeMethods::CLIP_ITEMS_PASTE => state.paste_item(params),
        HostBridgeMethods::CLIP_ITEMS_PASTE_BATCH => state.paste_items(params),
        HostBridgeMethods::CLIP_ITEMS_PASTE_PLAIN_TEXT => state.paste_plain(params),
        HostBridgeMethods::CLIP_ITEMS_PASTE_STANDARD_TEXT => state.paste_standard(params),
        HostBridgeMethods::CLIP_ITEMS_PIN_TOGGLE => state.toggle_pin(params),
        HostBridgeMethods::CLIP_ITEMS_DELETE => state.delete_item(params),
        HostBridgeMethods::CLIP_ITEMS_CLEAR_ALL => state.clear_all(false),
        HostBridgeMethods::CLIP_ITEMS_CLEAR_NON_TOP => state.clear_all(true),
        HostBridgeMethods::CLIP_ITEMS_CLEAR_CANCEL => state.clear_cancel(params),
        HostBridgeMethods::CLIP_ITEMS_CLEAR_PENDING_GET => Ok(state.clear_pending()),
        HostBridgeMethods::CLIP_ITEMS_COPY => state.copy_items(params),
        HostBridgeMethods::CLIP_ITEMS_DETAIL_GET => state.detail(params),
        HostBridgeMethods::CLIP_ITEMS_GET_BY_IDS => state.get_by_ids(params),
        HostBridgeMethods::CLIP_ITEMS_PATCH => state.patch_item(params),
        HostBridgeMethods::CLIP_ITEMS_FILES_LIST => state.files_list(params),
        HostBridgeMethods::CLIP_ITEMS_ADD_TEXT => state.add_text(params),
        HostBridgeMethods::CLIP_ITEMS_PIN_OVERLAY => state.pin_overlay(params),
        HostBridgeMethods::CLIP_ITEMS_COPY_GIF => state.copy_gif(params),
        HostBridgeMethods::CLIP_ITEMS_SPLIT_LINES => state.split_lines(params),
        HostBridgeMethods::CLIP_ITEMS_SPLIT_FILES => state.split_files(params),
        HostBridgeMethods::CLIP_ITEMS_REVEAL => state.reveal_item(params),
        HostBridgeMethods::CLIP_ITEMS_OPEN_PATH => state.open_item_path(params),
        HostBridgeMethods::CLIP_SHELL_OPEN_PATH => state.shell_open_path(params),
        HostBridgeMethods::CLIP_SHELL_REVEAL_PATH => state.shell_reveal(params),
        HostBridgeMethods::CLIP_SHELL_OPEN_URI => state.shell_open_uri(params),
        HostBridgeMethods::CLIP_PATH_EXISTENCE_GET => state.path_exists(params),
        HostBridgeMethods::CLIP_ITEMS_DISTINCT => state.distinct(params),
        HostBridgeMethods::CLIP_CONFIG_GET => Ok(state.config_get()),
        HostBridgeMethods::CLIP_FILTER_VIEW_GET => Ok(state.filter_view()),
        HostBridgeMethods::CLIP_PIN_OVERLAY_STATUS_GET => Ok(state.pin_overlay_status()),
        HostBridgeMethods::CLIP_SOURCE_PROCESSES_LIST => state.source_processes(),
        HostBridgeMethods::CLIP_PROCESS_FILTER_PRESETS_GET => state.process_filter_presets_get(),
        HostBridgeMethods::CLIP_PROCESS_FILTER_PRESETS_SAVE => state.process_filter_presets_save(params),
        HostBridgeMethods::CLIP_PROCESS_FILTER_PRESETS_IMPORT => state.process_filter_presets_import(params),
        HostBridgeMethods::CLIP_WINDOW_HIDE => Ok(state.window_hide()),
        HostBridgeMethods::CLIP_FIFO_PREVIEW => state.fifo_preview(params),
        HostBridgeMethods::CLIP_FIFO_ARM => state.fifo_arm(params),
        HostBridgeMethods::CLIP_FIFO_CANCEL => Ok(state.fifo_cancel()),
        HostBridgeMethods::CLIP_FIFO_STATUS => Ok(state.fifo_status()),
        HostBridgeMethods::CLIP_ITEMS_FAVORITE => state.favorite_from_clip(params),
        HostBridgeMethods::FAVOR_SEARCH => state.favor_search(params),
        HostBridgeMethods::FAVOR_TREE_GET => state.favor_tree(),
        HostBridgeMethods::FAVOR_ITEM_GET => state.favor_get(params),
        HostBridgeMethods::FAVOR_ITEM_GET_BY_IDS => state.favor_get_by_ids(params),
        HostBridgeMethods::FAVOR_ITEM_COPY => state.favor_copy(params),
        HostBridgeMethods::FAVOR_ITEM_PASTE => state.favor_paste(params),
        HostBridgeMethods::FAVOR_ITEM_DELETE => state.favor_delete(params),
        HostBridgeMethods::FAVOR_ITEM_UPDATE => state.favor_update(params),
        HostBridgeMethods::FAVOR_FOLDER_CREATE => state.favor_folder_create(params),
        HostBridgeMethods::FAVOR_PATHS_ADD => state.favor_add_paths(params),
        HostBridgeMethods::FAVOR_TEXT_ITEMS_ADD => state.favor_add_text_items(params),
        HostBridgeMethods::FAVOR_LINK_ADD => state.favor_add_link(params),
        HostBridgeMethods::FAVOR_CLIPBOARD_PASTE_OPTIONS_GET => Ok(state.favor_clipboard_options()),
        HostBridgeMethods::FAVOR_FROM_CLIPBOARD_ADD => state.favor_from_clipboard(params),
        HostBridgeMethods::FAVOR_BATCH_DELETE => state.favor_batch_delete(params),
        HostBridgeMethods::FAVOR_ADD_TEXT => state.favor_add_text(params),
        HostBridgeMethods::SETTINGS_CATALOG_GET => {
            let catalog = state.settings_catalog();
            #[cfg(feature = "private-sync")]
            let mut catalog = catalog;
            #[cfg(feature = "private-sync")]
            if let Some(configs) = catalog.get_mut("configs").and_then(Value::as_array_mut) {
                configs.push(serde_json::json!({"id":"mobileSync","label":"手机同步"}));
            }
            Ok(catalog)
        }
        HostBridgeMethods::SETTINGS_FORM_GET => state.settings_form_get(params),
        HostBridgeMethods::SETTINGS_FORM_UPDATE => state.settings_form_update(params),
        HostBridgeMethods::SETTINGS_PROCESS_OPTIONS_SEARCH => state.process_options(params),
        HostBridgeMethods::CLIP_CONFIG_UPDATE => state.config_update(params),
        HostBridgeMethods::CLIP_FILTERS_GET => Ok(state.filters_get()),
        HostBridgeMethods::PANEL_CATALOG_GET => state.panel_catalog(),
        HostBridgeMethods::PANEL_LAYOUT_GET => state.panel_layout(),
        HostBridgeMethods::PANEL_PAGE_GET => state.panel_page(params),
        HostBridgeMethods::PANEL_ITEM_INVOKE => state.panel_invoke(params),
        HostBridgeMethods::PANEL_REORDER => state.panel_reorder(params),
        HostBridgeMethods::PANEL_MOVE => state.panel_move(params),
        HostBridgeMethods::PANEL_SHOW => state.panel_show(params),
        HostBridgeMethods::PANEL_CONTEXT_MENU => state.panel_context_menu(params),
        HostBridgeMethods::FAVOR_CLOUD_SYNC_STATUS_GET => Ok(state.cloud_status()),
        HostBridgeMethods::FAVOR_CLOUD_SYNC_PROGRESS_GET => Ok(state.cloud_progress()),
        HostBridgeMethods::FAVOR_CLOUD_SYNC_LOGS_GET => Ok(state.cloud_logs(params)),
        HostBridgeMethods::FAVOR_CLOUD_SYNC_RUN => state.cloud_run(),
        HostBridgeMethods::FAVOR_CLOUD_SYNC_CLEANUP => state.cloud_cleanup(),
        HostBridgeMethods::FAVOR_CLOUD_SYNC_PURGE => state.cloud_purge(),
        HostBridgeMethods::CLIP_UI_PICK_FOLDER => state.pick_folder(params),
        HostBridgeMethods::CLIP_UI_PICK_FILES => state.pick_files(params),
        HostBridgeMethods::CLIP_UI_PICK_DIRECTORIES => state.pick_directories(params),
        HostBridgeMethods::FAVOR_COLLECT_DRAFT_GET => state.collect_get(params),
        HostBridgeMethods::FAVOR_COLLECT_DRAFT_COMMIT => state.collect_commit(params),
        HostBridgeMethods::FAVOR_COLLECT_DRAFT_DISCARD => state.collect_discard(params),
        HostBridgeMethods::QUICKER_STEPS_SUMMARY_GET => state.quicker_summary(params),
        HostBridgeMethods::QUICKER_STEPS_PRESENTATION_GET => state.quicker_presentation(params),
        HostBridgeMethods::QUICKER_ACTION_METADATA_BATCH_GET => state.quicker_metadata_batch(params),
        HostBridgeMethods::TEXT_ACTION_LIST_GET => Ok(state.text_action_list()),
        HostBridgeMethods::TEXT_ACTION_ITEM_UPSERT => state.text_action_upsert(params),
        HostBridgeMethods::TEXT_ACTION_ITEM_DELETE => state.text_action_delete(params),
        #[cfg(feature = "private-sync")]
        HostBridgeMethods::MOBILE_SYNC_STATUS_GET => Ok(state.sync_status()),
        #[cfg(feature = "private-sync")]
        HostBridgeMethods::MOBILE_SYNC_CONFIG_UPDATE => state.sync_config_update(params),
        #[cfg(feature = "private-sync")]
        HostBridgeMethods::MOBILE_SYNC_INVITATION_GET => state.sync_invitation(),
        #[cfg(feature = "private-sync")]
        HostBridgeMethods::MOBILE_SYNC_CLIENT_FORGET => {
            let device = params.get("deviceId").and_then(Value::as_str).ok_or("deviceId is required")?;
            state.sync_forget_client(device)?;
            Ok(serde_json::json!({"ok": true}))
        }
        HostBridgeMethods::DEVICE_SYNC_STATUS_GET => Ok(state.device_sync_status()),
        HostBridgeMethods::DEVICE_SYNC_CONFIG_UPDATE => state.device_sync_config_update(params),
        HostBridgeMethods::LAN_STATUS_GET => Ok(state.lan_status()),
        HostBridgeMethods::LAN_CONFIG_UPDATE => state.lan_config_update(params),
        HostBridgeMethods::LAN_PAIRING_CODE_REFRESH => Ok(state.lan_refresh_code()),
        HostBridgeMethods::LAN_PEERS_LIST => Ok(state.lan_peers()),
        HostBridgeMethods::LAN_PEERS_DISCOVER => state.lan_discover(),
        HostBridgeMethods::LAN_PEER_PAIR => state.lan_pair(params),
        HostBridgeMethods::LAN_PEER_FORGET => state.lan_forget(params),
        HostBridgeMethods::LAN_CONVERSATIONS_LIST => Ok(state.lan_conversations()),
        HostBridgeMethods::LAN_MESSAGES_LIST => state.lan_messages(params),
        HostBridgeMethods::LAN_MESSAGE_SEND => state.lan_send(params),
        HostBridgeMethods::LAN_CLIP_ITEMS_SEND => state.lan_send_clips(params),
        HostBridgeMethods::LAN_MESSAGE_RETRY => state.lan_retry(params),
        HostBridgeMethods::LAN_MESSAGE_CANCEL => state.lan_cancel(params),
        HostBridgeMethods::LAN_CONVERSATION_READ => state.lan_read(params),
        HostBridgeMethods::LAN_ATTACHMENT_OPEN => state.lan_attachment_open(params),
        HostBridgeMethods::LAN_ATTACHMENT_REVEAL => state.lan_attachment_reveal(params),
        _ => Err(format!("Unknown method: {method}")),
    }
}

/// C# `HostBridgeErrorCodes`: NotSupportedException → NOT_SUPPORTED, KeyNotFoundException → NOT_FOUND.
fn classify_host_error(message: &str) -> &'static str {
    if message.starts_with("WRITE_CONFLICT:") {
        "WRITE_CONFLICT"
    } else if message.starts_with("Unknown method:") || message.starts_with("NOT_SUPPORTED:") {
        "NOT_SUPPORTED"
    } else if message.to_ascii_lowercase().contains("not found")
        || message.starts_with("Unknown session")
        || message.contains("Unknown ")
    {
        "NOT_FOUND"
    } else {
        "INTERNAL_ERROR"
    }
}

/// Push event name for a successful mutating RPC, matching C# clipChanged / favoritesChanged.
pub fn rpc_event_name(method: &str) -> Option<&'static str> {
    const FAVOR_MUTATE: &[&str] = &[
        "clip.items.favorite",
        "favor.item.delete",
        "favor.item.update",
        "favor.folder.create",
        "favor.paths.add",
        "favor.textItems.add",
        "favor.link.add",
        "favor.fromClipboard.add",
        "favor.batchDelete",
        "panel.reorder",
        "panel.move",
    ];
    if FAVOR_MUTATE.contains(&method)
        || method.starts_with("clip.favorites.")
        || method.starts_with("clip.panel.")
    {
        if method.ends_with(".get")
            || method.ends_with(".list")
            || method.ends_with(".search")
            || method.ends_with(".status")
        {
            return None;
        }
        return Some("favoritesChanged");
    }
    if method.starts_with("clip.items.") {
        const READ: &[&str] = &[
            "clip.items.search",
            "clip.items.detail.get",
            "clip.items.getByIds",
            "clip.items.files.list",
            "clip.items.clear.pending.get",
            "clip.items.pinOverlay",
        ];
        if READ.contains(&method) {
            return None;
        }
        return Some("clipChanged");
    }
    if method == "favor.addText" {
        return Some("clipChanged");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use cliphost_core::paths::AppPaths;
    use tempfile::tempdir;

    fn state() -> HostState {
        cliphost_core::force_headless(true);
        std::env::set_var("CLIPHOST_TAURI_HEADLESS", "1");
        let dir = tempdir().unwrap();
        let paths = AppPaths::from_data_root(dir.path().join("BetterClipTauri"));
        HostState::open(paths, 28965).unwrap()
    }

    #[test]
    fn device_sync_controls_and_received_file_history_are_routed() {
        cliphost_core::force_headless(true);
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../.tmp/device-sync-bridge-tests");
        std::fs::create_dir_all(&root).unwrap();
        let directory = tempfile::tempdir_in(root).unwrap();
        let host = HostState::open(AppPaths::from_data_root(directory.path().join("host")), 28965).unwrap();
        let rpc = |method: &str, params: Value| handle_request(&host, &serde_json::json!({
            "id":"device-sync-test", "method":method, "params":params,
        }).to_string());
        let status = rpc("deviceSync.status.get", serde_json::json!({}));
        assert!(status.ok, "{status:?}");
        assert_eq!(status.result.unwrap()["enabled"], true);
        let paused = rpc("deviceSync.config.update", serde_json::json!({"enabled":false}));
        assert!(paused.ok, "{paused:?}");
        assert_eq!(paused.result.unwrap()["enabled"], false);
        let invalid = rpc("deviceSync.config.update", serde_json::json!({"enabled":"yes"}));
        assert!(!invalid.ok);
        assert_eq!(rpc("deviceSync.status.get", serde_json::json!({})).result.unwrap()["enabled"], false);
        let missing_peer = rpc("deviceSync.config.update", serde_json::json!({"deviceId":"unpaired", "sendEnabled":true}));
        assert!(!missing_peer.ok, "automatic sharing needs a paired device");
        let files = rpc("lan.config.update", serde_json::json!({"receivedFilesToHistory":true}));
        assert!(files.ok, "{files:?}");
        assert_eq!(rpc("lan.status.get", serde_json::json!({})).result.unwrap()["receivedFilesToHistory"], true);
        assert_eq!(rpc("deviceSync.status.get", serde_json::json!({})).result.unwrap()["enabled"], false,
            "manual file history must not resume explicitly paused clipboard sharing");
    }

    #[test]
    fn all_idl_methods_are_routed() {
        use crate::methods::ALL_METHODS;
        use std::sync::mpsc;
        use std::time::Duration;
        let host = state();
        for method in ALL_METHODS {
            let raw = serde_json::json!({ "id": "t", "method": method, "params": {} }).to_string();
            let host = host.clone();
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(handle_request(&host, &raw));
            });
            let response = rx
                .recv_timeout(Duration::from_secs(3))
                .unwrap_or_else(|_| panic!("host method hung: {method}"));
            let private_transport_excluded = !cfg!(feature = "private-sync") && method.starts_with("mobileSync.");
            if let Some(error) = response.error {
                if private_transport_excluded {
                    assert_eq!(error.code, "NOT_SUPPORTED", "private transport {method} must be excluded");
                } else {
                    assert_ne!(error.code, "NOT_SUPPORTED", "{method} should be routed");
                }
            } else {
                assert!(!private_transport_excluded, "private transport {method} must be excluded");
            }
        }
    }

    #[test]
    fn unknown_method_matches_csharp_not_supported() {
        std::env::set_var("CLIPHOST_TAURI_HEADLESS", "1");
        let host = state();
        let response = handle_request(&host, r#"{"id":"x","method":"no.such.method"}"#);
        assert!(!response.ok);
        let error = response.error.expect("error");
        assert_eq!(error.code, "NOT_SUPPORTED");
        assert_eq!(error.message, "Unknown method: no.such.method");
    }

    #[test]
    fn unknown_session_matches_csharp_not_found() {
        std::env::set_var("CLIPHOST_TAURI_HEADLESS", "1");
        let host = state();
        let response = handle_request(
            &host,
            r#"{"id":"x","method":"clip.webSession.delete","params":{"sessionId":"missing"}}"#,
        );
        assert!(!response.ok);
        let error = response.error.expect("error");
        assert_eq!(error.code, "NOT_FOUND");
        assert_eq!(error.message, "Unknown session.");
    }

    #[test]
    fn invalid_rpc_request_matches_csharp_bad_request() {
        std::env::set_var("CLIPHOST_TAURI_HEADLESS", "1");
        let host = state();
        for raw in [
            "{}",
            r#"{"id":"1"}"#,
            r#"{"id":"1","method":"  "}"#,
            "null",
        ] {
            let response = handle_request(&host, raw);
            assert!(!response.ok, "{raw}");
            let error = response.error.expect("error");
            assert_eq!(error.code, "BAD_REQUEST", "{raw}");
            assert_eq!(error.message, "Invalid RPC request.", "{raw}");
        }
        let malformed = handle_request(&host, "not-json");
        assert!(!malformed.ok);
        let error = malformed.error.expect("error");
        assert_eq!(error.code, "BAD_REQUEST");
        assert_ne!(error.message, "Invalid JSON request.");
        assert_ne!(error.message, "Invalid RPC request.");
    }

    #[test]
    fn ping_and_search_roundtrip() {
        std::env::set_var("CLIPHOST_TAURI_HEADLESS", "1");
        let host = state();
        let ping = handle_request(&host, r#"{"id":"1","method":"host.ping"}"#);
        assert!(ping.ok);
        let session = handle_request(&host, r#"{"id":"2","method":"clip.webSession.create"}"#);
        let session_id = session.result.unwrap()["sessionId"].as_str().unwrap().to_string();
        handle_request(
            &host,
            &serde_json::json!({
                "id": "3",
                "method": "clip.items.addText",
                "params": { "text": "hello-tauri" }
            })
            .to_string(),
        );
        let search = handle_request(
            &host,
            &serde_json::json!({
                "id": "4",
                "method": "clip.items.search",
                "params": { "webSessionId": session_id, "query": "hello", "take": 20 }
            })
            .to_string(),
        );
        assert!(search.ok, "{search:?}");
        assert!(search.result.unwrap()["total"].as_i64().unwrap() >= 1);
    }

    #[test]
    fn disabled_cloud_mutations_return_error_envelopes() {
        std::env::set_var("CLIPHOST_TAURI_HEADLESS", "1");
        let host = state();
        for method in ["favor.cloudSync.run", "favor.cloudSync.cleanup", "favor.cloudSync.purge"] {
            let response = handle_request(&host, &serde_json::json!({"id":"cloud-gate","method":method,"params":{"confirmed":true}}).to_string());
            assert!(!response.ok, "{method}");
            assert_eq!(response.error.unwrap().message, "云同步功能暂时下架。");
            assert!(response.result.is_none());
        }
        let response = handle_request(&host, r#"{"id":"cloud-status","method":"favor.cloudSync.status.get"}"#);
        assert!(response.ok);
        assert_eq!(response.result.unwrap()["cloudSyncHostEnabled"], false);
        assert!(!host.data_root().join("cloud").exists());
    }

    #[test]
    fn mutating_clip_rpc_emits_clip_changed() {
        assert_eq!(rpc_event_name("clip.items.addText"), Some("clipChanged"));
        assert_eq!(rpc_event_name("clip.items.search"), None);
        assert_eq!(rpc_event_name("clip.items.favorite"), Some("favoritesChanged"));
        assert_eq!(rpc_event_name("favor.item.delete"), Some("favoritesChanged"));
        assert_eq!(rpc_event_name("favor.search"), None);
        assert_eq!(rpc_event_name("favor.addText"), Some("clipChanged"));
        assert_eq!(rpc_event_name("clip.config.update"), None);
        assert_eq!(rpc_event_name("clip.window.hide"), None);
        assert_eq!(rpc_event_name("clip.favorites.item.delete"), Some("favoritesChanged"));
        assert_eq!(rpc_event_name("clip.favorites.tree.get"), None);
    }

    #[test]
    fn editor_revision_rejects_stale_clip_and_favorite_drafts() {
        cliphost_core::force_headless(true);
        let dir = tempdir().unwrap();
        let host = HostState::open(AppPaths::from_data_root(dir.path().join("BetterClipTauri")), 28965).unwrap();
        for (create, params, get, update, event) in [
            ("clip.items.addText", serde_json::json!({ "text": "original" }),
                "clip.items.detail.get", "clip.items.patch", "clipChanged"),
            ("favor.textItems.add", serde_json::json!({ "items": [{ "text": "original" }] }),
                "favor.item.get", "favor.item.update", "favoritesChanged"),
        ] {
            let rpc = |method: &str, params: Value| handle_request(&host, &serde_json::json!({
                "id": "editor", "method": method, "params": params,
            }).to_string());
            let added = rpc(create, params).result.unwrap();
            let id = &added["createdIds"][0];
            let original = rpc(get, serde_json::json!({ "id": id })).result.unwrap();
            let revision = &original["revision"];
            assert!(revision.as_str().unwrap().starts_with("sha256:"));
            assert_eq!(revision, &cliphost_core::dto::edit_revision(original["rawJson"].as_str().unwrap()));
            host.drain_push_events();

            let saved = rpc(update, serde_json::json!({
                "id": id, "expectedRevision": revision, "bodyText": "saved in first window",
            }));
            assert!(saved.ok, "{saved:?}");
            let saved = saved.result.unwrap();
            assert_ne!(saved["revision"], *revision);
            assert_eq!(host.drain_push_events().iter().filter(|(name, _)| name == event).count(), 1);

            let stale = rpc(update, serde_json::json!({
                "id": id, "expectedRevision": revision, "bodyText": "stale draft", "title": "stale title",
            }));
            assert!(!stale.ok);
            assert!(stale.result.is_none());
            let error = stale.error.unwrap();
            assert_eq!(error.code, "WRITE_CONFLICT");
            assert!(error.message.contains("本次操作未保存"));
            assert!(!error.message.starts_with("WRITE_CONFLICT:"));
            assert_eq!(rpc(get, serde_json::json!({ "id": id })).result.unwrap(), saved);
            assert!(host.drain_push_events().is_empty(), "rejected drafts must not announce a write");

            // The returned revision is the baseline for the next save, even before React reloads.
            let next = rpc(update, serde_json::json!({
                "id": id, "expectedRevision": saved["revision"], "bodyText": "next save",
            }));
            assert!(next.ok, "{next:?}");
            assert_eq!(next.result.unwrap()["bodyText"], "next save");
        }
    }

    #[test]
    fn concurrent_editor_revision_saves_have_only_one_winner() {
        use std::sync::{mpsc, Arc, Barrier};
        use std::time::Duration;
        cliphost_core::force_headless(true);
        let dir = tempdir().unwrap();
        let host = HostState::open(AppPaths::from_data_root(dir.path().join("BetterClipTauri")), 28965).unwrap();
        for favorite in [false, true] {
            let added = if favorite {
                host.favor_add_text_items(&serde_json::json!({ "items": [{ "text": "original" }] }))
            } else {
                host.add_text(&serde_json::json!({ "text": "original" }))
            }.unwrap();
            let id = added["createdIds"][0].as_str().unwrap().to_string();
            let params = serde_json::json!({ "id": id });
            let detail = if favorite { host.favor_get(&params) } else { host.detail(&params) }.unwrap();
            host.drain_push_events();
            let barrier = Arc::new(Barrier::new(3));
            let (tx, rx) = mpsc::channel();
            for body in ["first", "second"] {
                let host = host.clone();
                let barrier = barrier.clone();
                let tx = tx.clone();
                let params = serde_json::json!({ "id": id, "expectedRevision": detail["revision"], "bodyText": body });
                std::thread::spawn(move || {
                    barrier.wait();
                    let result = if favorite { host.favor_update(&params) } else { host.patch_item(&params) };
                    let _ = tx.send(result);
                });
            }
            barrier.wait();
            let results: Vec<_> = (0..2).map(|_| rx.recv_timeout(Duration::from_secs(5)).unwrap()).collect();
            assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
            assert!(results.iter().find_map(|result| result.as_ref().err()).unwrap().starts_with("WRITE_CONFLICT:"));
            let detail = if favorite { host.favor_get(&params) } else { host.detail(&params) }.unwrap();
            assert_eq!(detail, *results.iter().find_map(|result| result.as_ref().ok()).unwrap());
            assert_eq!(host.drain_push_events().len(), 1);
        }
    }

    #[test]
    fn folder_rename_dialog_rpc_roundtrip_and_conflict() {
        cliphost_core::force_headless(true);
        let dir = tempdir().unwrap();
        let host = HostState::open(AppPaths::from_data_root(dir.path().join("BetterClipTauri")), 28965).unwrap();
        let rpc = |method: &str, params: Value| handle_request(&host, &serde_json::json!({
            "id": "folder-dialog", "method": method, "params": params
        }).to_string());
        let folder = rpc("favor.folder.create", serde_json::json!({"title": "原标题"})).result.unwrap();
        let id = folder["id"].as_str().unwrap();
        let session = rpc("clip.webSession.create", serde_json::json!({"pendingFavorId": id})).result.unwrap();
        let current = rpc("favor.item.get", serde_json::json!({"id": id})).result.unwrap();
        let saved = rpc("favor.item.update", serde_json::json!({
            "id": id, "title": "新标题", "expectedRevision": current["revision"]
        }));
        assert!(saved.ok, "folder rename failed: {saved:?}");
        assert_eq!(saved.result.unwrap()["title"], "新标题");
        assert!(rpc("clip.webSession.delete", serde_json::json!({"sessionId": session["sessionId"]})).ok);
        let stale = rpc("favor.item.update", serde_json::json!({
            "id": id, "title": "旧草稿", "expectedRevision": current["revision"]
        }));
        assert!(!stale.ok);
        assert_eq!(stale.error.unwrap().code, "WRITE_CONFLICT");
        assert_eq!(rpc("favor.item.get", serde_json::json!({"id": id})).result.unwrap()["title"], "新标题");
        assert_eq!(rpc("panel.catalog.get", serde_json::json!({})).result.unwrap()["panels"][0]["title"], "新标题");
    }
}
