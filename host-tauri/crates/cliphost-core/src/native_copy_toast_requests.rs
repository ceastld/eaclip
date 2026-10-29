//! Native copy-toast actions retain their own card/window lifetime. Paste can
//! follow a new external receiver after a card action has been accepted.
//! Mount under host.rs (like native_surface_requests); retire the registration
//! from toast_hide_locked and toast_open_edit before changing visible state.

use super::*;
use std::sync::{atomic::AtomicBool, Mutex, OnceLock, Weak};

const COPY_TOAST_RETIRED: &str = "复制提示已关闭，已停止本次操作。";

struct Presentation {
    source_hwnd: isize,
    live: Weak<AtomicBool>,
}

fn presentations() -> &'static Mutex<HashMap<usize, Arc<Presentation>>> {
    static PRESENTATIONS: OnceLock<Mutex<HashMap<usize, Arc<Presentation>>>> = OnceLock::new();
    PRESENTATIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn owner_key(inner: &HostInner) -> usize {
    inner as *const HostInner as usize
}

/// Called with HostInner already locked, before every core hide/edit transition.
/// A new copied card does not retire the other cards in the same presentation.
pub(super) fn retire_copy_toast_native_locked(inner: &HostInner) {
    let previous = presentations()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&owner_key(inner));
    if let Some(live) = previous.and_then(|p| p.live.upgrade()) {
        live.store(false, Ordering::Release);
    }
}

#[derive(Clone)]
pub struct NativeCopyToastRequest {
    owner: usize,
    presentation: Arc<Presentation>,
    card_live: Arc<AtomicBool>,
    target: Option<InputTarget>,
}

impl HostState {
    /// Register the actual copy-toast HWND, never the history window's HWND.
    /// The retained GPUI surface creates a fresh `live` token after hide/reopen.
    pub fn register_copy_toast_native(
        &self,
        source_hwnd: isize,
        live: Arc<AtomicBool>,
    ) -> Result<(), String> {
        let inner = self.inner.lock();
        if !inner.toast.visible || source_hwnd == 0 || !live.load(Ordering::Acquire) {
            return Err(COPY_TOAST_RETIRED.into());
        }
        if !crate::clipboard::is_headless() && !crate::input_target::belongs_to_host(source_hwnd) {
            return Err(COPY_TOAST_RETIRED.into());
        }
        let mut current = presentations().lock().unwrap_or_else(|e| e.into_inner());
        let owner = owner_key(&inner);
        if current.get(&owner).is_some_and(|p| {
            p.source_hwnd == source_hwnd
                && p.live.upgrade().is_some_and(|old| Arc::ptr_eq(&old, &live))
        }) {
            return Ok(());
        }
        if let Some(previous) = current.insert(
            owner,
            Arc::new(Presentation {
                source_hwnd,
                live: Arc::downgrade(&live),
            }),
        ) {
            if let Some(old) = previous.live.upgrade() {
                old.store(false, Ordering::Release);
            }
        }
        Ok(())
    }

    /// The native stack supplies its longest remaining timer. A hovered/menu/
    /// busy/permanent card suspends fallback collapse, while other cards expire.
    pub fn sync_copy_toast_native_deadline(&self, source_hwnd: isize, remaining_ms: Option<u64>) {
        let mut inner = self.inner.lock();
        let current = presentations().lock().unwrap_or_else(|e| e.into_inner());
        if !inner.toast.visible
            || !current.get(&owner_key(&inner)).is_some_and(|p| {
                p.source_hwnd == source_hwnd
                    && p.live
                        .upgrade()
                        .is_some_and(|live| live.load(Ordering::Acquire))
            })
        {
            return;
        }
        // Leave one maintenance tick plus exit-animation slack for the surface.
        inner.toast.idle_deadline =
            remaining_ms.map(|ms| Instant::now() + Duration::from_millis(ms.saturating_add(700)));
    }

    /// UI acceptance performs no history I/O, clipboard preparation or input.
    pub fn capture_copy_toast_action(
        &self,
        source_hwnd: isize,
        card_live: Arc<AtomicBool>,
        require_target: bool,
    ) -> Result<NativeCopyToastRequest, String> {
        let (presentation, remembered_target) = {
            let inner = self.inner.lock();
            let current = presentations().lock().unwrap_or_else(|e| e.into_inner());
            let presentation = current
                .get(&owner_key(&inner))
                .filter(|p| p.source_hwnd == source_hwnd)
                .cloned()
                .ok_or(COPY_TOAST_RETIRED)?;
            (presentation, inner.last_non_host_fg)
        };
        let foreground = native_paste_foreground();
        let target_hwnd = if crate::clipboard::is_headless() {
            remembered_target
        } else if foreground != 0 && !crate::input_target::belongs_to_host(foreground) {
            foreground
        } else {
            self.last_window_except(source_hwnd)
        };
        let target = if target_hwnd == 0 {
            None
        } else {
            InputTarget::capture(target_hwnd).ok()
        };
        if require_target && target.is_none() {
            return Err("找不到可接收输入的目标窗口。".into());
        }
        let request = NativeCopyToastRequest {
            owner: Arc::as_ptr(&self.inner) as usize,
            presentation,
            card_live,
            target,
        };
        self.verify_copy_toast_request(&request, foreground, require_target)?;
        Ok(request)
    }

    fn verify_copy_toast_request(
        &self,
        request: &NativeCopyToastRequest,
        _foreground: isize,
        require_target: bool,
    ) -> Result<(), String> {
        if request.owner != Arc::as_ptr(&self.inner) as usize
            || !request.card_live.load(Ordering::Acquire)
            || !request
                .presentation
                .live
                .upgrade()
                .is_some_and(|live| live.load(Ordering::Acquire))
        {
            return Err(COPY_TOAST_RETIRED.into());
        }
        {
            let inner = self.inner.lock();
            if !inner.toast.visible
                || !presentations()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&owner_key(&inner))
                    .is_some_and(|p| Arc::ptr_eq(p, &request.presentation))
            {
                return Err(COPY_TOAST_RETIRED.into());
            }
        }
        if require_target {
            request.target.ok_or("找不到可接收输入的目标窗口。")?;
        }
        Ok(())
    }

    fn copy_toast_request_guard(
        &self,
        request: &NativeCopyToastRequest,
        require_target: bool,
    ) -> Arc<dyn Fn() -> Result<(), String> + Send + Sync> {
        let (host, request) = (self.clone(), request.clone());
        Arc::new(move || {
            host.verify_copy_toast_request(&request, native_paste_foreground(), require_target)
        })
    }

    /// Paste/simulate never enter QP's session state or borrow its ticket. Only
    /// successfully consumed cards leave the native stack; failures stay visible.
    pub fn run_copy_toast_native_action(
        &self,
        params: &Value,
        request: &NativeCopyToastRequest,
    ) -> Result<Value, String> {
        self.run_copy_toast_native_action_for_foreground(params, request, native_paste_foreground)
    }

    fn run_copy_toast_native_action_for_foreground(
        &self,
        params: &Value,
        request: &NativeCopyToastRequest,
        mut foreground: impl FnMut() -> isize,
    ) -> Result<Value, String> {
        let action = req_str(params, "action")?;
        let input = matches!(action.as_str(), "paste" | "paste-enter" | "simulate");
        self.verify_copy_toast_request(request, foreground(), input)?;
        let id = req_str(params, "id")?;
        let (store, paths, opts) = {
            let inner = self.inner.lock();
            (
                inner.store.clone(),
                inner.paths.clone(),
                paste_options_from_config(&inner.config),
            )
        };
        let item = store.lock().get_clip(&id).map_err(item_not_found)?;
        let item = if action == "favorite" {item} else {
            store.lock().core_io_clip(&item).map_err(|e|e.to_string())?
        };
        self.verify_copy_toast_request(request, foreground(), input)?;
        let guard = self.copy_toast_request_guard(request, input);
        match action.as_str() {
            "paste" | "paste-enter" => {
                let target = request.target.ok_or("找不到可接收输入的目标窗口。")?;
                let mut paste_opts = opts.clone();
                let activate_guard = guard.clone();
                self.run_input(move || {
                    activate_guard()?;
                    target.activate_for_paste()
                })?;
                paste_opts.expected_target = Some(target);
                let copy_opts = paste_opts.clone();
                let prepare_guard = guard.clone();
                let write_guard = guard.clone();
                self.run_clipboard_write_then_guarded(
                    move || {
                        prepare_guard()?;
                        crate::paste::prepare_copy_record(&item, &copy_opts)
                    },
                    |sequence| {
                        paste_opts.expected_clipboard_sequence = sequence;
                        let input_guard = guard.clone();
                        self.run_input(move || {
                            crate::paste::send_paste_with_guard(&paste_opts, || input_guard())
                        })
                    },
                    None,
                    Arc::new(move || {
                        write_guard()?;
                        target.verify_for_paste()
                    }),
                )?;
                self.touch_clip_usage(&id);
                if action == "paste-enter" {
                    let result = self.run_input(move || {
                        crate::paste::send_enter_after_to_with_guard(
                            crate::paste::TOAST_PASTE_ENTER_DELAY_MS,
                            Some(target),
                            || guard(),
                        )
                    });
                    if let Err(error) = result {
                        // The paste already succeeded: no replay or rollback.
                        return Ok(
                            json!({"ok":true,"consumed":true,"warning":format!("粘贴已完成，但追加回车失败：{error}")}),
                        );
                    }
                }
            }
            "simulate" => {
                if matches!(item.kind, ClipKind::FileClipItem | ClipKind::ImageClipItem) {
                    return Err("该剪贴项不支持模拟输入。".into());
                }
                let text = resolve_simulate_text(&item);
                if text.is_empty() {
                    return Err("没有可模拟输入的文本。".into());
                }
                let target = request.target.ok_or("找不到可接收输入的目标窗口。")?;
                let activating = guard.clone();
                self.run_input(move || {
                    activating()?;
                    target.activate_for_paste()
                })?;
                self.run_input(move || {
                    crate::paste::simulate_text_to_with_guard(&text, 0, Some(target), || guard())
                })?;
                self.touch_clip_usage(&id);
            }
            "copy-only" => {
                let prepare_guard = guard.clone();
                self.run_clipboard_write_then_guarded(
                    move || {
                        prepare_guard()?;
                        crate::paste::prepare_copy_record(&item, &opts)
                    },
                    |_| Ok(()),
                    None,
                    guard,
                )?;
                return Ok(json!({"ok":true,"consumed":false}));
            }
            "favorite" => {
                guard()?;
                // Reuse the same original-payload/image-library conversion,
                // then recheck after any image read/copy before inserting.
                let mut clip = item;
                let source_id = clip.id.clone();
                let source_revision = clip.storage_revision.clone();
                let stored_core = clip.storage_revision.as_deref().is_some_and(|r| r.starts_with("core2:"));
                apply_favorite_to_clip(&mut clip, &paths.image_library)?;
                let favor = FavorRecord::from_clip(&clip, None);
                guard()?;
                {
                    let db = store.lock();
                    if stored_core {
                        db.insert_favor_from_source(&favor, "clip_item", &source_id, source_revision.as_deref())
                    } else {
                        db.insert_favor(&favor)
                    }.map_err(|error| error.to_string())?;
                }
                self.emit_favorites_changed(&[favor.id.clone()], &[]);
                return Ok(json!({"ok":true,"consumed":false}));
            }
            "pin" => {
                let provider = req_str(params, "provider")?.to_ascii_lowercase();
                let (snipaste, pixpin) = crate::pin_overlay::status();
                if !matches!(
                    (provider.as_str(), snipaste, pixpin),
                    ("snipaste", true, _) | ("pixpin", _, true)
                ) || !crate::pin_overlay::item_ready_for_pin(&item, &paths)
                {
                    return Err("贴图工具或剪贴项当前不可用。".into());
                }
                let prepare_guard = guard.clone();
                let write_guard = guard.clone();
                self.run_clipboard_write_then_guarded(
                    move || {
                        prepare_guard()?;
                        crate::paste::prepare_copy_record(&item, &opts)
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
                        crate::pin_overlay::pin_clipboard(&provider)
                    },
                    None,
                    write_guard,
                )?;
            }
            "open" => {
                let value = req_str(params, "value")?;
                let uri = params["targetKind"].as_str() == Some("uri");
                let full = if uri {
                    crate::shell_path::normalize_shell_uri(&value)?
                } else {
                    crate::shell_path::normalize_local_shell_path(&value)?
                };
                if !uri
                    && matches!(item.kind, ClipKind::FileClipItem | ClipKind::ImageClipItem)
                    && !file_list_contains_member(&item.file_paths, &value)
                    && item.image_path.as_deref() != Some(value.as_str())
                {
                    return Err("该路径不在当前剪贴项中。".into());
                }
                guard()?;
                self.open_shell_target(&full)?;
            }
            "reveal-folder" => {
                let path = csharp_get_first_file(&item);
                let folder = Path::new(&path)
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .ok_or("无法解析目标文件夹路径。")?
                    .to_string_lossy()
                    .into_owned();
                guard()?;
                self.reveal_existing_local_path(&folder)?;
            }
            _ => return Err("不支持的复制提示操作。".into()),
        }
        Ok(json!({"ok":true,"consumed":true}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn fixture() -> (
        tempfile::TempDir,
        HostState,
        String,
        Arc<AtomicBool>,
        Arc<AtomicBool>,
    ) {
        crate::clipboard::force_headless(true);
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
        std::fs::create_dir_all(&root).unwrap();
        let temp = tempfile::Builder::new()
            .prefix("native-copy-toast-")
            .tempdir_in(root)
            .unwrap();
        let host =
            HostState::open(AppPaths::from_data_root(temp.path().to_path_buf()), 29065).unwrap();
        let id = host
            .add_text(&json!({"text":"original toast payload"}))
            .unwrap()["createdIds"][0]
            .as_str()
            .unwrap()
            .to_string();
        host.inner.lock().last_non_host_fg = 42;
        host.toast_show();
        let live = Arc::new(AtomicBool::new(true));
        let card = Arc::new(AtomicBool::new(true));
        host.register_copy_toast_native(100, live.clone()).unwrap();
        (temp, host, id, live, card)
    }

    #[test]
    fn copy_toast_uses_its_own_lifetime_and_allows_new_foreground() {
        for action in ["paste", "paste-enter", "simulate"] {
            let (_temp, host, id, _live, card) = fixture();
            let request = host.capture_copy_toast_action(100, card, true).unwrap();
            assert_eq!(request.presentation.source_hwnd, 100);
            assert_eq!(request.target.unwrap().hwnd(), 42);
            host.inner.lock().last_non_host_fg = 43;
            let mut checks = 0;
            let result = host.run_copy_toast_native_action_for_foreground(
                &json!({"action":action,"id":id}),
                &request,
                || {
                    checks += 1;
                    if checks == 1 {
                        42
                    } else {
                        43
                    }
                },
            );
            assert!(result.is_ok(), "{action}: {result:?}");
            assert_eq!(host.store().lock().get_clip(&id).unwrap().usage_count, 1);
            assert!(host.inner.lock().toast.visible);
        }
    }

    #[test]
    fn new_copy_keeps_old_card_tickets_but_hide_reopen_retires_them() {
        let (_temp, host, _id, live, card) = fixture();
        let request = host.capture_copy_toast_action(100, card, true).unwrap();
        host.toast_show();
        assert!(host.verify_copy_toast_request(&request, 42, true).is_ok());
        retire_copy_toast_native_locked(&host.inner.lock());
        assert!(!live.load(Ordering::Acquire));
        host.toast_hide("test");
        host.toast_show();
        host.register_copy_toast_native(100, Arc::new(AtomicBool::new(true)))
            .unwrap();
        assert!(host.verify_copy_toast_request(&request, 42, true).is_err());
    }

    #[test]
    fn card_retirement_stops_before_writing_even_after_foreground_changes() {
        for mode in ["card", "read", "queue"] {
            let (_temp, host, id, _live, card) = fixture();
            let request = host
                .capture_copy_toast_action(100, card.clone(), true)
                .unwrap();
            let writes = Arc::new(AtomicUsize::new(0));
            let written = writes.clone();
            let queued_card = card.clone();
            host.set_clipboard_ui_runner(move |job| {
                if mode == "queue" {
                    queued_card.store(false, Ordering::Release);
                }
                job()?;
                written.fetch_add(1, Ordering::SeqCst);
                Ok(())
            });
            if mode == "card" {
                card.store(false, Ordering::Release);
            }
            let mut calls = 0;
            let result = host.run_copy_toast_native_action_for_foreground(
                &json!({"action":"paste","id":id}),
                &request,
                || {
                    calls += 1;
                    if mode == "read" && calls == 2 {
                        card.store(false, Ordering::Release);
                    }
                    43
                },
            );
            assert!(result.is_err(), "{mode}");
            assert_eq!(writes.load(Ordering::SeqCst), 0, "{mode}");
            assert_eq!(host.store().lock().get_clip(&id).unwrap().usage_count, 0);
        }
    }

    #[test]
    fn copy_without_target_and_simulation_do_not_enter_qp_session() {
        let (_temp, host, id, _live, card) = fixture();
        host.inner.lock().last_non_host_fg = 0;
        let request = host
            .capture_copy_toast_action(100, card.clone(), false)
            .unwrap();
        assert!(host
            .run_copy_toast_native_action(&json!({"action":"copy-only","id":id}), &request)
            .is_ok());
        assert!(host
            .capture_copy_toast_action(100, card.clone(), true)
            .is_err());
        host.inner.lock().last_non_host_fg = 42;
        let request = host.capture_copy_toast_action(100, card, true).unwrap();
        assert!(host
            .run_copy_toast_native_action(&json!({"action":"simulate","id":id}), &request)
            .is_ok());
        assert!(host.inner.lock().qp.last_simulated_text.is_none());
    }
}
