//! Native row clicks and shortcut arming use the existing QP session. Mount as
//! a child of host.rs; no root clipboard-window input ticket is accepted here.
use super::*;
use crate::overlay_row_actions::{RowActionKind, RowItem, row_actions};

pub(super) const QP_ROW_RETIRED: &str = "便捷粘贴会话已失效，已停止操作。";

#[derive(Clone)]
struct NativeQpRowRequest {
    presentation_live: Option<Arc<AtomicBool>>,
    source: isize,
    target: Option<InputTarget>,
    foreground: isize,
    session_revision: i32,
    state_revision: Option<i32>,
    session_generation: u64,
    hook_generation: u64,
    cancel_generation: u64,
    fifo_generation: Option<u64>,
}

fn record_row(item: &ClipRecord) -> RowItem {
    let dto = serde_json::to_value(clip_to_dto(item, None)).unwrap_or(Value::Null);
    RowItem {
        kind: dto["kind"].as_str().unwrap_or("").to_string(),
        display_text: dto["displayText"].as_str().unwrap_or("").to_string(),
        summary: dto["summary"].as_str().unwrap_or("").to_string(),
        title: dto["title"].as_str().unwrap_or("").to_string(),
        file_paths: dto["filePaths"]
            .as_array()
            .map(|paths| {
                paths
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        file_count: dto["fileCount"].as_u64().unwrap_or(0) as usize,
    }
}

fn selected_actions(
    inner: &HostInner,
    item: &ClipRecord,
) -> Vec<crate::overlay_row_actions::RowAction> {
    let provider = if inner.qp.snipaste_available {
        Some("snipaste")
    } else if inner.qp.pixpin_available {
        Some("pixpin")
    } else {
        None
    };
    row_actions(
        &record_row(item),
        [
            &inner.qp_config.shortcut1_action,
            &inner.qp_config.shortcut2_action,
        ],
        provider,
    )
}

/// Call from qp_apply_command for ArmRowAction { shortcut }. The hook merely
/// queues that command; provider probing, record access and input stay off it.
impl HostState {
    /// Replace qp_execute_armed at the keyboard release call site with this
    /// method. The old accepted selection revision/cancellation ID are passed
    /// in even though qp_end_session_locked has already hidden its surface.
    pub(super) fn qp_execute_armed_native(
        &self,
        armed: &QpArmedAction,
        item: &ClipRecord,
        target: isize,
        generation: Option<u64>,
        session_revision: i32,
        cancel_generation: u64,
        hook_generation: u64,
    ) -> Result<(), String> {
        let (action, request, opts) = {
            let inner = self.inner.lock();
            let action = selected_actions(&inner, item)
                .into_iter()
                .find(|action| {
                    action.kind.name() == armed.kind
                        && match &action.kind {
                            RowActionKind::Pin { provider } => {
                                armed.pin_provider.as_deref() == Some(provider)
                            }
                            RowActionKind::Open { target_kind, value } => {
                                armed.open_target_kind.as_deref() == Some(*target_kind)
                                    && armed.open_target_value.as_deref() == Some(value)
                            }
                            RowActionKind::Simulate => true,
                        }
                })
                .ok_or("此快捷操作当前不可用。")?;
            let request = NativeQpRowRequest {
                presentation_live: None,
                source: inner.qp_hwnd,
                target: inner
                    .qp
                    .captured_target
                    .filter(|captured| captured.hwnd() == target)
                    .or_else(|| {
                        crate::clipboard::is_headless()
                            .then(|| InputTarget::capture(target).ok())
                            .flatten()
                    }),
                foreground: native_paste_foreground(),
                session_revision,
                state_revision: None,
                session_generation: inner.qp.session_generation,
                hook_generation,
                cancel_generation,
                fifo_generation: generation,
            };
            (action, request, paste_options_from_config(&inner.config))
        };
        self.verify_qp_row_request(&request)?;
        {
            let mut inner = self.inner.lock();
            if !qp_row_fields_current(&inner, &request) {
                return Err(QP_ROW_RETIRED.into());
            }
            inner.qp.last_armed_kind = Some(armed.kind.clone());
        }
        self.execute_qp_row_action(item, &action.kind, &request, opts)?;
        if matches!(action.kind, RowActionKind::Open { .. }) {
            self.touch_clip_usage(&item.id);
        }
        Ok(())
    }

    pub fn qp_arm_native_shortcut(&self, shortcut: usize) {
        let mut inner = self.inner.lock();
        if !inner.qp.overlay_visible {
            return;
        }
        let Some(item) = inner.qp.items.get(inner.qp.active_index).cloned() else {
            return;
        };
        let selected = selected_actions(&inner, &item)
            .into_iter()
            .find(|action| action.shortcut == shortcut);
        // An unavailable slot clears the old operation instead of leaving an
        // unrelated previously armed action ready for release.
        inner.qp.armed = selected.map(|action| {
            let mut armed = QpArmedAction {
                item_id: item.id.clone(),
                kind: action.kind.name().to_string(),
                pin_provider: None,
                open_target_kind: None,
                open_target_value: None,
            };
            match action.kind {
                RowActionKind::Pin { provider } => armed.pin_provider = Some(provider),
                RowActionKind::Open { target_kind, value } => {
                    armed.open_target_kind = Some(target_kind.to_string());
                    armed.open_target_value = Some(value);
                }
                RowActionKind::Simulate => {}
            }
            armed
        });
        inner.qp.state_revision = inner.qp.state_revision.wrapping_add(1);
        self.qp_push_state(inner);
    }

    /// qp_web_action("nativeRowAction"): click executes immediately and keeps
    /// the original surface open, as QuickPasteActionChip does in WebView.
    pub fn qp_native_row_action(&self, body: &Value) -> Value {
        self.qp_native_row_action_with_optional_lifetime(body, None)
    }

    /// The native surface's cancellation signal remains in every payload,
    /// owner-queue and input check after the UI worker has entered core.
    pub fn qp_native_row_action_with_lifetime(&self, body: &Value, live: Arc<AtomicBool>) -> Value {
        self.qp_native_row_action_with_optional_lifetime(body, Some(live))
    }

    fn qp_native_row_action_with_optional_lifetime(
        &self,
        body: &Value,
        live: Option<Arc<AtomicBool>>,
    ) -> Value {
        match self.qp_native_row_action_inner(body, live) {
            Ok(value) => value,
            Err(error) => {
                json!({ "ok": false, "reason": "native-row-action-failed", "error": error })
            }
        }
    }

    fn qp_native_row_action_inner(
        &self,
        body: &Value,
        live: Option<Arc<AtomicBool>>,
    ) -> Result<Value, String> {
        if live
            .as_ref()
            .is_some_and(|live| !live.load(Ordering::Acquire))
        {
            return Err(QP_ROW_RETIRED.into());
        }
        let id = body["itemId"].as_str().ok_or(QP_ROW_RETIRED)?;
        let shortcut = body["shortcut"].as_u64().ok_or(QP_ROW_RETIRED)? as usize;
        let foreground = native_paste_foreground();
        let (item, action, request, opts) = {
            let inner = self.inner.lock();
            if !inner.qp.overlay_visible
                || inner.qp.commit_in_flight
                || body["sessionRevision"].as_i64() != Some(inner.qp.session_revision as i64)
                || body["stateRevision"].as_i64() != Some(inner.qp.state_revision as i64)
                || body["sourceHwnd"].as_i64() != Some(inner.qp_hwnd as i64)
                || (!crate::clipboard::is_headless() && inner.qp_hwnd == 0)
            {
                return Err(QP_ROW_RETIRED.into());
            }
            let item = inner
                .qp
                .items
                .get(inner.qp.active_index)
                .filter(|item| item.id == id)
                .cloned()
                .ok_or(QP_ROW_RETIRED)?;
            // Do not trust an incoming path/provider. Resolve the configured
            // slot again from the accepted session's original record.
            let action = selected_actions(&inner, &item)
                .into_iter()
                .find(|action| action.shortcut == shortcut)
                .ok_or("此快捷操作当前不可用。")?;
            let target = inner.qp.captured_target.or_else(|| {
                crate::clipboard::is_headless()
                    .then(|| InputTarget::capture(inner.qp.captured_fg).ok())
                    .flatten()
            });
            if matches!(action.kind, RowActionKind::Simulate) && target.is_none() {
                return Err("找不到原便捷粘贴目标窗口。".into());
            }
            let request = NativeQpRowRequest {
                presentation_live: live,
                source: inner.qp_hwnd,
                target,
                foreground,
                session_revision: inner.qp.session_revision,
                state_revision: Some(inner.qp.state_revision),
                session_generation: inner.qp.session_generation,
                // Live row clicks still belong to the displayed chord. Only
                // accepted release work can preserve an already queued chord.
                hook_generation: inner.qp.session_generation,
                cancel_generation: inner.qp.cancel_generation,
                fifo_generation: inner.qp.fifo_generation,
            };
            let opts = paste_options_from_config(&inner.config);
            (item, action, request, opts)
        };
        let result = self.execute_qp_row_action(&item, &action.kind, &request, opts);
        result?;
        Ok(json!({ "ok": true, "itemId": id, "actionKind": action.kind.name() }))
    }

    fn verify_qp_row_request(&self, request: &NativeQpRowRequest) -> Result<(), String> {
        self.verify_qp_row_request_at_foreground(request, native_paste_foreground())
    }

    fn verify_qp_row_request_at_foreground(
        &self,
        request: &NativeQpRowRequest,
        foreground: isize,
    ) -> Result<(), String> {
        let inner = self.inner.lock();
        if !qp_row_fields_current(&inner, request) {
            return Err(QP_ROW_RETIRED.into());
        }
        drop(inner);
        // A captured paste target is a fallback, not a foreground lock. Input
        // actions resolve the current application when activating and sending;
        // pin/open actions need only the accepted session and record here.
        if request.target.is_none()
            && foreground != 0
            && foreground != request.source
            && foreground != request.foreground
        {
            Err("操作目标已变化，已停止操作。".into())
        } else {
            Ok(())
        }
    }

    fn execute_qp_row_action(
        &self,
        item: &ClipRecord,
        action: &RowActionKind,
        request: &NativeQpRowRequest,
        opts: PasteOptions,
    ) -> Result<(), String> {
        self.verify_qp_row_request(request)?;
        let projected=self.store().lock().core_io_clip(item).map_err(|e|e.to_string())?;
        let item=&projected;
        let host = self.clone();
        let ticket = request.clone();
        let guard: Arc<dyn Fn() -> Result<(), String> + Send + Sync> =
            Arc::new(move || host.verify_qp_row_request(&ticket));
        match action {
            RowActionKind::Simulate => {
                let text = resolve_simulate_text(item);
                if text.is_empty() {
                    return Err("没有可模拟输入的文本。".into());
                }
                let target = request.target.ok_or("找不到原便捷粘贴目标窗口。")?;
                let activating = guard.clone();
                self.run_input(move || {
                    activating()?;
                    target.activate_for_paste()?;
                    crate::paste::wait_for_modifier_release_to_with_guard(target, || activating())?;
                    activating()
                })?;
                self.run_input(move || {
                    crate::paste::simulate_text_to_with_guard(&text, 0, Some(target), || guard())
                })?;
                self.touch_clip_usage(&item.id);
                let mut inner = self.inner.lock();
                if qp_row_fields_current(&inner, request) {
                    inner.qp.last_simulated_text = Some(resolve_simulate_text(item));
                }
            }
            RowActionKind::Pin { provider } => {
                let (snipaste, pixpin) = crate::pin_overlay::status();
                if !match provider.as_str() {
                    "snipaste" => snipaste,
                    "pixpin" => pixpin,
                    _ => false,
                } {
                    return Err("贴图程序当前不可用。".into());
                }
                let paths = self.inner.lock().paths.clone();
                if !crate::pin_overlay::item_ready_for_pin(item, &paths) {
                    return Err("图片文件不可用。".into());
                }
                guard()?;
                let prepare = guard.clone();
                let write = guard.clone();
                self.run_clipboard_write_then_guarded(
                    || {
                        prepare()?;
                        crate::paste::prepare_copy_record(item, &opts)
                    },
                    |sequence| {
                        if !crate::clipboard::is_headless() {
                            std::thread::sleep(Duration::from_millis(60));
                        }
                        guard()?;
                        if sequence
                            .is_some_and(|written| written != crate::clipboard::sequence_number())
                        {
                            return Err("剪贴板内容已变化，已停止本次贴图。".into());
                        }
                        crate::pin_overlay::pin_clipboard(provider)
                    },
                    None,
                    write,
                )?;
                // Click and armed pinning both use the same successful-use
                // rule as the WebView pinOverlay path, exactly once.
                self.touch_clip_usage(&item.id);
            }
            RowActionKind::Open { target_kind, value } => {
                let projected_value=if *target_kind != "uri" && self.store().lock().is_core_store(){
                    let mut path_record=ClipRecord::new_text("","isolated-output");path_record.kind=ClipKind::FileClipItem;path_record.file_paths=vec![value.clone()];
                    self.store().lock().core_io_clip(&path_record).map_err(|e|e.to_string())?.file_paths.into_iter().next().ok_or("文件输出缺少定位。")?
                }else{value.clone()};
                let value=&projected_value;
                let full = if *target_kind == "uri" {
                    crate::shell_path::normalize_shell_uri(value)?
                } else {
                    // Resolve only the accepted record's path and validate it
                    // before the final guard; no later store lookup can change
                    // this click into opening a different record or target.
                    if matches!(
                        record_row(item).kind.as_str(),
                        "FileClipItem" | "ImageClipItem"
                    ) && !Path::new(value).exists()
                    {
                        return Err(format!("路径在磁盘上不存在或无法访问：{value}"));
                    }
                    crate::shell_path::normalize_local_shell_path(value)?
                };
                guard()?;
                self.open_shell_target(&full)?;
            }
        }
        Ok(())
    }
}

fn qp_row_fields_current(inner: &HostInner, request: &NativeQpRowRequest) -> bool {
    request
        .presentation_live
        .as_ref()
        .is_none_or(|live| live.load(Ordering::Acquire))
        && inner.qp_hwnd == request.source
        && inner.qp.session_revision == request.session_revision
        && request
            .state_revision
            .is_none_or(|revision| revision == inner.qp.state_revision)
        && inner.qp.cancel_generation == request.cancel_generation
        && inner.qp.session_generation == request.session_generation
        && inner.qp.hook_generation == request.hook_generation
        && request
            .fifo_generation
            .is_none_or(|generation| generation == inner.fifo_generation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn fixture() -> (tempfile::TempDir, HostState, Value, Arc<AtomicUsize>) {
        crate::clipboard::force_headless(true);
        let directory =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp/gpui-overlays-tests");
        std::fs::create_dir_all(&directory).unwrap();
        let root = tempfile::Builder::new()
            .prefix("qp-row-")
            .tempdir_in(directory)
            .unwrap();
        let host =
            HostState::open(AppPaths::from_data_root(root.path().to_path_buf()), 29065).unwrap();
        let id = host.add_text(&json!({ "text": "original\ntext" })).unwrap()["createdIds"][0]
            .as_str()
            .unwrap()
            .to_string();
        let item = host.store().lock().get_clip(&id).unwrap();
        {
            let mut inner = host.inner.lock();
            inner.qp.overlay_visible = true;
            inner.qp.items = vec![item.clone(), item];
            inner.qp.active_index = 0;
            inner.qp.session_revision = 4;
            inner.qp.state_revision = 8;
            inner.qp.session_generation = 2;
            inner.qp.hook_generation = 2;
            inner.qp_config.shortcut1_action = "simulate".into();
            inner.qp_config.remember_selection = true;
            inner.qp_config.selection_memory_seconds = 10;
        }
        let inputs = Arc::new(AtomicUsize::new(0));
        let counted = inputs.clone();
        host.set_input_runner(move |job| {
            counted.fetch_add(1, Ordering::SeqCst);
            job()
        });
        let body = json!({ "itemId": id, "shortcut": 1, "sessionRevision": 4, "stateRevision": 8, "sourceHwnd": 0 });
        (root, host, body, inputs)
    }

    #[test]
    fn native_row_action_keeps_presentation_cancellation_through_both_input_owner_jobs() {
        for retired_at in [0, 1, 2] {
            let (_root, host, body, _) = fixture();
            let live = Arc::new(AtomicBool::new(retired_at != 0));
            let worker_live = live.clone();
            let inputs = Arc::new(AtomicUsize::new(0));
            let counted = inputs.clone();
            host.set_input_runner(move |job| {
                if counted.fetch_add(1, Ordering::SeqCst) + 1 == retired_at {
                    worker_live.store(false, Ordering::Release);
                }
                job()
            });
            let result = host.qp_native_row_action_with_lifetime(&body, live);
            *host.input_runner.lock() = None;
            assert_eq!(result["ok"], false);
            assert_eq!(inputs.load(Ordering::SeqCst), retired_at);
            assert_eq!(
                host.store()
                    .lock()
                    .get_clip(body["itemId"].as_str().unwrap())
                    .unwrap()
                    .usage_count,
                0
            );
            assert!(
                host.inner.lock().qp.overlay_visible,
                "a destroyed view cannot dismiss a later core session"
            );
            assert!(host.inner.lock().qp.last_simulated_text.is_none());
        }
    }

    #[test]
    fn old_selection_or_reopened_session_rejects_before_activation() {
        for field in ["sessionRevision", "stateRevision", "sourceHwnd"] {
            let (_root, host, mut body, inputs) = fixture();
            body[field] = json!(99);
            let result = host.qp_native_row_action(&body);
            *host.input_runner.lock() = None;
            assert_eq!(result["ok"], false);
            assert_eq!(inputs.load(Ordering::SeqCst), 0);
            assert!(host.inner.lock().qp.overlay_visible);
        }
    }

    #[test]
    fn unavailable_slot_does_not_hide_or_dispatch() {
        let (_root, host, body, inputs) = fixture();
        host.inner.lock().qp_config.shortcut1_action = "none".into();
        assert_eq!(host.qp_native_row_action(&body)["ok"], false);
        assert_eq!(inputs.load(Ordering::SeqCst), 0);
        assert!(host.inner.lock().qp.overlay_visible);
    }

    #[test]
    fn row_click_keeps_original_surface_and_does_not_advance_release_memory() {
        let (_root, host, body, inputs) = fixture();
        assert_eq!(host.qp_native_row_action(&body)["ok"], true);
        assert_eq!(inputs.load(Ordering::SeqCst), 2);
        let inner = host.inner.lock();
        assert!(inner.qp.overlay_visible);
        assert!(!inner.qp.commit_in_flight);
        assert!(inner.qp.remembered.is_none());
    }

    #[test]
    fn changed_paste_target_keeps_row_request_but_cancellation_retires_it() {
        let (_root, host, _body, _inputs) = fixture();
        let live = Arc::new(AtomicBool::new(true));
        let request = NativeQpRowRequest {
            presentation_live: Some(live.clone()),
            source: 0,
            target: Some(InputTarget::capture(42).unwrap()),
            foreground: 42,
            session_revision: 4,
            state_revision: Some(8),
            session_generation: 2,
            hook_generation: 2,
            cancel_generation: host.inner.lock().qp.cancel_generation,
            fifo_generation: None,
        };
        assert!(host.verify_qp_row_request_at_foreground(&request, 84).is_ok());

        live.store(false, Ordering::Release);
        assert_eq!(
            host.verify_qp_row_request_at_foreground(&request, 84),
            Err(QP_ROW_RETIRED.into())
        );
        live.store(true, Ordering::Release);
        host.inner.lock().qp.cancel_generation += 1;
        assert_eq!(
            host.verify_qp_row_request_at_foreground(&request, 84),
            Err(QP_ROW_RETIRED.into())
        );
    }

    #[test]
    fn row_action_without_captured_target_keeps_its_foreground_constraint() {
        let (_root, host, _body, _inputs) = fixture();
        let request = NativeQpRowRequest {
            presentation_live: None,
            source: 0,
            target: None,
            foreground: 42,
            session_revision: 4,
            state_revision: Some(8),
            session_generation: 2,
            hook_generation: 2,
            cancel_generation: host.inner.lock().qp.cancel_generation,
            fifo_generation: None,
        };
        assert!(host.verify_qp_row_request_at_foreground(&request, 42).is_ok());
        assert_eq!(
            host.verify_qp_row_request_at_foreground(&request, 84),
            Err("操作目标已变化，已停止操作。".into())
        );
    }

    #[test]
    fn failure_or_cancellation_keeps_current_memory_and_usage() {
        for cancel in [false, true] {
            let (_root, host, body, _inputs) = fixture();
            let cancel_host = host.clone();
            host.set_input_runner(move |job| {
                if cancel {
                    cancel_host.inner.lock().qp.cancel_generation += 1;
                    job()
                } else {
                    Err("input failed".into())
                }
            });
            let result = host.qp_native_row_action(&body);
            *host.input_runner.lock() = None;
            assert_eq!(result["ok"], false);
            assert!(host.inner.lock().qp.remembered.is_none());
            assert_eq!(
                host.store()
                    .lock()
                    .get_clip(body["itemId"].as_str().unwrap())
                    .unwrap()
                    .usage_count,
                0
            );
        }
    }

    #[test]
    fn shortcut_arming_uses_configuration_and_clears_missing_slot() {
        let (_root, host, _body, _inputs) = fixture();
        host.qp_arm_native_shortcut(1);
        assert_eq!(
            host.inner.lock().qp.armed.as_ref().unwrap().kind,
            "simulate"
        );
        host.qp_arm_native_shortcut(2);
        assert!(host.inner.lock().qp.armed.is_none());
    }

    #[test]
    fn queued_new_chord_stops_old_row_before_injection() {
        let (_root, host, body, _inputs) = fixture();
        let changing = host.clone();
        host.set_input_runner(move |job| {
            // The next physical chord can retire work before its history load
            // has supplied a new session revision or selected item.
            changing.inner.lock().qp.hook_generation += 1;
            job()
        });
        let result = host.qp_native_row_action(&body);
        *host.input_runner.lock() = None;
        assert_eq!(result["ok"], false);
        assert_eq!(
            host.store()
                .lock()
                .get_clip(body["itemId"].as_str().unwrap())
                .unwrap()
                .usage_count,
            0
        );
        assert!(host.inner.lock().qp.remembered.is_none());
    }
}
