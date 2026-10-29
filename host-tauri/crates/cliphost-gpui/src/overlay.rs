//! Quick-paste presentation only. The core owns gestures, selection, target and commit.
use gpui_rsx::rsx;
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    io::Cursor,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Instant,
};

use cliphost_core::{host::HostState, i18n::tr};
use gpui::{
    div, img, prelude::*, px, AnimationExt, AnyElement, Context, Image, ImageFormat, ListAlignment,
    ListState, Task, Window, WindowHandle, WindowOptions,
};
use serde_json::{json, Value};

use crate::{
    native_content::{
        self,
        model::{ContentMode, ContentProjection, MixedPart, StepProjection},
        ContentImage, ContentResources, ListContentCache,
    },
    platform,
    preview::DesignTokens,
};

#[path = "overlay_actions.rs"]
pub(super) mod actions;
#[path = "overlay_html.rs"]
pub(super) mod html;
#[path = "overlay_html_view.rs"]
pub(super) mod html_view;
#[path = "overlay_placement.rs"]
mod placement;
#[path = "overlay_preview_model.rs"]
pub(crate) mod preview_model;
#[path = "overlay_style.rs"]
mod style;
#[path = "overlay_theme.rs"]
pub(super) mod theme;
use actions::{available_actions, display_index, RowAction};
use preview_model::RasterSource;

pub struct QuickPasteOverlay {
    host: Arc<HostState>,
    state: Value,
    tokens: DesignTokens,
    theme: theme::OverlayTheme,
    dark: bool,
    list: ListState,
    preview_key: Option<PreviewKey>,
    /// Item JSON the current/pending preview was loaded from.
    preview_item: Value,
    preview: Option<LoadedPreview>,
    preview_error: Option<String>,
    preview_task: Option<Task<()>>,
    preview_revision: u64,
    action_pending: Option<PreviewKey>,
    action_error: Option<String>,
    /// Advances when the row-action bar newly appears (entrance animation id).
    action_bar_epoch: crate::motion::OpenEpoch,
    motion: RefCell<crate::motion::Tweens<(String, MotionTrack)>>,
    hovered_actions: HashSet<usize>,
    presentation_live: Arc<AtomicBool>,
    row_generation: u64,
    row_snapshot: Value,
    row_cache: ListContentCache,
    row_tasks: HashMap<u64, Task<()>>,
    row_serial: u64,
    source_hwnd: isize,
    preview_offset_y: f32,
    /// Slide the preview only when the active row moved; content/size
    /// changes (first paint, image arrival) snap it into place.
    preview_offset_animate: bool,
    placement_last: Option<PlacementStamp>,
    placement_pending: Option<PlacementReport>,
    placement_task: Option<Task<()>>,
    #[cfg(debug_assertions)]
    design_only: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum MotionTrack {
    Row,
    Armed,
    Hover,
    PreviewOffset,
}

/// Detail requests belong to the selected row of one core presentation. A
/// retained overlay window must never display replies from a retired session.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PreviewKey {
    session_revision: u64,
    state_revision: u64,
    item_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PlacementIdentity {
    session_revision: u64,
    state_revision: u64,
    source_hwnd: isize,
}

fn placement_identity(state: &Value, source_hwnd: isize) -> Option<PlacementIdentity> {
    let identity = PlacementIdentity {
        session_revision: state["sessionRevision"].as_u64()?,
        state_revision: state["stateRevision"].as_u64()?,
        source_hwnd,
    };
    (state["visible"] == true
        && identity.session_revision > 0
        && identity.state_revision > 0
        && source_hwnd != 0)
        .then_some(identity)
}

#[derive(Clone, Debug, PartialEq)]
struct PlacementStamp {
    identity: PlacementIdentity,
    cluster: [i32; 4],
    viewport: [i32; 2],
    scale: f64,
}

#[derive(Clone)]
struct PlacementReport {
    identity: PlacementIdentity,
    frame: placement::Frame,
    has_preview: bool,
    has_actions: bool,
    viewport: [f32; 2],
    scale: f64,
}

impl PlacementReport {
    fn stamp(&self) -> Option<PlacementStamp> {
        if !self.scale.is_finite()
            || self.scale <= 0.0
            || !self
                .viewport
                .into_iter()
                .all(|value| value.is_finite() && value > 0.0)
        {
            return None;
        }
        Some(PlacementStamp {
            identity: self.identity.clone(),
            cluster: self
                .frame
                .visible_cluster(self.has_preview, self.has_actions, self.viewport)?
                .rounded(),
            viewport: self.viewport.map(|value| value.round() as i32),
            scale: self.scale,
        })
    }
    fn body(&self) -> Option<Value> {
        let cluster =
            self.frame
                .visible_cluster(self.has_preview, self.has_actions, self.viewport)?;
        self.stamp()?;
        Some(
            json!({"action":"placementReady", "stateRevision":self.identity.state_revision,
            "sessionRevision":self.identity.session_revision, "sourceHwnd":self.identity.source_hwnd,
            "clusterLeft":cluster.left, "clusterTop":cluster.top,
            "clusterWidth":cluster.width, "clusterHeight":cluster.height,
            "viewportWidth":self.viewport[0], "viewportHeight":self.viewport[1],
            "devicePixelRatio":self.scale}),
        )
    }
}

impl Drop for QuickPasteOverlay {
    fn drop(&mut self) {
        self.presentation_live.store(false, Ordering::Release);
    }
}

fn selected_key(state: &Value) -> Option<PreviewKey> {
    if state["visible"].as_bool() != Some(true) {
        return None;
    }
    let index = state["activeIndex"].as_u64().unwrap_or(0) as usize;
    let item_id = state["items"][index]["id"].as_str()?.to_string();
    if item_id.trim().is_empty() {
        return None;
    }
    Some(PreviewKey {
        session_revision: state["sessionRevision"].as_u64().unwrap_or(0),
        state_revision: state["stateRevision"].as_u64().unwrap_or(0),
        item_id,
    })
}

struct RichSegment {
    text: String,
    image: Option<ContentImage>,
}

struct LoadedPreview {
    text: String,
    body_text_loaded: bool,
    file_paths: Vec<String>,
    image: Option<ContentImage>,
    image_unavailable: bool,
    segments: Vec<RichSegment>,
    html: Option<html_view::LoadedHtml>,
    steps: Option<StepProjection>,
    resources: ContentResources,
    _image_holder: native_content::ContentImageHolderLease,
}

/// Images are decoded on workers. Preserve a small original (including GIF),
/// otherwise retain a bounded display bitmap and its original natural size.
fn raster_from_bytes(bytes: Vec<u8>, mime: &str, compact: bool) -> Option<ContentImage> {
    let reader = image::ImageReader::new(Cursor::new(&bytes))
        .with_guessed_format()
        .ok()?;
    let (width, height) = reader.into_dimensions().ok()?;
    if width == 0 || height == 0 {
        return None;
    }
    if !compact {
        let candidate = ContentImage {
            image: image_from_bytes(bytes.clone(), mime).ok()?,
            width: width as f32,
            height: height as f32,
            render: None,
        };
        let mut resources = ContentResources::default();
        resources
            .images
            .insert("image".into(), Some(candidate.clone()));
        resources.limit_retained_bytes(native_content::CONTENT_RESOURCE_BUDGET_BYTES);
        if resources.images["image"].is_some() {
            return Some(candidate);
        }
    }
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    let edge = if compact { 96 } else { 1200 };
    let decoded = reader.decode().ok()?.thumbnail(edge, edge);
    let mut output = Cursor::new(Vec::new());
    decoded
        .write_to(&mut output, image::ImageFormat::Png)
        .ok()?;
    Some(ContentImage {
        image: image_from_bytes(output.into_inner(), "image/png").ok()?,
        width: width as f32,
        height: height as f32,
        render: None,
    })
}

fn load_raster(host: &HostState, source: &RasterSource, compact: bool) -> Option<ContentImage> {
    let (bytes, mime) = match source {
        RasterSource::Item(id) => host.image_bytes(id).ok()?,
        RasterSource::Local(path) => host.local_image_bytes(path)?,
    };
    raster_from_bytes(bytes, &mime, compact)
}

/// An image item's list path goes through the local-image service first. When
/// that refuses it (a stored locator it cannot map), read the item's managed
/// image by id, as the history list does, rather than leave a blank thumbnail.
fn load_item_raster(
    host: &HostState,
    item: &Value,
    source: &RasterSource,
    compact: bool,
) -> Option<ContentImage> {
    load_raster(host, source, compact).or_else(|| {
        let id = item["id"].as_str().filter(|id| !id.is_empty())?;
        (matches!(source, RasterSource::Local(_))
            && item["kind"].as_str() == Some("ImageClipItem"))
        .then(|| load_raster(host, &RasterSource::Item(id.to_owned()), compact))
        .flatten()
    })
}

fn retain_preview_image(
    resources: &mut ContentResources,
    key: String,
    image: ContentImage,
) -> Option<ContentImage> {
    let remaining =
        native_content::CONTENT_RESOURCE_BUDGET_BYTES.saturating_sub(resources.retained_bytes());
    let mut candidate = ContentResources::default();
    candidate.images.insert(key.clone(), Some(image.clone()));
    if candidate.retained_bytes() > remaining {
        return None;
    }
    resources.images.insert(key, Some(image.clone()));
    Some(image)
}

fn row_projection(item: &Value) -> Option<ContentProjection> {
    if let Some(source) = preview_model::row_image_source(item) {
        let path = match source {
            RasterSource::Item(id) => format!("@clip-image:{id}"),
            RasterSource::Local(path) => path,
        };
        Some(ContentProjection::Mixed(vec![MixedPart::Image {
            path: Some(path),
        }]))
    } else {
        preview_model::file_lead(item).map(|(path, _)| {
            ContentProjection::Mixed(vec![MixedPart::File {
                name: String::new(),
                path,
            }])
        })
    }
}

fn load_row_resources(host: &HostState, item: &Value) -> ContentResources {
    let mut resources = ContentResources::default();
    if let Some(source) = preview_model::row_image_source(item) {
        let key = match &source {
            RasterSource::Item(id) => format!("@clip-image:{id}"),
            RasterSource::Local(path) => path.clone(),
        };
        resources
            .images
            .insert(key, load_item_raster(host, item, &source, true));
    } else if let Some((path, _)) = preview_model::file_lead(item) {
        let icon = host
            .file_icon_png(Some(&path), None, None)
            .ok()
            .and_then(|bytes| image_from_bytes(bytes, "image/png").ok());
        resources.icons.insert(path, icon);
    }
    resources.limit_retained_bytes(native_content::CONTENT_RESOURCE_BUDGET_BYTES);
    resources
}

fn image_from_bytes(bytes: Vec<u8>, mime: &str) -> Result<Arc<Image>, String> {
    let format = ImageFormat::from_mime_type(mime)
        .ok_or_else(|| tr("不支持的图片格式", "Unsupported image format").to_string())?;
    Ok(Arc::new(Image::from_bytes(format, bytes)))
}

fn detail_text(detail: &Value, item: &Value) -> String {
    // Steps already have the core's structured presentation; avoid rendering
    // the serialized action JSON as if it were ordinary clipboard text.
    if item["kind"].as_str() == Some("StepClipItem") {
        if let Some(text) = item["stepPresentation"]["previewText"]
            .as_str()
            .filter(|text| !text.is_empty())
        {
            return text.to_string();
        }
    }
    [
        detail["bodyText"].as_str(),
        detail["summary"].as_str(),
        item["displayText"].as_str(),
        item["summary"].as_str(),
        item["title"].as_str(),
    ]
    .into_iter()
    .flatten()
    .find(|text| !text.is_empty())
    .unwrap_or("")
    .to_string()
}

fn truncated_html_body<'a>(
    projection: &html::HtmlPreview,
    text: &'a str,
    body_text_loaded: bool,
) -> Option<&'a str> {
    (projection.truncated && body_text_loaded && !text.trim().is_empty()).then_some(text)
}

fn load_preview(host: &HostState, item: Value, foreground: &str) -> Result<LoadedPreview, String> {
    let id = item["id"].as_str().unwrap_or("");
    let detail = host.detail(&json!({ "id": id }))?;
    let source = preview_model::image_source(&item);
    let image = source
        .as_ref()
        .and_then(|source| load_item_raster(host, &item, source, false));
    let file_paths = detail["filePaths"]
        .as_array()
        .map(|paths| {
            paths
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let steps = native_content::model::project_steps(&item["stepPresentation"]);
    let mut resources = steps
        .as_ref()
        .map(|steps| {
            native_content::load_content_resources(
                host,
                &ContentProjection::Steps(steps.clone()),
                foreground,
            )
        })
        .unwrap_or_default();
    resources.limit_retained_bytes(native_content::CONTENT_RESOURCE_BUDGET_BYTES);
    let image =
        image.and_then(|image| retain_preview_image(&mut resources, "primary".into(), image));
    let segments = detail["imRichSegments"]
        .as_array()
        .map(|segments| {
            segments
                .iter()
                .enumerate()
                .map(|(index, segment)| {
                    let path = segment["path"].as_str().unwrap_or("");
                    let image = if segment["kind"].as_str() == Some("image") {
                        host.local_image_bytes(path)
                            .and_then(|(bytes, mime)| raster_from_bytes(bytes, &mime, false))
                    } else {
                        None
                    };
                    let image = if let Some(image) = image {
                        let key = format!("segment:{index}");
                        retain_preview_image(&mut resources, key, image)
                    } else {
                        None
                    };
                    let text = match segment["kind"].as_str().unwrap_or("") {
                        "image" if image.is_none() => {
                            tr("[图片加载失败]", "[Image failed to load]").to_string()
                        }
                        "file" => path.to_string(),
                        _ => segment["text"].as_str().unwrap_or("").to_string(),
                    };
                    RichSegment { text, image }
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(LoadedPreview {
        text: detail_text(&detail, &item),
        body_text_loaded: detail["bodyText"]
            .as_str()
            .is_some_and(|text| !text.trim().is_empty()),
        file_paths,
        image,
        image_unavailable: source.is_some()
            && resources
                .images
                .get("primary")
                .and_then(Option::as_ref)
                .is_none(),
        segments,
        steps,
        _image_holder: native_content::retain_content_images(&resources),
        resources,
        html: detail["htmlData"]
            .as_str()
            .filter(|html| !html.trim().is_empty())
            .map(|html| html_view::load_html_preview(host, html)),
    })
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ClusterLayout {
    list_left: f64,
    preview_left: f64,
}

/// Hidden draws before a reveal: the first paints the new session and reports
/// its geometry (active row, preview card); the second applies the measured
/// preview offset, so the first visible frame is already settled.
const QP_SETTLE_PASSES: u8 = 2;

fn cluster_layout(
    cluster_left: f64,
    panel_width: f64,
    preview_width: f64,
    gap: f64,
    preview_on_left: bool,
) -> ClusterLayout {
    if preview_on_left && preview_width > 0.0 {
        ClusterLayout {
            list_left: cluster_left + preview_width + gap,
            preview_left: cluster_left,
        }
    } else {
        ClusterLayout {
            list_left: cluster_left,
            preview_left: cluster_left + panel_width + gap,
        }
    }
}

fn fifo_banner(state: &Value) -> Option<String> {
    if state["fifoArmed"].as_bool() != Some(true) {
        return None;
    }
    if let Some(text) = state["fifoBanner"]
        .as_str()
        .filter(|text| !text.trim().is_empty())
    {
        return Some(text.to_string());
    }
    let remaining = state["fifoRemainingCount"].as_u64().unwrap_or(0);
    let total = state["fifoTotalCount"].as_u64().unwrap_or(remaining);
    Some(cliphost_core::trf!(
        "连续粘贴 · 剩余 {remaining} / {total}",
        "Sequential paste · {remaining} / {total} remaining"
    ))
}

fn overlay_hint(state: &Value) -> String {
    let raw = state["overlayHint"].as_str().unwrap_or("");
    if raw.contains("松开 X1") {
        return tr(
            "滚轮切换 · 松开 X1 粘贴 · 右键 / Esc 取消",
            "Wheel to select · Release X1 to paste · Right click / Esc to cancel",
        )
        .to_string();
    }
    if raw.contains("松开 X2") {
        return tr(
            "滚轮切换 · 松开 X2 粘贴 · 右键 / Esc 取消",
            "Wheel to select · Release X2 to paste · Right click / Esc to cancel",
        )
        .to_string();
    }
    if !raw.is_empty() && !raw.starts_with("按 1/2 选择操作") {
        return raw.to_string();
    }
    let mut parts = vec![tr("按 1/2 选择操作", "1/2 to select an action")];
    if state["enableDeleteShortcut"].as_bool() != Some(false) {
        parts.push(tr("D 删除", "D to delete"));
    }
    if state["enablePasteEnterShortcut"].as_bool() != Some(false) {
        parts.push(tr("E 粘贴并回车", "E to paste and Enter"));
    }
    if state["enableFifoShortcut"].as_bool() != Some(false) {
        parts.push(tr("F 连续粘贴", "F for sequential paste"));
    }
    parts.push(tr(
        "Esc 取消 · 松开 Alt 执行 · ` 下一条 / Q 上一条",
        "Esc to cancel · Release Alt to execute · ` next / Q previous",
    ));
    parts.join(" · ")
}

impl QuickPasteOverlay {
    fn new_presenter(host: Arc<HostState>, state: Value, tokens: DesignTokens, dark: bool) -> Self {
        Self {
            host,
            list: ListState::new(
                state["items"].as_array().map_or(0, Vec::len),
                ListAlignment::Top,
                px(100.0),
            ),
            state,
            theme: theme::OverlayTheme::new(&tokens),
            tokens,
            dark,
            preview_key: None,
            preview_item: Value::Null,
            preview: None,
            preview_error: None,
            preview_task: None,
            preview_revision: 0,
            action_pending: None,
            action_error: None,
            action_bar_epoch: crate::motion::OpenEpoch::default(),
            motion: RefCell::default(),
            hovered_actions: HashSet::new(),
            presentation_live: Arc::new(AtomicBool::new(true)),
            row_generation: 0,
            row_snapshot: Value::Null,
            row_cache: ListContentCache::default(),
            row_tasks: HashMap::new(),
            row_serial: 0,
            source_hwnd: 0,
            preview_offset_y: 0.0,
            preview_offset_animate: false,
            placement_last: None,
            placement_pending: None,
            placement_task: None,
            #[cfg(debug_assertions)]
            design_only: false,
        }
    }

    /// Debug-only visual fixture: uses the production presenter with an empty,
    /// headless, isolated core. There is no live session, placement or action.
    #[cfg(debug_assertions)]
    pub fn design_fixture(
        host: Arc<HostState>,
        state: Value,
        tokens: DesignTokens,
        dark: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        assert!(
            cliphost_core::is_headless(),
            "visual fixtures require a headless core"
        );
        let mut this = Self::new_presenter(host, state, tokens, dark);
        this.design_only = true;
        this.refresh_row_generation(cx);
        let foreground = this.tokens.theme(dark).text.clone();
        let items = this.state["items"].as_array().cloned().unwrap_or_default();
        for item in &items {
            if let Some(projection) = row_projection(item) {
                if let Some(ticket) = this.row_cache.reserve(
                    this.row_generation,
                    item["id"].as_str().unwrap_or_default(),
                    &projection,
                    ContentMode::Standard,
                    &foreground,
                ) {
                    let resources = load_row_resources(&this.host, item);
                    this.row_cache.finish(ticket, resources, true);
                }
            }
        }
        let active = this.state["activeIndex"].as_u64().unwrap_or(0) as usize;
        this.list.scroll_to(gpui::ListOffset {
            item_ix: visible_rows_start(items.len(), active),
            offset_in_item: px(0.0),
        });
        if let Some(item) = items.get(active) {
            let image = preview_model::image_source(item)
                .and_then(|source| load_item_raster(&this.host, item, &source, false));
            let mut resources = ContentResources::default();
            let image = image
                .and_then(|image| retain_preview_image(&mut resources, "primary".into(), image));
            this.preview = Some(LoadedPreview {
                text: item["displayText"].as_str().unwrap_or_default().to_string(),
                body_text_loaded: true,
                file_paths: item["filePaths"]
                    .as_array()
                    .map(|paths| {
                        paths
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default(),
                image,
                image_unavailable: false,
                segments: Vec::new(),
                html: None,
                steps: None,
                _image_holder: native_content::retain_content_images(&resources),
                resources,
            });
            this.preview_key = selected_key(&this.state);
        }
        this
    }

    pub fn show(
        existing: &mut Option<WindowHandle<Self>>,
        host: Arc<HostState>,
        tokens: DesignTokens,
        dark: bool,
        cx: &mut gpui::App,
    ) -> Result<(), String> {
        let state = host.qp_state_detail();
        if state["visible"].as_bool() != Some(true) {
            return Ok(());
        }
        let incoming_tokens = tokens.clone();
        let handle = if let Some(handle) = existing {
            *handle
        } else {
            let handle = cx
                .open_window(
                    WindowOptions {
                        titlebar: None,
                        focus: false,
                        show: false,
                        window_background: gpui::WindowBackgroundAppearance::Transparent,
                        ..Default::default()
                    },
                    |window, cx| {
                        let close_host = host.clone();
                        window.on_window_should_close(cx, move |_, _| {
                            close_host.dismiss_native_window("quick-paste");
                            false
                        });
                        window.set_window_title(tr(
                            "ClipHost GPUI · 便捷粘贴",
                            "ClipHost GPUI · Quick paste",
                        ));
                        cx.new(|_| Self::new_presenter(host.clone(), state.clone(), tokens, dark))
                    },
                )
                .map_err(|error| error.to_string())?;
            *existing = Some(handle);
            handle
        };
        handle
            .update(cx, |overlay, window, cx| {
                if !overlay.refresh_theme(window, true, cx) {
                    overlay.set_design_tokens(incoming_tokens, dark, cx);
                }
                #[cfg(windows)]
                {
                    overlay.source_hwnd = platform::hwnd(window)?.0 as isize;
                    host.set_qp_hwnd(overlay.source_hwnd);
                }
                overlay.update_state(state, cx);
                if let Some(rect) = host.qp_overlay_place() {
                    let expected = overlay.state["sessionRevision"].clone();
                    let guarded = host.clone();
                    let live = overlay.presentation_live.clone();
                    let _guard = platform::guard_condition(move || {
                        let current = guarded.qp_state_detail();
                        live.load(Ordering::Acquire)
                            && current["visible"].as_bool() == Some(true)
                            && current["sessionRevision"] == expected
                    });
                    platform::overlay_show_settled(window, host.host_hwnd(), rect, QP_SETTLE_PASSES)?;
                }
                Ok::<_, String>(())
            })
            .map_err(|error| error.to_string())??;
        Ok(())
    }

    pub fn maintain(handle: WindowHandle<Self>, host: &HostState, cx: &mut gpui::App) {
        let state = host.qp_state_detail();
        let _ = handle.update(cx, |overlay, window, cx| {
            if state["visible"].as_bool() == Some(true) {
                overlay.refresh_theme(window, false, cx);
            }
            if state == overlay.state {
                return;
            }
            // Bind posted Win32 work to the incoming presentation's token.
            // Capturing the previous token before this update would make a
            // first/reopened show retire itself before native dispatch.
            overlay.update_state(state.clone(), cx);
            if state["visible"].as_bool() != Some(true) {
                let guarded = host.clone();
                let _guard = platform::guard_condition(move || {
                    guarded.qp_state_detail()["visible"].as_bool() != Some(true)
                });
                let _ = platform::hide_cleared(window);
            } else if let Some(rect) = host.qp_overlay_place() {
                let expected = state["sessionRevision"].clone();
                let guarded = host.clone();
                let live = overlay.presentation_live.clone();
                let _guard = platform::guard_condition(move || {
                    let current = guarded.qp_state_detail();
                    live.load(Ordering::Acquire)
                        && current["visible"].as_bool() == Some(true)
                        && current["sessionRevision"] == expected
                });
                let _ = platform::overlay_show_settled(window, host.host_hwnd(), rect, QP_SETTLE_PASSES);
            }
        });
    }

    pub fn hide(handle: WindowHandle<Self>, cx: &mut gpui::App) {
        let _ = handle.update(cx, |overlay, window, cx| {
            if overlay.host.qp_state_detail()["visible"].as_bool() == Some(true) {
                // An old queued Hide command cannot retire a newer core show.
                return Ok::<_, String>(());
            }
            overlay.presentation_live.store(false, Ordering::Release);
            overlay.state["visible"] = json!(false);
            overlay.action_pending = None;
            overlay.action_error = None;
            overlay.motion.borrow_mut().clear();
            overlay.hovered_actions.clear();
            overlay.action_bar_epoch.observe(false);
            overlay.preview_offset_y = 0.0;
            overlay.preview_offset_animate = false;
            overlay.placement_last = None;
            overlay.placement_pending = None;
            // A finished preview is content, not session state: keep it so a
            // reopen on the same item paints its image in the first frame.
            if overlay.preview_task.is_some() || overlay.preview.is_none() {
                overlay.clear_preview(cx);
            }
            overlay.refresh_row_generation(cx);
            cx.notify();
            let guarded = overlay.host.clone();
            let _guard = platform::guard_condition(move || {
                guarded.qp_state_detail()["visible"].as_bool() != Some(true)
            });
            platform::hide_cleared(window)
        });
    }

    fn refresh_theme(&mut self, window: &Window, force: bool, cx: &mut Context<Self>) -> bool {
        let system_dark = matches!(
            window.appearance(),
            gpui::WindowAppearance::Dark | gpui::WindowAppearance::VibrantDark
        );
        if let Some((tokens, dark)) = self.theme.read(&self.host, system_dark, force) {
            self.set_design_tokens(tokens, dark, cx);
            true
        } else {
            false
        }
    }

    pub(super) fn set_design_tokens(
        &mut self,
        tokens: DesignTokens,
        dark: bool,
        cx: &mut Context<Self>,
    ) {
        if self.dark == dark && self.tokens.theme(dark) == tokens.theme(dark) {
            return;
        }
        let foreground_changed = self.tokens.theme(self.dark).text != tokens.theme(dark).text;
        self.tokens = tokens;
        self.dark = dark;
        self.motion.borrow_mut().clear();
        self.hovered_actions.clear();
        self.refresh_row_generation(cx);
        if foreground_changed {
            self.clear_preview(cx);
            self.refresh_preview(cx);
        }
        cx.notify();
    }

    fn clear_preview(&mut self, cx: &mut Context<Self>) {
        self.preview_revision = self
            .preview_revision
            .checked_add(1)
            .expect("QP preview generation exhausted");
        self.preview_key = None;
        self.preview_item = Value::Null;
        if let Some(preview) = self.preview.take() {
            for image in preview.resources.images_for_release() {
                native_content::release_content_image(image, cx);
            }
        }
        self.preview_error = None;
        self.preview_task = None;
    }

    fn refresh_row_generation(&mut self, cx: &mut Context<Self>) {
        if self.state["visible"].as_bool() != Some(true) {
            return;
        }
        let foreground = self.tokens.theme(self.dark).text.clone();
        let snapshot = row_snapshot(&self.state, &foreground);
        if self.row_snapshot != snapshot {
            self.row_generation = self
                .row_generation
                .checked_add(1)
                .expect("QP resource generation exhausted");
            self.row_cache.advance_retaining(self.row_generation, |key| {
                row_resource_unchanged(&self.row_snapshot, &snapshot, &key.item_id)
            });
            self.row_snapshot = snapshot;
            self.row_cache.release_evicted(cx);
            // Accepted workers retain their physical slot until completion.
            // Dropping their handles here would strand cache backpressure.
        }
    }

    fn row_lead(
        &mut self,
        item: &Value,
        active: f32,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        // A hidden HWND keeps its GPUI entity. Do not reserve a new worker
        // from a retained render after rejected replies release their slots.
        if !row_resources_live(&self.state, &self.presentation_live) {
            return None;
        }
        let foreground = self.tokens.theme(self.dark).text.clone();
        let theme = self.tokens.theme(self.dark);
        let lead_border = style::row_colors(theme, active, false).lead_border;
        if row_projection(item).is_none() {
            if let Some(swatch) = pure_row_color(item) {
                return Some(
                    rsx! {
                        <div size={px(26.0)} flex_shrink_0 relative rounded={px(6.0)} border_1
                            border_color={lead_border} bg={swatch_color(swatch)}>
                            {style::swatch_inset(theme, 6.0)}
                        </div>
                    }
                        .into_any_element(),
                );
            }
        }
        let projection = row_projection(item)?;
        let id = item["id"].as_str().unwrap_or_default();
        if let Some(ticket) = self.row_cache.reserve(
            self.row_generation,
            id,
            &projection,
            ContentMode::Standard,
            &foreground,
        ) {
            self.row_serial = self
                .row_serial
                .checked_add(1)
                .expect("QP resource worker serial exhausted");
            let serial = self.row_serial;
            let host = self.host.clone();
            let item = item.clone();
            let snapshot = self.row_snapshot.clone();
            let live = self.presentation_live.clone();
            let task = cx.spawn(async move |this, cx| {
                let work_live = live.clone();
                let work_snapshot = snapshot.clone();
                let work_foreground = foreground.clone();
                let resources = cx
                    .background_executor()
                    .spawn(async move {
                        if !work_live.load(Ordering::Acquire)
                            || row_snapshot(&host.qp_state_detail(), &work_foreground)
                                != work_snapshot
                        {
                            return ContentResources::default();
                        }
                        load_row_resources(&host, &item)
                    })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.row_tasks.remove(&serial);
                    let current = same_presentation(&this.presentation_live, &live)
                        && this.row_snapshot == snapshot
                        && row_snapshot(&this.host.qp_state_detail(), &foreground) == snapshot;
                    this.row_cache.finish(ticket, resources, current);
                    this.row_cache.release_evicted(cx);
                    // Even a rejected reply releases a slot; render schedules
                    // the remaining visible rows from the new generation.
                    cx.notify();
                });
            });
            self.row_tasks.insert(serial, task);
        }
        self.row_cache.release_evicted(cx);
        let resources = self.row_cache.get(
            self.row_generation,
            id,
            &projection,
            ContentMode::Standard,
            &self.tokens.theme(self.dark).text,
        );
        let image = resources.and_then(|resources| {
            resources
                .images
                .values()
                .find_map(Option::as_ref)
                .map(|image| image.image.clone())
                .or_else(|| resources.icons.values().find_map(Clone::clone))
        });
        let image_lead = preview_model::row_image_source(item).is_some();
        let lead_size = if image_lead { 44.0 } else { 40.0 };
        let frame = rsx! {
            <div size={px(lead_size)} flex_shrink_0 flex items_center justify_center
                rounded={px(8.0)} overflow_hidden border_1 border_color={lead_border}
                bg={crate::view::color(&theme.secondary)} />
        };
        Some(if let Some(image) = image {
            frame
                .child(native_content::render_resource_image(
                    image.clone(),
                    img(image)
                        .size(px(if image_lead { lead_size - 2.0 } else { 36.0 }))
                        .object_fit(if image_lead {
                            gpui::ObjectFit::Cover
                        } else {
                            gpui::ObjectFit::Contain
                        })
                        .into_any_element(),
                ))
                .into_any_element()
        } else {
            // The frame is reserved before the async reply, so row heights and
            // the virtual list's active-row scroll position remain stable.
            frame
                .child(
                    crate::crisp_svg::svg()
                        .path(self.tokens.icons[if image_lead { "image" } else { "file" }].clone())
                        .size(px(20.0))
                        .text_color(crate::view::color(&theme.text_muted)),
                )
                .into_any_element()
        })
    }

    fn update_state(&mut self, state: Value, cx: &mut Context<Self>) {
        let visible = state["visible"].as_bool() == Some(true);
        if !visible
            || state["sessionRevision"] != self.state["sessionRevision"]
            || !self.presentation_live.load(Ordering::Acquire)
        {
            self.presentation_live.store(false, Ordering::Release);
            self.presentation_live = Arc::new(AtomicBool::new(visible));
            self.action_pending = None;
            self.action_error = None;
            self.motion.borrow_mut().clear();
            self.hovered_actions.clear();
            self.action_bar_epoch.observe(false);
            self.preview_offset_y = 0.0;
            self.preview_offset_animate = false;
        } else if state["activeIndex"] != self.state["activeIndex"] {
            self.preview_offset_animate = true;
        }
        if placement_identity(&state, self.source_hwnd)
            != placement_identity(&self.state, self.source_hwnd)
        {
            self.placement_last = None;
            self.placement_pending = None;
        }
        let count = state["items"].as_array().map_or(0, Vec::len);
        if state["activeIndex"] != self.state["activeIndex"] {
            self.hovered_actions.clear();
        }
        if self.list.item_count() != count {
            self.list.reset(count);
        }
        if let Some(index) = state["activeIndex"].as_u64() {
            // Match frontend visibleRows. An explicit logical start avoids
            // revealing against the previous viewport when row heights vary.
            self.list.scroll_to(gpui::ListOffset {
                item_ix: visible_rows_start(count, index as usize),
                offset_in_item: px(0.0),
            });
        }
        self.state = state;
        self.refresh_row_generation(cx);
        let key = selected_key(&self.state);
        if self
            .action_pending
            .as_ref()
            .is_some_and(|pending| Some(pending) != key.as_ref())
        {
            self.action_pending = None;
            self.action_error = None;
        }
        self.refresh_preview(cx);
        cx.notify();
    }

    fn refresh_preview(&mut self, cx: &mut Context<Self>) {
        let key = selected_key(&self.state);
        let Some(key) = key else {
            // Hidden: keep a finished preview for the next show; an
            // unfinished load would otherwise strand an empty preview.
            if self.state["visible"].as_bool() != Some(true)
                && self.preview_task.is_none()
                && self.preview.is_some()
            {
                return;
            }
            self.clear_preview(cx);
            return;
        };
        let index = self.state["activeIndex"].as_u64().unwrap_or(0) as usize;
        let item = self.state["items"][index].clone();
        if self.preview_key.as_ref() == Some(&key) && self.preview_item == item {
            return;
        }
        let same_item = self
            .preview_key
            .as_ref()
            .is_some_and(|current| current.item_id == key.item_id);
        if same_item
            && self.preview_item == item
            && (self.preview.is_some() || self.preview_task.is_some())
        {
            // Session/state revisions advance on every show and item reload;
            // the item content is unchanged, so do not blank and reload it.
            self.preview_key = Some(key);
            return;
        }
        if same_item && self.preview.is_some() {
            // Same item, changed metadata: keep painting the old preview
            // until its replacement arrives instead of flashing empty.
            self.preview_revision = self
                .preview_revision
                .checked_add(1)
                .expect("QP preview generation exhausted");
            self.preview_error = None;
            self.preview_task = None;
        } else {
            self.clear_preview(cx);
        }
        self.preview_key = Some(key.clone());
        self.preview_item = item.clone();
        let host = self.host.clone();
        let foreground = self.tokens.theme(self.dark).text.clone();
        let revision = self.preview_revision;
        let item_id = key.item_id.clone();
        self.preview_task = Some(cx.spawn(async move |this, cx| {
            let work_id = item_id.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    if selected_key(&host.qp_state_detail())
                        .is_none_or(|selected| selected.item_id != work_id)
                    {
                        return None;
                    }
                    Some(load_preview(&host, item, &foreground))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.preview_revision != revision {
                    return;
                }
                this.preview_task = None;
                let selected = selected_key(&this.host.qp_state_detail())
                    .is_some_and(|selected| selected.item_id == item_id);
                match result {
                    Some(Ok(preview)) if selected => {
                        if let Some(old) = this.preview.replace(preview) {
                            for image in old.resources.images_for_release() {
                                native_content::release_content_image(image, cx);
                            }
                        }
                    }
                    Some(Err(error)) if selected => this.preview_error = Some(error),
                    // Retired before it finished: let the next refresh reload.
                    _ => {
                        if this.preview.is_none() {
                            this.preview_key = None;
                            this.preview_item = Value::Null;
                        }
                    }
                }
                cx.notify();
            });
        }));
    }

    fn accept_placement(
        &mut self,
        report: PlacementReport,
        live: &Arc<AtomicBool>,
        cx: &mut Context<Self>,
    ) {
        if !same_presentation(&self.presentation_live, live)
            || placement_identity(&self.state, self.source_hwnd).as_ref() != Some(&report.identity)
            || placement_identity(&self.host.qp_state_detail(), self.source_hwnd).as_ref()
                != Some(&report.identity)
        {
            return;
        }
        let Some(stamp) = report.stamp() else {
            return;
        };
        if report.has_preview {
            if let Some(offset) = report.frame.preview_offset(report.has_actions) {
                if (offset - self.preview_offset_y).abs() > 0.5 {
                    if !std::mem::take(&mut self.preview_offset_animate) {
                        // First paint / image arrival: settle, do not slide.
                        self.motion.borrow_mut().retain(|(key, track)| {
                            !(key == "preview-offset" && *track == MotionTrack::PreviewOffset)
                        });
                    }
                    self.preview_offset_y = offset;
                    cx.notify();
                }
            }
        }
        // A→B→A can return to the last acknowledged bounds while B is still
        // in flight. Preserve that latest paint so B cannot remain the hit box.
        if self.placement_task.is_some() || self.placement_last.as_ref() != Some(&stamp) {
            self.placement_pending = Some(report);
            self.start_placement(cx);
        }
    }

    fn start_placement(&mut self, cx: &mut Context<Self>) {
        // At most one placement worker enters the legacy native API. Retain it
        // across hide/reopen until it finishes; latest painted geometry wins.
        if self.placement_task.is_some() {
            return;
        }
        let Some(report) = self.placement_pending.take() else {
            return;
        };
        let Some(stamp) = report.stamp() else {
            return;
        };
        if self.placement_last.as_ref() == Some(&stamp)
            || placement_identity(&self.state, self.source_hwnd).as_ref() != Some(&report.identity)
        {
            return;
        }
        let Some(body) = report.body() else {
            return;
        };
        let host = self.host.clone();
        let live = self.presentation_live.clone();
        self.placement_task = Some(cx.spawn(async move |this, cx| {
            let work_live = live.clone();
            let work_identity = report.identity.clone();
            let accepted = cx
                .background_executor()
                .spawn(async move {
                    if !work_live.load(Ordering::Acquire)
                        || placement_identity(&host.qp_state_detail(), work_identity.source_hwnd)
                            .as_ref()
                            != Some(&work_identity)
                    {
                        return false;
                    }
                    host.qp_web_action(&body)["ok"].as_bool() == Some(true)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.placement_task = None;
                if accepted
                    && same_presentation(&this.presentation_live, &live)
                    && placement_identity(&this.state, this.source_hwnd).as_ref()
                        == Some(&report.identity)
                    && placement_identity(&this.host.qp_state_detail(), this.source_hwnd).as_ref()
                        == Some(&report.identity)
                {
                    this.placement_last = Some(stamp);
                }
                this.start_placement(cx);
            });
        }));
    }

    fn run_row_action(&mut self, action: RowAction, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = selected_key(&self.state) else {
            return;
        };
        if self.action_pending.is_some()
            || !available_actions(
                &self.state["items"][self.state["activeIndex"].as_u64().unwrap_or(0) as usize],
                &self.state,
            )
            .contains(&action)
        {
            return;
        }
        #[cfg(windows)]
        let source = platform::hwnd(window)
            .map(|hwnd| hwnd.0 as isize)
            .unwrap_or(0);
        #[cfg(not(windows))]
        let source = {
            let _ = window;
            0isize
        };
        let body = json!({ "action": "nativeRowAction", "itemId": key.item_id, "shortcut": action.shortcut, "sessionRevision": key.session_revision, "stateRevision": key.state_revision, "sourceHwnd": source });
        self.action_pending = Some(key.clone());
        self.action_error = None;
        let host = self.host.clone();
        let live = self.presentation_live.clone();
        cx.spawn(async move |this, cx| {
            let work_live = live.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    if !work_live.load(Ordering::Acquire) {
                        return json!({"ok":false,"reason":"presentation-retired"});
                    }
                    host.qp_native_row_action_with_lifetime(&body, work_live)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if !same_presentation(&this.presentation_live, &live)
                    || selected_key(&this.host.qp_state_detail()).as_ref() != Some(&key)
                    || this.action_pending.as_ref() != Some(&key)
                {
                    return;
                }
                this.action_pending = None;
                if result["ok"].as_bool() != Some(true) {
                    let error = result["error"]
                        .as_str()
                        .or_else(|| result["reason"].as_str())
                        .unwrap_or(tr("操作失败", "Action failed"))
                        .to_string();
                    this.action_error = Some(error.clone());
                    // A release may hide this retained surface while the
                    // accepted click finishes; errors still need visible UI.
                    if this.host.qp_state_detail()["sessionRevision"].as_u64()
                        == Some(key.session_revision)
                        && !error.contains("会话已失效")
                        && !error.contains("目标已变化")
                    {
                        this.host.show_host_toast(
                            &error,
                            "error",
                            Some(tr("便捷粘贴", "Quick paste")),
                            Some(3000),
                            None,
                        );
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn action_bar(&self, item: &Value, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.tokens.theme(self.dark);
        let mut bar = rsx! {
            <div flex items_start gap={px(6.0)} p={px(6.0)} />
        };
        let actions = available_actions(item, &self.state);
        let epoch = self.action_bar_epoch.observe(!actions.is_empty());
        let animate = !actions.is_empty() && !crate::motion::reduced_motion();
        for action in actions {
            let armed = self.state["armedItemId"]
                .as_str()
                .is_some_and(|id| Some(id) == item["id"].as_str())
                && self.state["armedKind"].as_str() == Some(action.kind.name());
            let label = match action.kind.name() {
                "pin" => tr("贴图", "Pin image"),
                "simulate" => tr("模拟输入", "Simulate typing"),
                _ => tr("打开", "Open"),
            };
            let shortcut = action.shortcut;
            let now = Instant::now();
            let motion_key = format!("action-{shortcut}");
            let armed_progress = self.motion.borrow_mut().value(
                (motion_key.clone(), MotionTrack::Armed),
                if armed { 1.0 } else { 0.0 },
                crate::motion::HOVER,
                crate::motion::EASE,
                now,
            );
            let hover_progress = self.motion.borrow_mut().value(
                (motion_key, MotionTrack::Hover),
                if self.hovered_actions.contains(&shortcut) {
                    1.0
                } else {
                    0.0
                },
                crate::motion::HOVER,
                crate::motion::EASE,
                now,
            );
            let palette = style::action_colors(theme, armed_progress, hover_progress);
            let icon = match &action.kind {
                actions::RowActionKind::Pin { .. } => "quick-paste-pin.svg",
                actions::RowActionKind::Simulate => "quick-paste-keyboard.svg",
                actions::RowActionKind::Open {
                    target_kind: "uri", ..
                } => "quick-paste-link.svg",
                _ => "quick-paste-folder.svg",
            };
            let pending = self.action_pending.is_some();
            let live = self.presentation_live.clone();
            let key = selected_key(&self.state);
            bar = bar.child(
                rsx! {
                    <div id={("qp-row-action", shortcut)} flex items_center gap={px(8.0)}
                        min_h={px(40.0)} flex_shrink_0 px={px(12.0)} py={px(6.0)}
                        rounded={px(10.0)} border_1 border_color={palette.border}
                        bg={palette.background} text_color={palette.text}
                        shadow={style::action_shadows(theme, armed_progress)} />
                }
                    .when(pending, |button| button.opacity(0.5))
                    .cursor_pointer()
                    .child(
                        rsx! {
                            <div min_w={px(20.0)} h={px(20.0)} px={px(5.0)} flex items_center
                                justify_center flex_shrink_0 rounded={px(6.0)} border_1
                                border_color={palette.shortcut_border}
                                bg={palette.shortcut_background} text_size={px(11.0)}
                                font_weight={gpui::FontWeight::BOLD}>
                                {shortcut.to_string()}
                            </div>
                        },
                    )
                    .child(
                        crate::crisp_svg::svg()
                            .path(icon)
                            .size(px(18.0))
                            .flex_shrink_0()
                            .text_color(palette.text),
                    )
                    .child(
                        rsx! {
                            <div text_size={px(13.0)} font_weight={gpui::FontWeight::SEMIBOLD}
                                whitespace_nowrap>
                                {label}
                            </div>
                        },
                    )
                    .on_hover(cx.listener(move |this, hovered, _, cx| {
                        if *hovered {
                            this.hovered_actions.insert(shortcut);
                        } else {
                            this.hovered_actions.remove(&shortcut);
                        }
                        cx.notify();
                    }))
                    .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if !same_presentation(&this.presentation_live, &live)
                            || selected_key(&this.host.qp_state_detail()) != key
                        {
                            return;
                        }
                        this.run_row_action(action.clone(), window, cx)
                    })),
            );
        }
        if self.motion.borrow().any_active(Instant::now()) {
            window.request_animation_frame();
        }
        // `quick-paste-action-chip-enter`: 140ms ease, opacity only.
        if animate {
            bar.with_animation(
                ("qp-action-bar-in", epoch),
                crate::motion::enter(crate::motion::FOCUS, crate::motion::EASE),
                |bar, t| bar.opacity(t),
            )
            .into_any_element()
        } else {
            bar.into_any_element()
        }
    }

    fn detail_preview(&self, item: &Value, width: f32, height: f32) -> AnyElement {
        let t = self.tokens.theme(self.dark);
        let color = crate::view::color;
        // Image previews are their own surface, not a padded card around a card.
        if let Some(image) = self
            .preview
            .as_ref()
            .and_then(|preview| preview.image.as_ref())
        {
            let caption = row_text(item);
            let (image_width, image_height) = preview_model::image_display_size(
                image.width,
                image.height,
                (width - 2.0).max(1.0),
                (height - if caption.is_empty() { 10.0 } else { 24.0 }).max(1.0),
            );
            return rsx! {
                <div flex flex_col items_center py={px(4.0)} />
            }
                .child(native_content::render_resource_image(
                    image.image.clone(),
                    img(image.image.clone())
                        .w(px(image_width + 2.0))
                        .h(px(image_height + 2.0))
                        .object_fit(gpui::ObjectFit::Contain)
                        .rounded(px(8.0))
                        .border_1()
                        .border_color(style::preview_border(t))
                        .bg(color(&t.secondary))
                        .shadow(style::preview_shadows(t))
                        .with_fallback(|| {
                            div()
                                .child(tr("图片加载失败", "Image failed to load"))
                                .into_any_element()
                        })
                        .into_any_element(),
                ))
                .when(!caption.is_empty(), |content| {
                    content.child(
                        rsx! {
                            <div max_w={px(width)} mt={px(2.0)} text_size={px(10.0)}
                                text_color={color(&t.text_muted)} text_ellipsis>
                                {caption}
                            </div>
                        },
                    )
                })
                .into_any_element();
        }
        if let Some(swatch) = self.color_preview(item) {
            return rsx! {
                <div flex items_center gap={px(10.0)} px={px(12.0)} py={px(10.0)}>
                    <div size={px(56.0)} flex_shrink_0 relative rounded={px(10.0)} border_1
                        border_color={color(&t.border_secondary)} bg={swatch_color(swatch)}>
                        {style::swatch_inset(t, 10.0)}
                    </div>
                    <div min_w_0 text_size={px(13.0)} font_family={"Consolas"} text_ellipsis>
                        {row_text(item)}
                    </div>
                </div>
            }
                .into_any_element();
        }
        let image_element = |image: &ContentImage| {
            let (width, height) =
                preview_model::image_display_size(image.width, image.height, width, height);
            native_content::render_resource_image(
                image.image.clone(),
                img(image.image.clone())
                    .w(px(width))
                    .h(px(height))
                    .object_fit(gpui::ObjectFit::Contain)
                    .rounded(px(4.0))
                    .with_fallback(|| {
                        div()
                            .child(tr("图片加载失败", "Image failed to load"))
                            .into_any_element()
                    })
                    .into_any_element(),
            )
        };
        let is_file = item["kind"].as_str() == Some("FileClipItem");
        let mut content = div()
            .flex()
            .flex_col()
            .gap(px(if is_file { 4.0 } else { 8.0 }));
        if let Some(preview) = &self.preview {
            if preview.image_unavailable {
                content = content.child(
                    rsx! {
                        <div text_size={px(11.0)} text_color={color(&t.text_muted)}>
                            {tr("图片加载失败", "Image failed to load")}
                        </div>
                    },
                );
            }
            if let Some(image) = &preview.image {
                content = content.child(image_element(image));
            } else if let Some(html) = &preview.html {
                // The parser bounds rich rendering, not the user's readable
                // body. Keep the existing bounded scroll card for full text.
                content = content.child(
                    if let Some(text) = truncated_html_body(
                        &html.projection,
                        &preview.text,
                        preview.body_text_loaded,
                    ) {
                        self.passive_text(item, text.to_string(), width)
                    } else {
                        html_view::render_html_preview(html, width, height, &self.tokens, self.dark)
                    },
                );
            } else if !preview.segments.is_empty() {
                // QQ/IM segments keep their original order and full detail,
                // including local images and file paths.
                for segment in &preview.segments {
                    content = content.when_some(segment.image.as_ref(), |content, image| {
                        content.child(image_element(image))
                    });
                    if !segment.text.is_empty() {
                        content = content.child(segment.text.clone());
                    }
                }
            } else if let Some(steps) = &preview.steps {
                content = content.child(self.step_detail(steps, &preview.resources));
            } else if item["kind"].as_str() == Some("FileClipItem")
                && !preview.file_paths.is_empty()
            {
                let count = preview.file_paths.len();
                content = content.child(
                    rsx! {
                        <div px={px(12.0)} pt={px(8.0)} pb={px(4.0)} text_size={px(10.0)}
                            font_weight={gpui::FontWeight::SEMIBOLD}
                            text_color={color(&t.text_muted)}>
                            {cliphost_core::trf!("文件 · {count}", "Files · {count}")}
                        </div>
                    },
                );
                let mut paths = rsx! {
                    <div flex flex_col gap={px(4.0)} px={px(8.0)} pt={px(2.0)} pb={px(8.0)} />
                };
                for (index, path) in preview.file_paths.iter().enumerate() {
                    paths = paths.child(
                        rsx! {
                            <div id={("qp-preview-file", index)} px={px(8.0)} py={px(6.0)}
                                rounded={px(8.0)} bg={color(&t.secondary)} text_size={px(12.0)}
                                line_height={gpui::relative(1.35)} text_ellipsis>
                                {path.rsplit(['\\', '/']).next().unwrap_or(path).to_string()}
                            </div>
                        },
                    );
                }
                content = content.child(paths);
            } else {
                content = content.child(self.passive_text(item, preview.text.clone(), width));
            }
        } else {
            content =
                content.child(self.passive_text(item, detail_text(&Value::Null, item), width));
            if self.preview_task.is_some() {
                content = content.child(
                    rsx! {
                        <div text_size={px(11.0)} text_color={color(&t.text_muted)}>
                            {tr("加载完整内容…", "Loading full content…")}
                        </div>
                    },
                );
            }
        }
        if let Some(error) = &self.preview_error {
            content = content.child(
                rsx! {
                    <div text_size={px(11.0)} text_color={color(&t.error)}>
                        {error.clone()}
                    </div>
                },
            );
        }
        content.into_any_element()
    }

    fn passive_text(&self, item: &Value, text: String, width: f32) -> AnyElement {
        // This shared renderer supplies inert color swatches and continuous
        // text wrapping. QP exposes only the already configured action chips.
        let max_lines = text.chars().count().max(1);
        let projection = native_content::model::TextProjection {
            text,
            links: Vec::new(),
            hit_ranges: Vec::new(),
        };
        native_content::render_text_content(
            item["id"].as_str().unwrap_or("qp"),
            native_content::ContentRole::Primary,
            &projection,
            native_content::ContentMetrics {
                width,
                font_size: 12.0,
                line_height: 1.45,
                max_lines,
            },
            &self.tokens,
            self.dark,
            std::rc::Rc::new(|_, _, _| {}),
        )
    }

    fn color_preview(&self, item: &Value) -> Option<html::HtmlColor> {
        color_preview_swatch(item, self.preview.as_ref(), self.preview_error.as_deref())
    }

    fn step_detail(&self, steps: &StepProjection, resources: &ContentResources) -> AnyElement {
        let theme = self.tokens.theme(self.dark);
        let color = crate::view::color;
        let mut lines = div().flex().flex_col().gap(px(5.0));
        lines = lines.child(
            rsx! {
                <div text_size={px(11.0)} text_color={color(&theme.text_muted)}>
                    {tr("步骤摘要", "Step summary")}
                </div>
            },
        );
        for line in &steps.lines {
            let mut row = div()
                .flex()
                .items_start()
                .gap(px(6.0))
                .when(line.disabled, |row| row.opacity(0.55));
            row = if let Some(icon) = resources.icons.get(&line.icon).and_then(Option::as_ref) {
                row.child(native_content::render_resource_image(
                    icon.clone(),
                    img(icon.clone())
                        .size(px(16.0))
                        .flex_shrink_0()
                        .object_fit(gpui::ObjectFit::Contain)
                        .into_any_element(),
                ))
            } else {
                row.child(
                    crate::crisp_svg::svg()
                        .path(self.tokens.icons["list"].clone())
                        .size(px(16.0))
                        .flex_shrink_0()
                        .text_color(color(&theme.text)),
                )
            };
            let text = format!("{}{}", line.title, line.suffix);
            let mut spans = vec![(
                0..line.title.len(),
                gpui::HighlightStyle {
                    font_weight: Some(gpui::FontWeight::SEMIBOLD),
                    ..Default::default()
                },
            )];
            if !line.suffix.is_empty() {
                spans.push((
                    line.title.len()..text.len(),
                    gpui::HighlightStyle {
                        color: Some(color(&theme.text_muted).into()),
                        ..Default::default()
                    },
                ));
            }
            lines = lines.child(
                row.child(
                    rsx! {
                        <div min_w_0 flex_1>
                            {gpui::StyledText::new(text).with_highlights(spans)}
                        </div>
                    },
                ),
            );
        }
        if steps.lines.is_empty() {
            lines = lines.child(steps.preview_text.clone());
        }
        if !steps.stats.is_empty() {
            lines = lines.child(
                rsx! {
                    <div text_size={px(11.0)} text_color={color(&theme.text_muted)}>
                        {steps.stats.clone()}
                    </div>
                },
            );
        }
        lines.into_any_element()
    }
}

fn row_text(item: &Value) -> String {
    ["displayText", "summary", "title"]
        .into_iter()
        .filter_map(|field| item[field].as_str())
        .find(|text| !text.is_empty())
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn pure_row_color(item: &Value) -> Option<html::HtmlColor> {
    let text = row_text(item);
    (!text
        .chars()
        .any(|ch| ch.is_whitespace() || ch == '\u{feff}'))
    .then(|| preview_model::pure_color(&text))
    .flatten()
}

/// CSS border-box height, including the 4px gap between floating row cards.
/// Resource loads reserve exactly the same space as their eventual thumbnail.
fn row_card_height(item: &Value) -> f32 {
    if preview_model::row_image_source(item).is_some() {
        58.0
    } else if preview_model::file_lead(item).is_some() {
        54.0
    } else if pure_row_color(item).is_some() {
        48.0
    } else {
        40.0
    }
}

fn color_preview_swatch(
    item: &Value,
    preview: Option<&LoadedPreview>,
    error: Option<&str>,
) -> Option<html::HtmlColor> {
    let preview = preview?;
    (error.is_none()
        && preview.image.is_none()
        && !preview.image_unavailable
        && preview.html.is_none()
        && preview.segments.is_empty()
        && preview.steps.is_none())
    .then(|| pure_row_color(item))
    .flatten()
}

fn visible_rows_start(count: usize, active: usize) -> usize {
    active
        .saturating_sub(2)
        .min(count.saturating_sub(count.min(8)))
}

fn visible_rows_height(items: &[Value], active: usize) -> f32 {
    let count = items.len().min(8);
    let start = visible_rows_start(items.len(), active);
    items[start..start + count]
        .iter()
        .map(|item| row_card_height(item) + 4.0)
        .sum()
}

fn swatch_color(color: html::HtmlColor) -> gpui::Rgba {
    gpui::rgba(u32::from_be_bytes([
        color.red,
        color.green,
        color.blue,
        color.alpha,
    ]))
}

/// Keep unchanged thumbnails across sessions, but never treat an unchanged ID
/// or image path as a content revision. Fixtures use their full small item.
fn row_snapshot(state: &Value, foreground: &str) -> Value {
    let items: serde_json::Map<String, Value> = state["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            Some((item["id"].as_str()?.to_string(),
                item.get("contentRevision").cloned().unwrap_or_else(|| item.clone())))
        })
        .collect();
    json!({ "foreground": foreground, "items": items })
}

fn row_resource_unchanged(previous: &Value, next: &Value, id: &str) -> bool {
    previous["foreground"] == next["foreground"]
        && next["items"].get(id).is_some_and(|revision| previous["items"].get(id) == Some(revision))
}

fn row_resources_live(state: &Value, live: &Arc<AtomicBool>) -> bool {
    state["visible"].as_bool() == Some(true) && live.load(Ordering::Acquire)
}

fn same_presentation(current: &Arc<AtomicBool>, accepted: &Arc<AtomicBool>) -> bool {
    Arc::ptr_eq(current, accepted) && accepted.load(Ordering::Acquire)
}

impl Render for QuickPasteOverlay {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::content_cache_owners::register(cx, |this, cx| {
            if !row_resources_live(&this.state, &this.presentation_live) {
                this.row_cache.set_visible_items(std::iter::empty::<&str>());
            }
            let changed = this.row_cache.apply_budget_pressure();
            this.row_cache.release_evicted(cx);
            if changed { cx.notify(); }
        });
        window.set_window_title(tr(
            "ClipHost GPUI · 便捷粘贴",
            "ClipHost GPUI · Quick paste",
        ));
        // `hide_cleared` paints this into the hidden HWND, so a later show can
        // never compose the previous session's layout at the new rect.
        #[cfg(debug_assertions)]
        let retained = self.design_only;
        #[cfg(not(debug_assertions))]
        let retained = false;
        if !retained && self.state["visible"].as_bool() != Some(true) {
            self.row_cache.set_visible_items(std::iter::empty::<&str>());
            return div().size_full();
        }
        let theme = self.tokens.theme(self.dark);
        let color = crate::view::color;
        let scale = window.scale_factor() as f64;
        let number =
            |name: &str, fallback: f64| self.state[name].as_f64().unwrap_or(fallback) / scale;
        let x = number("clusterOffsetLeftPx", 8.0);
        let y = number("clusterOffsetTopPx", 8.0);
        let panel_width = number("panelWidthPx", 380.0).max(100.0);
        // The core reserves the HWND, not CSS row heights. Use its available
        // client area so eight 58px image rows can fit in the 560px window.
        let available_height = (f32::from(window.viewport_size().height) - y as f32 - 8.0).max(1.0);
        let panel_height = f64::from(available_height);
        let preview_width = number("previewWidthPx", 0.0).max(0.0);
        let layout = cluster_layout(
            x,
            panel_width,
            preview_width,
            8.0,
            self.state["previewOnLeft"].as_bool() == Some(true),
        );
        let hover_cancel = self.state["hoverCancel"].as_bool() == Some(true);
        let fifo_banner = fifo_banner(&self.state);
        let hint = overlay_hint(&self.state);
        let active = self.state["activeIndex"].as_u64().unwrap_or(0) as usize;
        let items = self.state["items"].as_array().cloned().unwrap_or_default();
        self.row_cache.set_visible_items(items.iter().filter_map(|item| item["id"].as_str()));
        let selected = items.get(active).cloned().unwrap_or_default();
        let has_preview = preview_width > 0.0 && !items.is_empty();
        let has_actions = !available_actions(&selected, &self.state).is_empty();
        if !has_actions {
            self.action_bar_epoch.observe(false);
            self.hovered_actions.clear();
        }
        let image_preview = self
            .preview
            .as_ref()
            .is_some_and(|preview| preview.image.is_some());
        // An image item whose bitmap is still loading must not flash the
        // bordered text card first; reserve an empty, chrome-less body.
        let image_loading = !image_preview
            && self.preview_error.is_none()
            && self.preview.is_none()
            && preview_model::image_source(&selected).is_some();
        let image_preview = image_preview || image_loading;
        let color_preview = self.color_preview(&selected).is_some();
        let file_preview = !image_preview && selected["kind"].as_str() == Some("FileClipItem");
        let preview_padding = if image_preview {
            0.0
        } else if color_preview || file_preview {
            2.0
        } else {
            22.0
        };
        let preview_max_height = available_height
            .min(f32::from(window.viewport_size().height) * 0.7)
            .min(520.0);
        // CSS action bar: 40px buttons + 12px padding, then the 4px sibling gap.
        let preview_body_height = if image_preview {
            // The frontend passes the 70vh/520px budget to the image itself;
            // its detached actions and caption sit outside that image budget.
            preview_max_height + 24.0
        } else {
            (preview_max_height - if has_actions { 56.0 } else { 0.0 }).max(1.0)
        };
        let animated_preview_offset = self.motion.borrow_mut().value(
            ("preview-offset".to_string(), MotionTrack::PreviewOffset),
            self.preview_offset_y,
            crate::motion::FOCUS,
            crate::motion::EASE,
            Instant::now(),
        );
        if self.motion.borrow().any_active(Instant::now()) {
            window.request_animation_frame();
        }
        let text_preview_width = (preview_width as f32).min(400.0);
        let measure_text = |text: &str, font_size: f32, family: &str| {
            let font = gpui::font(family.to_string());
            text.lines()
                .map(|line| {
                    let run = gpui::TextRun {
                        len: line.len(),
                        font: font.clone(),
                        color: color(&theme.text).into(),
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    };
                    f32::from(
                        window
                            .text_system()
                            .shape_line(line.to_string().into(), px(font_size), &[run], None)
                            .width,
                    )
                })
                .fold(0.0f32, f32::max)
        };
        let layout_frame = Rc::new(RefCell::new(placement::Frame::default()));
        let layout_deferred = Rc::new(Cell::new(false));
        let layout_identity = placement_identity(&self.state, self.source_hwnd);
        let layout_live = self.presentation_live.clone();
        let layout_viewport = [
            f32::from(window.viewport_size().width),
            f32::from(window.viewport_size().height),
        ];
        let layout_entity = cx.weak_entity();
        #[cfg(debug_assertions)]
        let design_only = self.design_only;
        let observe: Rc<dyn Fn(placement::Part, AnyElement) -> AnyElement> =
            Rc::new(move |part, child| {
                let frame = layout_frame.clone();
                let deferred = layout_deferred.clone();
                let identity = layout_identity.clone();
                let live = layout_live.clone();
                let entity = layout_entity.clone();
                placement::observe(
                    child,
                    Rc::new(move |rect, window, cx| {
                        #[cfg(debug_assertions)]
                        if design_only {
                            frame.borrow_mut().record(part, rect);
                            if !deferred.replace(true) {
                                let (frame, deferred, entity) =
                                    (frame.clone(), deferred.clone(), entity.clone());
                                cx.defer(move |cx| {
                                    deferred.set(false);
                                    if let Some(offset) = frame.borrow().preview_offset(has_actions)
                                    {
                                        let _ = entity.update(cx, |this, cx| {
                                            if this.design_only
                                                && (offset - this.preview_offset_y).abs() > 0.5
                                            {
                                                this.preview_offset_y = offset;
                                                cx.notify();
                                            }
                                        });
                                    }
                                });
                            }
                            return;
                        }
                        let Some(identity) = identity.as_ref() else {
                            return;
                        };
                        #[cfg(windows)]
                        if platform::hwnd(window).ok().map(|hwnd| hwnd.0 as isize)
                            != Some(identity.source_hwnd)
                        {
                            return;
                        }
                        if !live.load(Ordering::Acquire)
                            || window.scale_factor() as f64 != scale
                            || [
                                f32::from(window.viewport_size().width),
                                f32::from(window.viewport_size().height),
                            ] != layout_viewport
                        {
                            return;
                        }
                        frame.borrow_mut().record(part, rect);
                        if deferred.replace(true) {
                            return;
                        }
                        let (frame, deferred, identity, live, entity) = (
                            frame.clone(),
                            deferred.clone(),
                            identity.clone(),
                            live.clone(),
                            entity.clone(),
                        );
                        cx.defer(move |cx| {
                            deferred.set(false);
                            let report = PlacementReport {
                                identity,
                                frame: frame.borrow().clone(),
                                has_preview,
                                has_actions,
                                viewport: layout_viewport,
                                scale,
                            };
                            let _ = entity
                                .update(cx, |this, cx| this.accept_placement(report, &live, cx));
                        });
                    }),
                )
            });
        // Keep the host's bounded cluster/anchor, but let the visible content
        // card hug small images and short text as in the Web preview. Measuring
        // is bounded to a short plain-text body; rich/long content keeps the cap.
        let preview_card_width =
            if let Some(image) = self.preview.as_ref().and_then(|p| p.image.as_ref()) {
                preview_model::image_display_size(
                    image.width,
                    image.height,
                    (preview_width as f32 - 2.0).max(1.0),
                    (preview_body_height - 24.0).max(1.0),
                )
                .0 + 2.0
            } else if color_preview {
                (measure_text(&row_text(&selected), 13.0, "Consolas") + 56.0 + 10.0 + 24.0 + 2.0)
                    .min(220.0)
                    .min(preview_width as f32)
            } else if file_preview {
                let paths = self
                    .preview
                    .as_ref()
                    .map(|p| p.file_paths.clone())
                    .unwrap_or_else(|| {
                        selected["filePaths"]
                            .as_array()
                            .map(|paths| {
                                paths
                                    .iter()
                                    .filter_map(Value::as_str)
                                    .map(str::to_string)
                                    .collect()
                            })
                            .unwrap_or_default()
                    });
                let width = paths
                    .iter()
                    .map(|path| {
                        measure_text(
                            path.rsplit(['\\', '/']).next().unwrap_or(path),
                            12.0,
                            &self.tokens.typography.font_family,
                        ) + 34.0
                    })
                    .fold(0.0f32, f32::max);
                let count = selected["fileCount"].as_u64().unwrap_or(paths.len() as u64);
                let label = cliphost_core::trf!("文件 · {count}", "Files · {count}");
                width
                    .max(measure_text(&label, 10.0, &self.tokens.typography.font_family) + 26.0)
                    .min(text_preview_width)
            } else {
                let plain = self
                    .preview
                    .as_ref()
                    .is_none_or(|p| p.html.is_none() && p.segments.is_empty() && p.steps.is_none());
                let fallback_text = if self.preview.is_none() {
                    detail_text(&Value::Null, &selected)
                } else {
                    String::new()
                };
                let text = self
                    .preview
                    .as_ref()
                    .map(|p| p.text.as_str())
                    .unwrap_or(&fallback_text);
                if plain
                    && !matches!(
                        selected["kind"].as_str(),
                        Some("FileClipItem" | "StepClipItem")
                    )
                    && preview_model::image_source(&selected).is_none()
                    && text.chars().take(161).count() <= 160
                {
                    let font = gpui::font(self.tokens.typography.font_family.clone());
                    let width = text
                        .lines()
                        .map(|line| {
                            let run = gpui::TextRun {
                                len: line.len(),
                                font: font.clone(),
                                color: color(&theme.text).into(),
                                background_color: None,
                                underline: None,
                                strikethrough: None,
                            };
                            f32::from(
                                window
                                    .text_system()
                                    .shape_line(line.to_owned().into(), px(12.0), &[run], None)
                                    .width,
                            ) + preview_model::inline_colors(line).len() as f32 * 16.0
                        })
                        .fold(0.0f32, f32::max);
                    (width + 22.0).min(text_preview_width)
                } else {
                    text_preview_width
                }
            };
        let weak = cx.weak_entity();
        let observe_row = observe.clone();
        let rows = gpui::list(self.list.clone(), move |index, window, cx| {
            weak.update(cx, |this, cx| {
                let item = this.state["items"][index].clone();
                let active = this.state["activeIndex"].as_u64().unwrap_or(0) as usize == index;
                let progress = this.motion.borrow_mut().value(
                    (
                        item["id"].as_str().unwrap_or_default().to_string(),
                        MotionTrack::Row,
                    ),
                    if active { 1.0 } else { 0.0 },
                    crate::motion::FOCUS,
                    crate::motion::EASE,
                    Instant::now(),
                );
                let lead = this.row_lead(&item, progress, cx);
                let with_lead = lead.is_some();
                let theme = this.tokens.theme(this.dark);
                let pinned: Vec<bool> = this.state["items"]
                    .as_array()
                    .map(|items| {
                        items
                            .iter()
                            .map(|item| item["isTop"].as_bool() == Some(true))
                            .collect()
                    })
                    .unwrap_or_default();
                let (number, is_pinned) = display_index(&pinned, index);
                let palette = style::row_colors(theme, progress, is_pinned);
                let text = row_text(&item);
                let file_count = (item["kind"].as_str() == Some("FileClipItem"))
                    .then(|| item["fileCount"].as_u64().unwrap_or(0))
                    .filter(|count| *count > 1);
                let lead_width = if preview_model::row_image_source(&item).is_some() {
                    44.0
                } else if preview_model::file_lead(&item).is_some() {
                    40.0
                } else if pure_row_color(&item).is_some() {
                    26.0
                } else {
                    0.0
                };
                // A definite width reaches GPUI's text measurer during the
                // first layout pass, so long single-line text gets an ellipsis.
                let text_width = (panel_width as f32
                    - 22.0
                    - 24.0
                    - if with_lead { 20.0 + lead_width } else { 8.0 })
                .max(1.0);
                let count_label = file_count
                    .map(|count| cliphost_core::trf!(" ({count} 个文件)", " ({count} files)"));
                let count_width = count_label.as_ref().map_or(0.0, |label| {
                    let mut font = gpui::font(this.tokens.typography.font_family.clone());
                    font.weight = if active {
                        gpui::FontWeight::SEMIBOLD
                    } else {
                        gpui::FontWeight::MEDIUM
                    };
                    let run = gpui::TextRun {
                        len: label.len(),
                        font,
                        color: color(&theme.text).into(),
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    };
                    f32::from(
                        window
                            .text_system()
                            .shape_line(label.to_owned().into(), px(12.0), &[run], None)
                            .width,
                    )
                    .ceil()
                });
                let mut text_font = gpui::font(this.tokens.typography.font_family.clone());
                text_font.weight = if active {
                    gpui::FontWeight::SEMIBOLD
                } else {
                    gpui::FontWeight::MEDIUM
                };
                let fallback = text.is_empty();
                let text: gpui::SharedString = if fallback {
                    preview_model::row_fallback_label(&item).into()
                } else {
                    text.into()
                };
                let mut text_runs = vec![gpui::TextRun {
                    len: text.len(),
                    font: text_font.clone(),
                    color: color(&theme.text).into(),
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                }];
                let text_limit = (text_width - count_width).max(1.0);
                let text = window
                    .text_system()
                    .line_wrapper(text_font, px(14.0))
                    .truncate_line(text, px(text_limit), "…", &mut text_runs);
                let main_width = f32::from(
                    window
                        .text_system()
                        .shape_line(text.clone(), px(14.0), &text_runs, None)
                        .width,
                )
                .ceil()
                .min(text_limit);
                let row = div()
                    .relative()
                    .flex()
                    .items_center()
                    .gap(px(if with_lead { 10.0 } else { 8.0 }))
                    .min_h(px(if with_lead { 48.0 } else { 40.0 }))
                    .px(px(10.0))
                    .py(px(6.0))
                    .rounded(px(10.0))
                    .border_1()
                    .border_color(gpui::transparent_black())
                    .shadow(style::row_shadows(theme, progress))
                    .child(style::row_surface(theme, progress))
                    .child(
                        rsx! {
                            <div size={px(24.0)} relative flex_shrink_0 flex items_center
                                justify_center rounded={px(6.0)} bg={palette.index_background}
                                text_size={px(12.0)} font_weight={gpui::FontWeight::BOLD}
                                text_color={palette.index_text} />
                        }
                            .when(is_pinned, |badge| {
                                badge.child(style::pinned_index_ring(theme, progress))
                            })
                            .child(number.to_string()),
                    )
                    .when_some(lead, |row, lead| row.child(lead))
                    .child(
                        rsx! {
                            <div w={px(text_width)} min_w_0 flex items_center
                                text_size={px(14.0)} line_height={gpui::relative(1.3)} />
                        }
                            .font_weight(if active {
                                gpui::FontWeight::SEMIBOLD
                            } else {
                                gpui::FontWeight::MEDIUM
                            })
                            .child(rsx! {
                                <div w={px(main_width)} truncate>
                                    {text}
                                </div>
                            }
                                .when(fallback, |label| {
                                    label.text_color(color(&theme.text_muted))
                                }))
                            .when_some(count_label, |text, label| {
                                text.child(
                                    rsx! {
                                        <div flex_shrink_0 whitespace_nowrap text_size={px(12.0)}
                                            opacity={0.82}>
                                            {label}
                                        </div>
                                    },
                                )
                            }),
                    );
                if this.motion.borrow().any_active(Instant::now()) {
                    window.request_animation_frame();
                }
                let row = if active {
                    observe_row(placement::Part::ActiveRow, row.into_any_element())
                } else {
                    row.into_any_element()
                };
                // The native virtual list clips at its bounds. Leave 1px for
                // the outside ring while preserving the CSS's 4px row gap.
                rsx! {
                    <div px={px(1.0)} pt={px(1.0)} pb={px(3.0)}>
                        {row}
                    </div>
                }
                    .into_any_element()
            })
            .unwrap_or_else(|_| div().into_any_element())
        })
        .w(px(panel_width as f32 + 2.0))
        .ml(px(-1.0))
        .h(px(visible_rows_height(&items, active)))
        .min_h_0();
        // A short work area or FIFO banner can shrink the eight-row viewport.
        // Reveal only after the new layout has painted, using its real bounds.
        let list_entity = cx.weak_entity();
        let list_live = self.presentation_live.clone();
        let list_key = selected_key(&self.state);
        let desired_rows_height = visible_rows_height(&items, active);
        let rows = placement::observe(
            rows.into_any_element(),
            Rc::new(move |rect, _, cx| {
                if rect.height + 0.5 >= desired_rows_height {
                    return;
                }
                let (entity, live, key) =
                    (list_entity.clone(), list_live.clone(), list_key.clone());
                cx.defer(move |cx| {
                    let _ = entity.update(cx, |this, cx| {
                        if !same_presentation(&this.presentation_live, &live)
                            || selected_key(&this.state) != key
                        {
                            return;
                        }
                        let before = this.list.logical_scroll_top();
                        this.list.scroll_to_reveal_item(active);
                        let after = this.list.logical_scroll_top();
                        if after.item_ix != before.item_ix
                            || after.offset_in_item != before.offset_in_item
                        {
                            cx.notify();
                        }
                    });
                });
            }),
        );
        rsx! {
            <div size_full relative bg={gpui::transparent_black()}
                font_family={self.tokens.typography.font_family.clone()} text_size={px(13.0)}
                text_color={color(&theme.text)} />
        }
            .child(observe(
                placement::Part::Panel,
                rsx! {
                    <div absolute left={px(layout.list_left as f32)} top={px(y as f32)}
                        w={px(panel_width as f32)} max_h={px(panel_height as f32)} flex flex_col />
                }
                    .when_some(fifo_banner, |panel, banner| {
                        panel.child(
                            rsx! {
                                <div mx={px(4.0)} mb={px(6.0)} px={px(6.0)} py={px(4.0)}
                                    rounded={px(5.0)}
                                    bg={crate::panel::css_mix(
                                    color(&theme.surface),
                                    color(&theme.accent),
                                    0.18,
                                )}
                                    text_center text_size={px(11.0)}
                                    font_weight={gpui::FontWeight::SEMIBOLD}>
                                    {banner}
                                </div>
                            },
                        )
                    })
                    .when(items.is_empty(), |panel| {
                        panel.child(
                            rsx! {
                                <div px={px(6.0)} py={px(10.0)} text_center text_size={px(12.0)}
                                    text_color={color(&theme.text_muted)}>
                                    {tr("暂无记录", "No clipboard records")}
                                </div>
                            },
                        )
                    })
                    .child(
                        rsx! {
                            <div min_h_0 flex flex_col relative />
                        }
                            .when(hover_cancel, |list| {
                                list.child(style::cancel_outline(theme))
                            })
                            .child(rows),
                    )
                    .when(preview_width <= 0.0 && !items.is_empty(), |panel| {
                        panel.child(
                            rsx! {
                                <div px={px(10.0)} py={px(5.0)}>
                                    {self.action_bar(&selected, window, cx)}
                                </div>
                            },
                        )
                    })
                    .child(
                        rsx! {
                            <div mt={px(2.0)} px={px(4.0)} flex_shrink_0 text_center
                                text_color={color(&theme.text_muted)} text_size={px(11.0)}
                                opacity={0.85}>
                                {hint}
                            </div>
                        },
                    )
                    .when_some(self.action_error.clone(), |panel, error| {
                        panel.child(
                            rsx! {
                                <div px={px(12.0)} py={px(5.0)} text_color={color(&theme.error)}
                                    text_size={px(11.0)}>
                                    {error}
                                </div>
                            },
                        )
                    })
                    .into_any_element(),
            ))
            .when(preview_width > 0.0 && !items.is_empty(), |element| {
                element.child(
                    rsx! {
                        <div absolute left={px(layout.preview_left as f32)}
                            top={px(y as f32 + animated_preview_offset)}
                            w={px(preview_width as f32)} max_h={px(preview_max_height)} />
                    }
                        .id("qp-detail-preview")
                        .flex()
                        .flex_col()
                        .gap(px(4.0))
                        .when(
                            self.state["previewOnLeft"].as_bool() == Some(true),
                            |preview| preview.items_end(),
                        )
                        .when(
                            self.state["previewOnLeft"].as_bool() != Some(true),
                            |preview| preview.items_start(),
                        )
                        // Actions are a sibling of the scrolling card, so they
                        // remain available while the full body is scrolled.
                        .when(has_actions, |preview| {
                            preview.child(observe(
                                placement::Part::Actions,
                                rsx! {
                                    <div max_w_full flex_shrink_0>
                                        {self.action_bar(&selected, window, cx)}
                                    </div>
                                }
                                    .into_any_element(),
                            ))
                        })
                        .child(observe(
                            placement::Part::Body,
                            rsx! {
                                <div id={"qp-detail-preview-body"}
                                    w={px(preview_card_width.max(1.0))} max_w_full
                                    max_h={px(preview_body_height)} min_h_0 />
                            }
                                .when(!image_preview, |body| body.overflow_y_scroll())
                                .when(!image_preview, |body| {
                                    body.rounded(px(12.0))
                                        .bg(color(&theme.surface))
                                        .border_1()
                                        .border_color(style::preview_border(theme))
                                        .shadow(style::preview_shadows(theme))
                                })
                                .when(!image_preview && !color_preview && !file_preview, |body| {
                                    body.p(px(10.0))
                                })
                                .when(!image_loading, |body| {
                                    body.child(self.detail_preview(
                                        &selected,
                                        (preview_card_width - preview_padding).max(1.0),
                                        (preview_body_height - preview_padding).max(1.0),
                                    ))
                                })
                                .into_any_element(),
                        )),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn color_card_does_not_replace_loaded_rich_content_or_hide_errors() {
        let item = json!({"displayText":"#abc"});
        let mut preview = LoadedPreview {
            text: "#abc".into(),
            body_text_loaded: true,
            file_paths: Vec::new(),
            image: None,
            image_unavailable: false,
            segments: Vec::new(),
            html: None,
            steps: None,
            resources: ContentResources::default(),
            _image_holder: native_content::ContentImageHolderLease::new(Vec::new()),
        };
        assert!(color_preview_swatch(&item, None, None).is_none());
        assert!(color_preview_swatch(&item, Some(&preview), None).is_some());
        assert!(color_preview_swatch(&item, Some(&preview), Some("failed")).is_none());
        preview.segments.push(RichSegment {
            text: "rich body".into(),
            image: None,
        });
        assert!(color_preview_swatch(&item, Some(&preview), None).is_none());
        preview.segments.clear();
        preview.image_unavailable = true;
        assert!(color_preview_swatch(&item, Some(&preview), None).is_none());
    }

    #[test]
    fn floating_row_height_reserves_loaded_thumbnails_and_eight_row_window() {
        let image = json!({"id":"image","kind":"ImageClipItem"});
        let file = json!({"kind":"FileClipItem","filePaths":["C:\\notes.txt"]});
        let color = json!({"displayText":"#abcdef"});
        let text = json!({"displayText":"plain"});
        assert_eq!(row_card_height(&image), 58.0);
        assert_eq!(row_card_height(&file), 54.0);
        assert_eq!(row_card_height(&color), 48.0);
        assert_eq!(row_card_height(&text), 40.0);
        assert_eq!(visible_rows_height(&vec![image; 12], 8), 496.0);
        assert_eq!(visible_rows_height(&[text.clone()], 0), 44.0);
        assert_eq!(visible_rows_height(&[], 0), 0.0);
        let mut mixed = vec![text; 12];
        mixed[11] = file;
        assert_eq!(visible_rows_height(&mixed, 0), 352.0);
        assert_eq!(visible_rows_height(&mixed, usize::MAX), 366.0);
        assert_eq!(visible_rows_start(12, 6), 4);
        assert_eq!(visible_rows_start(12, 11), 4);
    }

    use super::*;

    #[test]
    fn truncated_html_uses_the_complete_loaded_body_including_its_tail() {
        let body = format!("{}\nend of the article", "汉".repeat(16_010));
        let source = format!("<p>{body}</p>");
        let projection = html::parse_html_preview(&source);
        assert!(projection.truncated);
        let shown = truncated_html_body(&projection, &body, true).unwrap();
        assert_eq!(shown, body);
        assert!(shown.ends_with("end of the article"));
    }

    #[test]
    fn complete_or_bodyless_html_keeps_its_rich_preview() {
        let complete = html::parse_html_preview("<p>complete body</p>");
        assert!(truncated_html_body(&complete, "complete body", true).is_none());
        let limits = html::HtmlPreviewLimits {
            max_text_chars: 5,
            ..Default::default()
        };
        let truncated = html::parse_html_preview_with_limits("<p>long body</p>", &limits);
        assert!(truncated.truncated);
        assert!(truncated_html_body(&truncated, "summary only", false).is_none());
        assert!(truncated_html_body(&truncated, "\n \t", true).is_none());
    }

    #[test]
    fn painted_placement_retires_with_hidden_replaced_or_changed_source() {
        let state = json!({"visible":true,"sessionRevision":10,"stateRevision":20});
        let accepted = placement_identity(&state, 100).unwrap();
        let mut next = state.clone();
        next["visible"] = json!(false);
        assert!(placement_identity(&next, 100).is_none());
        for field in ["sessionRevision", "stateRevision"] {
            next = state.clone();
            next[field] = json!(21);
            assert_ne!(placement_identity(&next, 100).unwrap(), accepted);
        }
        assert_ne!(placement_identity(&state, 101).unwrap(), accepted);
        assert!(placement_identity(&state, 0).is_none());
        next = state.clone();
        next["stateRevision"] = json!(0);
        assert!(placement_identity(&next, 100).is_none());
    }

    #[test]
    fn placement_waits_for_painted_preview_and_reports_dips_with_separate_scale() {
        let state = json!({"visible":true,"sessionRevision":10,"stateRevision":20});
        let mut report = PlacementReport {
            identity: placement_identity(&state, 100).unwrap(),
            frame: placement::Frame::default(),
            has_preview: true,
            has_actions: true,
            viewport: [1000.0, 800.0],
            scale: 1.5,
        };
        report.frame.record(
            placement::Part::Panel,
            placement::Rect {
                left: 220.0,
                top: 16.0,
                width: 280.0,
                height: 300.0,
            },
        );
        report.frame.record(
            placement::Part::Body,
            placement::Rect {
                left: 508.0,
                top: 44.0,
                width: 72.0,
                height: 50.0,
            },
        );
        assert!(report.body().is_none());
        report.frame.record(
            placement::Part::Actions,
            placement::Rect {
                left: 508.0,
                top: 16.0,
                width: 128.0,
                height: 24.0,
            },
        );
        let body = report.body().unwrap();
        assert_eq!(body["action"], "placementReady");
        assert_eq!(body["stateRevision"], 20);
        assert_eq!(body["sessionRevision"], 10);
        assert_eq!(body["sourceHwnd"], 100);
        assert_eq!(body["clusterLeft"], 220.0);
        assert_eq!(body["clusterWidth"], 416.0);
        assert_eq!(body["viewportWidth"], 1000.0);
        assert_eq!(body["devicePixelRatio"], 1.5);
        report.viewport[0] = 600.0;
        assert_eq!(report.body().unwrap()["clusterWidth"], 380.0);
        report.viewport[0] = 200.0;
        assert!(report.body().is_none());
        report.viewport[0] = 1000.0;
        report.scale = f64::NAN;
        assert!(report.body().is_none());
        report.scale = 1.5;
        report.viewport[0] = 0.0;
        assert!(report.body().is_none());
    }

    #[test]
    fn hidden_reopened_surface_cannot_revive_an_old_action_reply() {
        let old = Arc::new(AtomicBool::new(true));
        let accepted = old.clone();
        assert!(same_presentation(&old, &accepted));
        old.store(false, Ordering::Release);
        let reopened = Arc::new(AtomicBool::new(true));
        assert!(!same_presentation(&reopened, &accepted));
        assert!(!same_presentation(&old, &accepted));
    }

    #[test]
    fn replacement_token_rejects_old_callbacks_even_if_the_old_token_is_live() {
        let old = Arc::new(AtomicBool::new(true));
        let replacement = Arc::new(AtomicBool::new(true));
        assert!(old.load(Ordering::Acquire));
        assert!(!same_presentation(&replacement, &old));
    }

    #[test]
    fn left_preview_offsets_list_inside_the_same_bounded_cluster() {
        assert_eq!(
            cluster_layout(8.0, 300.0, 240.0, 8.0, false),
            ClusterLayout {
                list_left: 8.0,
                preview_left: 316.0
            }
        );
        assert_eq!(
            cluster_layout(8.0, 300.0, 240.0, 8.0, true),
            ClusterLayout {
                list_left: 256.0,
                preview_left: 8.0
            }
        );
        assert_eq!(cluster_layout(8.0, 300.0, 0.0, 8.0, true).list_left, 8.0);
    }

    #[test]
    fn detail_keys_retire_with_hidden_or_replaced_core_sessions() {
        let state = json!({ "visible": true, "sessionRevision": 10, "stateRevision": 20, "activeIndex": 0, "items": [{ "id": "old" }, { "id": "new" }] });
        let first = selected_key(&state).unwrap();
        let mut hidden = state.clone();
        hidden["visible"] = json!(false);
        assert!(selected_key(&hidden).is_none());
        let mut reopened = state.clone();
        reopened["sessionRevision"] = json!(11);
        assert_ne!(selected_key(&reopened).unwrap(), first);
        let mut next = state.clone();
        next["activeIndex"] = json!(1);
        assert_ne!(selected_key(&next).unwrap(), first);
    }

    #[test]
    fn full_text_and_core_step_projection_take_precedence_over_list_summary() {
        assert_eq!(
            detail_text(
                &json!({ "bodyText": "full\ncontent" }),
                &json!({ "displayText": "short..." })
            ),
            "full\ncontent"
        );
        assert_eq!(
            detail_text(
                &json!({ "bodyText": "serialized step JSON" }),
                &json!({ "kind": "StepClipItem", "stepPresentation": { "previewText": "Step 1\nStep 2" } })
            ),
            "Step 1\nStep 2"
        );
    }

    #[test]
    fn fifo_banner_requires_current_armed_state() {
        assert!(fifo_banner(&json!({ "fifoArmed": false, "fifoBanner": "old" })).is_none());
        assert_eq!(
            fifo_banner(&json!({ "fifoArmed": true, "fifoBanner": "current" })).unwrap(),
            "current"
        );
        assert!(fifo_banner(
            &json!({ "fifoArmed": true, "fifoRemainingCount": 2, "fifoTotalCount": 3 })
        )
        .unwrap()
        .contains("2 / 3"));
    }

    #[test]
    fn hint_omits_disabled_shortcuts_and_keeps_mouse_cancel_semantics() {
        let hint = overlay_hint(
            &json!({ "enableDeleteShortcut": false, "enablePasteEnterShortcut": false, "enableFifoShortcut": false }),
        );
        assert!(!hint.contains("D ") && !hint.contains("E ") && !hint.contains("F "));
        assert!(hint.contains("Q "));
        assert!(overlay_hint(&json!({})).contains("F "));
        let hint =
            overlay_hint(&json!({ "overlayHint": "滚轮切换 · 松开 X2 粘贴 · 右键 / Esc 取消" }));
        assert!(hint.contains("X2") && hint.contains("Esc"));
    }

    #[test]
    fn row_images_survive_selection_session_and_hide_but_retire_on_theme() {
        let state = json!({"visible":true,"sessionRevision":10,"stateRevision":20,"activeIndex":0,"items":[{"id":"a","filePaths":["C:\\one.png"]},{"id":"b"}]});
        let accepted = row_snapshot(&state, "#ffffff");
        let mut next = state.clone();
        next["stateRevision"] = json!(21);
        next["activeIndex"] = json!(1);
        assert_eq!(row_snapshot(&next, "#ffffff"), accepted);
        assert_ne!(row_snapshot(&state, "#000000"), accepted);
        next = state.clone();
        next["sessionRevision"] = json!(11);
        assert_eq!(row_snapshot(&next, "#ffffff"), accepted);
        next = state.clone();
        next["visible"] = json!(false);
        assert_eq!(row_snapshot(&next, "#ffffff"), accepted);
    }

    #[test]
    fn row_images_retire_only_changed_content_revisions_even_when_source_paths_match() {
        let state = json!({"visible":true,"items":[
            {"id":"edited","kind":"ImageClipItem","contentRevision":["original",1,"hash"]},
            {"id":"unchanged","kind":"ImageClipItem","contentRevision":["stable",1,"hash"]}
        ]});
        let before = row_snapshot(&state, "#ffffff");
        let mut changed = state.clone();
        changed["items"][0]["contentRevision"] = json!(["replacement",2,"hash"]);
        let after = row_snapshot(&changed, "#ffffff");
        assert_eq!(row_projection(&state["items"][0]), row_projection(&changed["items"][0]));
        let mut cache = ListContentCache::default();
        cache.advance(1);
        for item in state["items"].as_array().unwrap() {
            let ticket = cache.reserve(1, item["id"].as_str().unwrap(),
                &row_projection(item).unwrap(), ContentMode::Standard, "#ffffff").unwrap();
            assert!(cache.finish(ticket, ContentResources::default(), true));
        }
        cache.advance_retaining(2, |key| row_resource_unchanged(&before, &after, &key.item_id));
        for (id, retained) in [("edited", false), ("unchanged", true)] {
            let item = state["items"].as_array().unwrap().iter().find(|item| item["id"] == id).unwrap();
            assert_eq!(cache.get(2, id, &row_projection(item).unwrap(), ContentMode::Standard,
                "#ffffff").is_some(), retained);
        }
        assert!(!row_resource_unchanged(&after, &row_snapshot(&changed, "#000000"), "unchanged"));
    }

    #[test]
    fn pure_color_pane_requires_the_entire_collapsed_row_token() {
        assert!(pure_row_color(&json!({"displayText":"  #abc  "})).is_some());
        assert!(pure_row_color(&json!({"displayText":"rgba(255,0,0,0.5)"})).is_some());
        assert!(pure_row_color(&json!({"displayText":"rgb(255, 0, 0)"})).is_none());
        assert!(pure_row_color(&json!({"displayText":"#abc extra"})).is_none());
        assert!(pure_row_color(&json!({"displayText":"#abcd"})).is_none());
        assert_eq!(
            row_text(&json!({"displayText":"","summary":"one\n two","title":"title"})),
            "one two"
        );
    }

    #[test]
    fn retained_hidden_render_cannot_schedule_more_row_workers() {
        let mut state = json!({"visible":true});
        let live = Arc::new(AtomicBool::new(true));
        assert!(row_resources_live(&state, &live));
        state["visible"] = json!(false);
        assert!(!row_resources_live(&state, &live));
        state["visible"] = json!(true);
        live.store(false, Ordering::Release);
        assert!(!row_resources_live(&state, &live));
        let reopened = Arc::new(AtomicBool::new(true));
        assert!(row_resources_live(&state, &reopened));
    }
}
