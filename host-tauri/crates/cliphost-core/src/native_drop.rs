//! Native OLE snapshots use the existing format decoders and capture buckets.
//! No IDataObject, HGLOBAL, native COM interface or clipboard peek enters here.
//!
//! Explicit drop format order follows existing panel/Favor typed drag behavior:
//! Quicker action > file drop > Quicker steps > rich/image/text collector order.
//! The first three use the existing capture bucket with the entire unmodified
//! snapshot; generic HTML/IM/PNG/text keeps collector's source-specific order.
//! A standalone image precedes its auxiliary plain-text fallback.
//! Explicit drops do not run automatic-monitor whitespace/text transformations.
//!
//! Admission happens on the UI at Drop acceptance with its HWND/page ticket and
//! captured destination. Already accepted writes keep that directory even after
//! navigation (the current Tauri panel-action-drop contract). Stale events that
//! have not been accepted fail admission; replies must use request_is_current.

use super::*;

const MAX_DROP_FORMAT_BYTES: usize = 32 * 1024 * 1024;
const DROP_RETIRED: &str = "Drag request belongs to a retired window or page.";

type DropGuard = Arc<dyn Fn() -> Result<(), String> + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Surface {
    History,
    Favorites,
    Panel,
}

/// Immutable accepted destination; cloned tickets share one persistence claim.
/// A view may inspect this after worker completion, but must not manufacture it.
#[derive(Clone)]
pub struct NativeDropRequest {
    owner: usize,
    surface: Surface,
    source_hwnd: isize,
    page_session: u64,
    window_epoch: u64,
    presentation_epoch: u64,
    parent_id: Option<String>,
    insert_index: i32,
    source_window: isize,
    used: Arc<AtomicBool>,
    guard: DropGuard,
}

impl std::fmt::Debug for NativeDropRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeDropRequest")
            .field("surface", &self.surface)
            .field("source_hwnd", &self.source_hwnd)
            .field("page_session", &self.page_session)
            .field("parent_id", &self.parent_id)
            .field("insert_index", &self.insert_index)
            .finish_non_exhaustive()
    }
}

impl NativeDropRequest {
    pub fn page_session(&self) -> u64 {
        self.page_session
    }
    pub fn source_hwnd(&self) -> isize {
        self.source_hwnd
    }
    pub fn parent_id(&self) -> Option<&str> {
        self.parent_id.as_deref()
    }
}

impl HostState {
    /// Scoped incoming OLE hover guard. It protects the owning window's hide
    /// policy without claiming caption dragging or an outgoing DoDragDrop.
    /// Call synchronously on DragEnter and release this returned ID on Leave.
    pub fn acquire_native_incoming_drag(&self, label: &str, hwnd: isize) -> Result<u64, String> {
        let surface = match label {
            "clipboard" | "history" => Surface::History,
            "favorites" | "favor" => Surface::Favorites,
            "panel" => Surface::Panel,
            _ => return Err("Unsupported native drop source.".into()),
        };
        let mut inner = self.inner.lock();
        verify_drop_surface(&inner, surface, hwnd)?;
        inner.native_incoming_drag_next_id = inner
            .native_incoming_drag_next_id
            .checked_add(1)
            .ok_or("Incoming drag sequence is exhausted.")?;
        let id = inner.native_incoming_drag_next_id;
        let owner = if surface == Surface::Panel {
            "panel"
        } else {
            "clipboard"
        };
        inner.native_incoming_drag = Some(NativeIncomingDragLease { id, owner });
        if owner == "panel" {
            drop_queued_panel_navigation(&mut inner);
        } else {
            drop_queued_clipboard_list_input(&mut inner);
            inner.clipboard_motion_generation = inner.clipboard_motion_generation.wrapping_add(1);
        }
        Ok(id)
    }

    /// A late Leave can only release its own lease, never a subsequent Enter.
    pub fn release_native_incoming_drag(&self, id: u64) -> bool {
        let mut inner = self.inner.lock();
        if !inner.native_incoming_drag.is_some_and(|drag| drag.id == id) {
            return false;
        }
        inner.native_incoming_drag = None;
        true
    }

    /// Production registered-text decoder, including bounded WPF NRBF strings.
    pub fn decode_native_drop_registered_text(bytes: &[u8]) -> Result<String, String> {
        check_format_size(bytes)?;
        crate::clipboard_text::decode(bytes)
            .ok_or_else(|| "Invalid registered text drag format.".into())
    }

    /// Retain each clipboard format's established byte/terminator policy.
    /// Chromium source metadata with no absolute URI becomes an empty optional
    /// value, as the clipboard reader already ignores invalid source metadata.
    pub fn decode_native_drop_rich_text(name: &str, bytes: &[u8]) -> Result<String, String> {
        check_format_size(bytes)?;
        match name {
            "HTML Format" => crate::clipboard::decode_cf_html_bytes(bytes)
                .ok_or_else(|| "Empty HTML drag format.".into()),
            "Chromium internal source URL" => {
                let raw = crate::clipboard::decode_cf_html_bytes(bytes);
                Ok(
                    crate::clipboard::sanitize_chromium_source_url(raw.as_deref())
                        .unwrap_or_default(),
                )
            }
            "QQ_Unicode_RichEdit_Format" | "WeChat_RichEdit_Format" => {
                crate::clipboard::decode_im_clipboard_bytes(bytes)
                    .ok_or_else(|| "Empty IM rich-text drag format.".into())
            }
            _ => Self::decode_native_drop_registered_text(bytes),
        }
    }

    pub fn decode_native_drop_quicker_action(bytes: &[u8]) -> Result<String, String> {
        check_format_size(bytes)?;
        crate::quicker_wire::decode(bytes)
            .ok_or_else(|| "Invalid Quicker action drag format.".into())
    }

    pub fn decode_native_drop_dib(bytes: &[u8], v5: bool) -> Result<Vec<u8>, String> {
        check_format_size(bytes)?;
        let png = if v5 {
            crate::clipboard::dibv5_to_png(bytes.to_vec())
        } else {
            crate::clipboard::dib_to_png(bytes.to_vec())
        };
        png.ok_or_else(|| "Invalid DIB image drag format.".into())
    }

    /// Pure admission, without database/image work. `ticket_guard` should be the
    /// GPUI DropTicket::verify closure captured with the copied DropEvent.
    /// Labels: clipboard/history adds history; favorites/favor and panel add to
    /// the captured favorite directory. Synthetic root is never a leaf target.
    pub fn accept_native_drop(
        &self,
        source_label: &str,
        source_hwnd: isize,
        page_session: u64,
        parent_id: Option<&str>,
        insert_index: i32,
        ticket_guard: impl Fn() -> Result<(), String> + Send + Sync + 'static,
    ) -> Result<NativeDropRequest, String> {
        let surface = match source_label {
            "clipboard" | "history" => Surface::History,
            "favorites" | "favor" => Surface::Favorites,
            "panel" => Surface::Panel,
            _ => return Err("Unsupported native drop source.".into()),
        };
        ticket_guard()?;
        let parent_id = fixed_parent(surface, parent_id)?;
        let request = {
            let inner = self.inner.lock();
            verify_drop_surface(&inner, surface, source_hwnd)?;
            let slot = if surface == Surface::Panel { 1 } else { 0 };
            NativeDropRequest {
                owner: Arc::as_ptr(&self.inner) as usize,
                surface,
                source_hwnd,
                page_session,
                window_epoch: inner.chrome_input_epochs[slot].window,
                presentation_epoch: inner.chrome_presentation_epochs[slot],
                parent_id,
                insert_index,
                source_window: inner.last_non_host_fg,
                used: Arc::new(AtomicBool::new(false)),
                guard: Arc::new(ticket_guard),
            }
        };
        (request.guard)()?;
        Ok(request)
    }

    /// Only for applying UI results/highlights; persistence deliberately uses
    /// the accepted destination instead of re-reading current folder state.
    pub fn native_drop_request_is_current(&self, request: &NativeDropRequest) -> bool {
        if request.owner != Arc::as_ptr(&self.inner) as usize || (request.guard)().is_err() {
            return false;
        }
        let inner = self.inner.lock();
        let slot = if request.surface == Surface::Panel {
            1
        } else {
            0
        };
        verify_drop_surface(&inner, request.surface, request.source_hwnd).is_ok()
            && inner.chrome_input_epochs[slot].window == request.window_epoch
            && inner.chrome_presentation_epochs[slot] == request.presentation_epoch
    }

    /// Worker-only. The caller has copied the entire IDataObject before passing
    /// this snapshot. A missing/deleted captured folder fails the transaction;
    /// it can never redirect the drop into a newly selected folder or root.
    pub fn save_native_drop(
        &self,
        snapshot: ClipboardSnapshot,
        request: &NativeDropRequest,
    ) -> Result<Value, String> {
        if request.owner != Arc::as_ptr(&self.inner) as usize {
            return Err(DROP_RETIRED.into());
        }
        if request
            .used
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err("This native drop has already been accepted for persistence.".into());
        }
        let (data_root, library) = {
            let inner = self.inner.lock();
            (
                inner.paths.data_root.clone(),
                inner.paths.image_library.clone(),
            )
        };
        let source = if request.source_window != 0 {
            crate::window_menu::inspect(request.source_window as i64)
                .map(|info| info.process_name)
                .unwrap_or_default()
        } else {
            String::new()
        };
        let mut clip = record_from_drop(snapshot, &data_root, &source)?;
        if request.surface == Surface::History {
            let ids = self.insert_clip_burst(vec![clip])?;
            self.trigger_cleanup();
            self.on_new_clip_added();
            return Ok(json!({"ok":true,"count":ids.len(),"createdIds":ids}));
        }
        apply_favorite_to_clip(&mut clip, &library)?;
        verify_favorite_images(&clip, &library)?;
        let mut favor = FavorRecord::from_clip(&clip, request.parent_id.clone());
        // The source exists only in this immutable drop, not history storage.
        favor.source_clip_id = None;
        if favor.kind == ClipKind::ActionClipItem {
            let title = action_clipboard_title(favor.body_text.as_deref().unwrap_or_default());
            if !title.is_empty() {
                favor.name = title;
            }
        }
        self.insert_favors_indexed(std::slice::from_ref(&favor), request.insert_index)?;
        if request.surface == Surface::Panel {
            self.enqueue_chrome(ChromeCommand::PanelRefresh);
        }
        Ok(json!({"ok":true,"count":1,"createdIds":[favor.id]}))
    }

    /// Worker-only raw preparation. Original types/HTML/QQ/files/image/action
    /// steps and output settings go through the same builder as history drag.
    pub fn prepare_favorite_drag(&self, params: &Value) -> Result<OleDragPayload, String> {
        let items = self.favorite_drag_records(params)?;
        self.favorite_drag_payload(&items)
    }

    fn favorite_drag_records(&self, params: &Value) -> Result<Vec<ClipRecord>, String> {
        let ids = param_ids(params);
        let store = self.inner.lock().store.clone();
        let db = store.lock();
        let mut records = Vec::with_capacity(ids.len());
        for id in ids {
            // Match history's missing/empty selection behavior. A selected
            // navigation folder has no external clipboard drag payload.
            match db.get_favor(&id) {
                Ok(favor) if !favor.is_folder && favor.kind != ClipKind::FavoriteClipItem => {
                    records.push(db.core_io_clip(&favor_as_clip(&favor)?).map_err(|e|e.to_string())?)
                }
                Ok(_) | Err(StoreError::NotFound(_)) => {}
                Err(error) => return Err(error.to_string()),
            }
        }
        Ok(records)
    }

    fn favorite_drag_payload(&self, items: &[ClipRecord]) -> Result<OleDragPayload,String> {
        let (plain, standard, newline, target) = {
            let inner = self.inner.lock();
            (
                inner.config.plain_text_paste_by_default,
                inner.config.standard_paste,
                inner.config.paste_with_new_line,
                inner.last_non_host_fg,
            )
        };
        let process = if target != 0 {
            crate::window_menu::inspect(target as i64)
                .map(|info| info.process_name)
                .unwrap_or_default()
        } else {
            String::new()
        };
        crate::ole_drag::payload_from_items(items, plain, standard, newline, &process)
    }

    /// Call through the GPUI DragOutDispatcher::start closure so its original
    /// HWND/session ticket gates the STA runner after background preparation.
    pub fn begin_favor_ole_drag(&self, params: &Value) -> Result<Value, String> {
        let internal_token = params["internalDragToken"].as_str().filter(|value| !value.is_empty()).map(str::to_owned);
        let items = if params["sourceKind"].as_str() == Some("history") {
            let store = self.inner.lock().store.clone();
            let db = store.lock();
            param_ids(params).iter().map(|id| {let item=db.get_clip(id).map_err(|error|error.to_string())?;db.core_io_clip(&item).map_err(|error|error.to_string())}).collect::<Result<Vec<_>, _>>()?
        } else { self.favorite_drag_records(params)? };
        if items.is_empty() && internal_token.is_none() {
            return Ok(json!({"ok":true,"started":false}));
        }
        {
            let mut inner = self.inner.lock();
            if inner.ole_drag_active {
                return Err("A clipboard drag is already in progress.".into());
            }
            // Ending DoDragDrop must not make a previously accepted menu key
            // valid again. Retire only its input binding, not drop admissions.
            inner.super_menu.native_keyboard.retire_binding();
            inner.native_fifo_keyboard.retire_binding();
            inner.ole_drag_active = true;
        }
        struct ActiveDrag<'a>(&'a HostState);
        impl Drop for ActiveDrag<'_> {
            fn drop(&mut self) {
                self.0.inner.lock().ole_drag_active = false;
            }
        }
        let _active = ActiveDrag(self);
        let mut payload = self.favorite_drag_payload(&items)?;
        if internal_token.is_some() {
            // A private token still permits the complete internal move/copy.
            // An external receiver must not receive a silently truncated
            // selection (folders, actions, links or mixed content families).
            let text_family=items.iter().all(|item|matches!(item.kind,ClipKind::TextClipItem|ClipKind::HtmlClipItem|ClipKind::QQRichTextItem));
            let file_family=items.iter().all(|item|matches!(item.kind,ClipKind::FileClipItem|ClipKind::ImageClipItem));
            if items.len()!=param_ids(params).len() || items.len()>1 && !text_family && !file_family {
                payload=OleDragPayload::default();
            }
        }
        payload.internal_drag_token = internal_token;
        for path in &payload.files {
            if !std::path::Path::new(path).exists() {
                return Err(format!("Dragged file no longer exists: {path}"));
            }
        }
        if payload.is_empty() {
            return Ok(json!({"ok":true,"started":false}));
        }
        let runner = self.ole_drag_ui.lock().clone();
        let has_runner = runner.is_some();
        match runner {
            Some(run) => run(payload)?,
            None if crate::clipboard::is_headless() => {}
            None => crate::ole_drag::begin_ole_drag(&payload)?,
        }
        Ok(
            json!({"ok":true,"started":has_runner || !crate::clipboard::is_headless(),"count":items.len()}),
        )
    }

    pub fn begin_favorite_drag(&self, params: &Value) -> Result<Value, String> {
        self.begin_favor_ole_drag(params)
    }
}

fn check_format_size(bytes: &[u8]) -> Result<(), String> {
    if bytes.is_empty() || bytes.len() > MAX_DROP_FORMAT_BYTES {
        Err("Invalid native drop format size.".into())
    } else {
        Ok(())
    }
}

fn fixed_parent(surface: Surface, parent: Option<&str>) -> Result<Option<String>, String> {
    let parent = parent.map(str::trim).filter(|parent| !parent.is_empty());
    if surface == Surface::History {
        if parent.is_some() {
            return Err("History drops do not accept a favorite directory.".into());
        }
        return Ok(None);
    }
    if parent.is_some_and(crate::panel::is_root_folder_id) {
        return Err("Use AddFavoriteFolder with root parent for top-level panels.".into());
    }
    Ok(parent
        .filter(|parent| *parent != crate::panel::DETACHED_ORPHANS_FOLDER_ID)
        .map(str::to_string))
}

fn verify_drop_surface(inner: &HostInner, surface: Surface, hwnd: isize) -> Result<(), String> {
    if hwnd == 0 || !inner.window_runtime_ready {
        return Err(DROP_RETIRED.into());
    }
    let current = match surface {
        Surface::History | Surface::Favorites => {
            inner.host_hwnd == hwnd
                && inner.window_visible
                && inner.clipboard_web_view_ready
                && !inner.is_collapsed
                && !inner.spa_modal_open
                && !inner.spa_overlay_open
                && !native_context_menu_for(inner, "clipboard")
        }
        Surface::Panel => {
            inner.panel_hwnd == hwnd
                && inner.ui.panel_visible
                && inner.panel_web_view_ready
                && !inner.ui.panel_spa_overlay_open
                && !native_context_menu_for(inner, "panel")
        }
    };
    if current {
        Ok(())
    } else {
        Err(DROP_RETIRED.into())
    }
}

fn record_from_drop(
    snapshot: ClipboardSnapshot,
    data_root: &Path,
    source: &str,
) -> Result<ClipRecord, String> {
    if snapshot.exclude_monitor {
        return Err("Excluded clipboard data cannot be imported.".into());
    }
    let source = crate::file_icon::normalize_exe_key(source);
    let typed = if let Some(action) = snapshot
        .quicker_action
        .as_deref()
        .filter(|action| !action.is_empty())
    {
        crate::quicker_action::ActionReference::parse(action)?.require_id()?;
        Some('1')
    } else if !snapshot.files.is_empty() {
        Some('3')
    } else if snapshot
        .quicker_steps
        .as_ref()
        .is_some_and(|steps| !steps.is_empty())
    {
        Some('2')
    } else {
        None
    };
    if let Some(bucket) = typed {
        return crate::clipboard::try_capture_bucket(
            &snapshot,
            data_root,
            &source,
            false,
            "",
            bucket,
            &|_, _| None,
        )
        .ok_or_else(|| "Cannot import the native drag format.".into());
    }
    // A browser PNG follows its existing image-first capture order, and rich
    // carriers use the original envelope/CF_HTML extraction and image library.
    let rich = snapshot
        .qq_rich
        .as_ref()
        .or(snapshot.wechat_rich.as_ref())
        .is_some_and(|text| !text.is_empty())
        || snapshot.html.as_ref().is_some_and(|text| !text.is_empty());
    let image = snapshot.png.is_some() || snapshot.has_image_format;
    let item = if image && !rich {
        crate::clipboard::try_capture_bucket(
            &snapshot,
            data_root,
            &source,
            false,
            "",
            '4',
            &|_, _| None,
        )
    } else {
        crate::clipboard::item_from_snapshot_with_transform(
            snapshot,
            data_root,
            &source,
            false,
            true,
            "",
            &|_, _| None,
        )
    }
    .ok_or_else(|| "Cannot import the native drag format.".to_string())?;
    if rich
        && !matches!(
            item.kind,
            ClipKind::HtmlClipItem | ClipKind::QQRichTextItem | ClipKind::ImageClipItem
        )
    {
        return Err(
            "The rich drag format could not be retained; no text-only record was saved.".into(),
        );
    }
    Ok(item)
}

fn verify_favorite_images(clip: &ClipRecord, library: &Path) -> Result<(), String> {
    let paths = if clip.kind == ClipKind::ImageClipItem {
        vec![clip
            .image_path
            .clone()
            .ok_or("Imported image path is missing.")?]
    } else if clip.kind == ClipKind::QQRichTextItem {
        let raw = clip.qq_rich_text.as_deref().unwrap_or_default();
        let origin = crate::im_rich::resolve_origin(&clip.source_process_name, raw);
        crate::im_rich::collect_image_paths(&crate::im_rich::parse_segments(raw, origin))
    } else {
        Vec::new()
    };
    for path in paths {
        let path = Path::new(&path);
        if !path.is_file()
            || !path.parent().is_some_and(|parent| {
                parent
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&library.to_string_lossy())
            })
        {
            return Err(format!(
                "Could not retain the dragged image in the favorite library: {}",
                path.display()
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_root() -> tempfile::TempDir {
        let root_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
        std::fs::create_dir_all(&root_dir).unwrap();
        tempfile::Builder::new()
            .prefix("native-drop-")
            .tempdir_in(root_dir)
            .unwrap()
    }

    struct Fixture {
        _root: tempfile::TempDir,
        host: HostState,
    }
    fn fixture() -> Fixture {
        crate::clipboard::force_headless(true);
        let root = test_root();
        let host =
            HostState::open(AppPaths::from_data_root(root.path().to_path_buf()), 29065).unwrap();
        {
            let mut inner = host.inner.lock();
            inner.host_hwnd = 101;
            inner.panel_hwnd = 102;
            inner.window_runtime_ready = true;
            inner.window_visible = true;
            inner.clipboard_web_view_ready = true;
            inner.ui.panel_visible = true;
            inner.panel_web_view_ready = true;
            inner.ui.clipboard_route = "/favorites".into();
        }
        Fixture { _root: root, host }
    }

    fn menu_key(vk: u32, down: bool) -> KeyEvent {
        KeyEvent {
            vk,
            down,
            alt: false,
            ctrl: false,
            shift: false,
            win: false,
            now_ms: 1,
        }
    }

    fn bind_menu_keyboard(host: &HostState) -> NativeSuperMenuRequest {
        host.set_super_menu_hwnd(200);
        let context = crate::menu_api::MenuBuildContext {
            source_window_handle: 200,
            target_window_handle: 0,
            ..Default::default()
        };
        let created = host.menus.create_fixture_session(
            &context,
            vec![crate::menu_api::command(
                "owned keyboard fixture",
                "fixture-noop",
                json!({}),
            )],
        );
        let session = created["sessionId"].as_str().unwrap();
        let generation = {
            let mut inner = host.inner.lock();
            inner.last_fg = 42;
            inner.super_menu.visible = true;
            inner.super_menu.generation = inner.super_menu.generation.wrapping_add(1);
            inner.super_menu.session_id = Some(session.into());
            inner.super_menu.snapshot = Some(created.clone());
            inner.super_menu.generation
        };
        let request = host
            .capture_native_super_menu_action(session, generation, 200)
            .unwrap();
        host.bind_native_super_menu_keyboard(&request, NativeSuperMenuKeyScope::WindowHub)
            .unwrap();
        request
    }

    fn dragged_favorite(host: &HostState) -> Value {
        let request = host
            .accept_native_drop("favorites", 101, 1, None, -1, || Ok(()))
            .unwrap();
        host.save_native_drop(
            ClipboardSnapshot {
                text: Some("original favorite drag".into()),
                html: Some("<b>original favorite drag</b>".into()),
                ..Default::default()
            },
            &request,
        )
        .unwrap()["createdIds"]
            .clone()
    }

    #[test]
    fn native_favorite_drag_retires_menu_keyboard_after_success_and_cancel_without_a_drag_time_key()
    {
        for cancel in [false, true] {
            let f = fixture();
            let ids = dragged_favorite(&f.host);
            let presentation = bind_menu_keyboard(&f.host);
            assert!(f
                .host
                .dispatch_low_level_key_for_foreground(menu_key(0x27, true), 42));
            let accepted = f.host.take_native_super_menu_key().unwrap();
            assert!(f.host.native_super_menu_key_is_current(&accepted));
            assert!(f
                .host
                .dispatch_low_level_key_for_foreground(menu_key(0x27, true), 42));
            let observed = f.host.clone();
            let runner = f.host.with_ole_drag_ui_runner(move |payload| {
                assert!(observed.inner.lock().ole_drag_active);
                assert!(payload
                    .html
                    .unwrap()
                    .contains("<b>original favorite drag</b>"));
                // Do not route/query a key during the drag: that could itself
                // retire the old binding and hide the missing begin transition.
                if cancel {
                    Err("owned native drag canceled".into())
                } else {
                    Ok(())
                }
            });
            let result = runner.begin_favor_ole_drag(&json!({"ids":ids}));
            if cancel {
                assert_eq!(result.unwrap_err(), "owned native drag canceled");
            } else {
                assert_eq!(result.unwrap()["started"], true);
            }
            assert!(!f.host.inner.lock().ole_drag_active);
            assert!(!f.host.native_super_menu_key_is_current(&accepted));
            assert!(!f
                .host
                .native_super_menu_request_is_current(accepted.request()));
            assert!(f.host.take_native_super_menu_key().is_none());
            assert!(
                f.host
                    .dispatch_low_level_key_for_foreground(menu_key(0x27, false), 42),
                "a held accepted press drains only its release after retirement"
            );
            assert!(f.host.take_native_super_menu_key().is_none());
            assert!(
                f.host.native_super_menu_request_is_current(&presentation),
                "the drag retires input, not the visible menu presentation"
            );
            assert!(
                f.host
                    .bind_native_super_menu_keyboard(
                        &presentation,
                        NativeSuperMenuKeyScope::WindowHub
                    )
                    .is_err(),
                "a delayed bind must not resurrect this old presentation's keys"
            );
            assert!(!f
                .host
                .dispatch_low_level_key_for_foreground(menu_key(0x27, true), 42));
        }
    }

    #[test]
    fn native_favorite_drag_read_only_preparation_and_empty_selection_keep_the_binding() {
        let f = fixture();
        let ids = dragged_favorite(&f.host);
        let folder = f
            .host
            .favor_folder_create(&json!({"title":"navigation folder"}))
            .unwrap();
        bind_menu_keyboard(&f.host);
        assert!(f
            .host
            .dispatch_low_level_key_for_foreground(menu_key(0x27, true), 42));
        let accepted = f.host.take_native_super_menu_key().unwrap();
        assert!(f
            .host
            .prepare_favorite_drag(&json!({"ids":ids}))
            .unwrap()
            .html
            .is_some());
        assert!(f.host.native_super_menu_key_is_current(&accepted));
        for ids in [
            json!([]),
            json!(["missing-favorite"]),
            json!([folder["id"]]),
        ] {
            assert_eq!(
                f.host.begin_favor_ole_drag(&json!({"ids":ids})).unwrap()["started"],
                false
            );
            assert!(f.host.native_super_menu_key_is_current(&accepted));
        }
        assert!(f
            .host
            .dispatch_low_level_key_for_foreground(menu_key(0x27, false), 42));
        assert!(f.host.take_native_super_menu_key().is_some());
    }

    #[test]
    fn native_favorite_drag_binding_retirement_preserves_the_accepted_drop_transaction() {
        let f = fixture();
        let outgoing = dragged_favorite(&f.host);
        let first = f
            .host
            .favor_folder_create(&json!({"title":"original destination"}))
            .unwrap();
        let second = f
            .host
            .favor_folder_create(&json!({"title":"later destination"}))
            .unwrap();
        let admitted = f
            .host
            .accept_native_drop("panel", 102, 8, first["id"].as_str(), -1, || Ok(()))
            .unwrap();
        bind_menu_keyboard(&f.host);
        assert!(f
            .host
            .dispatch_low_level_key_for_foreground(menu_key(0x27, true), 42));
        let accepted_key = f.host.take_native_super_menu_key().unwrap();
        f.host.inner.lock().ui.panel_folder_id = second["id"].as_str().map(str::to_string);
        let stored = Arc::new(Mutex::new(None::<Value>));
        let saved = stored.clone();
        let original_host = f.host.clone();
        let original_drop = admitted.clone();
        let runner = f.host.with_ole_drag_ui_runner(move |_| {
            assert!(original_host.inner.lock().ole_drag_active);
            *saved.lock() = Some(original_host.save_native_drop(
                ClipboardSnapshot {
                    text: Some("already accepted drop 🦀".into()),
                    html: Some("<b>already accepted drop 🦀</b>".into()),
                    ..Default::default()
                },
                &original_drop,
            )?);
            Ok(())
        });
        assert_eq!(
            runner
                .begin_favorite_drag(&json!({"ids":outgoing}))
                .unwrap()["started"],
            true
        );
        assert!(!f.host.native_super_menu_key_is_current(&accepted_key));
        let result = stored.lock().clone().unwrap();
        let detail = f
            .host
            .favor_get(&json!({"id":result["createdIds"][0]}))
            .unwrap();
        assert_eq!(detail["parentId"], first["id"]);
        assert_eq!(detail["htmlData"], "<b>already accepted drop 🦀</b>");
        assert!(f
            .host
            .save_native_drop(
                ClipboardSnapshot {
                    text: Some("replay".into()),
                    ..Default::default()
                },
                &admitted
            )
            .is_err());
    }

    #[test]
    fn native_incoming_drag_scopes_hide_input_and_late_leave_without_claiming_caption_or_outgoing_drag(
    ) {
        let f = fixture();
        let host = &f.host;
        let first = host.acquire_native_incoming_drag("favorites", 101).unwrap();
        assert!(host.hide_snapshot().move_interaction_active);
        assert!(
            !host.chrome_command_input_allowed(ChromeCommand::RouteDigit {
                digit: 1,
                repeat: false
            })
        );
        {
            let inner = host.inner.lock();
            assert!(!inner.move_interaction_active);
            assert!(!inner.ole_drag_active);
        }
        let next = host.acquire_native_incoming_drag("panel", 102).unwrap();
        assert!(!host.release_native_incoming_drag(first));
        assert!(
            !host.hide_snapshot().move_interaction_active,
            "panel lease only protects panel"
        );
        {
            let inner = host.inner.lock();
            assert!(native_incoming_drag_for(&inner, "panel"));
        }
        host.apply_panel_hide();
        assert!(
            !host.release_native_incoming_drag(next),
            "hide retires its own incoming drag"
        );
        let current = host.acquire_native_incoming_drag("clipboard", 101).unwrap();
        host.inner.lock().move_interaction_active = true;
        assert!(host.release_native_incoming_drag(current));
        assert!(
            host.inner.lock().move_interaction_active,
            "OLE Leave preserves independent caption move"
        );
        assert!(host.acquire_native_incoming_drag("clipboard", 999).is_err());
        host.inner.lock().window_visible = false;
        assert!(host.acquire_native_incoming_drag("clipboard", 101).is_err());
    }

    #[test]
    fn native_drop_decoders_reuse_production_html_im_and_action_wire() {
        let html = " Version:1.0\r\n<b>中😀</b> ";
        assert_eq!(
            HostState::decode_native_drop_rich_text(
                "HTML Format",
                &[html.as_bytes(), &[0, 1, 2]].concat()
            )
            .unwrap(),
            html
        );
        let qq = "<QQRichEditFormat><EditElement type=\"0\" text=\"中😀\" /></QQRichEditFormat>";
        let wire: Vec<_> = qq
            .encode_utf16()
            .chain([0])
            .flat_map(u16::to_le_bytes)
            .collect();
        assert!(
            HostState::decode_native_drop_rich_text("QQ_Unicode_RichEdit_Format", &wire)
                .unwrap()
                .contains("中😀")
        );
        let action = include_bytes!("../tests/fixtures/quicker-drag-v1-wpf.bin");
        assert_eq!(
            HostState::decode_native_drop_quicker_action(action).unwrap(),
            crate::quicker_wire::decode(action).unwrap()
        );
        assert!(HostState::decode_native_drop_quicker_action(&[0xff]).is_err());
        assert!(HostState::decode_native_drop_dib(&[0; 40], false).is_err());
        let png = png_fixture();
        let dib = crate::clipboard::encode_dib_from_bytes(&png).unwrap();
        let decoded = HostState::decode_native_drop_dib(&dib, false).unwrap();
        let image = image::load_from_memory(&decoded).unwrap().to_rgb8();
        assert_eq!(image.dimensions(), (1, 1));
        assert_eq!(image.get_pixel(0, 0).0, [10, 20, 30]);
    }

    fn png_fixture() -> Vec<u8> {
        let mut output = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            1,
            1,
            image::Rgba([10, 20, 30, 128]),
        ))
        .write_to(&mut output, image::ImageFormat::Png)
        .unwrap();
        output.into_inner()
    }

    #[test]
    fn native_drop_format_matrix_preserves_typed_payloads_and_rich_text() {
        let dir = test_root();
        let action = "{\"Id\":\"action-id\",\"Title\":\"动作 🦀\"}";
        let html = "<b>HTML 中😀</b>";
        let qq = "<QQRichEditFormat><EditElement type=\"0\" text=\"QQ 中😀\" /></QQRichEditFormat>";
        for (snapshot, kind) in [
            (
                ClipboardSnapshot {
                    text: Some("fallback".into()),
                    quicker_action: Some(action.into()),
                    ..Default::default()
                },
                ClipKind::ActionClipItem,
            ),
            (
                ClipboardSnapshot {
                    text: Some("fallback".into()),
                    files: vec!["D:\\测试.txt".into(), "D:\\two.txt".into()],
                    ..Default::default()
                },
                ClipKind::FileClipItem,
            ),
            (
                ClipboardSnapshot {
                    text: Some("fallback".into()),
                    quicker_steps: Some("{\"steps\":[]}".into()),
                    ..Default::default()
                },
                ClipKind::StepClipItem,
            ),
            (
                ClipboardSnapshot {
                    text: Some("HTML 中😀".into()),
                    html: Some(html.into()),
                    ..Default::default()
                },
                ClipKind::HtmlClipItem,
            ),
            (
                ClipboardSnapshot {
                    text: Some("QQ 中😀".into()),
                    qq_rich: Some(qq.into()),
                    ..Default::default()
                },
                ClipKind::QQRichTextItem,
            ),
            (
                ClipboardSnapshot {
                    text: Some(" \r\n中😀 \n".into()),
                    ..Default::default()
                },
                ClipKind::TextClipItem,
            ),
        ] {
            let clip = record_from_drop(snapshot, dir.path(), "").unwrap();
            assert_eq!(clip.kind, kind);
            match kind {
                ClipKind::ActionClipItem => assert_eq!(clip.body_text.as_deref(), Some(action)),
                ClipKind::FileClipItem => {
                    assert_eq!(clip.file_paths, ["D:\\测试.txt", "D:\\two.txt"])
                }
                ClipKind::StepClipItem => {
                    assert_eq!(clip.body_text.as_deref(), Some("{\"steps\":[]}"))
                }
                ClipKind::HtmlClipItem => assert_eq!(clip.html_data.as_deref(), Some(html)),
                ClipKind::QQRichTextItem => assert!(clip.qq_rich_text.unwrap().contains("QQ 中😀")),
                ClipKind::TextClipItem => {
                    assert_eq!(clip.body_text.as_deref(), Some(" \r\n中😀 \n"))
                }
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn accepted_drop_keeps_original_directory_while_stale_events_and_replay_fail() {
        let f = fixture();
        let first = f
            .host
            .favor_folder_create(&json!({"title":"first"}))
            .unwrap();
        let second = f
            .host
            .favor_folder_create(&json!({"title":"second"}))
            .unwrap();
        let ticket = f
            .host
            .accept_native_drop("panel", 102, 5, first["id"].as_str(), -1, || Ok(()))
            .unwrap();
        let valid = Arc::new(AtomicBool::new(true));
        let marker = valid.clone();
        let late = f
            .host
            .accept_native_drop("favorites", 101, 8, None, -1, move || {
                if marker.load(Ordering::Acquire) {
                    Ok(())
                } else {
                    Err(DROP_RETIRED.into())
                }
            })
            .unwrap();
        valid.store(false, Ordering::Release);
        assert!(!f.host.native_drop_request_is_current(&late));
        assert!(f
            .host
            .accept_native_drop("favorites", 101, 8, None, -1, || Err(DROP_RETIRED.into()))
            .is_err());
        f.host.inner.lock().ui.panel_folder_id = second["id"].as_str().map(str::to_string);
        let raw = ClipboardSnapshot {
            text: Some("HTML 🦀".into()),
            html: Some("<b>HTML 🦀</b>".into()),
            ..Default::default()
        };
        let result = f.host.save_native_drop(raw.clone(), &ticket).unwrap();
        let detail = f
            .host
            .favor_get(&json!({"id":result["createdIds"][0]}))
            .unwrap();
        assert_eq!(detail["parentId"], first["id"]);
        assert_eq!(detail["htmlData"], "<b>HTML 🦀</b>");
        assert!(f.host.save_native_drop(raw, &ticket).is_err());
    }

    #[test]
    fn captured_deleted_parent_fails_without_importing_a_detached_record() {
        let f = fixture();
        f.host
            .favor_folder_create(&json!({"title":"protected home"}))
            .unwrap();
        let folder = f
            .host
            .favor_folder_create(&json!({"title":"gone"}))
            .unwrap();
        let request = f
            .host
            .accept_native_drop("panel", 102, 1, folder["id"].as_str(), -1, || Ok(()))
            .unwrap();
        f.host.favor_delete(&json!({"id":folder["id"]})).unwrap();
        let before = f.host.store().lock().list_favors().unwrap().len();
        assert!(f
            .host
            .save_native_drop(
                ClipboardSnapshot {
                    text: Some("drop".into()),
                    ..Default::default()
                },
                &request
            )
            .is_err());
        assert_eq!(f.host.store().lock().list_favors().unwrap().len(), before);
    }

    #[test]
    fn favorite_drag_uses_original_html_and_the_shared_plain_text_setting() {
        let f = fixture();
        let request = f
            .host
            .accept_native_drop("favorites", 101, 1, None, -1, || Ok(()))
            .unwrap();
        let result = f
            .host
            .save_native_drop(
                ClipboardSnapshot {
                    text: Some("original 🦀".into()),
                    html: Some("<b>original 🦀</b>".into()),
                    ..Default::default()
                },
                &request,
            )
            .unwrap();
        let params = json!({"ids":result["createdIds"]});
        let payload = f.host.prepare_favorite_drag(&params).unwrap();
        assert!(payload.html.unwrap().contains("<b>original 🦀</b>"));
        f.host.inner.lock().config.plain_text_paste_by_default = true;
        let payload = f.host.prepare_favorite_drag(&params).unwrap();
        assert_eq!(payload.text, "original 🦀");
        assert!(payload.html.is_none());
    }

    #[test]
    fn dragged_png_is_kept_in_favorite_library_and_outgoing_payload_keeps_original_image() {
        let f = fixture();
        let original = png_fixture();
        let request = f
            .host
            .accept_native_drop("favorites", 101, 2, None, -1, || Ok(()))
            .unwrap();
        let result = f
            .host
            .save_native_drop(
                ClipboardSnapshot {
                    png: Some(original.clone()),
                    has_image_format: true,
                    text: Some("auxiliary text".into()),
                    ..Default::default()
                },
                &request,
            )
            .unwrap();
        let id = result["createdIds"][0].as_str().unwrap();
        let store = f.host.store();
        let item = store.lock().get_favor(id).unwrap();
        assert_eq!(item.kind, ClipKind::ImageClipItem);
        let path = Path::new(item.image_path.as_deref().unwrap());
        assert_eq!(
            path.parent().unwrap(),
            f.host.inner.lock().paths.image_library
        );
        assert_eq!(std::fs::read(path).unwrap(), original);
        let payload = f.host.prepare_favorite_drag(&json!({"ids":[id]})).unwrap();
        assert_eq!(payload.files, [path.to_string_lossy().to_string()]);
        assert!(payload.dib.is_some());
    }

    #[test]
    fn favorite_drag_sta_runner_receives_raw_payload_and_releases_active_guard_on_error() {
        let f = fixture();
        let request = f
            .host
            .accept_native_drop("favorites", 101, 1, None, -1, || Ok(()))
            .unwrap();
        let result = f
            .host
            .save_native_drop(
                ClipboardSnapshot {
                    text: Some("original".into()),
                    html: Some("<b>original</b>".into()),
                    ..Default::default()
                },
                &request,
            )
            .unwrap();
        let params = json!({"ids":result["createdIds"]});
        let observed = f.host.clone();
        f.host.set_ole_drag_ui_runner(move |payload| {
            assert!(observed.inner.lock().ole_drag_active);
            assert!(payload.html.unwrap().contains("<b>original</b>"));
            Err("fixture drag canceled".into())
        });
        assert!(f
            .host
            .begin_favor_ole_drag(&params)
            .unwrap_err()
            .contains("fixture drag canceled"));
        assert!(!f.host.inner.lock().ole_drag_active);
        // Release the test callback's captured HostState ownership.
        f.host.set_ole_drag_ui_runner(|_| Ok(()));
        assert_eq!(
            f.host.begin_favor_ole_drag(&params).unwrap()["started"],
            true
        );
    }
}
