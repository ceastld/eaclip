//! Clipboard-window actions which do not emit input must not depend on an
//! external receiver. Their opaque ticket still owns the original list/page.

use super::*;

const CLIPBOARD_ACTION_RETIRED: &str = "剪贴板操作会话已失效，已停止复制。";

#[derive(Debug, Clone)]
pub struct NativeClipboardActionRequest {
    owner: usize,
    source_hwnd: isize,
    input_epoch: ChromeInputEpoch,
    presentation_generation: u64,
    route: String,
    task: Option<ChromeTask>,
}

impl HostState {
    /// Capture only: no store, clipboard, foreground or InputTarget queries.
    pub fn capture_native_clipboard_action(
        &self,
        task: Option<ChromeTask>,
    ) -> Result<NativeClipboardActionRequest, String> {
        let inner = self.inner.lock();
        let request = NativeClipboardActionRequest {
            owner: Arc::as_ptr(&self.inner) as usize,
            source_hwnd: inner.host_hwnd,
            input_epoch: inner.chrome_input_epochs[0],
            presentation_generation: inner.chrome_presentation_epochs[0],
            route: inner.ui.clipboard_route.clone(),
            task,
        };
        self.verify_native_clipboard_action_locked(&inner, &request)?;
        Ok(request)
    }

    pub(super) fn verify_native_clipboard_action_locked(
        &self,
        inner: &HostInner,
        request: &NativeClipboardActionRequest,
    ) -> Result<(), String> {
        let epoch = inner.chrome_input_epochs[0];
        if request.owner != Arc::as_ptr(&self.inner) as usize
            || request.source_hwnd == 0
            || request.source_hwnd != inner.host_hwnd
            || request.input_epoch.window != epoch.window
            || request.input_epoch.list != epoch.list
            || request.presentation_generation != inner.chrome_presentation_epochs[0]
            || request.route != inner.ui.clipboard_route
            || !inner.window_runtime_ready
            || !inner.clipboard_web_view_ready
            || !inner.window_visible
            || inner.is_collapsed
            || inner.edge_tucked
            || inner.ui.clipboard_editing
            || inner.spa_modal_open
            || request.task.is_some_and(|task| {
                task.command.input_window() != Some("clipboard")
                    || !chrome_task_epoch_allowed(inner, task)
                    || task.input_epoch.is_some_and(|accepted| {
                        accepted.window != epoch.window || accepted.list != epoch.list
                    })
            })
        {
            return Err(CLIPBOARD_ACTION_RETIRED.into());
        }
        Ok(())
    }

    fn verify_native_clipboard_action(
        &self,
        request: &NativeClipboardActionRequest,
    ) -> Result<(), String> {
        self.verify_native_clipboard_action_locked(&self.inner.lock(), request)
    }

    fn native_clipboard_action_guard(
        &self,
        request: &NativeClipboardActionRequest,
    ) -> Arc<dyn Fn() -> Result<(), String> + Send + Sync> {
        let host = self.clone();
        let request = request.clone();
        Arc::new(move || host.verify_native_clipboard_action(&request))
    }

    /// History Copy preserves the ordinary copy formats and needs no receiver.
    pub fn copy_items_native(
        &self,
        params: &Value,
        request: &NativeClipboardActionRequest,
    ) -> Result<Value, String> {
        self.copy_items_native_with_hooks(params, request, || {}, || {})
    }

    fn copy_items_native_with_hooks(
        &self,
        params: &Value,
        request: &NativeClipboardActionRequest,
        after_read: impl FnOnce(),
        after_prepare: impl FnOnce(),
    ) -> Result<Value, String> {
        self.verify_native_clipboard_action(request)?;
        let ids = req_clip_ids(params)?;
        let (store, opts) = {
            let inner = self.inner.lock();
            (
                inner.store.clone(),
                paste_options_from_config(&inner.config),
            )
        };
        let items = get_items_by_ids(&store.lock(), &ids);
        after_read();
        self.verify_native_clipboard_action(request)?;
        let guard = self.native_clipboard_action_guard(request);
        let preparing = guard.clone();
        self.run_clipboard_write_then_guarded(
            move || {
                preparing()?;
                let prepared = Self::prepare_clip_records(&items, &opts)?;
                after_prepare();
                preparing()?;
                Ok(prepared)
            },
            |_| Ok(()),
            None,
            guard,
        )
        .map_err(|error| {
            if error == CLIPBOARD_ACTION_RETIRED {
                error
            } else {
                "Operation failed.".into()
            }
        })?;
        Ok(json!({ "ok": true }))
    }

    pub fn copy_gif_native(
        &self,
        params: &Value,
        request: &NativeClipboardActionRequest,
    ) -> Result<Value, String> {
        self.copy_gif_native_with_hooks(params, request, || {}, || {})
    }

    fn copy_gif_native_with_hooks(
        &self,
        params: &Value,
        request: &NativeClipboardActionRequest,
        after_read: impl FnOnce(),
        after_prepare: impl FnOnce(),
    ) -> Result<Value, String> {
        self.verify_native_clipboard_action(request)?;
        let id = req_id(params)?;
        // Same durable file-drop export as copy_gif, outside capture cleanup.
        let directory = self
            .inner
            .lock()
            .paths
            .image_library
            .join("ClipboardExports");
        let guard = self.native_clipboard_action_guard(request);
        let preparing = guard.clone();
        self.run_clipboard_write_then_guarded(
            || {
                preparing()?;
                let (source, _) = self.image_bytes(&id)?;
                after_read();
                preparing()?;
                let gif = crate::image_gif::prepare(&source, &directory)?;
                preparing()?;
                let prepared = crate::clipboard_payload::prepare_gif_file(&gif.to_string_lossy())?;
                after_prepare();
                preparing()?;
                Ok(prepared)
            },
            |_| Ok(()),
            None,
            guard,
        )?;
        Ok(json!({ "ok": true }))
    }

    /// Preserve pin_overlay's asynchronous accepted response. The worker owns
    /// this ticket, rather than capturing whichever page exists when it starts.
    pub fn pin_overlay_native(
        &self,
        params: &Value,
        request: &NativeClipboardActionRequest,
    ) -> Result<Value, String> {
        self.verify_native_clipboard_action(request)?;
        self.schedule_pin_overlay_guarded(
            params,
            false,
            self.native_clipboard_action_guard(request),
            Some(request.clone()),
        )
    }

    pub fn favor_copy_favorites_native(
        &self,
        params: &Value,
        request: &NativeClipboardActionRequest,
    ) -> Result<Value, String> {
        self.favor_copy_favorites_native_after_read(params, request, || {})
    }

    fn favor_copy_favorites_native_after_read(
        &self,
        params: &Value,
        request: &NativeClipboardActionRequest,
        after_read: impl FnOnce(),
    ) -> Result<Value, String> {
        self.verify_native_clipboard_action(request)?;
        let id = req_id(params)?;
        let (store, opts) = {
            let inner = self.inner.lock();
            (
                inner.store.clone(),
                paste_options_from_config(&inner.config),
            )
        };
        let item = store.lock().get_favor(&id).map_err(item_not_found)?;
        let clip = favor_as_clip(&item).map_err(|_| "Item not found.".to_string())?;
        let clip=store.lock().core_io_clip(&clip).map_err(|e|e.to_string())?;
        after_read();
        self.verify_native_clipboard_action(request)?;
        let guard = self.native_clipboard_action_guard(request);
        let preparing = guard.clone();
        self.run_clipboard_write_then_guarded(
            move || {
                preparing()?;
                let prepared = crate::paste::prepare_copy_record(&clip, &opts)?;
                preparing()?;
                Ok(prepared)
            },
            |_| Ok(()),
            None,
            guard,
        )?;
        // Match favor.copy: no input, usage or clipboard-window dismissal.
        Ok(json!({ "ok": true }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    struct Fixture {
        _root: tempfile::TempDir,
        host: HostState,
        id: String,
        writes: Arc<AtomicUsize>,
    }

    fn fixture(target: isize) -> Fixture {
        crate::clipboard::force_headless(true);
        let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
        std::fs::create_dir_all(&directory).unwrap();
        let root = tempfile::Builder::new()
            .prefix("native-clipboard-action-")
            .tempdir_in(directory)
            .unwrap();
        let host =
            HostState::open(AppPaths::from_data_root(root.path().to_path_buf()), 29065).unwrap();
        let id = host
            .favor_add_text_items(&json!({"text":"favorite copied without receiver"}))
            .unwrap()["createdIds"][0]
            .as_str()
            .unwrap()
            .to_string();
        host.set_window_runtime_ready(true);
        host.set_clipboard_web_view_ready(true);
        {
            let mut inner = host.inner.lock();
            inner.host_hwnd = 100;
            inner.window_visible = true;
            inner.ui.clipboard_route = "/favorites".into();
            inner.last_non_host_fg = target;
            inner.keyboard_target_hwnd = target;
        }
        while host.take_chrome_task().is_some() {}
        let writes = Arc::new(AtomicUsize::new(0));
        let counted = writes.clone();
        host.set_clipboard_ui_runner(move |job| {
            job()?;
            counted.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        host.set_input_runner(|_| panic!("copy must not activate or emit input"));
        Fixture {
            _root: root,
            host,
            id,
            writes,
        }
    }

    fn usage(f: &Fixture) -> i32 {
        f.host
            .store()
            .lock()
            .get_usage(&crate::store::favor_usage_key(&f.id))
            .unwrap()
            .usage_count
    }

    fn insert_history(f: &Fixture, item: &ClipRecord) -> String {
        f.host.store().lock().insert_clip(item, true).unwrap();
        item.id.clone()
    }

    fn history_image(f: &Fixture) -> ClipRecord {
        let path = f.host.inner.lock().paths.image_temp.join("owned-image.png");
        image::RgbaImage::from_pixel(3, 2, image::Rgba([210, 30, 40, 255]))
            .save(&path)
            .unwrap();
        let mut item = ClipRecord::new_text("image", "native-test");
        item.kind = ClipKind::ImageClipItem;
        item.image_path = Some(path.to_string_lossy().into_owned());
        insert_history(f, &item);
        item
    }

    fn mixed_pin_history(f: &Fixture) -> Vec<ClipRecord> {
        let text = ClipRecord::new_text("first pin", "native-test");
        insert_history(f, &text);
        vec![text, history_image(f)]
    }

    fn assert_history_untouched(f: &Fixture, items: &[ClipRecord]) {
        let store = f.host.store();
        let store = store.lock();
        for item in items {
            let stored = store.get_clip(&item.id).unwrap();
            assert_eq!(stored.usage_count, 0);
            assert!(stored.last_access.is_none());
        }
    }

    fn pin_request(
        f: &Fixture,
        request: &NativeClipboardActionRequest,
        items: &[ClipRecord],
        provider: &str,
    ) -> (Vec<ClipRecord>, String) {
        f.host
            .prepare_pin_overlay_request_guarded(
                &json!({"ids":items.iter().map(|item| &item.id).collect::<Vec<_>>(), "provider":provider}),
                &f.host.native_clipboard_action_guard(request),
                || (true, true),
                || {},
            )
            .unwrap()
    }

    #[derive(Debug, PartialEq)]
    struct ToastCommitSnapshot {
        visible: bool,
        hovered: bool,
        generation: i32,
        last_action: Option<String>,
        last_item_id: Option<String>,
        last_simulated_text: Option<String>,
        pending_route: Option<String>,
        pending_push: Option<Value>,
        overlay_place: Option<crate::window_place::ToastOverlayPlacement>,
        duration_ms: Option<i32>,
        idle_deadline: Option<Instant>,
    }

    impl ToastCommitSnapshot {
        fn read(inner: &HostInner) -> Self {
            Self {
                visible: inner.toast.visible,
                hovered: inner.toast.hovered,
                generation: inner.toast.generation,
                last_action: inner.toast.last_action.clone(),
                last_item_id: inner.toast.last_item_id.clone(),
                last_simulated_text: inner.toast.last_simulated_text.clone(),
                pending_route: inner.toast.pending_route.clone(),
                pending_push: inner.toast.pending_push.clone(),
                overlay_place: inner.toast.overlay_place,
                duration_ms: inner.toast.duration_ms,
                idle_deadline: inner.toast.idle_deadline,
            }
        }
    }

    fn clear_feedback_queues(f: &Fixture) {
        while f.host.take_chrome_task().is_some() {}
        f.host.drain_push_events();
    }

    fn toast_events(f: &Fixture) -> Vec<Value> {
        f.host
            .drain_push_events()
            .into_iter()
            .filter_map(|(name, detail)| (name == "toast").then_some(detail))
            .collect()
    }

    #[test]
    fn native_pin_error_toast_rechecks_after_contended_commit_lock_and_retirement() {
        for phase in ["list", "route", "hide-reopen", "root"] {
            let f = fixture(0);
            let request = f.host.capture_native_clipboard_action(None).unwrap();
            // The old lock-outside preflight succeeds. The following write-lock
            // contention then forces retirement before actual toast publication.
            f.host.verify_native_clipboard_action(&request).unwrap();
            clear_feedback_queues(&f);
            let mut inner = f.host.inner.lock();
            inner.toast.visible = true;
            inner.toast.hovered = true;
            inner.toast.generation = 17;
            inner.toast.last_action = Some("current action".into());
            inner.toast.last_item_id = Some("current item".into());
            inner.toast.last_simulated_text = Some("current simulated text".into());
            inner.toast.pending_route = Some("current route".into());
            inner.toast.pending_push = Some(json!({"message":"current toast"}));
            inner.toast.duration_ms = Some(2222);
            inner.toast.idle_deadline = Some(Instant::now() + Duration::from_secs(3));
            let (started, begin) = std::sync::mpsc::channel();
            let (finished, completion) = std::sync::mpsc::channel();
            let host = f.host.clone();
            let worker = std::thread::spawn(move || {
                assert!(
                    host.inner.try_lock().is_none(),
                    "owner must hold the actual toast commit lock"
                );
                started.send(()).unwrap();
                let result =
                    host.report_pin_overlay_error("owned provider failure", Some(&request));
                finished.send(result).unwrap();
            });
            begin.recv_timeout(Duration::from_secs(2)).unwrap();
            assert!(matches!(
                completion.recv_timeout(Duration::from_millis(40)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ));
            match phase {
                "list" => drop_queued_clipboard_list_input(&mut inner),
                "route" => {
                    drop_queued_clipboard_list_input(&mut inner);
                    inner.ui.clipboard_route = "/clipboard".into();
                }
                "hide-reopen" => {
                    hide_clipboard_window_locked(&mut inner);
                    // A new presentation may be visible on the same HWND, but
                    // the old display/input generations must not revive.
                    inner.window_visible = true;
                }
                _ => {
                    drop_queued_window_input(&mut inner, "clipboard");
                    cancel_window_presentation(&mut inner, "clipboard");
                    inner.host_hwnd = 101;
                }
            }
            let before_commit = ToastCommitSnapshot::read(&inner);
            drop(inner);
            assert_eq!(
                completion
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap()
                    .unwrap_err(),
                CLIPBOARD_ACTION_RETIRED,
                "{phase}"
            );
            worker.join().unwrap();
            let inner = f.host.inner.lock();
            assert_eq!(ToastCommitSnapshot::read(&inner), before_commit, "{phase}");
            assert!(!inner.chrome.contains(&ChromeCommand::ShowToast), "{phase}");
            drop(inner);
            assert!(toast_events(&f).is_empty(), "{phase}");
            assert_eq!(f.writes.load(Ordering::SeqCst), 0);
            assert_eq!(usage(&f), 0);
        }
    }

    #[test]
    fn native_pin_error_toast_preserves_current_native_and_web_feedback() {
        for native in [false, true] {
            let f = fixture(0);
            let request = f.host.capture_native_clipboard_action(None).unwrap();
            if !native {
                f.host.window_hide();
            }
            clear_feedback_queues(&f);
            let before = f.host.inner.lock().toast.generation;
            f.host.finish_pin_overlay_sequence(
                Err("owned provider failure".into()),
                "stub-provider",
                native.then_some(&request),
            );
            let expected = json!({"kind":"error", "placement":"bottom-center", "message":"贴图失败：owned provider failure", "title":"贴图", "durationMs":3000});
            let inner = f.host.inner.lock();
            assert!(inner.toast.visible);
            assert_eq!(inner.toast.generation, before + 1);
            assert_eq!(inner.toast.pending_push, Some(expected.clone()));
            assert_eq!(inner.toast.duration_ms, Some(3000));
            assert!(inner.toast.idle_deadline.is_some());
            drop(inner);
            let commands: Vec<_> = std::iter::from_fn(|| f.host.take_chrome_task())
                .map(|task| task.command)
                .collect();
            assert_eq!(
                commands
                    .iter()
                    .filter(|command| **command == ChromeCommand::ShowToast)
                    .count(),
                1
            );
            assert_eq!(toast_events(&f), vec![expected]);
            assert_eq!(f.writes.load(Ordering::SeqCst), 0);
            assert_eq!(usage(&f), 0);
            assert_eq!(f.host.inner.lock().window_visible, native);
        }
    }

    #[test]
    fn native_pin_success_and_retired_error_do_not_publish_feedback() {
        for success in [false, true] {
            let f = fixture(0);
            let request = f.host.capture_native_clipboard_action(None).unwrap();
            if !success {
                f.host.window_hide();
                f.host.request_show();
            }
            clear_feedback_queues(&f);
            let before = ToastCommitSnapshot::read(&f.host.inner.lock());
            f.host.finish_pin_overlay_sequence(
                if success {
                    Ok(())
                } else {
                    Err("late owned provider failure".into())
                },
                "stub-provider",
                Some(&request),
            );
            let inner = f.host.inner.lock();
            assert_eq!(ToastCommitSnapshot::read(&inner), before);
            assert!(!inner.chrome.contains(&ChromeCommand::ShowToast));
            drop(inner);
            assert!(toast_events(&f).is_empty());
            assert_eq!(f.writes.load(Ordering::SeqCst), 0);
            assert_eq!(usage(&f), 0);
            assert!(f.host.inner.lock().window_visible);
        }
    }

    #[test]
    fn native_history_copy_matches_web_copy_without_receiver_usage_or_hide() {
        for receiver in [0, 42] {
            let f = fixture(receiver);
            let mut html = ClipRecord::new_text("rich body", "native-test");
            html.kind = ClipKind::HtmlClipItem;
            html.html_data = Some("<b>rich body</b>".into());
            let text = ClipRecord::new_text("second body", "native-test");
            let mut files = ClipRecord::new_text("file", "native-test");
            files.kind = ClipKind::FileClipItem;
            let file = f._root.path().join("owned-file.txt");
            std::fs::write(&file, b"owned file").unwrap();
            files.file_paths = vec![file.to_string_lossy().into_owned()];
            for item in [&html, &text, &files] {
                insert_history(&f, item);
            }
            let request = f.host.capture_native_clipboard_action(None).unwrap();
            // Copy never replaces its ticket based on a newly focused receiver.
            f.host.inner.lock().last_non_host_fg = 777;
            for ids in [
                vec![html.id.as_str()],
                vec![files.id.as_str()],
                vec![html.id.as_str(), text.id.as_str()],
                vec![text.id.as_str(), files.id.as_str()],
                vec![
                    text.id.as_str(),
                    text.id.as_str(),
                    html.id.as_str(),
                    "missing",
                ],
            ] {
                let params = json!({"ids":ids});
                assert_eq!(
                    f.host.copy_items(&params).unwrap(),
                    f.host.copy_items_native(&params, &request).unwrap()
                );
            }
            assert_eq!(f.writes.load(Ordering::SeqCst), 10);
            for params in [Value::Null, json!({"ids":[]}), json!({"ids":["missing"]})] {
                assert_eq!(
                    f.host.copy_items(&params).unwrap_err(),
                    f.host.copy_items_native(&params, &request).unwrap_err()
                );
            }
            assert_history_untouched(&f, &[html, text, files]);
            assert!(f.host.inner.lock().window_visible);
        }
    }

    #[test]
    fn native_history_outputs_reject_retired_root_page_and_list() {
        for phase in ["hide", "root", "document", "list", "route"] {
            let f = fixture(0);
            let image = history_image(&f);
            let request = f.host.capture_native_clipboard_action(None).unwrap();
            match phase {
                "hide" => {
                    f.host.window_hide();
                    f.host.request_show();
                }
                "root" => f.host.set_host_hwnd(101),
                "document" => {
                    f.host.set_clipboard_web_view_ready(false);
                    f.host.set_clipboard_web_view_ready(true);
                }
                "list" => drop_queued_clipboard_list_input(&mut f.host.inner.lock()),
                _ => {
                    f.host.chrome_web_action(&json!({"kind":"clipboardHostChrome","sourceHostWindow":"clipboard","route":"/clipboard"}));
                }
            }
            assert_eq!(
                f.host
                    .copy_items_native(&json!({"ids":[image.id]}), &request)
                    .unwrap_err(),
                CLIPBOARD_ACTION_RETIRED,
                "{phase}"
            );
            assert_eq!(
                f.host
                    .copy_gif_native(&json!({"id":image.id}), &request)
                    .unwrap_err(),
                CLIPBOARD_ACTION_RETIRED,
                "{phase}"
            );
            assert_eq!(
                f.host
                    .pin_overlay_native(&json!({"ids":[image.id],"provider":"snipaste"}), &request)
                    .unwrap_err(),
                CLIPBOARD_ACTION_RETIRED,
                "{phase}"
            );
            assert_eq!(f.writes.load(Ordering::SeqCst), 0);
            assert_history_untouched(&f, &[image]);
        }
    }

    #[test]
    fn native_history_copy_and_gif_recheck_read_prepare_and_owner_queue() {
        for gif in [false, true] {
            for phase in ["read", "prepare", "queue"] {
                let f = fixture(0);
                let item = history_image(&f);
                let request = f.host.capture_native_clipboard_action(None).unwrap();
                if phase == "queue" {
                    let host = f.host.clone();
                    let writes = f.writes.clone();
                    f.host.set_clipboard_ui_runner(move |job| {
                        host.window_hide();
                        host.request_show();
                        job()?;
                        writes.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    });
                }
                let retire = |at| {
                    if phase == at {
                        f.host.window_hide();
                        f.host.request_show();
                    }
                };
                let result = if gif {
                    f.host.copy_gif_native_with_hooks(
                        &json!({"id":item.id}),
                        &request,
                        || retire("read"),
                        || retire("prepare"),
                    )
                } else {
                    f.host.copy_items_native_with_hooks(
                        &json!({"ids":[item.id]}),
                        &request,
                        || retire("read"),
                        || retire("prepare"),
                    )
                };
                assert_eq!(
                    result.unwrap_err(),
                    CLIPBOARD_ACTION_RETIRED,
                    "gif={gif} {phase}"
                );
                assert_eq!(f.writes.load(Ordering::SeqCst), 0);
                assert_history_untouched(&f, &[item]);
            }
        }
    }

    #[test]
    fn native_history_copy_rechecks_after_waiting_for_clipboard_reservation() {
        let f = fixture(0);
        let item = ClipRecord::new_text("reserved copy", "native-test");
        insert_history(&f, &item);
        let request = f.host.capture_native_clipboard_action(None).unwrap();
        let reservation = f.host.clipboard_operation.lock();
        let (read, read_done) = std::sync::mpsc::channel();
        let (completed, completion) = std::sync::mpsc::channel();
        let host = f.host.clone();
        let id = item.id.clone();
        let worker = std::thread::spawn(move || {
            let result = host.copy_items_native_with_hooks(
                &json!({"ids":[id]}),
                &request,
                || read.send(()).unwrap(),
                || {},
            );
            completed.send(result).unwrap();
        });
        read_done.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(
            matches!(
                completion.recv_timeout(Duration::from_millis(40)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ),
            "copy must wait for the existing clipboard reservation"
        );
        f.host.window_hide();
        f.host.request_show();
        drop(reservation);
        assert_eq!(
            completion
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap_err(),
            CLIPBOARD_ACTION_RETIRED
        );
        worker.join().unwrap();
        assert_eq!(f.writes.load(Ordering::SeqCst), 0);
        assert_history_untouched(&f, &[item]);
    }

    #[test]
    fn native_history_gif_reuses_the_web_export_and_preserves_input_bytes() {
        let f = fixture(0);
        let item = history_image(&f);
        let original = std::fs::read(item.image_path.as_ref().unwrap()).unwrap();
        let request = f.host.capture_native_clipboard_action(None).unwrap();
        assert_eq!(
            f.host.copy_gif(&json!({"id":item.id})).unwrap(),
            f.host
                .copy_gif_native(&json!({"id":item.id}), &request)
                .unwrap()
        );
        let directory = f
            .host
            .inner
            .lock()
            .paths
            .image_library
            .join("ClipboardExports");
        let exports: Vec<_> = std::fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(exports.len(), 1);
        assert_eq!(
            image::open(&exports[0]).unwrap().to_rgba8(),
            image::load_from_memory(&original).unwrap().to_rgba8()
        );
        assert_eq!(
            std::fs::read(item.image_path.as_ref().unwrap()).unwrap(),
            original
        );
        assert_eq!(f.writes.load(Ordering::SeqCst), 2);
        assert_history_untouched(&f, &[item]);
        assert!(f.host.inner.lock().window_visible);
    }

    #[test]
    fn native_pin_rechecks_provider_status_and_history_read_before_output() {
        for phase in ["status", "read"] {
            let f = fixture(0);
            let items = mixed_pin_history(&f);
            let request = f.host.capture_native_clipboard_action(None).unwrap();
            let guard = f.host.native_clipboard_action_guard(&request);
            let result = f.host.prepare_pin_overlay_request_guarded(
                &json!({"ids":items.iter().map(|item| &item.id).collect::<Vec<_>>(),"provider":"pixpin"}),
                &guard,
                || { if phase == "status" { f.host.window_hide(); f.host.request_show(); } (true, true) },
                || { if phase == "read" { f.host.window_hide(); f.host.request_show(); } },
            );
            assert_eq!(result.unwrap_err(), CLIPBOARD_ACTION_RETIRED);
            assert_eq!(f.writes.load(Ordering::SeqCst), 0);
            assert_history_untouched(&f, &items);
        }
    }

    #[test]
    fn native_pin_batches_match_web_without_receiver_usage_or_hide() {
        for provider in ["snipaste", "pixpin"] {
            for mixed in [false, true] {
                let f = fixture(0);
                let items = if mixed {
                    mixed_pin_history(&f)
                } else {
                    ["first text", "second text"]
                        .into_iter()
                        .map(|text| {
                            let item = ClipRecord::new_text(text, "native-test");
                            insert_history(&f, &item);
                            item
                        })
                        .collect()
                };
                let request = f.host.capture_native_clipboard_action(None).unwrap();
                let (prepared, selected) = pin_request(&f, &request, &items, provider);
                let mut native_pins = 0;
                let mut waits = Vec::new();
                f.host
                    .run_pin_overlay_sequence_guarded(
                        &prepared,
                        &selected,
                        false,
                        |called| {
                            assert_eq!(called, provider);
                            native_pins += 1;
                            Ok(())
                        },
                        f.host.native_clipboard_action_guard(&request),
                        |duration| waits.push(duration.as_millis()),
                    )
                    .unwrap();
                let mut web_pins = 0;
                f.host
                    .run_pin_overlay_sequence(&prepared, &selected, false, |_| {
                        web_pins += 1;
                        Ok(())
                    })
                    .unwrap();
                assert_eq!(native_pins, if mixed { 2 } else { 1 });
                assert_eq!(native_pins, web_pins);
                assert_eq!(f.writes.load(Ordering::SeqCst), native_pins * 2);
                assert_eq!(
                    waits,
                    if mixed {
                        vec![60, 850, 60, 850]
                    } else {
                        vec![60]
                    }
                );
                assert_history_untouched(&f, &items);
                assert!(f.host.inner.lock().window_visible);
            }
        }
    }

    #[test]
    fn native_pin_worker_keeps_the_original_ticket_before_dispatch() {
        let f = fixture(0);
        let items = mixed_pin_history(&f);
        let request = f.host.capture_native_clipboard_action(None).unwrap();
        let (prepared, provider) = pin_request(&f, &request, &items, "snipaste");
        let guard = f.host.native_clipboard_action_guard(&request);
        let (release, queued) = std::sync::mpsc::channel();
        let host = f.host.clone();
        let worker = std::thread::spawn(move || {
            queued.recv_timeout(Duration::from_secs(2)).unwrap();
            host.run_pin_overlay_sequence_guarded(
                &prepared,
                &provider,
                false,
                |_| panic!("retired worker must not call provider"),
                guard,
                |_| {},
            )
        });
        f.host.window_hide();
        f.host.request_show();
        release.send(()).unwrap();
        assert_eq!(
            worker.join().unwrap().unwrap_err(),
            CLIPBOARD_ACTION_RETIRED
        );
        assert_eq!(f.writes.load(Ordering::SeqCst), 0);
        assert_history_untouched(&f, &items);
    }

    #[test]
    fn native_pin_stops_in_owner_queue_after_waits_and_between_batches() {
        for phase in ["queue", "60ms", "850ms", "provider"] {
            let f = fixture(0);
            let items = mixed_pin_history(&f);
            let request = f.host.capture_native_clipboard_action(None).unwrap();
            let (prepared, provider) = pin_request(&f, &request, &items, "pixpin");
            if phase == "queue" {
                let host = f.host.clone();
                let writes = f.writes.clone();
                f.host.set_clipboard_ui_runner(move |job| {
                    host.window_hide();
                    host.request_show();
                    job()?;
                    writes.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                });
            }
            let mut pins = 0;
            let result = f.host.run_pin_overlay_sequence_guarded(
                &prepared,
                &provider,
                false,
                |_| {
                    pins += 1;
                    if phase == "provider" {
                        f.host.window_hide();
                        f.host.request_show();
                    }
                    Ok(())
                },
                f.host.native_clipboard_action_guard(&request),
                |duration| {
                    if (phase == "60ms" && duration.as_millis() == 60)
                        || (phase == "850ms" && duration.as_millis() == 850)
                    {
                        f.host.window_hide();
                        f.host.request_show();
                    }
                },
            );
            assert_eq!(result.unwrap_err(), CLIPBOARD_ACTION_RETIRED, "{phase}");
            assert_eq!(
                pins,
                usize::from(matches!(phase, "850ms" | "provider")),
                "{phase}"
            );
            assert_eq!(
                f.writes.load(Ordering::SeqCst),
                usize::from(phase != "queue"),
                "{phase}"
            );
            assert_history_untouched(&f, &items);
        }
    }

    #[test]
    fn native_favorites_copy_works_with_or_without_a_receiver_and_keeps_usage() {
        for target in [0, 42] {
            let f = fixture(target);
            let request = f.host.capture_native_clipboard_action(None).unwrap();
            f.host.inner.lock().last_non_host_fg = 43;
            assert_eq!(
                f.host
                    .favor_copy_favorites_native(&json!({"id":f.id,"sourceHandle":999}), &request)
                    .unwrap()["ok"],
                true
            );
            assert_eq!(f.writes.load(Ordering::SeqCst), 1);
            assert_eq!(usage(&f), 0);
            assert!(f.host.inner.lock().window_visible);
        }
    }

    #[test]
    fn native_favorites_copy_rejects_hide_reopen_root_and_document_retirement() {
        for mode in ["hide", "root", "document", "list"] {
            let f = fixture(0);
            let request = f.host.capture_native_clipboard_action(None).unwrap();
            match mode {
                "hide" => {
                    f.host.window_hide();
                    f.host.request_show();
                }
                "root" => f.host.set_host_hwnd(101),
                "document" => {
                    f.host.set_clipboard_web_view_ready(false);
                    f.host.set_clipboard_web_view_ready(true);
                }
                _ => drop_queued_clipboard_list_input(&mut f.host.inner.lock()),
            }
            assert!(
                f.host
                    .favor_copy_favorites_native(&json!({"id":f.id}), &request)
                    .is_err(),
                "{mode}"
            );
            assert_eq!(f.writes.load(Ordering::SeqCst), 0);
            assert_eq!(usage(&f), 0);
        }
    }

    #[test]
    fn native_favorites_copy_rejects_route_change_and_return_to_the_old_route() {
        let f = fixture(0);
        let request = f.host.capture_native_clipboard_action(None).unwrap();
        for route in ["/clipboard", "/favorites"] {
            f.host.chrome_web_action(
                &json!({"kind":"clipboardHostChrome","sourceHostWindow":"clipboard","route":route}),
            );
            assert!(f
                .host
                .favor_copy_favorites_native(&json!({"id":f.id}), &request)
                .is_err());
        }
        assert_eq!(f.writes.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn native_favorites_copy_rechecks_after_store_read_and_in_owner_queue() {
        for phase in ["read", "queue"] {
            let f = fixture(0);
            let request = f.host.capture_native_clipboard_action(None).unwrap();
            if phase == "queue" {
                let host = f.host.clone();
                f.host.set_clipboard_ui_runner(move |job| {
                    host.window_hide();
                    host.request_show();
                    job()
                });
            }
            assert!(f
                .host
                .favor_copy_favorites_native_after_read(&json!({"id":f.id}), &request, || {
                    if phase == "read" {
                        f.host.window_hide();
                        f.host.request_show();
                    }
                })
                .is_err());
            assert_eq!(f.writes.load(Ordering::SeqCst), 0);
            assert_eq!(usage(&f), 0);
        }
    }

    #[test]
    fn native_favorites_copy_rejects_a_dequeued_task_from_the_previous_list() {
        let f = fixture(0);
        f.host.enqueue_chrome(ChromeCommand::ClipboardNavKey {
            key: "Enter",
            shift: false,
        });
        let task = f.host.take_chrome_task().unwrap();
        let request = f.host.capture_native_clipboard_action(Some(task)).unwrap();
        drop_queued_clipboard_list_input(&mut f.host.inner.lock());
        assert!(f.host.capture_native_clipboard_action(Some(task)).is_err());
        assert!(f
            .host
            .favor_copy_favorites_native(&json!({"id":f.id}), &request)
            .is_err());
        assert_eq!(f.writes.load(Ordering::SeqCst), 0);
    }
}
