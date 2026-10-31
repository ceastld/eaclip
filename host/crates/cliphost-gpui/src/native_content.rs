//! Compact, read-only list content. Add only `pub mod native_content` at the
//! crate root; the dedicated model modules below need no separate root entries.
//!
//! Parent integration:
//! 1. Project the existing list DTO with `model::project_item` on page arrival.
//!    Keep that DTO/id for edit, paste, drag and output; this is display only.
//! 2. Load `ContentResources` on a background executor for visible projections.
//!    Cache by projection + theme foreground, and check the page generation/id
//!    before applying a reply. No details request is needed.
//!    `cache.advance` retires resources for a replaced page. A refresh of the
//!    same presentation can use `advance_retaining` for unchanged DTOs and keys.
//!    Changed IDs and unknown reloads must invalidate resources even when file
//!    paths stay the same. External file changes are not continuously polled.
//! 3. Use `render_content` for ordinary text. Primary history images use the
//!    display-only history_image_resources loader and managed ContentResources,
//!    never a raw filesystem Path asset; copy/edit/hover keep their original IDs.
//!    Use ContentMode::Minimal for minimal rows.
//!    History can opt into `model::project_history_item_with_text` and
//!    `render_history_content_with_raster_scale` for files and local path previews,
//!    passing the current window scale. Fixed chips and inline images keep their
//!    existing logical sizes.
//! 4. The OpenHandler dispatches `execute_open` on a background executor and
//!    sends its Result back to the parent's existing error presentation.
//!
//! The content wrappers are passive: parent selection, context menus and drag
//! keep working. Only the small hover Open button stops left-button propagation,
//! including repeated clicks. A link's label is never the activation target.
//! HTML detail previews can reuse `render_loaded_html` and `LoadedHtml`; this
//! module does not parse another HTML/XML/StepData representation.

#[path = "list_content_cache.rs"]
pub mod cache;
#[path = "content_resource_budget.rs"]
pub mod budget;
#[path = "native_content_colors.rs"]
pub mod colors;
#[path = "native_content_file_resources.rs"]
mod file_resources;
#[path = "native_content_file_view.rs"]
mod file_view;
#[path = "native_content_files.rs"]
pub mod files;
#[path = "native_icon_svg.rs"]
pub(super) mod icon_svg;
#[path = "native_content_icons.rs"]
pub mod icons;
#[path = "native_content_image_holders.rs"]
pub(super) mod image_holders;
#[path = "native_inline_flow.rs"]
pub(super) mod inline_flow;
#[path = "native_inline_layout.rs"]
pub(super) mod inline_layout;
#[path = "native_content_links.rs"]
pub mod links;
#[path = "native_content_model.rs"]
pub mod model;
use gpui_rsx::rsx;
pub use cache::{ContentLoadTicket, ContentResourceCache, ContentResourceKey, ListContentCache};
pub use budget::{shared_content_budget_snapshot, ContentBudgetSnapshot};
pub use image_holders::ImageHolderLease as ContentImageHolderLease;

use std::{
    cell::Cell,
    collections::{BTreeMap, HashSet},
    io::Cursor,
    rc::Rc,
    sync::{Arc, Mutex, OnceLock, Weak},
};

use cliphost_core::{host::HostState, i18n::tr};
use gpui::{
    div, img, point, prelude::*, px, relative, size, AnyElement, App, AvailableSpace, Bounds,
    DispatchPhase, Element, ElementId, FontWeight, GlobalElementId, HighlightStyle, Image,
    ImageFormat, InspectorElementId, LayoutId, MouseButton, MouseMoveEvent, ObjectFit, Pixels,
    RenderImage, StyledText, UnderlineStyle, Window,
};
use serde_json::json;

use crate::{preview::DesignTokens, view::color};
use links::OpenTarget;
use model::{ContentProjection, MixedPart, StepProjection, TextProjection};

pub use crate::overlay::html_view::{load_html_preview, LoadedHtml};

/// Use only for an already prepared HTML detail projection. This delegates to
/// the existing inert model/renderer; list DTOs normally have no HTML source.
pub fn render_loaded_html(
    loaded: &LoadedHtml,
    width: f32,
    height: f32,
    tokens: &DesignTokens,
    dark: bool,
) -> AnyElement {
    crate::overlay::html_view::render_html_preview(loaded, width, height, tokens, dark)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContentOpenRequest {
    pub item_id: String,
    pub target: OpenTarget,
}

/// The parent owns asynchronous dispatch, stale-session checks and errors.
pub type OpenHandler = Rc<dyn Fn(ContentOpenRequest, &mut Window, &mut App)>;

/// Wrap only a real prepared image widget. The hover owner decides how to
/// register its actual painted bounds and retire its exact original DTO target.
pub type ThumbnailWrapper = Rc<dyn Fn(usize, Arc<Image>, AnyElement) -> AnyElement>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryThumbnailTarget {
    /// Original DTO file_paths index, before trimming/filtering/truncation.
    FileImage(usize),
    FileMedia(usize),
    TextImage,
}

pub type HistoryThumbnailWrapper =
    Rc<dyn Fn(HistoryThumbnailTarget, Option<Arc<Image>>, AnyElement) -> AnyElement>;

/// Existing core actions perform target validation and Shell opening. Call on a
/// background worker, then report its error through the parent; never paste the
/// row or substitute a rendered summary as the action payload.
pub fn execute_open(host: &HostState, request: &ContentOpenRequest) -> Result<(), String> {
    match &request.target {
        OpenTarget::Http(uri) => host.shell_open_uri(&json!({ "uri": uri })),
        OpenTarget::ShellPath(path) => host.shell_open_path(&json!({ "path": path })),
    }
    .map(|_| ())
}

#[derive(Clone)]
pub struct ContentImage {
    pub image: Arc<Image>,
    /// Original natural dimensions, independent of thumbnail resource sizing.
    pub width: f32,
    pub height: f32,
    /// Worker-decoded BGRA raster at list display size. When present, `image`
    /// is only an identity token for holders and release; never decode it.
    pub render: Option<Arc<RenderImage>>,
}

impl ContentImage {
    /// Prefer the prepared raster: GPUI paints it on the first frame instead
    /// of decoding an encoded asset asynchronously while the row is visible.
    pub fn source(&self) -> gpui::ImageSource {
        match &self.render {
            Some(render) => gpui::ImageSource::Render(render.clone()),
            None => gpui::ImageSource::Image(self.image.clone()),
        }
    }
}

/// In-memory resources only. None means a completed unavailable load; a missing
/// key means the parent has not prepared it yet. Neither triggers render I/O.
#[derive(Clone, Default)]
pub struct ContentResources {
    pub images: BTreeMap<String, Option<ContentImage>>,
    pub icons: BTreeMap<String, Option<Arc<Image>>>,
    pub paths: BTreeMap<String, ContentPathInfo>,
}

/// A worker's completed path snapshot. None is unknown (including a rejected
/// text-path probe); image decode failure must never imply a missing path.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ContentPathInfo {
    pub exists: Option<bool>,
    pub size_bytes: Option<u64>,
}

pub const CONTENT_RESOURCE_BUDGET_BYTES: usize = 32 * 1024 * 1024;
// Pixel counts alone undercharge tiny-frame animations and their decode loop.
const MAX_ANIMATION_FRAMES: usize = 2048;
const ANIMATION_FRAME_METADATA_BYTES: usize = 512;

/// Retained previews outside ListContentCache can register the same ownership
/// rule. Lease clones share one claim until their final Drop; caches do this
/// automatically when accepting an entry.
pub fn retain_content_images(resources: &ContentResources) -> ContentImageHolderLease {
    ContentImageHolderLease::new(resources.images_for_release())
}

impl ContentResources {
    fn metadata_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            .saturating_add(
                self.images
                    .keys()
                    .map(|key| key.capacity().saturating_add(128))
                    .sum::<usize>(),
            )
            .saturating_add(
                self.icons
                    .keys()
                    .map(|key| key.capacity().saturating_add(96))
                    .sum::<usize>(),
            )
            .saturating_add(
                self.paths
                    .keys()
                    .map(|key| key.capacity().saturating_add(128))
                    .sum::<usize>(),
            )
    }

    /// Conservative retained estimate: encoded allocation + decoded RGBA (and
    /// one texture copy), plus map metadata. In-memory headers only; no I/O or
    /// GPUI asset lookup. Cache insertion computes this once, outside render.
    pub fn retained_bytes(&self) -> usize {
        let mut encoded = HashSet::new();
        let mut decoded = HashSet::new();
        let mut bytes = self.metadata_bytes();
        for (image, render) in self
            .images
            .values()
            .filter_map(Option::as_ref)
            .map(|image| (&image.image, image.render.as_deref()))
            .chain(self.icons.values().filter_map(Option::as_ref).map(|icon| (icon, None)))
        {
            if encoded.insert(Arc::as_ptr(image) as usize) {
                bytes = bytes
                    .saturating_add(image.bytes.capacity())
                    .saturating_add(64);
            }
            if decoded.insert(image.id()) {
                bytes = bytes.saturating_add(
                    render.map(render_image_bytes).or_else(|| decoded_image_bytes(image))
                        .unwrap_or(usize::MAX)
                        .saturating_mul(2),
                );
            }
        }
        bytes
    }

    /// Oversized/invalid resources become the existing missing-image/generic
    /// icon fallback, while the cache retains the completed failure snapshot.
    pub fn limit_retained_bytes(&mut self, budget: usize) {
        let base = self.metadata_bytes();
        if base > budget {
            self.images.clear();
            self.icons.clear();
            self.paths.clear();
            return;
        }
        let mut available = budget - base;
        for image in self.images.values_mut() {
            if let Some(value) = image {
                let cost = content_image_retained_bytes(value);
                if cost > available {
                    *image = None;
                } else {
                    available -= cost;
                }
            }
        }
        for icon in self.icons.values_mut() {
            if let Some(value) = icon {
                let cost = image_retained_bytes(value);
                if cost > available {
                    *icon = None;
                } else {
                    available -= cost;
                }
            }
        }
    }

    pub fn images_for_release(&self) -> Vec<Arc<Image>> {
        let mut seen = HashSet::new();
        self.images
            .values()
            .filter_map(Option::as_ref)
            .map(|image| image.image.clone())
            .chain(self.icons.values().filter_map(Option::as_ref).cloned())
            .filter(|image| seen.insert(image.id()))
            .collect()
    }
}

fn image_retained_bytes(image: &Image) -> usize {
    image.bytes.capacity().saturating_add(64).saturating_add(
        decoded_image_bytes(image)
            .unwrap_or(usize::MAX)
            .saturating_mul(2),
    )
}

/// A prepared raster is charged for its CPU frames plus one atlas copy.
fn content_image_retained_bytes(image: &ContentImage) -> usize {
    match &image.render {
        Some(render) => image.image.bytes.capacity().saturating_add(64)
            .saturating_add(render_image_bytes(render).saturating_mul(2)),
        None => image_retained_bytes(&image.image),
    }
}

fn render_image_bytes(render: &RenderImage) -> usize {
    (0..render.frame_count())
        .map(|frame| render.as_bytes(frame).map_or(0, <[u8]>::len))
        .fold(0, usize::saturating_add)
}

fn rgba_bytes(width: u64, height: u64) -> Option<usize> {
    if width == 0 || height == 0 {
        return None;
    }
    usize::try_from(width.checked_mul(height)?.checked_mul(4)?).ok()
}

/// Estimates GPUI's actual in-memory decoder, including GIF's composited
/// logical canvases. The small generated SVG has explicit intrinsic dimensions.
fn decoded_image_bytes(image: &Image) -> Option<usize> {
    match image.format() {
        ImageFormat::Gif => gif_decoded_bytes(image.bytes()),
        ImageFormat::Webp => webp_decoded_bytes(image.bytes()).map(|(bytes, _)| bytes),
        ImageFormat::Svg => {
            let svg = std::str::from_utf8(image.bytes()).ok()?;
            let svg = svg.get(..svg.find('>')?)?;
            let dimension = |name: &str| -> Option<u64> {
                let start = svg.find(&format!(" {name}=\""))? + name.len() + 3;
                let end = svg[start..].find('"')? + start;
                svg[start..end].parse().ok()
            };
            rgba_bytes(dimension("width")?, dimension("height")?)
        }
        format => {
            let format = match format {
                ImageFormat::Png => image::ImageFormat::Png,
                ImageFormat::Jpeg => image::ImageFormat::Jpeg,
                ImageFormat::Webp => image::ImageFormat::WebP,
                ImageFormat::Bmp => image::ImageFormat::Bmp,
                ImageFormat::Tiff => image::ImageFormat::Tiff,
                _ => return None,
            };
            let (width, height) =
                image::ImageReader::with_format(Cursor::new(image.bytes()), format)
                    .into_dimensions()
                    .ok()?;
            rgba_bytes(width as u64, height as u64)
        }
    }
}

/// Walk GIF block boundaries without LZW decoding or frame allocation. Reject
/// malformed descriptors and oversized individual/accumulated canvas buffers
/// before GPUI's unrestricted animation decoder can allocate them.
fn gif_decoded_bytes(bytes: &[u8]) -> Option<usize> {
    if !matches!(bytes.get(..6)?, b"GIF87a" | b"GIF89a") {
        return None;
    }
    let word = |at: usize| -> Option<u64> {
        Some(u16::from_le_bytes([*bytes.get(at)?, *bytes.get(at + 1)?]) as u64)
    };
    let (width, height) = (word(6)?, word(8)?);
    let canvas = rgba_bytes(width, height)?;
    if canvas > CONTENT_RESOURCE_BUDGET_BYTES / 2 {
        return None;
    }
    let table_size = |packed: u8| -> usize { 3 * (1usize << ((packed & 7) + 1)) };
    let mut at = 13usize;
    let packed = *bytes.get(10)?;
    if packed & 0x80 != 0 {
        at = at.checked_add(table_size(packed))?;
    }
    if at > bytes.len() {
        return None;
    }
    let mut total = 0usize;
    let mut frames = 0usize;
    let skip_blocks = |at: &mut usize| -> Option<()> {
        loop {
            let length = *bytes.get(*at)? as usize;
            *at = at.checked_add(1)?;
            if length == 0 {
                return Some(());
            }
            *at = at.checked_add(length)?;
            if *at > bytes.len() {
                return None;
            }
        }
    };
    loop {
        let marker = *bytes.get(at)?;
        at = at.checked_add(1)?;
        match marker {
            0x3b => return (total > 0).then_some(total),
            0x21 => {
                bytes.get(at)?;
                at = at.checked_add(1)?;
                skip_blocks(&mut at)?;
            }
            0x2c => {
                let (left, top, frame_width, frame_height) =
                    (word(at)?, word(at + 2)?, word(at + 4)?, word(at + 6)?);
                if left.checked_add(frame_width)? > width || top.checked_add(frame_height)? > height
                {
                    return None;
                }
                let frame = rgba_bytes(frame_width, frame_height)?;
                if frame.checked_add(canvas)? > CONTENT_RESOURCE_BUDGET_BYTES / 2 {
                    return None;
                }
                let packed = *bytes.get(at + 8)?;
                at = at.checked_add(9)?;
                if packed & 0x80 != 0 {
                    at = at.checked_add(table_size(packed))?;
                }
                // LZW minimum code size, followed by length-delimited data.
                if !(2..=8).contains(bytes.get(at)?) {
                    return None;
                }
                at = at.checked_add(1)?;
                skip_blocks(&mut at)?;
                frames = frames.checked_add(1)?;
                if frames > MAX_ANIMATION_FRAMES {
                    return None;
                }
                total = total.checked_add(canvas.checked_add(ANIMATION_FRAME_METADATA_BYTES)?)?;
                if total > CONTENT_RESOURCE_BUDGET_BYTES / 2 {
                    return None;
                }
            }
            _ => return None,
        }
    }
}

/// WebP's animation decoder retains complete canvas frames, including small
/// subrect frames. Inspect RIFF boundaries before allowing GPUI to decode it.
/// The bool distinguishes an animation (even one frame) from a static WebP.
fn webp_decoded_bytes(bytes: &[u8]) -> Option<(usize, bool)> {
    if bytes.get(..4)? != b"RIFF" || bytes.get(8..12)? != b"WEBP" { return None; }
    let declared = u32::from_le_bytes(bytes.get(4..8)?.try_into().ok()?) as usize;
    let end = declared.checked_add(8)?;
    if end > bytes.len() || end < 12 { return None; }
    let word24 = |data: &[u8]| -> Option<u64> {
        Some(u32::from_le_bytes([*data.first()?, *data.get(1)?, *data.get(2)?, 0]) as u64)
    };
    let mut at = 12usize;
    let mut canvas = None;
    let mut animated = false;
    let mut animation_header = false;
    let mut frames = 0usize;
    while at < end {
        let kind = bytes.get(at..at.checked_add(4)?)?;
        let length = u32::from_le_bytes(bytes.get(at + 4..at + 8)?.try_into().ok()?) as usize;
        let start = at.checked_add(8)?;
        let chunk_end = start.checked_add(length)?;
        if chunk_end > end { return None; }
        let data = bytes.get(start..chunk_end)?;
        match kind {
            b"VP8X" => {
                if length != 10 || canvas.is_some() { return None; }
                animated = data[0] & 0x02 != 0;
                canvas = Some((word24(data.get(4..7)?)?.checked_add(1)?,
                    word24(data.get(7..10)?)?.checked_add(1)?));
            }
            b"ANIM" => { if length != 6 || animation_header { return None; } animation_header = true; }
            b"ANMF" => {
                if !animated || !animation_header || length < 16 { return None; }
                let (width, height) = canvas?;
                let left = word24(data.get(..3)?)?.checked_mul(2)?;
                let top = word24(data.get(3..6)?)?.checked_mul(2)?;
                let frame_width = word24(data.get(6..9)?)?.checked_add(1)?;
                let frame_height = word24(data.get(9..12)?)?.checked_add(1)?;
                if left.checked_add(frame_width)? > width || top.checked_add(frame_height)? > height {
                    return None;
                }
                frames = frames.checked_add(1)?;
                if frames > MAX_ANIMATION_FRAMES
                    || rgba_bytes(width, height)?.checked_add(ANIMATION_FRAME_METADATA_BYTES)?
                        .checked_mul(frames)? > CONTENT_RESOURCE_BUDGET_BYTES / 2 {
                    return None;
                }
            }
            _ => {}
        }
        at = chunk_end.checked_add(length & 1)?;
        if at > end { return None; }
    }
    if animated {
        if frames == 0 { return None; }
        let (width, height) = canvas?;
        Some((rgba_bytes(width, height)?.checked_add(ANIMATION_FRAME_METADATA_BYTES)?
            .checked_mul(frames)?, true))
    } else {
        if frames != 0 || animation_header { return None; }
        let (width, height) = image::ImageReader::with_format(Cursor::new(bytes), image::ImageFormat::WebP)
            .into_dimensions().ok()?;
        Some((rgba_bytes(width as u64, height as u64)?, false))
    }
}

/// All disk/Shell/decoding work happens here, off the UI thread. FontAwesome
/// vectors use the existing core catalog and follow the current Web CSS
/// foreground rule. The caller caches this snapshot rather than reloading in
/// each render. Remote icon specs are fetched and admitted here; load failures
/// use the existing generic step fallback.
pub fn load_content_resources(
    host: &HostState,
    projection: &ContentProjection,
    foreground: &str,
) -> ContentResources {
    let mut resources = ContentResources::default();
    match projection {
        ContentProjection::Files(files) => {
            file_resources::load_file_resources(host, files, &mut resources);
            return resources;
        }
        ContentProjection::LocalPath(path) => {
            file_resources::load_local_path_resources(host, path, &mut resources);
            return resources;
        }
        _ => {}
    }
    for path in projection.image_paths() {
        let image = host
            .local_image_bytes(&path)
            .and_then(|(bytes, _)| compact_image(bytes));
        resources.images.insert(path, image);
        resources.limit_retained_bytes(CONTENT_RESOURCE_BUDGET_BYTES);
    }
    for spec in projection.icon_specs() {
        let icon = load_step_icon(host, &spec, foreground);
        resources.icons.insert(spec, icon);
        resources.limit_retained_bytes(CONTENT_RESOURCE_BUDGET_BYTES);
    }
    resources
}

/// Favorites can reuse history's bounded file worker without encoding paths
/// into a text/step projection or changing their immutable action DTO.
pub fn load_file_list_resources(
    host: &HostState,
    projection: &files::FileListProjection,
) -> ContentResources {
    let mut resources = ContentResources::default();
    file_resources::load_file_resources(host, projection, &mut resources);
    resources
}

fn compact_image(bytes: Vec<u8>) -> Option<ContentImage> {
    bounded_bitmap(bytes, 2048, 56)
}

/// History's full local-image preview uses the configured display height up to
/// 640px. Preserve natural dimensions while bounding retained raster pixels.
pub(super) fn list_image(bytes: Vec<u8>) -> Option<ContentImage> {
    bounded_bitmap(bytes, 1024, 640)
}

/// The list row's own thumbnail: decoded once and resized straight to the
/// painted height, so cold loads skip the 1024x640 intermediate PNG.
pub(super) fn list_display_image(bytes: Vec<u8>, max_height_px: u32) -> Option<ContentImage> {
    bounded_bitmap(bytes, DISPLAY_RASTER_MAX_WIDTH, max_height_px.clamp(1, 640))
}

fn bounded_bitmap(bytes: Vec<u8>, max_width: u32, max_height: u32) -> Option<ContentImage> {
    let mut reader = image::ImageReader::new(Cursor::new(&bytes))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(64 * 1024 * 1024);
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    reader.limits(limits);
    let format = reader.format()?;
    let animation = match format {
        image::ImageFormat::Gif => Some(gif_decoded_bytes(&bytes)?),
        image::ImageFormat::WebP => {
            let (cost, animated) = webp_decoded_bytes(&bytes)?;
            animated.then_some(cost)
        }
        _ => None,
    };
    if let Some(decoded) = animation {
        if bytes.capacity().checked_add(decoded.checked_mul(2)?)? > CONTENT_RESOURCE_BUDGET_BYTES {
            return None;
        }
        let (width, height) = reader.into_dimensions().ok()?;
        // Keep admitted animation payloads intact. Oversized animations fail
        // closed rather than quietly becoming a static first frame.
        return Some(ContentImage {
            image: Arc::new(Image::from_bytes(
                if format == image::ImageFormat::Gif {
                    ImageFormat::Gif
                } else {
                    ImageFormat::Webp
                },
                bytes,
            )),
            width: width as f32,
            height: height as f32,
            render: None,
        });
    }
    let image = reader.decode().ok()?;
    let (width, height) = (image.width() as f32, image.height() as f32);
    let image = image.thumbnail(image.width().min(max_width), image.height().min(max_height));
    let mut bytes = Cursor::new(Vec::new());
    image.write_to(&mut bytes, image::ImageFormat::Png).ok()?;
    Some(ContentImage {
        image: Arc::new(Image::from_bytes(ImageFormat::Png, bytes.into_inner())),
        width,
        height,
        render: None,
    })
}

const DISPLAY_RASTER_MAX_WIDTH: u32 = 1024;

/// Worker-only. Decode a static image (first frame) straight into the list
/// display raster without an intermediate PNG round trip.
pub fn display_bitmap(bytes: &[u8], max_height_px: u32) -> Option<ContentImage> {
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(64 * 1024 * 1024);
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    reader.limits(limits);
    let decoded = reader.decode().ok()?;
    let (width, height) = (decoded.width() as f32, decoded.height() as f32);
    display_image(&decoded, width, height, max_height_px)
}

/// Worker-only. Convert a bounded static list PNG (e.g. a disk thumbnail hit)
/// into the display raster. Animated or other payloads are returned unchanged
/// and keep GPUI's encoded-asset path.
pub fn display_from_list_image(image: ContentImage, max_height_px: u32) -> ContentImage {
    if image.render.is_some() || image.image.format() != ImageFormat::Png {
        return image;
    }
    let decoded = image::load_from_memory_with_format(image.image.bytes(), image::ImageFormat::Png);
    decoded.ok()
        .and_then(|decoded| display_image(&decoded, image.width, image.height, max_height_px))
        .unwrap_or(image)
}

/// The list never paints a raster taller than the image-height setting in
/// physical pixels (see `raster_layout`), so resizing to it on the worker is
/// lossless on screen and keeps decoded/atlas memory proportional to what is
/// shown. Rows then paint BGRA directly on their first frame. Equal pixels
/// share one RenderImage process-wide (history, favorites, re-loads).
pub(crate) fn display_image(
    decoded: &image::DynamicImage,
    natural_width: f32,
    natural_height: f32,
    max_height_px: u32,
) -> Option<ContentImage> {
    let resized = decoded.thumbnail(
        decoded.width().min(DISPLAY_RASTER_MAX_WIDTH),
        decoded.height().min(max_height_px.clamp(1, 640)),
    );
    let mut buffer = resized.into_rgba8();
    if buffer.width() == 0 || buffer.height() == 0 {
        return None;
    }
    let digest = blake3::hash(buffer.as_raw());
    for pixel in buffer.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    // An identity token, not encoded pixels: holders, budget and release key
    // on Image ids. It is never handed to GPUI's decoder (see `source`).
    let mut token = b"cliphost-display-raster-v1\0".to_vec();
    token.extend_from_slice(&buffer.width().to_le_bytes());
    token.extend_from_slice(&buffer.height().to_le_bytes());
    token.extend_from_slice(digest.as_bytes());
    let image = Arc::new(Image::from_bytes(ImageFormat::Png, token));
    let render = intern_display_render(image.id(), move || {
        Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]))
    });
    Some(ContentImage {
        image,
        width: natural_width,
        height: natural_height,
        render: Some(render),
    })
}

fn intern_display_render(id: u64, make: impl FnOnce() -> Arc<RenderImage>) -> Arc<RenderImage> {
    static RENDERS: OnceLock<Mutex<BTreeMap<u64, Weak<RenderImage>>>> = OnceLock::new();
    let mut renders = RENDERS.get_or_init(Default::default).lock().unwrap();
    if let Some(render) = renders.get(&id).and_then(Weak::upgrade) {
        return render;
    }
    if renders.len() >= 512 {
        renders.retain(|_, render| render.strong_count() > 0);
    }
    let render = make();
    renders.insert(id, Arc::downgrade(&render));
    render
}

fn xml_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
fn explicit_icon_color(spec: &str) -> bool {
    let spec = spec.trim();
    let spec = if spec
        .get(..3)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("fa:"))
    {
        &spec[3..]
    } else {
        spec
    };
    spec.split_once(':')
        .is_some_and(|(_, color)| !color.trim().is_empty())
}

fn load_step_icon(host: &HostState, spec: &str, foreground: &str) -> Option<Arc<Image>> {
    load_icon_spec(host, spec, foreground)
        .or_else(|| load_icon_spec(host, "fa:Light_ListOl", foreground))
}

/// Resolve one icon spec (`fa:`, shell `icon:`/path, remote URL) without the
/// step fallback, so callers such as menu rows can keep their own glyph.
/// Blocking: call from a background worker.
pub fn load_icon_spec(host: &HostState, spec: &str, foreground: &str) -> Option<Arc<Image>> {
    match icons::parse_step_icon(spec) {
        icons::StepIconSpec::FontAwesome(spec) => {
            let vector = host.fa_vector(&spec)?;
            let fill = effective_fa_fill(foreground);
            let svg = format!(
                "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"56\" height=\"56\" viewBox=\"0 0 {} {}\"><path fill=\"#{:02x}{:02x}{:02x}\" fill-opacity=\"{}\" d=\"{}\"/></svg>",
                vector["width"].as_u64()?, vector["height"].as_u64()?,
                fill.red, fill.green, fill.blue, fill.alpha as f32 / 255.0,
                xml_attribute(vector["path"].as_str()?),
            );
            Some(Arc::new(Image::from_bytes(
                ImageFormat::Svg,
                svg.into_bytes(),
            )))
        }
        icons::StepIconSpec::Shell(spec) => host
            .file_icon_png(None, None, Some(&spec))
            .ok()
            .and_then(prepare_icon_bytes),
        icons::StepIconSpec::Remote(url) => icons::fetch_remote_icon(&url)
            .ok()
            .and_then(prepare_icon_bytes),
    }
}

fn effective_fa_fill(foreground: &str) -> colors::RgbaColor {
    // The current Web .file-icon path rule overrides every FA presentation
    // fill attribute with currentColor, including embedded/API/fallback icons.
    colors::parse_swatch_color(foreground)
        .or_else(|| crate::overlay::html::parse_html_color(foreground))
        .unwrap_or(colors::RgbaColor {
            red: 0,
            green: 0,
            blue: 0,
            alpha: 255,
        })
}

/// An icon worker produces a bounded, self-contained resource. Remote SVG is
/// validated/rasterized here so drawing never resolves external SVG hrefs.
fn prepare_icon_bytes(bytes: Vec<u8>) -> Option<Arc<Image>> {
    let bytes = if icon_svg::is_svg_bytes(&bytes) {
        icon_svg::rasterize_icon_svg(&bytes).ok()?
    } else {
        bytes
    };
    let image = compact_image(bytes)?.image;
    (image_retained_bytes(&image) <= CONTENT_RESOURCE_BUDGET_BYTES).then_some(image)
}

fn rendered_images() -> &'static Mutex<BTreeMap<u64, Weak<gpui::RenderImage>>> {
    static IMAGES: OnceLock<Mutex<BTreeMap<u64, Weak<gpui::RenderImage>>>> = OnceLock::new();
    IMAGES.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Isolated native paint tests inspect the real asset registered by TrackedImage.
#[cfg(feature = "test-support")]
pub fn rendered_image_weak(image_id: u64) -> Option<Weak<gpui::RenderImage>> {
    rendered_images().lock().unwrap().get(&image_id).cloned()
}

/// Native paint tests inspect a painted prepared raster until its release.
#[cfg(feature = "test-support")]
pub fn painted_render_weak(image_id: u64) -> Option<Weak<gpui::RenderImage>> {
    painted_renders().lock().unwrap().get(&image_id)?.first().map(Arc::downgrade)
}

/// Exercise the real foreground pressure handoff with isolated cache owners.
#[cfg(feature = "test-support")]
pub fn register_cache_owner_for_test<T: 'static>(
    cx: &mut gpui::Context<T>,
    drain: fn(&mut T, &mut gpui::Context<T>),
) {
    crate::content_cache_owners::register(cx, drain);
}

struct ContentImageRetirementPump {
    _task: gpui::Task<()>,
}
impl gpui::Global for ContentImageRetirementPump {}

/// One UI task drains final cache-holder drops, including cache/entity/window
/// destruction where no caller remains to invoke `release_evicted`.
pub(super) fn ensure_image_retirement_scheduler(cx: &mut App) {
    if cx.has_global::<ContentImageRetirementPump>() {
        return;
    }
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<Vec<Arc<Image>>>();
    let _ = image_holders::set_retirement_scheduler(Arc::new(move |images| {
        // App shutdown drops the receiver. Then no live GPU owner remains.
        let _ = sender.send(images);
    }));
    let task = cx.spawn(async move |cx| {
        while let Some(images) = receiver.recv().await {
            if cx
                .update(move |cx| {
                    for image in images {
                        release_content_image(image, cx);
                    }
                })
                .is_err()
            {
                break;
            }
        }
    });
    cx.set_global(ContentImageRetirementPump { _task: task });
}

/// Retire CPU assets and actual painted atlas tiles only after all registered
/// cache holders left. Recheck inside the deferred UI turn because a different
/// cache may acquire the same image after eviction but before cleanup runs.
pub(super) fn release_content_image(image: Arc<Image>, cx: &mut App) {
    ensure_image_retirement_scheduler(cx);
    cx.defer(move |cx| {
        let id = image.id();
        if image_holders::image_has_holders(id) {
            return;
        }
        let decoded = rendered_images()
            .lock()
            .unwrap()
            .remove(&id)
            .and_then(|image| image.upgrade());
        image.remove_asset(cx);
        if let Some(decoded) = decoded {
            cx.drop_image(decoded, None);
        }
        // Prepared rasters bypass GPUI's asset cache: their painted copies are
        // held here until this final release, then their atlas tiles go too.
        let painted = painted_renders().lock().unwrap().remove(&id).unwrap_or_default();
        for render in painted {
            cx.drop_image(render, None);
        }
        image_holders::acknowledge_image_retirement(id);
    });
}

/// Painted prepared rasters by holder image id. Strong on purpose: a cache can
/// drop its entry before the deferred release runs, and the atlas tiles can
/// only be removed through the exact RenderImage. Every entry comes from a
/// cache-held image, so `release_content_image` always drains it.
fn painted_renders() -> &'static Mutex<BTreeMap<u64, Vec<Arc<RenderImage>>>> {
    static RENDERS: OnceLock<Mutex<BTreeMap<u64, Vec<Arc<RenderImage>>>>> = OnceLock::new();
    RENDERS.get_or_init(Default::default)
}

struct TrackedImage {
    image: Arc<Image>,
    render: Option<Arc<RenderImage>>,
    child: AnyElement,
}

fn tracked_image(image: Arc<Image>, child: AnyElement) -> AnyElement {
    TrackedImage { image, render: None, child }.into_any_element()
}

/// Wrap an already prepared in-memory image element so shared list/favorite
/// resource caches can retire its CPU asset and actually painted atlas tiles.
/// The child remains responsible for dimensions and its existing fallback.
pub fn render_resource_image(image: Arc<Image>, child: AnyElement) -> AnyElement {
    tracked_image(image, child)
}

/// Like `render_resource_image`, for a child painted from `image.source()`.
pub fn render_content_image(image: &ContentImage, child: AnyElement) -> AnyElement {
    TrackedImage { image: image.image.clone(), render: image.render.clone(), child }.into_any_element()
}

impl IntoElement for TrackedImage {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for TrackedImage {
    type RequestLayoutState = ();
    type PrepaintState = ();
    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }
    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.child.request_layout(window, cx), ())
    }
    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.prepaint(window, cx);
    }
    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        ensure_image_retirement_scheduler(cx);
        self.child.paint(window, cx);
        if let Some(render) = &self.render {
            // The token image must never reach GPUI's decoder/asset cache.
            let mut painted = painted_renders().lock().unwrap();
            let renders = painted.entry(self.image.id()).or_default();
            if !renders.iter().any(|current| Arc::ptr_eq(current, render)) {
                renders.push(render.clone());
            }
            return;
        }
        // The child already requested/painted this exact asset. This reads its
        // ready shared result rather than initiating a cache-budget probe.
        if let Some(decoded) = self.image.clone().get_render_image(window, cx) {
            let mut images = rendered_images().lock().unwrap();
            images.insert(self.image.id(), Arc::downgrade(&decoded));
            if images.len() > 4096 {
                images.retain(|_, image| image.strong_count() > 0);
                while images.len() > 4096 {
                    let key = *images.keys().next().unwrap();
                    images.remove(&key);
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ContentMetrics {
    /// Actual content width after row padding/minimal metadata reservation.
    pub width: f32,
    pub font_size: f32,
    pub line_height: f32,
    pub max_lines: usize,
}

#[derive(Clone, Copy, Debug)]
pub enum ContentRole {
    Primary,
    Secondary,
}

/// Read-only QA sample from the actual paint pass, in window-client logical
/// pixels. This contains no source text, image specification, URL or path.
#[derive(Clone, Copy, Debug)]
pub struct ContentPaintPart {
    pub role: &'static str,
    pub index: usize,
    pub bounds: Bounds<Pixels>,
    /// The displayed resource is ready; this does not identify whether an
    /// original remote/Shell specification fell back to the generic icon.
    pub available: bool,
}

pub type ContentPaintObserver = Rc<dyn Fn(ContentPaintPart)>;

pub(super) fn report_content_paint(
    observer: &Option<ContentPaintObserver>,
    mut part: ContentPaintPart,
    window: &Window,
) {
    if let Some(observer) = observer {
        part.bounds = part.bounds.intersect(&window.content_mask().bounds);
        if part.bounds.size.width > px(0.0) && part.bounds.size.height > px(0.0) {
            observer(part);
        }
    }
}

struct ObservedContentPart {
    child: AnyElement,
    observer: ContentPaintObserver,
    role: &'static str,
    index: usize,
    available: bool,
    image: Option<Arc<Image>>,
}

fn observed_content_part(
    child: AnyElement,
    observer: &Option<ContentPaintObserver>,
    role: &'static str,
    index: usize,
    available: bool,
    image: Option<Arc<Image>>,
) -> AnyElement {
    let Some(observer) = observer else {
        return child;
    };
    ObservedContentPart {
        child,
        observer: observer.clone(),
        role,
        index,
        available,
        image,
    }
    .into_any_element()
}

impl IntoElement for ObservedContentPart {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for ObservedContentPart {
    type RequestLayoutState = ();
    type PrepaintState = ();
    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }
    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.child.request_layout(window, cx), ())
    }
    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.prepaint(window, cx);
    }
    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(window, cx);
        let available = self.image.as_ref().map_or(self.available, |image| {
            // The img already requested this asset. Query the ready result;
            // a pending/error img that painted fallback is not available yet.
            image.clone().get_render_image(window, cx).is_some()
        });
        report_content_paint(
            &Some(self.observer.clone()),
            ContentPaintPart {
                role: self.role,
                index: self.index,
                bounds,
                available,
            },
            window,
        );
    }
}

/// Compact list body. It neither fetches details/resources nor mutates the DTO.
/// Additional summary text uses `render_text_content` with Secondary, giving
/// its hover state a distinct stable ID within the same row.
pub fn render_content(
    item_id: &str,
    projection: &ContentProjection,
    resources: &ContentResources,
    metrics: ContentMetrics,
    tokens: &DesignTokens,
    dark: bool,
    on_open: OpenHandler,
) -> AnyElement {
    render_content_with_observer(
        item_id, projection, resources, metrics, tokens, dark, on_open, None,
    )
}

/// Optional QA observer. None delegates to exactly the ordinary elements,
/// without adding a layout node or changing their painting/event behavior.
pub fn render_content_with_observer(
    item_id: &str,
    projection: &ContentProjection,
    resources: &ContentResources,
    metrics: ContentMetrics,
    tokens: &DesignTokens,
    dark: bool,
    on_open: OpenHandler,
    observer: Option<ContentPaintObserver>,
) -> AnyElement {
    render_content_with_bindings(
        item_id,
        projection,
        resources,
        metrics,
        tokens,
        dark,
        on_open,
        observer,
        &[],
        None,
    )
}

/// The index slice is parallel to `model::project_mixed`, obtained from
/// `model::project_mixed_source_indices` using the same original segment DTO.
/// Missing mappings never bind a thumbnail using a guessed flow index.
pub fn render_content_with_bindings(
    item_id: &str,
    projection: &ContentProjection,
    resources: &ContentResources,
    metrics: ContentMetrics,
    tokens: &DesignTokens,
    dark: bool,
    on_open: OpenHandler,
    observer: Option<ContentPaintObserver>,
    original_segment_indices: &[usize],
    thumbnail_wrapper: Option<ThumbnailWrapper>,
) -> AnyElement {
    match projection {
        ContentProjection::Text(text) => render_text_content_observed(
            item_id,
            ContentRole::Primary,
            text,
            metrics,
            tokens,
            dark,
            on_open,
            observer,
        ),
        ContentProjection::Mixed(parts) => {
            if thumbnail_wrapper.is_some() {
                inline_flow::render_mixed_inline_with_callbacks(
                    parts,
                    resources,
                    metrics,
                    tokens,
                    dark,
                    observer,
                    original_segment_indices,
                    thumbnail_wrapper,
                )
            } else {
                render_mixed(parts, resources, metrics, tokens, dark, observer)
            }
        }
        ContentProjection::Steps(steps) => {
            render_steps(steps, resources, metrics.width, tokens, dark, observer)
        }
        ContentProjection::Files(files) => file_view::render_files(
            item_id, files, resources, metrics, 160.0, 1.0, tokens, dark, on_open, observer, None,
        ),
        ContentProjection::LocalPath(path) => file_view::render_local_path(
            item_id, path, resources, metrics, 160.0, 1.0, tokens, dark, on_open, observer, None,
        ),
    }
}

/// The body badge owner uses this same classification to suppress a file-count
/// pill for a single image, including while its bitmap is still loading.
pub fn history_file_image_preview<'a>(
    projection: &'a files::FileListProjection,
    resources: &ContentResources,
) -> Option<&'a files::FileEntryProjection> {
    let [entry] = projection.entries.as_slice() else {
        return None;
    };
    (projection.total_entries == 1
        && entry.is_image
        && !resources
            .paths
            .get(&entry.path)
            .is_some_and(|info| info.exists == Some(false)))
    .then_some(entry)
}

/// History's opt-in renderer adds full local-image height and typed thumbnail
/// targets. QQ continues to use its existing original-segment index callback.
/// File Open requests contain the original DTO path; the parent may route those
/// through its existing open_item_path action, keeping row/output payloads intact.
pub fn render_history_content_with_bindings(
    item_id: &str,
    projection: &ContentProjection,
    resources: &ContentResources,
    metrics: ContentMetrics,
    image_max_height: f32,
    tokens: &DesignTokens,
    dark: bool,
    on_open: OpenHandler,
    observer: Option<ContentPaintObserver>,
    original_segment_indices: &[usize],
    thumbnail_wrapper: Option<ThumbnailWrapper>,
    history_thumbnail_wrapper: Option<HistoryThumbnailWrapper>,
) -> AnyElement {
    render_history_content_with_raster_scale(
        item_id,
        projection,
        resources,
        metrics,
        image_max_height,
        1.0,
        tokens,
        dark,
        on_open,
        observer,
        original_segment_indices,
        thumbnail_wrapper,
        history_thumbnail_wrapper,
    )
}

/// DPI-aware natural file/path image rendering. `raster_scale` is the current
/// window's pixels per DIP; it affects only large natural image previews.
/// Fixed chip icons, QQ compact images and original metadata stay unchanged.
pub fn render_history_content_with_raster_scale(
    item_id: &str,
    projection: &ContentProjection,
    resources: &ContentResources,
    metrics: ContentMetrics,
    image_max_height: f32,
    raster_scale: f32,
    tokens: &DesignTokens,
    dark: bool,
    on_open: OpenHandler,
    observer: Option<ContentPaintObserver>,
    original_segment_indices: &[usize],
    thumbnail_wrapper: Option<ThumbnailWrapper>,
    history_thumbnail_wrapper: Option<HistoryThumbnailWrapper>,
) -> AnyElement {
    match projection {
        ContentProjection::Files(files) => file_view::render_files(
            item_id,
            files,
            resources,
            metrics,
            image_max_height,
            raster_scale,
            tokens,
            dark,
            on_open,
            observer,
            history_thumbnail_wrapper,
        ),
        ContentProjection::LocalPath(path) => file_view::render_local_path(
            item_id,
            path,
            resources,
            metrics,
            image_max_height,
            raster_scale,
            tokens,
            dark,
            on_open,
            observer,
            history_thumbnail_wrapper,
        ),
        _ => render_content_with_bindings(
            item_id,
            projection,
            resources,
            metrics,
            tokens,
            dark,
            on_open,
            observer,
            original_segment_indices,
            thumbnail_wrapper,
        ),
    }
}

/// Favorites use the same passive file chips, missing state and original
/// indices. Their Web list has no per-file size/dimension labels or chip hover.
pub fn render_favorite_file_chips(
    item_id: &str,
    projection: &files::FileListProjection,
    resources: &ContentResources,
    metrics: ContentMetrics,
    tokens: &DesignTokens,
    dark: bool,
    on_open: OpenHandler,
    thumbnail_wrapper: Option<HistoryThumbnailWrapper>,
) -> AnyElement {
    if projection.entries.is_empty() {
        return render_text_content(
            item_id,
            ContentRole::Primary,
            &projection.fallback,
            metrics,
            tokens,
            dark,
            on_open,
        );
    }
    file_view::render_file_chips(
        item_id, projection, resources, metrics, tokens, dark, on_open, None, false,
        thumbnail_wrapper,
    )
}

pub fn render_text_content(
    item_id: &str,
    role: ContentRole,
    projection: &TextProjection,
    metrics: ContentMetrics,
    tokens: &DesignTokens,
    dark: bool,
    on_open: OpenHandler,
) -> AnyElement {
    render_text_content_observed(
        item_id, role, projection, metrics, tokens, dark, on_open, None,
    )
}

/// Existing list secondary text: `.summary-text` is 12px, normal weight, two
/// lines, with 1.4 compact / 1.48 relaxed line height. Its projection must be
/// prepared from the host's existing summary, never used as an action payload.
/// Keep this separate from smallFontSize, which also controls chrome metadata.
pub fn render_secondary_content(
    item_id: &str,
    projection: &TextProjection,
    width: f32,
    relaxed: bool,
    tokens: &DesignTokens,
    dark: bool,
    on_open: OpenHandler,
) -> AnyElement {
    rsx! {
        <div w_full min_w_0 text_color={color(&tokens.theme(dark).text_secondary)}
            font_weight={FontWeight::NORMAL} />
    }
        .child(render_text_content(
            item_id,
            ContentRole::Secondary,
            projection,
            ContentMetrics {
                width,
                font_size: 12.0,
                line_height: if relaxed { 1.48 } else { 1.4 },
                max_lines: 2,
            },
            tokens,
            dark,
            on_open,
        ))
        .into_any_element()
}

fn render_text_content_observed(
    item_id: &str,
    role: ContentRole,
    projection: &TextProjection,
    metrics: ContentMetrics,
    tokens: &DesignTokens,
    dark: bool,
    on_open: OpenHandler,
    observer: Option<ContentPaintObserver>,
) -> AnyElement {
    let boundaries = projection
        .links
        .iter()
        .map(|link| &link.range)
        .chain(&projection.hit_ranges)
        .flat_map(|range| [range.start, range.end])
        .collect::<Vec<_>>();
    if !colors::collect_color_tokens_with_boundaries(&projection.text, &boundaries).is_empty() {
        return match observer {
            None => inline_flow::render_text_inline(
                item_id, role, projection, metrics, tokens, dark, on_open,
            ),
            observer => inline_flow::render_text_inline_with_observer(
                item_id, role, projection, metrics, tokens, dark, on_open, observer,
            ),
        };
    }
    let theme = tokens.theme(dark);
    let highlights = projection
        .styled_spans()
        .into_iter()
        .map(|span| {
            (
                span.range,
                HighlightStyle {
                    color: span.link.then(|| color(&theme.accent)),
                    background_color: span.hit.then(|| color(&theme.selected)),
                    underline: span.link.then_some(UnderlineStyle {
                        thickness: px(1.0),
                        color: Some(color(&theme.accent)),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )
        })
        .collect::<Vec<_>>();
    let text = StyledText::new(projection.text.clone()).with_highlights(highlights);
    let linked = LinkedText {
        id: gpui::SharedString::from(format!(
            "native-inline-{item_id}-{}",
            match role {
                ContentRole::Primary => "primary",
                ContentRole::Secondary => "secondary",
            }
        ))
        .into(),
        text,
        projection: projection.clone(),
        item_id: item_id.to_owned(),
        on_open,
        observer,
        accent: color(&theme.accent),
        surface: color(&theme.surface),
    };
    rsx! {
        <div w_full min_w_0 text_size={px(metrics.font_size)}
            line_height={relative(metrics.line_height)} />
    }
        .line_clamp(metrics.max_lines.max(1))
        .max_h(px(metrics.font_size
            * metrics.line_height
            * metrics.max_lines.max(1) as f32))
        .overflow_hidden()
        .child(linked)
        .into_any_element()
}

fn chip(text: String, tokens: &DesignTokens, dark: bool, missing: bool) -> gpui::Div {
    let theme = tokens.theme(dark);
    rsx! {
        <div mx={px(2.0)} px={px(6.0)} py={px(2.0)} rounded={px(4.0)} bg={color(&theme.tertiary)} />
    }
        .text_color(color(if missing {
            &theme.error
        } else {
            &theme.text_secondary
        }))
        .child(text)
}

fn render_mixed(
    parts: &[MixedPart],
    resources: &ContentResources,
    metrics: ContentMetrics,
    tokens: &DesignTokens,
    dark: bool,
    observer: Option<ContentPaintObserver>,
) -> AnyElement {
    match observer {
        None => inline_flow::render_mixed_inline(parts, resources, metrics, tokens, dark),
        observer => inline_flow::render_mixed_inline_with_observer(
            parts, resources, metrics, tokens, dark, observer,
        ),
    }
}

struct ContentTooltip(String);
impl gpui::Render for ContentTooltip {
    fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        rsx! {
            <div p_1 text_size={px(12.0)}>
                {self.0.clone()}
            </div>
        }
    }
}

fn render_steps(
    steps: &StepProjection,
    resources: &ContentResources,
    width: f32,
    tokens: &DesignTokens,
    dark: bool,
    observer: Option<ContentPaintObserver>,
) -> AnyElement {
    let theme = tokens.theme(dark);
    let mut lines = rsx! {
        <div flex flex_col gap={px(1.0)} min_w_0 />
    };
    for (index, line) in steps.lines.iter().enumerate() {
        let icon = resources.icons.get(&line.icon).and_then(Option::as_ref);
        let mut row = rsx! {
            <div flex items_center gap={px(6.0)} min_w_0 />
        }
            .when(line.disabled, |row| row.opacity(0.55));
        row = if let Some(icon) = icon {
            let fallback_path = tokens.icons["list"].clone();
            let foreground = color(&theme.text);
            row.child(observed_content_part(
                tracked_image(
                    icon.clone(),
                    img(icon.clone())
                        .size(px(14.0))
                        .flex_shrink_0()
                        .object_fit(ObjectFit::Contain)
                        .with_fallback(move || {
                            crate::crisp_svg::svg()
                                .path(fallback_path.clone())
                                .size(px(14.0))
                                .text_color(foreground)
                                .into_any_element()
                        })
                        .into_any_element(),
                ),
                &observer,
                "step-icon",
                index,
                true,
                Some(icon.clone()),
            ))
        } else {
            // Use the established generic list asset; never resolve icon paths in render.
            row.child(observed_content_part(
                crate::crisp_svg::svg()
                    .path(tokens.icons["list"].clone())
                    .size(px(14.0))
                    .flex_shrink_0()
                    .text_color(color(&theme.text))
                    .into_any_element(),
                &observer,
                "step-icon",
                index,
                false,
                None,
            ))
        };
        let title_len = line.title.len();
        let text = format!("{}{}", line.title, line.suffix);
        let mut highlights = vec![(
            0..title_len,
            HighlightStyle {
                font_weight: Some(FontWeight::SEMIBOLD),
                ..Default::default()
            },
        )];
        if text.len() > title_len {
            highlights.push((
                title_len..text.len(),
                HighlightStyle {
                    color: Some(color(&theme.text_muted)),
                    ..Default::default()
                },
            ));
        }
        lines = lines.child(
            row.child(
                rsx! {
                    <div min_w_0 flex_1 whitespace_nowrap text_ellipsis overflow_hidden>
                        {observed_content_part(
                        StyledText::new(text)
                            .with_highlights(highlights)
                            .into_any_element(),
                        &observer,
                        "step-text",
                        index,
                        true,
                        None,
                    )}
                    </div>
                },
            ),
        );
    }
    if steps.lines.is_empty() {
        lines = lines.child(if !steps.preview_text.is_empty() {
            steps.preview_text.clone()
        } else {
            tr("Quicker 步骤", "Quicker steps").to_owned()
        });
    }
    rsx! {
        <div relative w={px(width.max(1.0))} min_w_0 pr={px(54.0)} text_size={px(12.0)}
            line_height={relative(1.35)}>
            {lines}
        </div>
    }
        .when(!steps.stats.is_empty(), |root| {
            root.child(
                rsx! {
                    <div absolute right={px(2.0)} bottom_0 text_size={px(11.0)}
                        line_height={relative(1.2)} text_color={color(&theme.text_muted)}>
                        {steps.stats.clone()}
                    </div>
                },
            )
        })
        .into_any_element()
}

/// Keep normal GPUI text shaping/wrapping intact. Only the small hover button
/// is positioned from StyledText's measured byte indices; there is no tokenized
/// text flex layout, so paths/links do not introduce artificial wraps or spaces.
struct LinkedText {
    id: ElementId,
    text: StyledText,
    projection: TextProjection,
    item_id: String,
    on_open: OpenHandler,
    observer: Option<ContentPaintObserver>,
    accent: gpui::Hsla,
    surface: gpui::Hsla,
}

#[derive(Default)]
struct LinkState {
    text: String,
    hovered: Rc<Cell<Option<usize>>>,
}

struct LinkPrepaint {
    hovered: Rc<Cell<Option<usize>>>,
    button: Option<AnyElement>,
    button_bounds: Option<Bounds<Pixels>>,
}

impl IntoElement for LinkedText {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for LinkedText {
    type RequestLayoutState = ();
    type PrepaintState = LinkPrepaint;
    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }
    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }
    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        inspector: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        self.text.request_layout(None, inspector, window, cx)
    }
    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        state: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> LinkPrepaint {
        self.text
            .prepaint(None, inspector, bounds, state, window, cx);
        window.with_element_state::<LinkState, _>(id.unwrap(), |state, window| {
            let mut state = state.unwrap_or_default();
            if state.text != self.projection.text {
                state.text.clone_from(&self.projection.text);
                state.hovered.set(None);
            }
            let mut button = None;
            let mut button_bounds = None;
            if let Some(index) = state.hovered.get() {
                if let Some(link) = self.projection.links.get(index) {
                    if let Some(origin) = self.text.layout().position_for_index(link.range.start) {
                        let height = self.text.layout().line_height().max(px(18.0));
                        let width = px(40.0).min(bounds.size.width);
                        let area = Bounds::new(point(origin.x, origin.y), size(width, height));
                        if bounds.intersects(&area) {
                            let request = ContentOpenRequest {
                                item_id: self.item_id.clone(),
                                target: link.target.clone(),
                            };
                            let on_open = self.on_open.clone();
                            let mut open = rsx! {
                                <div id={("open-inline-link", index)} w={width} h={height} flex
                                    items_center justify_center rounded={px(3.0)}
                                    bg={self.accent} text_color={self.surface}
                                    text_size={px(10.0)} line_height={relative(1.0)}
                                    cursor_pointer />
                            }
                                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                .on_mouse_up(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                .on_click(move |event, window, cx| {
                                    cx.stop_propagation();
                                    if event.click_count() <= 1 {
                                        on_open(request.clone(), window, cx);
                                    }
                                })
                                .child(tr("打开", "Open"))
                                .into_any_element();
                            open.prepaint_as_root(
                                area.origin,
                                size(
                                    AvailableSpace::Definite(width),
                                    AvailableSpace::Definite(height),
                                ),
                                window,
                                cx,
                            );
                            button = Some(open);
                            button_bounds = Some(area);
                        }
                    }
                }
            }
            (
                LinkPrepaint {
                    hovered: state.hovered.clone(),
                    button,
                    button_bounds,
                },
                state,
            )
        })
    }
    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        state: &mut (),
        prepaint: &mut LinkPrepaint,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.text
            .paint(None, inspector, bounds, state, &mut (), window, cx);
        let layout = self.text.layout().clone();
        report_content_paint(
            &self.observer,
            ContentPaintPart {
                role: "step-text",
                index: 0,
                bounds: layout.bounds(),
                available: true,
            },
            window,
        );
        let links = self.projection.links.clone();
        let hovered = prepaint.hovered.clone();
        let button_bounds = prepaint.button_bounds;
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, _| {
            if phase != DispatchPhase::Bubble {
                return;
            }
            let updated = if button_bounds.is_some_and(|area| area.contains(&event.position)) {
                hovered.get()
            } else if bounds.contains(&event.position) {
                layout
                    .index_for_position(event.position)
                    .ok()
                    .and_then(|position| {
                        links.iter().position(|link| link.range.contains(&position))
                    })
            } else {
                None
            };
            if hovered.replace(updated) != updated {
                window.refresh();
            }
        });
        if let Some(button) = prepaint.button.as_mut() {
            button.paint(window, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vector_attributes_and_raw_fill_spec_detection_are_safe() {
        assert_eq!(xml_attribute("a&\"'<b>"), "a&amp;&quot;&apos;&lt;b&gt;");
        assert!(!explicit_icon_color("fa:Light_ListOl"));
        assert!(!explicit_icon_color("Light_ListOl:"));
        assert!(explicit_icon_color("FA:Light_Scroll:#6aaded"));
        assert!(explicit_icon_color("Light_ListOl:warning"));
    }

    #[test]
    fn fa_paint_follows_the_final_web_current_color_rule() {
        assert_eq!(
            effective_fa_fill("#224466"),
            colors::RgbaColor {
                red: 0x22,
                green: 0x44,
                blue: 0x66,
                alpha: 255,
            }
        );
        assert_eq!(
            effective_fa_fill("rgba(1, 2, 3, .5)"),
            colors::RgbaColor {
                red: 1,
                green: 2,
                blue: 3,
                alpha: 128,
            }
        );
        assert_eq!(
            effective_fa_fill("bad-css"),
            colors::RgbaColor {
                red: 0,
                green: 0,
                blue: 0,
                alpha: 255,
            }
        );
    }

    #[test]
    fn background_icon_preparation_accepts_raster_and_rejects_invalid_bytes() {
        let raster = image::DynamicImage::new_rgba8(16, 16);
        let mut bytes = Cursor::new(Vec::new());
        raster
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        let prepared = prepare_icon_bytes(bytes.into_inner()).unwrap();
        assert_eq!(prepared.format(), ImageFormat::Png);
        assert!(image_retained_bytes(&prepared) < CONTENT_RESOURCE_BUDGET_BYTES);
        assert!(prepare_icon_bytes(vec![1, 2, 3]).is_none());
    }

    #[test]
    fn compact_and_list_image_loaders_preserve_natural_dimensions_and_distinct_pixel_bounds() {
        let image = image::DynamicImage::new_rgb8(600, 300);
        let mut bytes = Cursor::new(Vec::new());
        image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
        let bytes = bytes.into_inner();
        let full = list_image(bytes.clone()).unwrap();
        let full_dimensions = image::ImageReader::with_format(
            Cursor::new(full.image.bytes()),
            image::ImageFormat::Png,
        )
        .into_dimensions()
        .unwrap();
        assert_eq!(full_dimensions, (600, 300));
        assert_eq!((full.width, full.height), (600.0, 300.0));
        let loaded = compact_image(bytes).unwrap();
        let compact_dimensions = image::ImageReader::with_format(
            Cursor::new(loaded.image.bytes()),
            image::ImageFormat::Png,
        )
        .into_dimensions()
        .unwrap();
        assert_eq!(compact_dimensions, (112, 56));
        assert_eq!((loaded.width, loaded.height), (600.0, 300.0));
        assert!(compact_image(vec![0, 1, 2]).is_none());
        assert!(list_image(vec![0, 1, 2]).is_none());
    }

    fn gif_header(width: u16, height: u16) -> Vec<u8> {
        let mut bytes = b"GIF89a".to_vec();
        bytes.extend(width.to_le_bytes());
        bytes.extend(height.to_le_bytes());
        bytes.extend([0, 0, 0]);
        bytes
    }

    fn gif_descriptor(bytes: &mut Vec<u8>, width: u16, height: u16) {
        bytes.extend([0x2c, 0, 0, 0, 0]);
        bytes.extend(width.to_le_bytes());
        bytes.extend(height.to_le_bytes());
        // Descriptor-looking bytes inside a subblock must not count as frames.
        bytes.extend([0, 2, 2, 0x2c, 0x21, 0]);
    }

    #[test]
    fn gif_budget_walks_blocks_counts_full_canvases_and_rejects_bad_subframes() {
        let mut bytes = gif_header(20, 30);
        gif_descriptor(&mut bytes, 20, 30);
        bytes.extend([0x21, 0xfe, 2, 0x2c, 0x2c, 0]);
        gif_descriptor(&mut bytes, 1, 1);
        bytes.push(0x3b);
        assert_eq!(gif_decoded_bytes(&bytes), Some((20 * 30 * 4 + ANIMATION_FRAME_METADATA_BYTES) * 2));
        bytes.pop();
        assert!(gif_decoded_bytes(&bytes).is_none());
        let mut bad = gif_header(1, 1);
        gif_descriptor(&mut bad, 60_000, 60_000);
        bad.push(0x3b);
        assert!(gif_decoded_bytes(&bad).is_none());
    }

    #[test]
    fn gif_budget_rejects_accumulated_frames_and_large_canvas_before_decode() {
        let mut bytes = gif_header(2048, 1024);
        for _ in 0..3 {
            gif_descriptor(&mut bytes, 1, 1);
        }
        bytes.push(0x3b);
        assert!(gif_decoded_bytes(&bytes).is_none());
        assert!(gif_decoded_bytes(&gif_header(60_000, 60_000)).is_none());
    }

    #[test]
    fn gif_budget_caps_tiny_frame_count_and_charges_frame_metadata_without_decoding() {
        let make = |frames| {
            let mut bytes = gif_header(1, 1);
            for _ in 0..frames { gif_descriptor(&mut bytes, 1, 1); }
            bytes.push(0x3b);
            bytes
        };
        let expected = (4 + ANIMATION_FRAME_METADATA_BYTES) * MAX_ANIMATION_FRAMES;
        assert!(expected * 2 < CONTENT_RESOURCE_BUDGET_BYTES);
        assert_eq!(gif_decoded_bytes(&make(MAX_ANIMATION_FRAMES)), Some(expected));
        assert_eq!(gif_decoded_bytes(&make(MAX_ANIMATION_FRAMES + 1)), None);
    }

    #[test]
    fn webp_budget_caps_tiny_frame_count_and_charges_frame_metadata_without_decoding() {
        let make = |frames| {
            let chunk = |tag: &[u8; 4], data: &[u8], bytes: &mut Vec<u8>| {
                bytes.extend(tag);
                bytes.extend((data.len() as u32).to_le_bytes());
                bytes.extend(data);
                if data.len() % 2 != 0 { bytes.push(0); }
            };
            let mut body = b"WEBP".to_vec();
            chunk(b"VP8X", &[2, 0, 0, 0, 0, 0, 0, 0, 0, 0], &mut body);
            chunk(b"ANIM", &[0; 6], &mut body);
            // Parser-only frame descriptors; never ask a pixel decoder to
            // allocate or execute these thousands of frame payloads.
            let mut frame = vec![0; 16];
            chunk(b"VP8L", &[0x2f, 0, 0, 0, 0], &mut frame);
            for _ in 0..frames { chunk(b"ANMF", &frame, &mut body); }
            let mut bytes = b"RIFF".to_vec();
            bytes.extend((body.len() as u32).to_le_bytes());
            bytes.extend(body);
            bytes
        };
        let expected = (4 + ANIMATION_FRAME_METADATA_BYTES) * MAX_ANIMATION_FRAMES;
        assert!(expected * 2 < CONTENT_RESOURCE_BUDGET_BYTES);
        assert_eq!(webp_decoded_bytes(&make(MAX_ANIMATION_FRAMES)), Some((expected, true)));
        assert_eq!(webp_decoded_bytes(&make(MAX_ANIMATION_FRAMES + 1)), None);
    }

    #[test]
    fn retained_resource_weight_counts_intrinsic_svg_and_loaded_thumbnail_pixels() {
        let mut resources = ContentResources::default();
        let svg = Arc::new(Image::from_bytes(
            ImageFormat::Svg,
            b"<svg width=\"56\" height=\"56\" viewBox=\"0 0 512 512\"></svg>".to_vec(),
        ));
        resources.icons.insert("step".into(), Some(svg));
        assert!(resources.retained_bytes() >= 56 * 56 * 4 * 2);
        let image = image::DynamicImage::new_rgb8(600, 300);
        let mut bytes = Cursor::new(Vec::new());
        image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
        let loaded = compact_image(bytes.into_inner()).unwrap();
        assert!(image_retained_bytes(&loaded.image) >= 112 * 56 * 4 * 2);
        assert!(image_retained_bytes(&loaded.image) < 600 * 300 * 4);
    }

    #[test]
    fn oversized_and_unknown_resource_dimensions_become_bounded_fallback() {
        let mut resources = ContentResources::default();
        resources.icons.insert(
            "huge".into(),
            Some(Arc::new(Image::from_bytes(
                ImageFormat::Svg,
                b"<svg width=\"60000\" height=\"60000\"></svg>".to_vec(),
            ))),
        );
        resources.icons.insert(
            "bad".into(),
            Some(Arc::new(Image::from_bytes(ImageFormat::Png, vec![1, 2]))),
        );
        resources.limit_retained_bytes(CONTENT_RESOURCE_BUDGET_BYTES);
        assert!(resources.icons.values().all(Option::is_none));
        assert!(resources.retained_bytes() < 1024);
    }
}
