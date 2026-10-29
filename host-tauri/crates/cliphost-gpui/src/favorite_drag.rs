//! One payload for in-process history/favorites drags. The external OLE
//! representation is deliberately separate: it must never perform a move.

use gpui_rsx::rsx;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicU8, Ordering},
        Arc, LazyLock, Mutex,
    },
};

use cliphost_core::{i18n::tr, HostState};
use gpui::{
    canvas, div, prelude::*, px, AnyElement, AnyWindowHandle, App, Context, DispatchPhase,
    DragMoveEvent, Global, MouseButton, MouseMoveEvent, MouseUpEvent, PlatformInput, Render,
    Window,
};
use serde_json::{json, Value};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DragSource {
    Favorite {
        parent_id: Option<String>,
        revision: String,
    },
    History {
        revision: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Placement {
    Before,
    After,
    Into,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DropTarget {
    pub parent_id: Option<String>,
    pub anchor_id: Option<String>,
    pub placement: Placement,
}

struct Session {
    // 0: active, 1: accepted exactly once, 2: cancelled.
    state: AtomicU8,
    native: AtomicU8,
    native_accepted: AtomicBool,
    /// The popup preview owns the label; the in-window drag view stays empty.
    preview: AtomicBool,
    revision: Mutex<Option<String>>,
    leases: Mutex<Vec<(Arc<HostState>, u64)>>,
}

impl Session {
    fn release(&self) {
        for (host, lease) in self.leases.lock().unwrap().drain(..) {
            host.release_native_incoming_drag(lease);
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.release();
    }
}

#[derive(Clone)]
pub(crate) struct ContentDrag {
    pub source: DragSource,
    pub item_ids: Vec<String>,
    pub source_paths: Vec<Vec<String>>,
    pub generation: u64,
    pub session: u64,
    pub title: String,
    pub operation_id: String,
    state: Arc<Session>,
}

impl ContentDrag {
    pub fn favorite(
        parent_id: Option<String>,
        item_ids: Vec<String>,
        source_paths: Vec<Vec<String>>,
        revision: String,
        generation: u64,
        session: u64,
        title: String,
    ) -> Self {
        Self::new(
            DragSource::Favorite {
                parent_id,
                revision,
            },
            item_ids,
            source_paths,
            generation,
            session,
            title,
        )
    }

    pub fn history(
        item_ids: Vec<String>,
        revision: String,
        generation: u64,
        session: u64,
        title: String,
    ) -> Self {
        Self::new(
            DragSource::History { revision },
            item_ids,
            Vec::new(),
            generation,
            session,
            title,
        )
    }

    fn new(
        source: DragSource,
        item_ids: Vec<String>,
        source_paths: Vec<Vec<String>>,
        generation: u64,
        session: u64,
        title: String,
    ) -> Self {
        Self {
            source,
            item_ids,
            source_paths,
            generation,
            session,
            title,
            operation_id: format!("drag-{}", uuid::Uuid::new_v4()),
            state: Arc::new(Session {
                state: AtomicU8::new(0),
                native: AtomicU8::new(0),
                native_accepted: AtomicBool::new(false),
                preview: AtomicBool::new(false),
                revision: Mutex::new(None),
                leases: Mutex::new(Vec::new()),
            }),
        }
    }

    pub fn active(&self) -> bool {
        self.state.state.load(Ordering::Acquire) == 0
    }
    pub fn is_claimed(&self) -> bool {
        self.state.state.load(Ordering::Acquire) == 1
    }
    pub fn claim(&self) -> bool {
        let accepted = self
            .state
            .state
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
        if accepted {
            self.state.release();
        }
        accepted
    }
    pub fn cancel(&self) {
        if self
            .state
            .state
            .compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.state.release();
        }
    }
    pub fn request(&self, target: &DropTarget, copy: bool) -> Value {
        let (kind, parent, revision) = match &self.source {
            DragSource::Favorite {
                parent_id,
                revision,
            } => ("favorite", parent_id.clone(), revision),
            DragSource::History { revision } => ("history", None, revision),
        };
        let prepared_revision = self.state.revision.lock().unwrap();
        let revision = prepared_revision.as_deref().unwrap_or(revision);
        json!({"sourceKind":kind, "sourceParentId":parent, "itemIds":self.item_ids,
            "targetParentId":target.parent_id, "anchorId":target.anchor_id,
            "placement":match target.placement { Placement::Before=>"before",Placement::After=>"after",Placement::Into=>"into" },
            "copy":copy || kind == "history", "expectedRevision":revision,"operationId":self.operation_id})
    }
    pub fn set_revision(&self, revision: String) {
        *self.state.revision.lock().unwrap() = Some(revision);
    }
    pub fn mark_native_accepted(&self) {
        self.state.native_accepted.store(true, Ordering::Release);
    }
    /// Until accepted, cancelled or the OLE loop for this gesture returns.
    pub(crate) fn preview_live(&self) -> bool {
        self.active() && self.state.native.load(Ordering::Acquire) != 2
    }
    /// This gesture handed over to an OLE loop for other programs.
    pub(crate) fn went_native(&self) -> bool {
        self.state.native.load(Ordering::Acquire) != 0
    }
    /// Outside our windows a drop only ever copies to the other program.
    pub(crate) fn verb(&self, control: bool, external: bool) -> &'static str {
        match self.source {
            _ if external => tr("复制", "Copy"),
            DragSource::History { .. } => tr("收藏", "Save favorite"),
            _ if control => tr("复制", "Copy"),
            _ => tr("移动", "Move"),
        }
    }
    pub(crate) fn preview_label(&self, control: bool, external: bool) -> String {
        let verb = self.verb(control, external);
        format!(
            "{verb} · {}{}",
            self.title,
            if self.item_ids.len() > 1 {
                format!(" (+{})", self.item_ids.len() - 1)
            } else {
                String::new()
            }
        )
    }
}

/// In-window fallback, used only when the popup preview cannot open.
impl Render for ContentDrag {
    fn render(&mut self, window: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        if self.state.preview.load(Ordering::Acquire) {
            return div().into_any_element();
        }
        crate::drag_preview::label(self.preview_label(window.modifiers().control, false), None, window)
            .into_any_element()
    }
}

/// Shared hit testing: only folders accept a centre drop; filtered projections
/// cannot supply before/after anchors.
pub(crate) fn hit_placement(
    folder: bool,
    offset: f32,
    length: f32,
    can_order: bool,
) -> Option<Placement> {
    if !offset.is_finite() || !length.is_finite() || length <= 0. || offset < 0. || offset > length
    {
        return None;
    }
    if !can_order {
        return folder.then_some(Placement::Into);
    }
    if folder {
        let edge = 10.0_f32.min(length / 3.);
        if offset < edge {
            Some(Placement::Before)
        } else if offset > length - edge {
            Some(Placement::After)
        } else {
            Some(Placement::Into)
        }
    } else if offset < length / 2. {
        Some(Placement::Before)
    } else {
        Some(Placement::After)
    }
}

/// The visible first child of an expanded folder: the next projected row,
/// exactly one level deeper beneath the same occurrence.
pub(crate) fn first_child_below(path: &[String], next: Option<&[String]>) -> Option<String> {
    next.filter(|next| next.len() == path.len() + 1 && next.starts_with(path))
        .and_then(|next| next.last().cloned())
}

/// An expanded folder's lower edge touches its first visible child, so it
/// inserts before that child at child level instead of after the subtree
/// (DESIGN 2026-10-15 drop indicators).
pub(crate) fn retarget_expanded_edge(
    target: DropTarget,
    folder_id: &str,
    first_child: Option<&str>,
) -> DropTarget {
    match first_child {
        Some(child)
            if target.placement == Placement::After
                && target.anchor_id.as_deref() == Some(folder_id) =>
        {
            DropTarget {
                parent_id: Some(folder_id.into()),
                anchor_id: Some(child.into()),
                placement: Placement::Before,
            }
        }
        _ => target,
    }
}

/// The occurrence that draws the indicator for a target hit on `hit`: the
/// folder itself for Into, otherwise the anchor row, which may be the first
/// child of the hit folder after `retarget_expanded_edge`.
pub(crate) fn indicator_path(hit: &[String], target: &DropTarget) -> Vec<String> {
    match (&target.anchor_id, target.placement) {
        (Some(anchor), Placement::Before | Placement::After)
            if hit.last().is_some() && target.parent_id.as_ref() == hit.last() =>
        {
            let mut path = hit.to_vec();
            path.push(anchor.clone());
            path
        }
        _ => hit.to_vec(),
    }
}

/// The occurrence of the folder that receives the drop.
pub(crate) fn destination_path(hit: &[String], target: &DropTarget) -> Vec<String> {
    let mut path = indicator_path(hit, target);
    if target.placement != Placement::Into {
        path.pop();
    }
    path
}

/// Insertion line: a dot at the target level's indent plus a 2 px rule, kept
/// inside the row so neighbouring rows never paint over it.
pub(crate) fn insertion_marker(
    placement: Placement,
    left: f32,
    right: f32,
    accent: gpui::Hsla,
) -> AnyElement {
    rsx! {
        <div absolute left={px(left.max(0.))} right={px(right.max(0.))} h={px(6.)} />
    }
        .when(placement == Placement::Before, |marker| marker.top_0())
        .when(placement != Placement::Before, |marker| marker.bottom_0())
        .flex()
        .items_center()
        .child(rsx! {
            <div flex_none size={px(6.)} rounded_full bg={accent} />
        })
        .child(rsx! {
            <div flex_1 h={px(2.)} rounded={px(1.)} bg={accent} />
        })
        .into_any_element()
}

/// Into: an outlined, tinted row. It never uses a line, so it cannot be read
/// as a sibling insertion.
pub(crate) fn into_highlight(radius: f32, accent: gpui::Hsla) -> AnyElement {
    rsx! {
        <div absolute top_0 bottom_0 left_0 right_0 rounded={px(radius)} border_2
            border_color={accent} bg={accent.opacity(0.12)} />
    }
        .into_any_element()
}

#[derive(Clone)]
struct Surface {
    handle: AnyWindowHandle,
    hwnd: isize,
    host: Arc<HostState>,
    label: &'static str,
}

#[derive(Default)]
struct Routing {
    surfaces: Vec<Surface>,
    current: Option<(ContentDrag, AnyWindowHandle)>,
    forwarding: bool,
}
impl Global for Routing {}

#[derive(Clone)]
enum OfferedDestination {
    Favorite(DropTarget),
    Lan(String),
}

#[derive(Clone)]
struct OfferedTarget {
    drag: ContentDrag,
    destination: OfferedDestination,
    host: Arc<HostState>,
    bounds: (i32, i32, i32, i32),
}

/// Painted conversation geometry is available before an OLE gesture produces
/// its first synthetic GPUI drag-move. COM reads only this immutable snapshot.
#[derive(Clone)]
struct LanSurface {
    host: Arc<HostState>,
    conversation: String,
    bounds: (i32, i32, i32, i32),
}

pub(crate) struct NativeAcceptance {
    pub host: Arc<HostState>,
    pub request: Value,
    pub drag: ContentDrag,
    pub lan_conversation: Option<String>,
}

static NATIVE: Mutex<Option<ContentDrag>> = Mutex::new(None);
static TARGETS: LazyLock<Mutex<HashMap<isize, OfferedTarget>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static LAN_SURFACES: LazyLock<Mutex<HashMap<isize, LanSurface>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) fn clear_target(window: &Window) {
    #[cfg(windows)]
    if let Ok(hwnd) = crate::platform::hwnd(window) {
        clear_target_hwnd(hwnd.0 as isize);
    }
}
pub(crate) fn clear_target_hwnd(hwnd: isize) {
    TARGETS.lock().unwrap().remove(&hwnd);
    LAN_SURFACES.lock().unwrap().remove(&hwnd);
}

pub(crate) fn register_lan_region(
    conversation: &str,
    bounds: gpui::Bounds<gpui::Pixels>,
    window: &Window,
    host: Arc<HostState>,
) {
    if conversation.is_empty() {
        clear_target(window);
        return;
    }
    #[cfg(windows)]
    if let Some((hwnd, bounds)) = screen_region(bounds, window) {
        LAN_SURFACES.lock().unwrap().insert(hwnd, LanSurface {
            host, conversation: conversation.into(), bounds,
        });
    } else {
        clear_target(window);
    }
    #[cfg(not(windows))]
    let _ = (bounds, window, host);
}

#[cfg(windows)]
fn screen_region(bounds: gpui::Bounds<gpui::Pixels>, window: &Window) -> Option<(isize, (i32, i32, i32, i32))> {
    use crate::windows_api::Win32::{Foundation::POINT, Graphics::Gdi::ClientToScreen};
    let hwnd = crate::platform::hwnd(window).ok()?;
    let mut origin = POINT::default();
    if !unsafe { ClientToScreen(crate::windows_api::Win32::Foundation::HWND(hwnd.0), &mut origin) }.as_bool() {
        return None;
    }
    let scale = window.scale_factor();
    let edges = [bounds.left(), bounds.top(), bounds.right(), bounds.bottom()].map(|edge| f32::from(edge) * scale);
    if !edges.iter().all(|edge| edge.is_finite()) { return None; }
    let screen = (
        origin.x.saturating_add(edges[0] as i32), origin.y.saturating_add(edges[1] as i32),
        origin.x.saturating_add(edges[2] as i32), origin.y.saturating_add(edges[3] as i32),
    );
    (screen.2 > screen.0 && screen.3 > screen.1).then_some((hwnd.0 as isize, screen))
}

pub(crate) fn offer_target(
    drag: &ContentDrag,
    target: &DropTarget,
    bounds: gpui::Bounds<gpui::Pixels>,
    window: &Window,
    host: Arc<HostState>,
) {
    offer_destination(drag, OfferedDestination::Favorite(target.clone()), bounds, window, host);
}

/// LAN accepts only our history gesture and freezes the visible conversation.
pub(crate) fn offer_lan_target(
    drag: &ContentDrag,
    conversation: &str,
    bounds: gpui::Bounds<gpui::Pixels>,
    window: &Window,
    host: Arc<HostState>,
) {
    if !matches!(drag.source, DragSource::History { .. }) || conversation.is_empty() {
        clear_target(window);
        return;
    }
    offer_destination(drag, OfferedDestination::Lan(conversation.into()), bounds, window, host);
}

fn offer_destination(
    drag: &ContentDrag,
    destination: OfferedDestination,
    bounds: gpui::Bounds<gpui::Pixels>,
    window: &Window,
    host: Arc<HostState>,
) {
    #[cfg(windows)]
    if let Some((hwnd, bounds)) = screen_region(bounds, window) {
        TARGETS.lock().unwrap().insert(hwnd, OfferedTarget {
            drag: drag.clone(), destination, host, bounds,
        });
    }
}

fn painted_lan_target(token: &str, hwnd: isize) -> Option<OfferedTarget> {
    let drag = NATIVE.lock().unwrap().as_ref().filter(|drag| {
        drag.operation_id == token && drag.active()
            && drag.state.native.load(Ordering::Acquire) == 1
            && matches!(drag.source, DragSource::History { .. })
    }).cloned()?;
    let surface = LAN_SURFACES.lock().unwrap().get(&hwnd).cloned()?;
    Some(OfferedTarget {
        drag, destination: OfferedDestination::Lan(surface.conversation),
        host: surface.host, bounds: surface.bounds,
    })
}

pub(crate) fn begin_native(drag: ContentDrag) -> Result<(), String> {
    let mut native = NATIVE.lock().unwrap();
    if native
        .as_ref()
        .is_some_and(|drag| drag.state.native.load(Ordering::Acquire) == 1)
    {
        return Err(tr("已有拖动正在进行", "A drag is already in progress").into());
    }
    drag.state.native.store(1, Ordering::Release);
    *native = Some(drag);
    Ok(())
}

pub(crate) fn finish_native(token: &str) {
    let native = NATIVE.lock().unwrap();
    if let Some(drag) = native.as_ref().filter(|drag| drag.operation_id == token) {
        drag.state.native.store(2, Ordering::Release);
    }
}

pub(crate) fn verify_native(token: &str) -> Result<(), String> {
    if NATIVE
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|drag| drag.operation_id == token && drag.active())
    {
        Ok(())
    } else {
        Err("The internal drag gesture has ended.".into())
    }
}

pub(crate) fn native_guard(token: &str) -> Result<(), String> {
    if NATIVE.lock().unwrap().as_ref().is_some_and(|drag| {
        drag.operation_id == token
            && (drag.active()
                || drag.is_claimed() && drag.state.native_accepted.load(Ordering::Acquire))
    }) {
        Ok(())
    } else {
        Err("The internal drag gesture has ended.".into())
    }
}

/// COM callbacks queue immutable acceptance only; they never borrow GPUI App.
pub(crate) fn accept_native(
    token: &str,
    hwnd: isize,
    point: (i32, i32),
    control: bool,
) -> Result<NativeAcceptance, String> {
    verify_native(token)?;
    let offered = TARGETS
        .lock()
        .unwrap()
        .get(&hwnd)
        .cloned()
        .filter(|target| target.drag.operation_id == token && target.drag.active());
    let offered = offered.or_else(|| painted_lan_target(token, hwnd))
        .ok_or_else(|| {
            tr("没有有效的投放位置", "No valid drop location").to_string()
        })?;
    let (left, top, right, bottom) = offered.bounds;
    if point.0 < left || point.0 >= right || point.1 < top || point.1 >= bottom {
        return Err(tr(
            "投放位置已改变，请重新拖动",
            "The drop location changed; drag again",
        )
        .into());
    }
    let (request, lan_conversation) = match &offered.destination {
        OfferedDestination::Favorite(target) => (offered.drag.request(target, control), None),
        OfferedDestination::Lan(conversation) => {
            if !matches!(offered.drag.source, DragSource::History { .. }) {
                return Err("LAN accepts history items only.".into());
            }
            (json!({"ids":offered.drag.item_ids,"conversationId":conversation}), Some(conversation.clone()))
        }
    };
    Ok(NativeAcceptance {
        host: offered.host,
        request,
        drag: offered.drag,
        lan_conversation,
    })
}

/// Register only our content windows. It is safe to dispatch GPUI events into
/// a different window; no native input or synchronous HWND message is injected.
pub(crate) fn register_surface(
    window: &mut Window,
    host: Arc<HostState>,
    label: &'static str,
    cx: &mut App,
) {
    #[cfg(windows)]
    let hwnd = crate::platform::hwnd(window)
        .map(|hwnd| hwnd.0 as isize)
        .unwrap_or(0);
    #[cfg(not(windows))]
    let hwnd = 0;
    if !cx.has_global::<Routing>() {
        cx.set_global(Routing::default());
    }
    let handle = window.window_handle();
    let routing = cx.global_mut::<Routing>();
    routing
        .surfaces
        .retain(|surface| surface.handle != handle && surface.hwnd != hwnd);
    routing.surfaces.push(Surface {
        handle,
        hwnd,
        host,
        label,
    });
}

pub(crate) fn activate_drag(drag: &ContentDrag, window: &mut Window, cx: &mut App) {
    if !cx.has_global::<Routing>() {
        cx.set_global(Routing::default());
    }
    let surfaces = cx.global::<Routing>().surfaces.clone();
    if let Some((previous, _)) = cx.global_mut::<Routing>().current.take() {
        previous.cancel();
    }
    for surface in surfaces
        .iter()
        .filter(|surface| surface.handle == window.window_handle())
    {
        if let Ok(lease) = surface
            .host
            .acquire_native_incoming_drag(surface.label, surface.hwnd)
        {
            drag.state
                .leases
                .lock()
                .unwrap()
                .push((surface.host.clone(), lease));
        }
    }
    cx.global_mut::<Routing>().current = Some((drag.clone(), window.window_handle()));
    // Set by GPUI only while an `on_drag` constructor runs: the dragged row.
    let source = cx.drag_source_bounds();
    if crate::drag_preview::begin(drag, source, window, cx) {
        drag.state.preview.store(true, Ordering::Release);
    }
}

pub(crate) fn cancel_active(window: &mut Window, cx: &mut App) -> bool {
    if !cx.has_global::<Routing>() {
        return false;
    }
    let Some((drag, _)) = cx.global_mut::<Routing>().current.take() else {
        return false;
    };
    if !drag.active() {
        return false;
    }
    drag.cancel();
    TARGETS
        .lock()
        .unwrap()
        .retain(|_, target| target.drag.operation_id != drag.operation_id);
    cx.stop_active_drag(window);
    true
}

/// Release the hold even when a mouse-up had no valid target.
pub(crate) fn maintain(cx: &mut App) {
    let native = NATIVE.lock().unwrap().clone();
    if let Some(drag) = native {
        if drag.state.native.load(Ordering::Acquire) == 2 {
            let source = cx
                .try_global::<Routing>()
                .and_then(|routing| routing.current.as_ref().map(|(_, handle)| *handle));
            if let Some(source) = source {
                let _ = source.update(cx, |_, window, cx| {
                    cx.stop_active_drag(window);
                });
            }
            drag.cancel();
            TARGETS
                .lock()
                .unwrap()
                .retain(|_, target| target.drag.operation_id != drag.operation_id);
            *NATIVE.lock().unwrap() = None;
        } else if drag.active() {
            // A native OLE loop owns mouse capture. Refresh only our hovered
            // content window so its existing hit testing/indicators still run.
            if let Some((surface, point)) = hovered_surface(cx) {
                let _=surface.handle.update(cx,|_,target,cx| {
                    let point=point/target.scale_factor();
                    let mut modifiers=target.modifiers();
                    #[cfg(windows)]
                    {modifiers.control=unsafe {crate::windows_api::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(0x11)<0};}
                    target.dispatch_input(PlatformInput::MouseMove(MouseMoveEvent {position:point,pressed_button:Some(MouseButton::Left),modifiers}),cx);
                });
            }
        }
    }
    if cx.has_global::<Routing>() && !cx.has_active_drag() {
        if let Some((drag, _)) = cx.global_mut::<Routing>().current.take() {
            drag.cancel();
        }
    }
}

#[cfg(windows)]
fn client_point(hwnd: isize, screen: (i32, i32)) -> Option<gpui::Point<gpui::Pixels>> {
    use crate::windows_api::Win32::{
        Foundation::{HWND, POINT},
        Graphics::Gdi::ScreenToClient,
    };
    let mut point = POINT {
        x: screen.0,
        y: screen.1,
    };
    unsafe {
        ScreenToClient(HWND(hwnd as *mut _), &mut point)
            .as_bool()
            .then(|| gpui::point(px(point.x as f32), px(point.y as f32)))
    }
}
#[cfg(not(windows))]
fn client_point(_: isize, screen: (i32, i32)) -> Option<gpui::Point<gpui::Pixels>> {
    Some(gpui::point(px(screen.0 as f32), px(screen.1 as f32)))
}

#[cfg(windows)]
fn hovered_surface(cx: &App) -> Option<(Surface, gpui::Point<gpui::Pixels>)> {
    use crate::windows_api::Win32::{
        Foundation::POINT,
        UI::WindowsAndMessaging::{GetAncestor, GetCursorPos, WindowFromPoint, GA_ROOT},
    };
    unsafe {
        let mut point = POINT::default();
        GetCursorPos(&mut point).ok()?;
        let root = GetAncestor(WindowFromPoint(point), GA_ROOT);
        let surface = cx
            .try_global::<Routing>()?
            .surfaces
            .iter()
            .find(|surface| surface.hwnd == root.0 as isize)?
            .clone();
        let client = client_point(surface.hwnd, (point.x, point.y))?;
        Some((surface, client))
    }
}
#[cfg(not(windows))]
fn hovered_surface(_: &App) -> Option<(Surface, gpui::Point<gpui::Pixels>)> {
    None
}

/// The pointer is over one of our registered content windows.
pub(crate) fn pointer_over_surface(cx: &App) -> bool {
    hovered_surface(cx).is_some()
}

/// A content window currently offers this drag a valid drop under the pointer.
pub(crate) fn offered_here(drag: &ContentDrag, cx: &App) -> bool {
    let (Some((surface, _)), Some((x, y))) = (hovered_surface(cx), crate::platform::cursor_position()) else {
        return false;
    };
    TARGETS.lock().unwrap().get(&surface.hwnd).is_some_and(|target| {
        let (left, top, right, bottom) = target.bounds;
        target.drag.operation_id == drag.operation_id && target.drag.active()
            && x >= left && x < right && y >= top && y < bottom
    })
}

pub(crate) fn active(cx: &App) -> bool {
    cx.try_global::<Routing>()
        .and_then(|routing| routing.current.as_ref())
        .is_some_and(|(drag, _)| drag.active())
}

#[cfg(windows)]
fn other_surface(
    window: &Window,
    cx: &App,
    position: gpui::Point<gpui::Pixels>,
) -> Option<(Surface, gpui::Point<gpui::Pixels>)> {
    use crate::windows_api::Win32::Graphics::Gdi::{ClientToScreen, ScreenToClient};
    use crate::windows_api::Win32::{
        Foundation::{HWND, POINT},
        UI::WindowsAndMessaging::{GetAncestor, WindowFromPoint, GA_ROOT},
    };
    let routing = cx.try_global::<Routing>()?;
    if routing.forwarding || routing.current.as_ref()?.1 != window.window_handle() {
        return None;
    }
    unsafe {
        let source = crate::platform::hwnd(window).ok()?;
        let mut point = POINT {
            x: (f32::from(position.x) * window.scale_factor()) as i32,
            y: (f32::from(position.y) * window.scale_factor()) as i32,
        };
        if !ClientToScreen(HWND(source.0), &mut point).as_bool() {
            return None;
        }
        let root = GetAncestor(WindowFromPoint(point), GA_ROOT);
        let surface = routing
            .surfaces
            .iter()
            .find(|surface| {
                surface.hwnd == root.0 as isize && surface.handle != window.window_handle()
            })?
            .clone();
        if !ScreenToClient(HWND(surface.hwnd as *mut _), &mut point).as_bool() {
            return None;
        }
        Some((surface, gpui::point(px(point.x as f32), px(point.y as f32))))
    }
}

#[cfg(not(windows))]
fn other_surface(
    _: &Window,
    _: &App,
    _: gpui::Point<gpui::Pixels>,
) -> Option<(Surface, gpui::Point<gpui::Pixels>)> {
    None
}

/// Returns true after forwarding into another registered content window.
pub(crate) fn route_drag_move(
    event: &DragMoveEvent<ContentDrag>,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    let Some((surface, mut point)) = other_surface(window, cx, event.event.position) else {
        return false;
    };
    let modifiers = event.event.modifiers;
    cx.global_mut::<Routing>().forwarding = true;
    let _ = surface.handle.update(cx, |_, target, cx| {
        point.x /= target.scale_factor();
        point.y /= target.scale_factor();
        target.dispatch_input(
            PlatformInput::MouseMove(MouseMoveEvent {
                position: point,
                pressed_button: Some(MouseButton::Left),
                modifiers,
            }),
            cx,
        );
    });
    cx.global_mut::<Routing>().forwarding = false;
    cx.stop_propagation();
    true
}

/// Capture the source mouse-up before GPUI clears its global drag. Windows
/// keeps mouse capture on the source HWND throughout the gesture.
pub(crate) fn surface_events() -> AnyElement {
    canvas(
        |_, _, _| (),
        |_, _, window, _| {
            window.on_mouse_event(|event: &MouseUpEvent, phase, window, cx| {
                if phase != DispatchPhase::Capture || event.button != MouseButton::Left {
                    return;
                }
                let Some((surface, mut point)) = other_surface(window, cx, event.position) else {
                    return;
                };
                cx.global_mut::<Routing>().forwarding = true;
                let modifiers = event.modifiers;
                let _ = surface.handle.update(cx, |_, target, cx| {
                    point.x /= target.scale_factor();
                    point.y /= target.scale_factor();
                    // Hitboxes must see the final position even if mouse-up arrived
                    // before the next paint of the receiving window.
                    target.dispatch_input(
                        PlatformInput::MouseMove(MouseMoveEvent {
                            position: point,
                            pressed_button: Some(MouseButton::Left),
                            modifiers,
                        }),
                        cx,
                    );
                    target.dispatch_input(
                        PlatformInput::MouseUp(MouseUpEvent {
                            position: point,
                            button: MouseButton::Left,
                            modifiers,
                            click_count: 1,
                        }),
                        cx,
                    );
                });
                cx.global_mut::<Routing>().forwarding = false;
                cx.stop_propagation();
            });
        },
    )
    .absolute()
    .size_full()
    .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    static NATIVE_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn one_gesture_can_commit_once_even_across_cloned_payloads() {
        let drag = ContentDrag::favorite(
            Some("a".into()),
            vec!["b".into()],
            vec![],
            "r".into(),
            1,
            2,
            "B".into(),
        );
        let other = drag.clone();
        assert!(drag.claim());
        assert!(!other.claim());
        other.cancel();
        assert!(drag.is_claimed());
    }

    #[test]
    fn cancelled_payload_never_commits_and_history_is_always_a_copy() {
        let drag = ContentDrag::history(vec!["h".into()], "r".into(), 1, 2, "H".into());
        let request = drag.request(
            &DropTarget {
                parent_id: Some("f".into()),
                anchor_id: None,
                placement: Placement::Into,
            },
            false,
        );
        assert_eq!(request["sourceKind"], "history");
        assert_eq!(request["copy"], true);
        drag.cancel();
        assert!(!drag.claim());
    }

    #[test]
    fn filtered_rows_only_accept_folder_centres_and_edges_do_not_overlap() {
        assert_eq!(hit_placement(false, 10., 30., false), None);
        assert_eq!(hit_placement(true, 1., 30., false), Some(Placement::Into));
        assert_eq!(hit_placement(true, 2., 24., true), Some(Placement::Before));
        assert_eq!(hit_placement(true, 12., 24., true), Some(Placement::Into));
        assert_eq!(hit_placement(true, 23., 24., true), Some(Placement::After));
        assert_eq!(hit_placement(false, f32::NAN, 24., true), None);
    }

    fn ids(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| id.to_string()).collect()
    }

    #[test]
    fn favorites_expanded_folder_lower_edge_inserts_before_first_child() {
        let folder = ids(&["root", "work"]);
        assert_eq!(
            first_child_below(&folder, Some(&ids(&["root", "work", "report"]))).as_deref(),
            Some("report")
        );
        // A following sibling or a deeper auto-expanded row is not a first child.
        assert_eq!(first_child_below(&folder, Some(&ids(&["root", "life"]))), None);
        assert_eq!(
            first_child_below(&folder, Some(&ids(&["root", "work", "a", "b"]))),
            None
        );
        assert_eq!(first_child_below(&folder, None), None);

        let after = DropTarget {
            parent_id: Some("root".into()),
            anchor_id: Some("work".into()),
            placement: Placement::After,
        };
        let child = retarget_expanded_edge(after.clone(), "work", Some("report"));
        assert_eq!(
            child,
            DropTarget {
                parent_id: Some("work".into()),
                anchor_id: Some("report".into()),
                placement: Placement::Before,
            }
        );
        assert_eq!(retarget_expanded_edge(after.clone(), "work", None), after);
        let before = DropTarget {
            placement: Placement::Before,
            ..after.clone()
        };
        assert_eq!(retarget_expanded_edge(before.clone(), "work", Some("report")), before);

        // The line belongs to the child row; the folder only draws Into.
        assert_eq!(indicator_path(&folder, &child), ids(&["root", "work", "report"]));
        assert_eq!(destination_path(&folder, &child), folder);
        assert_eq!(indicator_path(&folder, &after), folder);
        assert_eq!(destination_path(&folder, &after), ids(&["root"]));
        let into = DropTarget {
            parent_id: Some("work".into()),
            anchor_id: None,
            placement: Placement::Into,
        };
        assert_eq!(indicator_path(&folder, &into), folder);
        assert_eq!(destination_path(&folder, &into), folder);
        let top = DropTarget {
            parent_id: None,
            anchor_id: Some("work".into()),
            placement: Placement::Before,
        };
        assert_eq!(indicator_path(&ids(&["work"]), &top), ids(&["work"]));
        assert_eq!(destination_path(&ids(&["work"]), &top), Vec::<String>::new());
    }

    #[test]
    fn native_drop_captures_exact_target_and_claimed_success_cannot_start_another_drag() {
        let _native_scope = NATIVE_TEST_LOCK.lock().unwrap();
        let scratch = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../.tmp/favorite-drag-unit");
        std::fs::create_dir_all(&scratch).unwrap();
        let temporary = tempfile::tempdir_in(scratch).unwrap();
        let host = Arc::new(
            HostState::open(cliphost_core::AppPaths::from_data_root(
                temporary.path().join("data"),
            ), 0)
            .unwrap(),
        );
        let drag = ContentDrag::favorite(
            Some("source".into()),
            vec!["item".into()],
            vec![],
            "revision".into(),
            1,
            2,
            "item".into(),
        );
        begin_native(drag.clone()).unwrap();
        let original = DropTarget {
            parent_id: Some("original-folder".into()),
            anchor_id: Some("anchor".into()),
            placement: Placement::Before,
        };
        TARGETS.lock().unwrap().insert(
            99,
            OfferedTarget {
                drag: drag.clone(),
                destination: OfferedDestination::Favorite(original),
                host: host.clone(),
                bounds: (0, 0, 100, 100),
            },
        );
        assert!(accept_native("different-gesture", 99, (20, 20),false).is_err());
        assert!(accept_native(&drag.operation_id, 99, (120, 20),false).is_err());
        let accepted = accept_native(&drag.operation_id, 99, (20, 20),true).unwrap();
        assert_eq!(accepted.request["copy"],true);
        TARGETS.lock().unwrap().insert(
            99,
            OfferedTarget {
                drag: drag.clone(),
                destination: OfferedDestination::Favorite(DropTarget {
                    parent_id: Some("later-folder".into()),
                    anchor_id: None,
                    placement: Placement::Into,
                }),
                host,
                bounds: (0, 0, 100, 100),
            },
        );
        assert_eq!(accepted.request["targetParentId"], "original-folder");
        assert_eq!(accepted.request["anchorId"], "anchor");
        assert!(accepted.drag.claim());
        accepted.drag.mark_native_accepted();
        assert!(
            verify_native(&drag.operation_id).is_err(),
            "accepted gestures cannot enter a late native start"
        );
        assert!(
            native_guard(&drag.operation_id).is_ok(),
            "the existing OLE call may finish its accepted copy effect"
        );
        TARGETS.lock().unwrap().remove(&99);
        *NATIVE.lock().unwrap() = None;
    }

    #[test]
    fn native_lan_drop_freezes_conversation_and_rejects_duplicate_acceptance() {
        let _native_scope = NATIVE_TEST_LOCK.lock().unwrap();
        let scratch = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp/lan-drag-unit");
        std::fs::create_dir_all(&scratch).unwrap();
        let temporary = tempfile::tempdir_in(scratch).unwrap();
        let host = Arc::new(HostState::open(cliphost_core::AppPaths::from_data_root(temporary.path().join("data")), 0).unwrap());
        let drag = ContentDrag::history(vec!["history-a".into()], "rev".into(), 1, 2, "item".into());
        begin_native(drag.clone()).unwrap();
        TARGETS.lock().unwrap().insert(98, OfferedTarget {
            drag: drag.clone(), destination: OfferedDestination::Lan("dm:peer-a".into()), host: host.clone(), bounds: (0,0,100,100),
        });
        assert!(accept_native(&drag.operation_id,98,(100,50),false).is_err());
        let accepted = accept_native(&drag.operation_id,98,(50,50),false).unwrap();
        TARGETS.lock().unwrap().insert(98, OfferedTarget {
            drag: drag.clone(), destination: OfferedDestination::Lan("all".into()), host, bounds: (0,0,100,100),
        });
        assert_eq!(accepted.lan_conversation.as_deref(), Some("dm:peer-a"));
        assert_eq!(accepted.request["ids"], json!(["history-a"]));
        assert!(accepted.drag.claim());
        assert!(accept_native(&drag.operation_id,98,(50,50),false).is_err());
        TARGETS.lock().unwrap().remove(&98);
        *NATIVE.lock().unwrap() = None;
    }

    #[test]
    fn native_lan_fast_drop_uses_painted_region_without_a_drag_move_offer() {
        let _native_scope = NATIVE_TEST_LOCK.lock().unwrap();
        let scratch = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp/lan-drag-unit");
        std::fs::create_dir_all(&scratch).unwrap();
        let temporary = tempfile::tempdir_in(scratch).unwrap();
        let host = Arc::new(HostState::open(cliphost_core::AppPaths::from_data_root(temporary.path().join("data")), 0).unwrap());
        let hwnd = 97;
        clear_target_hwnd(hwnd);
        LAN_SURFACES.lock().unwrap().insert(hwnd, LanSurface {
            host: host.clone(), conversation: "dm:peer-a".into(), bounds: (10, 20, 110, 120),
        });
        let favorite = ContentDrag::favorite(None, vec!["favorite".into()], vec![], "rev".into(), 1, 2, "item".into());
        begin_native(favorite.clone()).unwrap();
        assert!(accept_native(&favorite.operation_id, hwnd, (50, 50), false).is_err());
        finish_native(&favorite.operation_id);

        let drag = ContentDrag::history(vec!["history-a".into()], "rev".into(), 1, 2, "item".into());
        begin_native(drag.clone()).unwrap();
        assert!(TARGETS.lock().unwrap().get(&hwnd).is_none(), "no GPUI drag-move offer was made");
        assert!(accept_native("external-token", hwnd, (50, 50), false).is_err());
        assert!(accept_native(&drag.operation_id, hwnd, (110, 50), false).is_err());
        let accepted = accept_native(&drag.operation_id, hwnd, (50, 50), false).unwrap();
        LAN_SURFACES.lock().unwrap().get_mut(&hwnd).unwrap().conversation = "dm:peer-b".into();
        assert_eq!(accepted.lan_conversation.as_deref(), Some("dm:peer-a"));
        assert_eq!(accepted.request["ids"], json!(["history-a"]));
        clear_target_hwnd(hwnd);
        assert!(accept_native(&drag.operation_id, hwnd, (50, 50), false).is_err(), "a hidden/modal/retired region cannot receive a late drop");
        assert!(accepted.drag.claim());
        LAN_SURFACES.lock().unwrap().insert(hwnd, LanSurface {
            host, conversation: "dm:peer-b".into(), bounds: (10, 20, 110, 120),
        });
        assert!(accept_native(&drag.operation_id, hwnd, (50, 50), false).is_err(), "claim blocks a second send through the painted fallback too");
        clear_target_hwnd(hwnd);
        *NATIVE.lock().unwrap() = None;
    }
}
