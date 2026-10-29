//! The embedded FIFO preview owns a clipboard context-menu lease, rather than
//! a history paste ticket. Reads and arming retain that exact presentation.
//!
//! Mount as a host.rs child and re-export NativeFifoPreviewRequest. The GPUI
//! parent passes `nativeMenuLease` in the preview params and keeps that lease
//! until arming completes; panel retirement releases it before discarding UI.

use super::*;

const NATIVE_FIFO_PREVIEW_RETIRED: &str = "连续粘贴预览会话已失效，请重新选择队列。";

#[derive(Clone, Debug, PartialEq, Eq)]
struct FifoPreviewIdentity {
    owner: usize,
    source_hwnd: isize,
    window_epoch: u64,
    presentation_epoch: u64,
    lease: u64,
    lease_owner: Option<&'static str>,
    popup_hwnd: isize,
    fifo_generation: u64,
    clipboard_sequence: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct NativeFifoPreviewRequest {
    identity: FifoPreviewIdentity,
    arm_receipt: Arc<Mutex<Option<FifoArmReceipt>>>,
}

#[derive(Clone, Debug)]
struct FifoArmReceipt {
    window_epoch: u64,
    presentation_epoch: u64,
    generation: u64,
    ids: Vec<String>,
}

#[derive(Clone, Debug)]
struct FifoPreviewSnapshot {
    identity: FifoPreviewIdentity,
    ready: bool,
}

fn fifo_preview_request_matches(
    request: &NativeFifoPreviewRequest,
    snapshot: &FifoPreviewSnapshot,
) -> bool {
    snapshot.ready
        && request.identity == snapshot.identity
        && request.identity.source_hwnd != 0
        && request.identity.lease != 0
        && request.identity.lease_owner == Some("clipboard")
        && request.identity.popup_hwnd == request.identity.source_hwnd
}

fn fifo_preview_snapshot(
    inner: &HostInner,
    owner: usize,
    clipboard_sequence: Option<u32>,
) -> FifoPreviewSnapshot {
    let menu = inner.native_context_menu;
    FifoPreviewSnapshot {
        identity: FifoPreviewIdentity {
            owner,
            source_hwnd: inner.host_hwnd,
            window_epoch: inner.chrome_input_epochs[0].window,
            presentation_epoch: inner.chrome_presentation_epochs[0],
            lease: menu.map_or(0, |menu| menu.id),
            lease_owner: menu.map(|menu| menu.owner),
            popup_hwnd: menu.map_or(0, |menu| menu.popup_hwnd),
            fifo_generation: inner.fifo_generation,
            clipboard_sequence,
        },
        ready: inner.window_runtime_ready
            && inner.clipboard_web_view_ready
            && inner.window_visible
            && !inner.is_collapsed
            && !inner.edge_tucked
            && !inner.ui.clipboard_editing,
    }
}

fn fifo_preview_clipboard_sequence() -> Option<u32> {
    // Headless fixtures must never probe the user's clipboard. Pure guard
    // tests inject sequences to cover copies before capture is committed.
    (!crate::clipboard::is_headless()).then(crate::clipboard::sequence_number)
}

#[derive(Clone, Copy)]
enum ArmCheckpoint {
    AfterRead,
    BeforeCommit,
}

impl HostState {
    /// UI acceptance performs no history reads, clipboard preparation or input.
    /// Capture before preview loading so a new copy also retires an unarmed
    /// preview (its OS sequence changes even if no FIFO existed to cancel).
    pub fn capture_native_fifo_preview(
        &self,
        lease: u64,
        source_hwnd: isize,
    ) -> Result<NativeFifoPreviewRequest, String> {
        let inner = self.inner.lock();
        let snapshot = fifo_preview_snapshot(
            &inner,
            Arc::as_ptr(&self.inner) as usize,
            fifo_preview_clipboard_sequence(),
        );
        let request = NativeFifoPreviewRequest {
            identity: FifoPreviewIdentity {
                lease,
                source_hwnd,
                ..snapshot.identity.clone()
            },
            arm_receipt: Arc::new(Mutex::new(None)),
        };
        if !fifo_preview_request_matches(&request, &snapshot) {
            return Err(NATIVE_FIFO_PREVIEW_RETIRED.into());
        }
        Ok(request)
    }

    fn verify_native_fifo_preview(&self, request: &NativeFifoPreviewRequest) -> Result<(), String> {
        let inner = self.inner.lock();
        self.verify_native_fifo_preview_locked(&inner, request)
    }

    pub fn native_fifo_preview_is_current(&self, request: &NativeFifoPreviewRequest) -> bool {
        self.verify_native_fifo_preview(request).is_ok()
    }

    /// Successful arming owns precisely the source hide performed by that
    /// transaction. A later show/hide, copy, rearm or different queue retires
    /// this receipt; hidden alone can never authorize a late completion.
    pub fn native_fifo_arm_reply_is_current(
        &self,
        request: &NativeFifoPreviewRequest,
        status: &Value,
    ) -> bool {
        let Some(receipt) = request.arm_receipt.lock().clone() else {
            return false;
        };
        let inner = self.inner.lock();
        request.identity.owner == Arc::as_ptr(&self.inner) as usize
            && status["armed"].as_bool() == Some(true)
            && status["remainingIds"] == json!(receipt.ids)
            && inner.host_hwnd == request.identity.source_hwnd
            && inner.window_runtime_ready
            && inner.clipboard_web_view_ready
            && !inner.window_visible
            && inner.native_context_menu.is_none()
            && inner.chrome_input_epochs[0].window == receipt.window_epoch
            && inner.chrome_presentation_epochs[0] == receipt.presentation_epoch
            && inner.fifo_generation == receipt.generation
            && inner.fifo.as_ref() == Some(&receipt.ids)
            && fifo_preview_clipboard_sequence() == request.identity.clipboard_sequence
    }

    pub(super) fn verify_native_fifo_preview_locked(
        &self,
        inner: &HostInner,
        request: &NativeFifoPreviewRequest,
    ) -> Result<(), String> {
        let current = fifo_preview_snapshot(
            inner,
            Arc::as_ptr(&self.inner) as usize,
            fifo_preview_clipboard_sequence(),
        );
        if !fifo_preview_request_matches(request, &current) {
            return Err(NATIVE_FIFO_PREVIEW_RETIRED.into());
        }
        Ok(())
    }

    pub fn fifo_preview_native(
        &self,
        params: &Value,
        request: &NativeFifoPreviewRequest,
    ) -> Result<Value, String> {
        self.verify_native_fifo_preview(request)?;
        let preview = self.fifo_preview(params)?;
        self.verify_native_fifo_preview(request)?;
        Ok(preview)
    }

    /// FIFO prepares IDs, not clipboard formats. The eventual QP paste keeps
    /// the existing payload/target/write/input guards and queue generation.
    pub fn fifo_arm_native_preview(
        &self,
        params: &Value,
        request: &NativeFifoPreviewRequest,
    ) -> Result<Value, String> {
        self.fifo_arm_native_preview_at_checkpoints(params, request, |_| {})
    }

    fn fifo_arm_native_preview_at_checkpoints(
        &self,
        params: &Value,
        request: &NativeFifoPreviewRequest,
        mut checkpoint: impl FnMut(ArmCheckpoint),
    ) -> Result<Value, String> {
        self.verify_native_fifo_preview(request)?;
        if params.is_null() {
            return Err("Request body is required.".into());
        }
        let raw = params["ids"]
            .as_array()
            .filter(|ids| !ids.is_empty())
            .ok_or("ids is empty.")?;
        let mut seen = HashSet::new();
        let ids: Vec<String> = raw
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .filter(|id| seen.insert((*id).to_string()))
            .map(str::to_string)
            .collect();
        if ids.is_empty() {
            return Err("Unable to arm FIFO paste.".into());
        }

        let store = self.inner.lock().store.clone();
        let items = {
            let db = store.lock();
            ids.iter()
                .map(|id| db.get_clip(id).map_err(item_not_found))
                .collect::<Result<Vec<_>, _>>()?
        };
        checkpoint(ArmCheckpoint::AfterRead);
        // Do not turn a stale history result into a prepared queue. No locks
        // over history I/O are retained when checking the presentation.
        self.verify_native_fifo_preview(request)?;
        let next_summary = items.first().and_then(|item| {
            let dto = clip_to_dto(item, None);
            crate::fifo::hud_preview_text(&dto.summary, &dto.display_text, &dto.title)
        });
        let queue = ids.clone();
        checkpoint(ArmCheckpoint::BeforeCommit);
        {
            let mut inner = self.inner.lock();
            // Lease retirement, cancel/rearm and the source's hide/reopen all
            // serialize against this final check AND mutation. Never call the
            // unguarded arm helper or hide after unlocking this presentation.
            self.verify_native_fifo_preview_locked(&inner, request)?;
            Self::qp_retire_fifo_session_locked(&mut inner);
            inner.fifo_generation = inner.fifo_generation.wrapping_add(1);
            inner.fifo_total = queue.len() as i32;
            inner.fifo = Some(queue);
            inner.fifo_last_activity = Some(Instant::now());
            hide_clipboard_window_locked(&mut inner);
            *request.arm_receipt.lock() = Some(FifoArmReceipt {
                window_epoch: inner.chrome_input_epochs[0].window,
                presentation_epoch: inner.chrome_presentation_epochs[0],
                generation: inner.fifo_generation,
                ids: ids.clone(),
            });
        }
        // Existing HUD code reads outside the state lock and verifies its own
        // queue generation. A subsequent copy/cancel must remain effective.
        self.fifo_show_armed_hud();
        Ok(json!({
            "armed": true,
            "ids": ids,
            "remainingIds": ids,
            "remainingCount": ids.len(),
            "totalCount": ids.len(),
            "nextId": ids.first(),
            "nextSummary": next_summary,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard_fixture() -> (NativeFifoPreviewRequest, FifoPreviewSnapshot) {
        let identity = FifoPreviewIdentity {
            owner: 1,
            source_hwnd: 100,
            window_epoch: 2,
            presentation_epoch: 3,
            lease: 4,
            lease_owner: Some("clipboard"),
            popup_hwnd: 100,
            fifo_generation: 5,
            clipboard_sequence: Some(6),
        };
        (
            NativeFifoPreviewRequest {
                identity: identity.clone(),
                arm_receipt: Arc::new(Mutex::new(None)),
            },
            FifoPreviewSnapshot {
                identity,
                ready: true,
            },
        )
    }

    #[test]
    fn fifo_guard_rejects_lease_source_owner_generation_and_new_copy_changes() {
        let (request, current) = guard_fixture();
        assert!(fifo_preview_request_matches(&request, &current));
        for mode in [
            "owner",
            "source",
            "window",
            "presentation",
            "lease",
            "lease-owner",
            "popup",
            "fifo",
            "copy",
            "hidden",
        ] {
            let mut changed = current.clone();
            match mode {
                "owner" => changed.identity.owner += 1,
                "source" => changed.identity.source_hwnd += 1,
                "window" => changed.identity.window_epoch += 1,
                "presentation" => changed.identity.presentation_epoch += 1,
                "lease" => changed.identity.lease += 1,
                "lease-owner" => changed.identity.lease_owner = Some("copy-toast"),
                "popup" => changed.identity.popup_hwnd += 1,
                "fifo" => changed.identity.fifo_generation += 1,
                "copy" => changed.identity.clipboard_sequence = Some(7),
                _ => changed.ready = false,
            }
            assert!(!fifo_preview_request_matches(&request, &changed), "{mode}");
        }
    }

    #[test]
    fn fifo_guard_rejects_unregistered_sources_and_other_surface_leases() {
        for mode in ["source", "lease", "owner", "popup"] {
            let (mut request, mut snapshot) = guard_fixture();
            match mode {
                "source" => snapshot.identity.source_hwnd = 0,
                "lease" => snapshot.identity.lease = 0,
                "owner" => snapshot.identity.lease_owner = Some("panel"),
                _ => snapshot.identity.popup_hwnd = 200,
            }
            request.identity = snapshot.identity.clone();
            assert!(!fifo_preview_request_matches(&request, &snapshot), "{mode}");
        }
    }

    fn fixture() -> (tempfile::TempDir, HostState, String) {
        crate::clipboard::force_headless(true);
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
        std::fs::create_dir_all(&root).unwrap();
        let temp = tempfile::Builder::new()
            .prefix("native-fifo-preview-")
            .tempdir_in(root)
            .unwrap();
        let host =
            HostState::open(AppPaths::from_data_root(temp.path().to_path_buf()), 29065).unwrap();
        let id = host
            .add_text(&json!({"text":"original FIFO payload"}))
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
        }
        assert!(host.acquire_native_context_menu(7, "clipboard"));
        assert!(host.set_native_context_menu_hwnd(7, 100));
        (temp, host, id)
    }

    #[test]
    fn native_fifo_arm_uses_exact_ids_and_hides_its_own_source_atomically() {
        let (_temp, host, id) = fixture();
        let request = host.capture_native_fifo_preview(7, 100).unwrap();
        assert!(
            host.fifo_preview_native(&json!({"ids":[id]}), &request)
                .is_ok()
        );
        // Opening the preview's own SPA guard retires list shortcuts only.
        host.apply_window_action("setSpaOverlayOpen", true);
        let status = host
            .fifo_arm_native_preview(&json!({"ids":[id]}), &request)
            .unwrap();
        assert_eq!(status["remainingIds"], json!([id]));
        assert!(!host.inner.lock().window_visible);
        assert!(host.verify_native_fifo_preview(&request).is_err());
        assert!(host.native_fifo_arm_reply_is_current(&request, &status));
        host.request_show();
        host.window_hide();
        assert!(
            !host.native_fifo_arm_reply_is_current(&request, &status),
            "a second hide cannot revive the arm receipt"
        );
    }

    #[test]
    fn native_fifo_close_after_read_and_before_commit_preserves_existing_queue() {
        for stage in [ArmCheckpoint::AfterRead, ArmCheckpoint::BeforeCommit] {
            let (_temp, host, id) = fixture();
            host.arm_fifo_queue(vec![id.clone()]);
            let request = host.capture_native_fifo_preview(7, 100).unwrap();
            let generation = host.inner.lock().fifo_generation;
            let result = host.fifo_arm_native_preview_at_checkpoints(
                &json!({"ids":[id]}),
                &request,
                |checkpoint| {
                    if std::mem::discriminant(&checkpoint) == std::mem::discriminant(&stage) {
                        host.release_native_context_menu(7);
                    }
                },
            );
            assert!(result.is_err());
            let inner = host.inner.lock();
            assert_eq!(inner.fifo.as_ref().unwrap(), &[id]);
            assert_eq!(inner.fifo_generation, generation);
            assert!(inner.window_visible);
        }
    }

    #[test]
    fn native_fifo_old_preview_cannot_overwrite_rearmed_queue_or_source() {
        for mode in ["cancel", "rearm", "hide-reopen", "hwnd", "lease"] {
            let (_temp, host, id) = fixture();
            let request = host.capture_native_fifo_preview(7, 100).unwrap();
            match mode {
                "cancel" => {
                    host.fifo_cancel();
                }
                "rearm" => {
                    host.arm_fifo_queue(vec!["replacement".into()]);
                }
                "hide-reopen" => {
                    host.window_hide();
                    host.inner.lock().window_visible = true;
                }
                "hwnd" => host.inner.lock().host_hwnd = 101,
                _ => {
                    host.acquire_native_context_menu(8, "clipboard");
                    host.set_native_context_menu_hwnd(8, 100);
                }
            }
            let before = host.inner.lock().fifo.clone();
            assert!(
                host.fifo_arm_native_preview(&json!({"ids":[id]}), &request)
                    .is_err(),
                "{mode}"
            );
            assert_eq!(host.inner.lock().fifo, before, "{mode}");
            assert!(host.inner.lock().window_visible, "{mode}");
        }
    }

    #[test]
    fn native_fifo_new_copy_after_read_does_not_replace_its_new_queue() {
        let (_temp, host, id) = fixture();
        let next = host.add_text(&json!({"text":"new FIFO payload"})).unwrap()["createdIds"][0]
            .as_str()
            .unwrap()
            .to_string();
        host.arm_fifo_queue(vec![id.clone()]);
        let request = host.capture_native_fifo_preview(7, 100).unwrap();
        let result = host.fifo_arm_native_preview_at_checkpoints(
            &json!({"ids":[id]}),
            &request,
            |checkpoint| {
                if matches!(checkpoint, ArmCheckpoint::AfterRead) {
                    host.on_new_clip_added();
                    host.arm_fifo_queue(vec![next.clone()]);
                }
            },
        );
        assert!(result.is_err());
        assert_eq!(host.inner.lock().fifo.as_ref().unwrap(), &[next]);
        assert!(host.inner.lock().window_visible);
    }
}
