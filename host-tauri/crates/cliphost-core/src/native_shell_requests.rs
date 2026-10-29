//! Native content opens retain the accepted clipboard page through target
//! preparation and the final Shell entry. Successful launches have no post-
//! check: their natural foreground/visibility change is not a failed request.

use super::*;

impl HostState {
    pub fn shell_open_path_native(
        &self,
        params: &Value,
        request: &NativeClipboardActionRequest,
    ) -> Result<Value, String> {
        self.open_native_shell_with_prepare(
            request,
            || {
                let path = req_str(params, "path")?;
                crate::shell_path::normalize_local_shell_path(&path)
            },
            || {},
        )
    }

    pub fn shell_open_uri_native(
        &self,
        params: &Value,
        request: &NativeClipboardActionRequest,
    ) -> Result<Value, String> {
        self.open_native_shell_with_prepare(
            request,
            || {
                let uri = req_str(params, "uri")?;
                crate::shell_path::normalize_shell_uri(&uri)
            },
            || {},
        )
    }

    /// Match open_item_path's original-record membership requirement. A page
    /// ticket authorizes the operation, not an arbitrary replacement path.
    pub fn open_item_path_native(
        &self,
        params: &Value,
        request: &NativeClipboardActionRequest,
    ) -> Result<Value, String> {
        self.open_item_path_native_with_hooks(params, request, || {}, || {})
    }

    fn open_item_path_native_with_hooks(
        &self,
        params: &Value,
        request: &NativeClipboardActionRequest,
        after_read: impl FnOnce(),
        after_prepare: impl FnOnce(),
    ) -> Result<Value, String> {
        self.open_native_shell_with_prepare(
            request,
            || {
                let id = req_id(params)?;
                let raw = params.get("path").and_then(Value::as_str).unwrap_or("");
                if raw.trim().is_empty() {
                    return Err("path is required.".into());
                }
                let store = self.store();
                let item = store
                    .lock()
                    .get_clip(&id)
                    .map_err(|_| "剪贴项不存在。".to_string());
                after_read();
                self.verify_native_shell_request(request)?;
                Self::prepare_open_clip_item_path(&item?, raw)
            },
            after_prepare,
        )
    }

    fn verify_native_shell_request(
        &self,
        request: &NativeClipboardActionRequest,
    ) -> Result<(), String> {
        self.verify_native_clipboard_action_locked(&self.inner.lock(), request)
    }

    fn open_native_shell_with_prepare(
        &self,
        request: &NativeClipboardActionRequest,
        prepare: impl FnOnce() -> Result<String, String>,
        after_prepare: impl FnOnce(),
    ) -> Result<Value, String> {
        self.verify_native_shell_request(request)?;
        let target = prepare();
        after_prepare();
        self.verify_native_shell_request(request)?;
        let target = target?;

        // Capture the existing provider outside the business locks. Only this
        // dispatch clone receives the wrapper; other host runners are intact.
        let provider = self.shell_open_runner.lock().clone();
        let host = self.clone();
        let original = request.clone();
        let mut scoped = self.clone();
        scoped.shell_open_runner = Arc::new(Mutex::new(Some(Arc::new(move |target: &str| {
            let verify = || host.verify_native_shell_request(&original);
            verify()?;
            match &provider {
                Some(provider) => provider(target),
                None => crate::shell_open::open_guarded(target, &verify),
            }
        }) as ShellOpenRunner)));
        scoped.open_shell_target(&target)?;
        Ok(json!({"ok":true}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        host: HostState,
        path: String,
        id: String,
        calls: Arc<Mutex<Vec<String>>>,
        _root: tempfile::TempDir,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            // The controlled provider observes this host. Break that fixture-
            // only cycle before dropping its SQLite root and temporary files.
            *self.host.shell_open_runner.lock() = None;
        }
    }

    fn fixture() -> Fixture {
        crate::clipboard::force_headless(true);
        let temporary = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
        std::fs::create_dir_all(&temporary).unwrap();
        let root = tempfile::Builder::new()
            .prefix("native-shell-")
            .tempdir_in(temporary)
            .unwrap();
        let host =
            HostState::open(AppPaths::from_data_root(root.path().join("data")), 29065).unwrap();
        host.set_window_runtime_ready(true);
        host.set_clipboard_web_view_ready(true);
        {
            let mut inner = host.inner.lock();
            inner.host_hwnd = 100;
            inner.window_visible = true;
            inner.ui.clipboard_route = "/clipboard".into();
            inner.last_non_host_fg = 0;
            inner.keyboard_target_hwnd = 0;
        }
        while host.take_chrome_task().is_some() {}
        let path = root.path().join("%中文 +#.txt");
        std::fs::write(&path, b"owned file").unwrap();
        let path = path.to_string_lossy().into_owned();
        let mut item = ClipRecord::new_text("owned file", "native-shell-test");
        item.kind = ClipKind::FileClipItem;
        item.file_paths = vec![format!(" {path} ")];
        host.store().lock().insert_clip(&item, true).unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let counted = calls.clone();
        let observed = host.clone();
        host.set_shell_open_runner(move |target| {
            assert!(
                observed.inner.try_lock().is_some(),
                "business lock must be released before provider"
            );
            assert!(
                observed.shell_open_runner.try_lock().is_some(),
                "provider lock must be released before provider"
            );
            assert!(
                observed.store().try_lock().is_some(),
                "store lock must be released before provider"
            );
            counted.lock().push(target.to_string());
            Ok(())
        });
        host.set_input_runner(|_| panic!("shell open must not emit global input"));
        host.set_clipboard_ui_runner(|_| panic!("shell open must not write clipboard"));
        Fixture {
            _root: root,
            host,
            path,
            id: item.id,
            calls,
        }
    }

    fn assert_retired(result: Result<Value, String>) {
        assert!(result.unwrap_err().contains("会话已失效"));
    }

    #[test]
    fn native_shell_keeps_original_uri_file_url_and_member_path_once_without_receiver() {
        let f = fixture();
        let request = f.host.capture_native_clipboard_action(None).unwrap();
        let uri = "HTTPS://Example.Invalid/Original?x=原始%20值#片段";
        let file_uri = url::Url::from_file_path(&f.path).unwrap();
        // File URLs normalize the local path; record-member opening preserves its stored path.
        let normalized_file_path = crate::shell_path::normalize_local_shell_path(&f.path).unwrap();
        assert_eq!(
            Path::new(&normalized_file_path).file_name().unwrap(),
            "%中文 +#.txt"
        );
        assert_eq!(
            f.host
                .shell_open_uri_native(
                    &json!({"uri":format!(" {uri} "),"sourceWindowHandle":999}),
                    &request
                )
                .unwrap(),
            json!({"ok":true})
        );
        assert_eq!(
            f.host
                .shell_open_path_native(&json!({"path":file_uri.as_str()}), &request)
                .unwrap(),
            json!({"ok":true})
        );
        assert_eq!(
            f.host
                .open_item_path_native(&json!({"id":f.id,"path":format!(" {} ",f.path)}), &request)
                .unwrap(),
            json!({"ok":true})
        );
        assert_eq!(
            *f.calls.lock(),
            vec![uri.to_string(), normalized_file_path, f.path.clone()]
        );
        assert_eq!(
            f.host.store().lock().get_clip(&f.id).unwrap().usage_count,
            0
        );
        assert!(f.host.inner.lock().window_visible);
    }

    #[test]
    fn native_shell_rechecks_after_paused_preparation_and_source_reopen() {
        for uri in [false, true] {
            let f = fixture();
            let request = f.host.capture_native_clipboard_action(None).unwrap();
            let (prepared, reached) = std::sync::mpsc::channel();
            let (resume, waiting) = std::sync::mpsc::channel();
            let host = f.host.clone();
            let path = f.path.clone();
            let worker = std::thread::spawn(move || {
                host.open_native_shell_with_prepare(
                    &request,
                    || {
                        if uri {
                            crate::shell_path::normalize_shell_uri(
                                "https://example.invalid/original",
                            )
                        } else {
                            crate::shell_path::normalize_local_shell_path(&path)
                        }
                    },
                    || {
                        prepared.send(()).unwrap();
                        waiting.recv_timeout(Duration::from_secs(2)).unwrap();
                    },
                )
            });
            reached.recv_timeout(Duration::from_secs(2)).unwrap();
            f.host.window_hide();
            f.host.request_show();
            resume.send(()).unwrap();
            assert_retired(worker.join().unwrap());
            assert!(f.calls.lock().is_empty());
        }
    }

    #[test]
    fn native_shell_rejects_foreign_host_route_aba_and_replaced_hwnd() {
        for phase in ["foreign-host", "route", "hwnd"] {
            let f = fixture();
            let request = f.host.capture_native_clipboard_action(None).unwrap();
            if phase == "foreign-host" {
                let other = fixture();
                assert_retired(other.host.shell_open_uri_native(
                    &json!({"uri":"https://example.invalid/original"}),
                    &request,
                ));
                assert!(other.calls.lock().is_empty());
            } else {
                if phase == "hwnd" {
                    f.host.set_host_hwnd(101);
                } else {
                    for route in ["/favorites", "/clipboard"] {
                        f.host.chrome_web_action(&json!({"kind":"clipboardHostChrome","sourceHostWindow":"clipboard","route":route}));
                    }
                }
                assert_retired(
                    f.host
                        .shell_open_path_native(&json!({"path":f.path}), &request),
                );
                assert_retired(f.host.shell_open_uri_native(
                    &json!({"uri":"https://example.invalid/original"}),
                    &request,
                ));
                assert_retired(
                    f.host
                        .open_item_path_native(&json!({"id":f.id,"path":f.path}), &request),
                );
            }
            assert!(f.calls.lock().is_empty());
        }
    }

    #[test]
    fn native_item_path_keeps_record_membership_and_rechecks_after_store_read() {
        let f = fixture();
        let request = f.host.capture_native_clipboard_action(None).unwrap();
        let outside = f._root.path().join("outside.txt");
        std::fs::write(&outside, b"outside record").unwrap();
        let text = ClipRecord::new_text("text, not file", "native-shell-test");
        f.host.store().lock().insert_clip(&text, true).unwrap();
        for params in [
            json!({"id":f.id,"path":outside}),
            json!({"id":f.id,"path":" "}),
            json!({"id":"missing-record","path":f.path}),
            json!({"id":text.id,"path":f.path}),
            json!({"id":f.id,"path":url::Url::from_file_path(&f.path).unwrap().as_str()}),
        ] {
            assert_eq!(
                f.host.open_item_path_native(&params, &request).unwrap_err(),
                f.host.open_item_path(&params).unwrap_err()
            );
        }
        assert!(f.calls.lock().is_empty());
        assert_retired(f.host.open_item_path_native_with_hooks(
            &json!({"id":f.id,"path":f.path}),
            &request,
            || {
                f.host.window_hide();
                f.host.request_show();
            },
            || {},
        ));
        assert!(f.calls.lock().is_empty());
    }

    #[test]
    fn native_shell_rechecks_after_waiting_for_provider_without_replacing_global_runner() {
        let f = fixture();
        let request = f.host.capture_native_clipboard_action(None).unwrap();
        let provider_lock = f.host.shell_open_runner.lock();
        let (prepared, reached) = std::sync::mpsc::channel();
        let (finished, completion) = std::sync::mpsc::channel();
        let host = f.host.clone();
        let worker = std::thread::spawn(move || {
            let result = host.open_native_shell_with_prepare(
                &request,
                || crate::shell_path::normalize_shell_uri("https://example.invalid/original"),
                || prepared.send(()).unwrap(),
            );
            finished.send(result).unwrap();
        });
        reached.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(
            completion.recv_timeout(Duration::from_millis(40)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        f.host.window_hide();
        f.host.request_show();
        drop(provider_lock);
        assert_retired(completion.recv_timeout(Duration::from_secs(2)).unwrap());
        worker.join().unwrap();
        assert!(f.calls.lock().is_empty());
        f.host
            .shell_open_uri(&json!({"uri":"https://example.invalid/ordinary"}))
            .unwrap();
        assert_eq!(
            *f.calls.lock(),
            vec!["https://example.invalid/ordinary".to_string()]
        );
    }

    #[test]
    fn native_shell_success_does_not_fail_after_natural_foreground_or_source_retirement() {
        let f = fixture();
        let request = f.host.capture_native_clipboard_action(None).unwrap();
        let host = f.host.clone();
        let calls = f.calls.clone();
        f.host.set_shell_open_runner(move |target| {
            calls.lock().push(target.to_string());
            host.inner.lock().last_non_host_fg = 77;
            host.window_hide();
            Ok(())
        });
        assert_eq!(
            f.host
                .shell_open_uri_native(&json!({"uri":"https://example.invalid/original"}), &request)
                .unwrap(),
            json!({"ok":true})
        );
        assert_eq!(f.calls.lock().len(), 1);
        assert_retired(
            f.host
                .shell_open_uri_native(&json!({"uri":"https://example.invalid/replay"}), &request),
        );
        assert_eq!(f.calls.lock().len(), 1);
    }
}
