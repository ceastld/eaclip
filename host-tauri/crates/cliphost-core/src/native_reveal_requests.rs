//! History menu navigation owns the original page and external destination.
//! Never enumerate other dialogs after a menu has accepted an operation.

use super::*;

#[derive(Clone)]
pub struct NativeRevealRequest {
    page: NativeClipboardActionRequest,
    target: Option<InputTarget>,
    _hold: Arc<RevealHold>,
}

/// Keep automatic Fold/Hide from retiring the accepted menu while a bounded
/// native navigation is pending. Explicit hide, Esc and replacement still
/// retire the original page; the final request clone releases the hold.
struct RevealHold {
    inner: Arc<host_lock::HostLock<HostInner>>,
}

impl Drop for RevealHold {
    fn drop(&mut self) {
        let mut inner = self.inner.lock();
        inner.native_reveal_in_flight = inner.native_reveal_in_flight.saturating_sub(1);
    }
}

impl HostState {
    /// Capture on the UI thread before handing navigation to a worker. A shell
    /// reveal also works without a receiver, unlike a clipboard paste.
    pub fn capture_native_reveal(&self) -> Result<NativeRevealRequest, String> {
        let page = self.capture_native_clipboard_action(None)?;
        let hwnd = if crate::clipboard::is_headless() {
            self.inner.lock().last_non_host_fg
        } else {
            self.last_window_except(self.host_hwnd())
        };
        let target = (hwnd != 0)
            .then(|| InputTarget::capture(hwnd))
            .transpose()?;
        let hold = {
            let mut inner = self.inner.lock();
            self.verify_native_clipboard_action_locked(&inner, &page)?;
            inner.native_reveal_in_flight += 1;
            Arc::new(RevealHold {
                inner: self.inner.clone(),
            })
        };
        let request = NativeRevealRequest {
            page,
            target,
            _hold: hold,
        };
        self.verify_native_reveal(&request)?;
        Ok(request)
    }

    fn verify_native_reveal(&self, request: &NativeRevealRequest) -> Result<(), String> {
        self.verify_native_clipboard_action_locked(&self.inner.lock(), &request.page)?;
        if let Some(target) = request.target {
            target.verify_identity()?;
        }
        Ok(())
    }

    pub fn reveal_item_native(
        &self,
        params: &Value,
        request: &NativeRevealRequest,
    ) -> Result<Value, String> {
        self.reveal_item_native_after_read(params, request, || {})
    }

    fn reveal_item_native_after_read(
        &self,
        params: &Value,
        request: &NativeRevealRequest,
        after_read: impl FnOnce(),
    ) -> Result<Value, String> {
        self.verify_native_reveal(request)?;
        let path = self.reveal_item_path(params);
        after_read();
        self.verify_native_reveal(request)?;
        let path = path?;
        crate::shell_reveal::reveal_with_dialog(
            &path,
            |full| {
                self.verify_native_reveal(request)?;
                let Some(target) = request.target else {
                    return Ok(false);
                };
                if !crate::file_dialog_locate::is_file_dialog_hwnd(target.hwnd() as i64) {
                    return Ok(false);
                }
                let located = crate::file_dialog_locate::try_locate_hwnd_guarded(
                    target.hwnd() as i64,
                    full,
                    || self.verify_native_reveal(request),
                )?;
                if !located {
                    return Err(crate::i18n::tr(
                        "文件窗口未能完成目录跳转，请重试。",
                        "The file dialog could not navigate to the folder. Please retry.",
                    )
                    .into());
                }
                Ok(true)
            },
            |folder| {
                self.verify_native_reveal(request)?;
                self.open_shell_target(folder)
            },
        )?;
        Ok(json!({"ok":true}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_reveal_keeps_accepted_target_and_rejects_retirement_after_read() {
        crate::clipboard::force_headless(true);
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
        std::fs::create_dir_all(&base).unwrap();
        let root = tempfile::tempdir_in(base).unwrap();
        let host =
            HostState::open(AppPaths::from_data_root(root.path().join("data")), 29065).unwrap();
        host.set_window_runtime_ready(true);
        host.set_clipboard_web_view_ready(true);
        {
            let mut inner = host.inner.lock();
            inner.host_hwnd = 100;
            inner.window_visible = true;
            inner.last_non_host_fg = 42;
        }
        let path = root.path().join("sample.txt");
        std::fs::write(&path, b"owned").unwrap();
        let mut item = ClipRecord::new_text("sample", "native-reveal-test");
        item.kind = ClipKind::FileClipItem;
        item.file_paths = vec![path.to_string_lossy().into_owned()];
        host.store().lock().insert_clip(&item, true).unwrap();
        let request = host.capture_native_reveal().unwrap();
        assert!(
            host.hide_snapshot().spa_modal_open,
            "accepted navigation must hold automatic Fold/Hide during its wait"
        );
        host.inner.lock().last_non_host_fg = 43;
        assert_eq!(request.target.unwrap().hwnd(), 42);
        host.set_shell_open_runner(|_| panic!("retired reveal must not open Explorer"));
        let error = host
            .reveal_item_native_after_read(
                &json!({"id":item.id,"target":"folder"}),
                &request,
                || {
                    host.window_hide();
                    host.request_show();
                },
            )
            .unwrap_err();
        assert!(error.contains("会话已失效"), "{error}");
        drop(request);
        assert!(
            !host.hide_snapshot().spa_modal_open,
            "a completed or retired request must release its automatic-hide hold"
        );
    }

    #[test]
    fn native_reveal_without_receiver_opens_record_parent_once() {
        crate::clipboard::force_headless(true);
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
        std::fs::create_dir_all(&base).unwrap();
        let root = tempfile::tempdir_in(base).unwrap();
        let host =
            HostState::open(AppPaths::from_data_root(root.path().join("data")), 29065).unwrap();
        host.set_window_runtime_ready(true);
        host.set_clipboard_web_view_ready(true);
        {
            let mut inner = host.inner.lock();
            inner.host_hwnd = 100;
            inner.window_visible = true;
        }
        let path = root.path().join("sample.txt");
        std::fs::write(&path, b"owned").unwrap();
        let mut item = ClipRecord::new_text("sample", "native-reveal-test");
        item.kind = ClipKind::FileClipItem;
        item.file_paths = vec![path.to_string_lossy().into_owned()];
        host.store().lock().insert_clip(&item, true).unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let observed = calls.clone();
        host.set_shell_open_runner(move |path| {
            observed.lock().push(path.to_string());
            Ok(())
        });
        host.set_clipboard_ui_runner(|_| panic!("reveal must not write clipboard"));
        let request = host.capture_native_reveal().unwrap();
        assert!(request.target.is_none());
        host.reveal_item_native(&json!({"id":item.id,"target":"folder"}), &request)
            .unwrap();
        assert_eq!(
            *calls.lock(),
            vec![
                crate::shell_path::normalize_local_shell_path(&root.path().to_string_lossy(),)
                    .unwrap()
            ]
        );
    }
}
