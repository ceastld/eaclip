//! Full-window wallpaper resources and the existing Web blend contract.
//!
//! On startup/config replies call `BackgroundState::configure`, then dispatch
//! its request through the owner's background executor with `load_background`.
//! Apply the reply on the UI owner with its current config; notify only for an
//! accepted result. Rendering performs no filesystem/config lookup. Keep one
//! physical load task until its worker finishes; new config only replaces the
//! queued request. Then dispatch `pending_request()` after the old completion.
//! Dropping a task wrapper cannot stop a blocking Core file read. Share a
//! prepared snapshot with other window surfaces.
//!
//! This supplies Cover + translucent theme layers. GPUI has no equivalent of
//! Web backdrop-filter blur(10px) saturate(1.08); OS acrylic is not that effect.
//! The shell's only backdrop is this fixed wallpaper, so frost is baked into
//! the prepared raster once instead (see `frost_raster`).

use std::{
    io::Cursor,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use cliphost_core::host::HostState;
use gpui::{div, img, prelude::*, AnyElement, Image, ImageFormat, ObjectFit};
use serde_json::Value;

use crate::native_content::{
    self, ContentImage, ContentImageHolderLease, ContentResources, CONTENT_RESOURCE_BUDGET_BYTES,
};

const MAX_SOURCE_BYTES: usize = 50 * 1024 * 1024;
const MAX_DECODE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_RASTER_EDGE: u32 = 1536;
/// Web frost: `blur(10px) saturate(1.08)` in CSS px.
const FROST_BLUR_DIP: f32 = 10.0;
const FROST_SATURATION: f32 = 1.08;
/// Cover scale differs per window. Sigma targets a ~600 DIP short client edge
/// (main 420×640, settings 780×580); other sizes get a proportional blur.
const FROST_REFERENCE_EDGE_DIP: f32 = 600.0;
/// Blurred content is low-frequency: a small raster keeps the CPU pass and
/// retained bytes low, and the linear sprite sampler scales it to the window.
const FROST_RASTER_EDGE: u32 = 480;

fn configured_frost(config: &Value) -> bool {
    config["windowBackgroundFrostedGlass"]
        .as_bool()
        .unwrap_or(true)
}

fn js_trim(value: &str) -> &str {
    value.trim_matches(|c: char| {
        matches!(c as u32,
        0x09..=0x0D | 0x20 | 0xA0 | 0x1680 | 0x2000..=0x200A
        | 0x2028 | 0x2029 | 0x202F | 0x205F | 0x3000 | 0xFEFF)
    })
}

fn configured_path(config: &Value) -> String {
    js_trim(config["windowBackgroundImage"].as_str().unwrap_or_default()).to_string()
}

#[derive(Clone, Debug, PartialEq)]
pub struct WindowBackgroundOptions {
    pub path: String,
    pub blend: u8,
    pub frosted_glass: bool,
}

impl Default for WindowBackgroundOptions {
    fn default() -> Self {
        Self {
            path: String::new(),
            blend: 70,
            frosted_glass: true,
        }
    }
}

impl WindowBackgroundOptions {
    pub fn from_config(config: &Value) -> Self {
        let blend = config["windowBackgroundBlend"]
            .as_f64()
            .filter(|value| value.is_finite())
            .map(|value| {
                let floor = value.floor();
                (floor + if value - floor >= 0.5 { 1.0 } else { 0.0 }).clamp(0.0, 100.0) as u8
            })
            .unwrap_or(70);
        Self {
            path: configured_path(config),
            blend,
            frosted_glass: configured_frost(config),
        }
    }

    pub fn image_opacity(&self) -> f32 {
        0.15 + 0.85 * f32::from(self.blend) / 100.0
    }

    pub fn overlay_strength(&self) -> f32 {
        1.0 - 0.85 * f32::from(self.blend) / 100.0
    }

    /// Multiply the existing theme color's alpha, retaining Custom transparency.
    /// Web shell: .78; row: .72; secondary/control: .68; tertiary: .60;
    /// toolbar button: .62; selected (after its existing color mix): .74.
    /// Apply only while a prepared wallpaper is actually displayed.
    pub fn glass_alpha(&self, original_alpha: f32, surface_weight: f32) -> f32 {
        let alpha = |value: f32| {
            if value.is_finite() {
                value.clamp(0.0, 1.0)
            } else {
                0.0
            }
        };
        alpha(original_alpha) * alpha(surface_weight) * self.overlay_strength()
    }
}

#[derive(Clone)]
pub struct BackgroundRequest {
    generation: u64,
    path: String,
    frosted: bool,
    epoch: Arc<AtomicU64>,
}

impl BackgroundRequest {
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn path(&self) -> &str {
        &self.path
    }
    pub fn frosted(&self) -> bool {
        self.frosted
    }
    fn current(&self) -> bool {
        self.epoch.load(Ordering::Acquire) == self.generation
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackgroundError {
    ReadFailed,
    DecodeFailed,
    SourceTooLarge,
    ResourceLimit,
}

/// A bounded prepared resource. The lease travels with shared snapshots; its
/// last drop uses the existing App-side image retirement scheduler.
pub struct PreparedBackground {
    path: String,
    frosted: bool,
    image: Arc<Image>,
    _lease: ContentImageHolderLease,
}

impl PreparedBackground {
    pub fn path(&self) -> &str {
        &self.path
    }
    pub fn frosted(&self) -> bool {
        self.frosted
    }
    fn matches(&self, options: &WindowBackgroundOptions) -> bool {
        self.path == options.path && self.frosted == options.frosted_glass
    }
}

enum BackgroundResult {
    Ready(Arc<PreparedBackground>),
    Unavailable(BackgroundError),
    Superseded,
}

pub struct BackgroundReply {
    request: BackgroundRequest,
    result: BackgroundResult,
}

pub struct BackgroundState {
    options: WindowBackgroundOptions,
    epoch: Arc<AtomicU64>,
    pending: Option<BackgroundRequest>,
    image: Option<Arc<PreparedBackground>>,
    error: Option<BackgroundError>,
}

impl Default for BackgroundState {
    fn default() -> Self {
        Self {
            options: WindowBackgroundOptions::default(),
            epoch: Arc::new(AtomicU64::new(0)),
            pending: None,
            image: None,
            error: None,
        }
    }
}

impl BackgroundState {
    pub fn options(&self) -> &WindowBackgroundOptions {
        &self.options
    }
    pub fn snapshot(&self) -> Option<Arc<PreparedBackground>> {
        self.image.clone()
    }
    pub fn is_loading(&self) -> bool {
        self.pending.is_some()
    }
    pub fn pending_request(&self) -> Option<BackgroundRequest> {
        self.pending.clone()
    }
    pub fn error(&self) -> Option<BackgroundError> {
        self.error
    }

    /// Called for authoritative config arrivals, not from every paint. Blend
    /// changes reuse the resource; path or frost changes prepare a new raster
    /// (frost is baked into it). The previous image stays while loading, as
    /// Web does; a failed latest request clears that image. Polling the same
    /// failed source does not retry until the path or frost changes.
    pub fn configure(&mut self, config: &Value) -> Option<BackgroundRequest> {
        let options = WindowBackgroundOptions::from_config(config);
        let source_changed = options.path != self.options.path
            || options.frosted_glass != self.options.frosted_glass;
        self.options = options;
        if self.options.path.is_empty() {
            self.retire();
            return None;
        }
        if !source_changed && self.error.is_some() {
            return None;
        }
        let displayed = self
            .image
            .as_ref()
            .is_some_and(|image| image.matches(&self.options));
        if source_changed && displayed {
            self.advance();
            self.pending = None;
            self.error = None;
            return None;
        }
        if !source_changed && (self.pending.is_some() || displayed) {
            return None;
        }
        let request = BackgroundRequest {
            generation: self.advance(),
            path: self.options.path.clone(),
            frosted: self.options.frosted_glass,
            epoch: self.epoch.clone(),
        };
        self.pending = Some(request.clone());
        self.error = None;
        Some(request)
    }

    /// Returns true only for an accepted completion that changes owner state.
    /// Stale replies cannot clear or replace a newer image, including A→B→A.
    pub fn apply(&mut self, reply: BackgroundReply, current_config: &Value) -> bool {
        let request = &reply.request;
        let accepts = self.pending.as_ref().is_some_and(|pending| {
            Arc::ptr_eq(&self.epoch, &request.epoch)
                && pending.generation == request.generation
                && request.current()
                && pending.path == request.path
                && pending.frosted == request.frosted
                && self.options.path == request.path
                && self.options.frosted_glass == request.frosted
                && configured_path(current_config) == request.path
                && configured_frost(current_config) == request.frosted
        });
        if !accepts {
            return false;
        }
        match reply.result {
            BackgroundResult::Ready(image) => {
                // Incoming lease was acquired before releasing the old holder.
                self.image = Some(image);
                self.error = None;
            }
            BackgroundResult::Unavailable(error) => {
                self.image = None;
                self.error = Some(error);
            }
            // A current worker may notice a Core path change before the UI
            // config arrives. End its logical pending state without clearing
            // the previous image; the next config can retry the same path.
            BackgroundResult::Superseded => {}
        }
        self.pending = None;
        true
    }

    /// Empty path, window retirement and owner disposal invalidate all replies.
    /// Blend/frost preferences remain available when the image is cleared.
    pub fn retire(&mut self) {
        self.advance();
        self.pending = None;
        self.image = None;
        self.error = None;
    }

    fn advance(&self) -> u64 {
        self.epoch.fetch_add(1, Ordering::AcqRel).wrapping_add(1)
    }
}

impl Drop for BackgroundState {
    fn drop(&mut self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
    }
}

/// Blocking worker entry; never call this from render or the UI executor.
/// Core captures the actual source path under its config lock and reads that
/// captured source. Accept only that path and the original owner epoch, so an
/// intervening Core A→B→A change cannot relabel B bytes as an A image.
/// Core bounds encoded content at MAX_SOURCE_BYTES, including file growth
/// during the read. Decode and retained-resource limits apply afterwards.
pub fn load_background(host: &HostState, request: BackgroundRequest) -> BackgroundReply {
    if !request.current() {
        return BackgroundReply {
            request,
            result: BackgroundResult::Superseded,
        };
    }
    let bounded = host.window_background_bytes_snapshot_bounded(MAX_SOURCE_BYTES);
    from_snapshot(
        request,
        bounded.snapshot.captured_path,
        bounded.snapshot.content,
        bounded.oversized,
    )
}

fn from_snapshot(
    request: BackgroundRequest,
    captured_path: Option<String>,
    content: Option<(Vec<u8>, String)>,
    source_too_large: bool,
) -> BackgroundReply {
    let result =
        if !request.current() || captured_path.as_deref().map(js_trim) != Some(request.path()) {
            BackgroundResult::Superseded
        } else if source_too_large {
            BackgroundResult::Unavailable(BackgroundError::SourceTooLarge)
        } else {
            match content {
                Some((bytes, _mime)) => match prepare_background(
                    &request.path,
                    request.frosted,
                    bytes,
                ) {
                    Ok(image) if request.current() => BackgroundResult::Ready(image),
                    Ok(_) => BackgroundResult::Superseded,
                    Err(error) if request.current() => BackgroundResult::Unavailable(error),
                    Err(_) => BackgroundResult::Superseded,
                },
                None => BackgroundResult::Unavailable(BackgroundError::ReadFailed),
            }
        };
    BackgroundReply { request, result }
}

fn prepare_background(
    path: &str,
    frosted: bool,
    bytes: Vec<u8>,
) -> Result<Arc<PreparedBackground>, BackgroundError> {
    if bytes.len() > MAX_SOURCE_BYTES {
        return Err(BackgroundError::SourceTooLarge);
    }
    let mut reader = image::ImageReader::new(Cursor::new(&bytes))
        .with_guessed_format()
        .map_err(|_| BackgroundError::DecodeFailed)?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    limits.max_image_width = Some(16384);
    limits.max_image_height = Some(16384);
    reader.limits(limits);
    let format = reader.format().ok_or(BackgroundError::DecodeFailed)?;
    let decoded = reader.decode().map_err(|_| BackgroundError::DecodeFailed)?;
    let source = if format == image::ImageFormat::Gif {
        // Keep GIF animation only when the existing retained-frame estimator
        // admits the original stream; never silently turn it into a still.
        // Frames stay unfrosted: per-frame blur would need a GIF re-encode.
        ContentImage {
            width: decoded.width() as f32,
            height: decoded.height() as f32,
            image: Arc::new(Image::from_bytes(ImageFormat::Gif, bytes)),
            render: None,
        }
    } else {
        let raster = if frosted {
            frost_raster(&decoded)
        } else {
            decoded.thumbnail(MAX_RASTER_EDGE, MAX_RASTER_EDGE)
        };
        let mut encoded = Cursor::new(Vec::new());
        raster
            .write_to(&mut encoded, image::ImageFormat::Png)
            .map_err(|_| BackgroundError::DecodeFailed)?;
        let mut encoded = encoded.into_inner();
        encoded.shrink_to_fit();
        ContentImage {
            width: raster.width() as f32,
            height: raster.height() as f32,
            image: Arc::new(Image::from_bytes(ImageFormat::Png, encoded)),
            render: None,
        }
    };
    let mut resources = ContentResources::default();
    resources
        .images
        .insert("window-background".into(), Some(source));
    let budget = CONTENT_RESOURCE_BUDGET_BYTES
        .saturating_sub(ContentImageHolderLease::estimate_retained_bytes(1));
    resources.limit_retained_bytes(budget);
    let image = resources.images["window-background"]
        .as_ref()
        .ok_or(BackgroundError::ResourceLimit)?
        .image
        .clone();
    let lease = native_content::retain_content_images(&resources);
    Ok(Arc::new(PreparedBackground {
        path: path.to_string(),
        frosted,
        image,
        _lease: lease,
    }))
}

/// Web blurs the shell's backdrop, which is only this fixed Cover wallpaper
/// over a uniform page color, so blurring the wallpaper once yields the same
/// picture without a per-frame backdrop pass. Blur runs on premultiplied
/// sRGB values like Chromium's filter; saturate is the CSS filter matrix.
fn frost_raster(decoded: &image::DynamicImage) -> image::DynamicImage {
    let raster = if decoded.width().max(decoded.height()) > FROST_RASTER_EDGE {
        decoded.thumbnail(FROST_RASTER_EDGE, FROST_RASTER_EDGE)
    } else {
        decoded.clone()
    };
    let short_edge = raster.width().min(raster.height()) as f32;
    let sigma = (FROST_BLUR_DIP * short_edge / FROST_REFERENCE_EDGE_DIP).max(0.5);
    let mut pixels = raster.to_rgba32f();
    for pixel in pixels.pixels_mut() {
        let alpha = pixel[3];
        for channel in &mut pixel.0[..3] {
            *channel *= alpha;
        }
    }
    let mut pixels = image::imageops::blur(&pixels, sigma);
    let s = FROST_SATURATION;
    for pixel in pixels.pixels_mut() {
        let alpha = pixel[3].clamp(0.0, 1.0);
        let [r, g, b] = if alpha > 0.0 {
            [pixel[0] / alpha, pixel[1] / alpha, pixel[2] / alpha]
        } else {
            [0.0; 3]
        };
        let saturated = [
            (0.213 + 0.787 * s) * r + (0.715 - 0.715 * s) * g + (0.072 - 0.072 * s) * b,
            (0.213 - 0.213 * s) * r + (0.715 + 0.285 * s) * g + (0.072 - 0.072 * s) * b,
            (0.213 - 0.213 * s) * r + (0.715 - 0.715 * s) * g + (0.072 + 0.928 * s) * b,
        ];
        for (channel, value) in pixel.0[..3].iter_mut().zip(saturated) {
            *channel = value.clamp(0.0, 1.0);
        }
        pixel[3] = alpha;
    }
    image::DynamicImage::ImageRgba8(image::DynamicImage::ImageRgba32F(pixels).to_rgba8())
}

/// Passive full-client Cover layer. Image layout fills the parent and may
/// enlarge/crop naturally; it never uses the list image's 1/DPI sizing rule.
/// Put this first in a relative window root, before the translucent shell.
pub fn render_background_image(
    image: &Arc<PreparedBackground>,
    options: &WindowBackgroundOptions,
) -> AnyElement {
    let child = img(image.image.clone())
        .size_full()
        .object_fit(ObjectFit::Cover)
        .with_fallback(|| div().into_any_element())
        .into_any_element();
    div()
        .absolute()
        .inset_0()
        .overflow_hidden()
        .opacity(options.image_opacity())
        .child(native_content::render_resource_image(
            image.image.clone(),
            child,
        ))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config(path: &str) -> Value {
        json!({"windowBackgroundImage":path, "windowBackgroundBlend":70, "windowBackgroundFrostedGlass":true})
    }

    fn ready(request: BackgroundRequest) -> BackgroundReply {
        let mut bytes = Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(1, 1)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        let image =
            prepare_background(request.path(), request.frosted(), bytes.into_inner()).unwrap();
        BackgroundReply {
            request,
            result: BackgroundResult::Ready(image),
        }
    }

    #[test]
    fn blend_matches_web_endpoints_and_preserves_custom_alpha() {
        for (blend, image, overlay) in [(0, 0.15, 1.0), (70, 0.745, 0.405), (100, 1.0, 0.15)] {
            let options =
                WindowBackgroundOptions::from_config(&json!({"windowBackgroundBlend":blend}));
            assert!((options.image_opacity() - image).abs() < 0.0001);
            assert!((options.overlay_strength() - overlay).abs() < 0.0001);
            assert!((options.glass_alpha(0.5, 0.78) - 0.5 * 0.78 * overlay).abs() < 0.0001);
        }
        let defaults = WindowBackgroundOptions::from_config(&Value::Null);
        assert_eq!(defaults, WindowBackgroundOptions::default());
        let options = WindowBackgroundOptions::from_config(
            &json!({"windowBackgroundImage":" \u{FEFF}a\u{A0}", "windowBackgroundBlend":101.4, "windowBackgroundFrostedGlass":false}),
        );
        assert_eq!(
            (options.path.as_str(), options.blend, options.frosted_glass),
            ("a", 100, false)
        );
        assert_eq!(
            WindowBackgroundOptions::from_config(&json!({"windowBackgroundBlend":-1})).blend,
            0
        );
        assert_eq!(
            WindowBackgroundOptions::from_config(&json!({"windowBackgroundBlend":69.5})).blend,
            70
        );
    }

    #[test]
    fn newest_failure_clears_old_image_and_superseded_requests_can_retry() {
        let mut state = BackgroundState::default();
        let a = config("memory-a");
        let b = config("memory-b");
        let request = state.configure(&a).unwrap();
        assert!(state.apply(ready(request), &a));
        let pending = state.configure(&b).unwrap();
        assert_eq!(state.snapshot().unwrap().path(), "memory-a");
        assert!(state.apply(
            BackgroundReply {
                request: pending.clone(),
                result: BackgroundResult::Superseded
            },
            &b
        ));
        assert!(!state.is_loading());
        assert_eq!(state.snapshot().unwrap().path(), "memory-a");
        // The displayed A must not be mistaken for a successful B cache entry.
        let retry = state.configure(&b).unwrap();
        assert!(!state.apply(ready(pending), &b));
        let mut blend_only = b.clone();
        blend_only["windowBackgroundBlend"] = json!(45);
        assert!(state.configure(&blend_only).is_none());
        assert_eq!(
            state.pending_request().unwrap().generation(),
            retry.generation()
        );
        assert!(state.apply(
            BackgroundReply {
                request: retry,
                result: BackgroundResult::Unavailable(BackgroundError::DecodeFailed)
            },
            &blend_only
        ));
        assert!(state.snapshot().is_none());
        assert_eq!(state.error(), Some(BackgroundError::DecodeFailed));
        assert!(state.configure(&blend_only).is_none());
    }

    #[test]
    fn failed_path_polls_and_blend_changes_do_not_retry_until_path_changes() {
        for error in [BackgroundError::ReadFailed, BackgroundError::DecodeFailed] {
            let mut state = BackgroundState::default();
            let a = config("memory-a");
            let b = config("memory-b");
            let request = state.configure(&a).unwrap();
            assert!(state.apply(ready(request), &a));
            let request = state.configure(&b).unwrap();
            let failed_generation = request.generation();
            assert!(state.apply(
                BackgroundReply {
                    request,
                    result: BackgroundResult::Unavailable(error),
                },
                &b,
            ));
            assert!(state.snapshot().is_none());
            for _ in 0..3 {
                assert!(state.configure(&b).is_none());
                assert!(state.pending_request().is_none());
                assert_eq!(state.error(), Some(error));
            }
            let mut blend_only = b.clone();
            blend_only["windowBackgroundImage"] = json!(" memory-b ");
            blend_only["windowBackgroundBlend"] = json!(45);
            assert!(state.configure(&blend_only).is_none());
            assert_eq!(state.options().blend, 45);
            assert_eq!(state.error(), Some(error));

            // Frost is baked into the raster, so toggling it retries once.
            let mut sharp = blend_only.clone();
            sharp["windowBackgroundFrostedGlass"] = json!(false);
            let retry = state.configure(&sharp).unwrap();
            assert!(!retry.frosted());
            assert!(state.error().is_none());
            assert!(state.apply(
                BackgroundReply {
                    request: retry,
                    result: BackgroundResult::Unavailable(error),
                },
                &sharp,
            ));
            assert!(state.configure(&sharp).is_none());
            assert_eq!(state.error(), Some(error));

            let changed = state.configure(&a).unwrap();
            assert_ne!(changed.generation(), failed_generation);
            assert!(state.error().is_none());
            assert!(state.configure(&b).is_some());

            assert!(state.configure(&config("")).is_none());
            assert!(state.configure(&b).is_some());
        }
    }

    #[test]
    fn aba_cached_return_and_clear_reject_old_and_foreign_replies() {
        let mut state = BackgroundState::default();
        let a = config("memory-a");
        let b = config("memory-b");
        let old_a = state.configure(&a).unwrap();
        let old_b = state.configure(&b).unwrap();
        let new_a = state.configure(&a).unwrap();
        assert!(!state.apply(ready(old_a), &a));
        assert!(state.apply(ready(new_a), &a));
        assert!(!state.apply(
            BackgroundReply {
                request: old_b,
                result: BackgroundResult::Unavailable(BackgroundError::ReadFailed)
            },
            &a
        ));
        let pending_b = state.configure(&b).unwrap();
        assert!(state.configure(&a).is_none());
        assert!(!state.is_loading());
        assert!(!state.apply(ready(pending_b), &a));
        assert_eq!(state.snapshot().unwrap().path(), "memory-a");
        assert!(state.configure(&a).is_none());

        let mut other = BackgroundState::default();
        let foreign = other.configure(&b).unwrap();
        let pending = state.configure(&b).unwrap();
        assert!(!state.apply(ready(foreign), &b));
        let cleared = json!({"windowBackgroundImage":"", "windowBackgroundBlend":45, "windowBackgroundFrostedGlass":false});
        assert!(state.configure(&cleared).is_none());
        assert!(!state.apply(ready(pending), &cleared));
        assert!(state.snapshot().is_none());
        assert!(!state.is_loading());
        assert_eq!(state.options().blend, 45);
        assert!(!state.options().frosted_glass);
    }

    #[test]
    fn snapshot_actual_path_prevents_relabeling_different_source_bytes() {
        let mut state = BackgroundState::default();
        let a = config("memory-a");
        let request = state.configure(&a).unwrap();
        // Even if UI config is A again, a snapshot that actually captured B
        // must not decode or publish those bytes under the A cache key.
        let reply = from_snapshot(
            request,
            Some("memory-b".into()),
            Some((vec![1, 2, 3], "image/png".into())),
            false,
        );
        assert!(matches!(reply.result, BackgroundResult::Superseded));
        assert!(state.apply(reply, &a));
        assert!(state.snapshot().is_none());
        assert!(state.error().is_none());
        let request = state.configure(&a).unwrap();
        let reply = from_snapshot(request, Some("memory-a".into()), None, false);
        assert!(state.apply(reply, &a));
        assert_eq!(state.error(), Some(BackgroundError::ReadFailed));
    }

    #[test]
    fn oversized_source_requires_actual_path_and_latest_failure_clears_old_image() {
        let mut state = BackgroundState::default();
        let a = config("memory-a");
        let b = config("memory-b");
        let request = state.configure(&a).unwrap();
        assert!(state.apply(ready(request), &a));
        let request = state.configure(&b).unwrap();
        let mismatched = from_snapshot(request, Some("memory-c".into()), None, true);
        assert!(matches!(mismatched.result, BackgroundResult::Superseded));
        assert!(state.apply(mismatched, &b));
        assert_eq!(state.snapshot().unwrap().path(), "memory-a");
        assert!(state.error().is_none());

        let request = state.configure(&b).unwrap();
        let oversized = from_snapshot(request, Some("memory-b".into()), None, true);
        assert!(matches!(
            oversized.result,
            BackgroundResult::Unavailable(BackgroundError::SourceTooLarge)
        ));
        assert!(state.apply(oversized, &b));
        assert!(state.snapshot().is_none());
        assert!(!state.is_loading());
        assert_eq!(state.error(), Some(BackgroundError::SourceTooLarge));
        assert!(state.configure(&b).is_none());
    }

    #[test]
    fn oversized_aba_worker_and_completed_error_cannot_clear_newer_image() {
        let mut state = BackgroundState::default();
        let a = config("memory-a");
        let b = config("memory-b");
        let old_a = state.configure(&a).unwrap();
        let completed_error = from_snapshot(old_a.clone(), Some("memory-a".into()), None, true);
        assert!(matches!(
            completed_error.result,
            BackgroundResult::Unavailable(BackgroundError::SourceTooLarge)
        ));

        state.configure(&b).unwrap();
        let new_a = state.configure(&a).unwrap();
        assert!(state.apply(ready(new_a), &a));
        let displayed = state.snapshot().unwrap();
        let stale_worker = from_snapshot(old_a, Some("memory-a".into()), None, true);
        assert!(matches!(stale_worker.result, BackgroundResult::Superseded));
        assert!(!state.apply(stale_worker, &a));
        assert!(!state.apply(completed_error, &a));
        assert!(Arc::ptr_eq(&displayed, &state.snapshot().unwrap()));
        assert!(state.error().is_none());
        assert!(!state.is_loading());
    }

    #[test]
    fn frost_toggle_reprepares_and_rejects_mismatched_replies() {
        let mut state = BackgroundState::default();
        let frosted = config("memory-a");
        let request = state.configure(&frosted).unwrap();
        assert!(request.frosted());
        assert!(state.apply(ready(request), &frosted));
        assert!(state.snapshot().unwrap().frosted());

        let mut sharp = frosted.clone();
        sharp["windowBackgroundFrostedGlass"] = json!(false);
        let pending = state.configure(&sharp).unwrap();
        assert!(!pending.frosted());
        // The frosted raster stays visible while the sharp one loads.
        assert!(state.snapshot().unwrap().frosted());
        // Toggling back returns to the displayed raster and drops the request.
        assert!(state.configure(&frosted).is_none());
        assert!(!state.is_loading());
        assert!(!state.apply(ready(pending), &frosted));
        assert!(state.snapshot().unwrap().frosted());

        let pending = state.configure(&sharp).unwrap();
        assert!(!state.apply(ready(pending.clone()), &frosted));
        assert!(state.apply(ready(pending), &sharp));
        assert!(!state.snapshot().unwrap().frosted());
        let mut blend_only = sharp.clone();
        blend_only["windowBackgroundBlend"] = json!(20);
        assert!(state.configure(&blend_only).is_none());
    }

    #[test]
    fn frost_raster_blurs_saturates_and_bounds_size() {
        // Hard black/white edge plus a saturated red column.
        let mut source = image::RgbaImage::from_pixel(960, 540, image::Rgba([0, 0, 0, 255]));
        for (x, _, pixel) in source.enumerate_pixels_mut() {
            if x >= 480 {
                *pixel = image::Rgba([255, 255, 255, 255]);
            }
            if (100..140).contains(&x) {
                *pixel = image::Rgba([200, 60, 60, 255]);
            }
        }
        let frosted = frost_raster(&image::DynamicImage::ImageRgba8(source)).to_rgba8();
        assert_eq!((frosted.width(), frosted.height()), (480, 270));
        // sigma = 10 * 270 / 600 = 4.5 raster px: the edge spreads over ~±13 px.
        let row = 135;
        let luma = |x: u32| frosted.get_pixel(x, row)[0];
        assert!(luma(200) < 4 && luma(280) > 251);
        assert!((110..146).contains(&luma(240)));
        assert!(luma(236) > 20 && luma(244) < 235);
        for x in 228..252 {
            assert!(luma(x) <= luma(x + 1));
        }
        assert!(frosted.pixels().all(|pixel| pixel[3] == 255));
        // Blur alone leaves the column center near (195, 58, 58);
        // saturate(1.08) pushes it further from gray.
        let red = frosted.get_pixel(60, row);
        assert!(red[0] > 200 && red[1] < 58 && red[2] < 58);

        let transparent = image::DynamicImage::new_rgba8(4, 4);
        let frosted = frost_raster(&transparent).to_rgba8();
        assert_eq!((frosted.width(), frosted.height()), (4, 4));
        assert!(frosted.pixels().all(|pixel| pixel.0 == [0, 0, 0, 0]));
    }

    #[test]
    fn frost_preference_selects_prepared_raster() {
        let mut bytes = Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(2000, 1000)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        let bytes = bytes.into_inner();
        let size = |prepared: &PreparedBackground| {
            let decoded = image::load_from_memory(&prepared.image.bytes).unwrap();
            (decoded.width(), decoded.height())
        };
        let sharp = prepare_background("memory", false, bytes.clone()).unwrap();
        assert!(!sharp.frosted());
        assert_eq!(size(&sharp), (1536, 768));
        let frosted = prepare_background("memory", true, bytes).unwrap();
        assert!(frosted.frosted());
        assert_eq!(size(&frosted), (480, 240));
    }
}
