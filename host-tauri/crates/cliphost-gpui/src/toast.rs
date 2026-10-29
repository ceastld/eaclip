//! Copy toast presentation. The core owns placement, the idle collapse timer,
//! hover pause and the menu lease; this retained no-activate overlay stacks
//! cards newest-first, reports hover and content height, and runs card actions.

use gpui_rsx::rsx;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use cliphost_core::{
    i18n::{displayed_language, tr, LanguageMode},
    HostState,
};
use gpui::{
    div, img, prelude::*, px, relative, Animation, AnimationExt, AnyElement, App, Bounds,
    Context, Element, ElementId, GlobalElementId, Hsla, InspectorElementId, LayoutId, Pixels, Task,
    Timer, Window, WindowHandle, WindowOptions,
};
use crate::crisp_svg::svg;
use serde_json::{json, Value};

use crate::{
    menu::{ContextMenuPopup, MenuAction, MenuEntry, SelectHandler},
    native_content::{
        self, load_content_resources,
        model::{project_mixed, project_steps, ContentProjection, TextProjection},
        ContentImageHolderLease, ContentMetrics, ContentResources,
    },
    platform,
    preview::DesignTokens,
    view::color,
};

#[path = "copy_toast_paste.rs"]
mod copy_toast_paste;
#[path = "toast_stack_motion.rs"]
mod toast_stack_motion;
use crate::overlay::actions::{auto_row_actions, row_item, RowAction, RowActionKind};
use crate::overlay::preview_model::{image_source, RasterSource};
use crate::overlay::theme::OverlayTheme;
use copy_toast_paste::CardTimer;
use toast_stack_motion::{height_report_needed, MotionFrame, StackAnchor, StackMotion};

const MAX_CARDS: usize = 5;
const DEFAULT_DURATION: Duration = Duration::from_millis(3_200);
const ENTER: Duration = Duration::from_millis(220);
const EXIT: Duration = Duration::from_millis(380);
/// Insets leave room for the shadow; bounds reports include this space.
const LANE_WIDTH: f32 = 384.0;
const LANE_PADDING_X: f32 = 16.0;
const LANE_PADDING_Y: f32 = 12.0;
/// A staged (hidden) toast normally reveals on its first fitted frame; this
/// only covers a frame that never reports, so the toast cannot stay hidden.
const REVEAL_FALLBACK: Duration = Duration::from_millis(200);
const PREVIEW_WIDTH: f32 = 280.0;
const PREVIEW_HEIGHT: f32 = 56.0;
const IMAGE_PREVIEW_WIDTH: f32 = 112.0;
const IMAGE_PREVIEW_HEIGHT: f32 = 64.0;

/// Replies retain the token they started with. A fresh live token on the same
/// retained entity/HWND must never authorize a reply from its old presentation.
#[derive(Clone)]
struct SurfaceReplyIdentity {
    source_hwnd: isize,
    alive: Arc<AtomicBool>,
}

impl SurfaceReplyIdentity {
    fn current(&self, source_hwnd: isize, alive: &Arc<AtomicBool>) -> bool {
        self.source_hwnd == source_hwnd
            && Arc::ptr_eq(&self.alive, alive)
            && self.alive.load(Ordering::Acquire)
            && alive.load(Ordering::Acquire)
    }
}

#[derive(Clone)]
struct CardReplyIdentity {
    surface: SurfaceReplyIdentity,
    key: u64,
    alive: Arc<AtomicBool>,
}

impl CardReplyIdentity {
    fn live(&self) -> bool {
        self.surface.alive.load(Ordering::Acquire) && self.alive.load(Ordering::Acquire)
    }
    fn current_card(&self, key: u64, alive: &Arc<AtomicBool>) -> bool {
        self.key == key
            && Arc::ptr_eq(&self.alive, alive)
            && self.alive.load(Ordering::Acquire)
            && alive.load(Ordering::Acquire)
    }
}

#[derive(Clone)]
struct LayoutReplyIdentity {
    surface: SurfaceReplyIdentity,
    revision: u64,
    scale: f32,
}

impl LayoutReplyIdentity {
    fn current(
        &self,
        source_hwnd: isize,
        alive: &Arc<AtomicBool>,
        revision: u64,
        scale: f32,
    ) -> bool {
        self.surface.current(source_hwnd, alive)
            && self.revision == revision
            && (self.scale - scale).abs() <= f32::EPSILON
    }
}

#[cfg(test)]
mod reply_identity_tests {
    use super::*;

    fn surface(source_hwnd: isize) -> SurfaceReplyIdentity {
        SurfaceReplyIdentity {
            source_hwnd,
            alive: Arc::new(AtomicBool::new(true)),
        }
    }

    #[test]
    fn hide_reopen_on_same_hwnd_does_not_authorize_an_old_reply() {
        let old = surface(100);
        old.alive.store(false, Ordering::Release);
        let reopened = surface(100);
        assert!(!old.current(100, &reopened.alive));
        assert!(reopened.current(100, &reopened.alive));
    }

    #[test]
    fn replacing_window_token_rejects_even_a_still_live_old_token() {
        let old = surface(100);
        let replacement = surface(100);
        assert!(old.alive.load(Ordering::Acquire));
        assert!(!old.current(100, &replacement.alive));
        assert!(!old.current(101, &old.alive));
    }

    #[test]
    fn card_key_reuse_or_expiration_cannot_authorize_old_content_or_errors() {
        let card = CardReplyIdentity {
            surface: surface(100),
            key: 7,
            alive: Arc::new(AtomicBool::new(true)),
        };
        assert!(card.current_card(7, &card.alive));
        assert!(!card.current_card(7, &Arc::new(AtomicBool::new(true))));
        assert!(!card.current_card(8, &card.alive));
        card.alive.store(false, Ordering::Release);
        assert!(!card.current_card(7, &card.alive));
    }

    #[test]
    fn preview_work_stops_when_its_captured_surface_or_card_retires() {
        let card = CardReplyIdentity {
            surface: surface(100),
            key: 7,
            alive: Arc::new(AtomicBool::new(true)),
        };
        assert!(card.live());
        card.surface.alive.store(false, Ordering::Release);
        assert!(!card.live());
        let reopened = CardReplyIdentity {
            surface: surface(100),
            key: 7,
            alive: Arc::new(AtomicBool::new(true)),
        };
        assert!(reopened.live());
        assert!(!card.live());
        reopened.alive.store(false, Ordering::Release);
        assert!(!reopened.live());
    }

    #[test]
    fn layout_reply_rejects_old_stack_revision_scale_and_surface() {
        let layout = LayoutReplyIdentity {
            surface: surface(100),
            revision: 3,
            scale: 1.25,
        };
        assert!(layout.current(100, &layout.surface.alive, 3, 1.25));
        assert!(!layout.current(100, &layout.surface.alive, 4, 1.25));
        assert!(!layout.current(100, &layout.surface.alive, 3, 1.5));
        assert!(!layout.current(100, &surface(100).alive, 3, 1.25));
    }
}

fn invalidate_obsolete_resource_request(
    pending: &mut Option<String>,
    revision: &mut u64,
    foreground: &str,
) {
    if pending.as_deref().is_some_and(|value| value != foreground) {
        *pending = None;
        *revision = revision.wrapping_add(1);
    }
}

#[cfg(test)]
mod resource_theme_tests {
    use super::*;

    #[test]
    fn returning_to_a_ready_foreground_retires_the_old_request_before_retry() {
        // A is already rendered while a B request is in flight.
        let mut pending = Some("B".to_owned());
        let mut revision = 1;
        let old_b_request = revision;
        invalidate_obsolete_resource_request(&mut pending, &mut revision, "A");
        assert!(pending.is_none());
        assert_ne!(revision, old_b_request);

        // Returning to B leaves no abandoned B marker blocking the new load.
        invalidate_obsolete_resource_request(&mut pending, &mut revision, "B");
        assert!(pending.is_none());
        pending = Some("B".to_owned());
        revision = revision.wrapping_add(1);
        let new_b_request = revision;
        invalidate_obsolete_resource_request(&mut pending, &mut revision, "B");
        assert_eq!(pending.as_deref(), Some("B"));
        assert_eq!(revision, new_b_request);
        assert_ne!(revision, old_b_request);
    }
}

struct Card {
    key: u64,
    detail: Value,
    timer: CardTimer,
    image: Option<ToastImage>,
    alive: Arc<AtomicBool>,
    hovered: bool,
    busy: bool,
    error: Option<String>,
    actions: Vec<RowAction>,
    projection: Option<ContentProjection>,
    resources: ContentResources,
    resource_holder: Option<ContentImageHolderLease>,
    resource_foreground: String,
    resource_pending: Option<String>,
    resource_revision: u64,
}

/// Only the pushed list/Toast DTO is projected here. Full QQ/XML/HTML/StepData
/// remains in core and never becomes the copy/paste/edit payload.
fn compact_projection(item: &Value) -> Option<ContentProjection> {
    match item["kind"].as_str()? {
        "StepClipItem" => project_steps(&item["stepPresentation"]).map(ContentProjection::Steps),
        "QQRichTextItem" => {
            let parts = project_mixed(item["imRichSegments"].as_array()?);
            (!parts.is_empty()).then_some(ContentProjection::Mixed(parts))
        }
        _ => None,
    }
}

#[cfg(test)]
mod compact_preview_tests {
    use super::*;
    use native_content::model::MixedPart;

    #[test]
    fn qq_uses_pushed_segments_with_filename_and_unavailable_image_chip() {
        let item = json!({"kind":"QQRichTextItem", "qqRichText":"<original payload/>",
            "imRichSegments":[{"kind":"text","text":"before"},
                {"kind":"image","path":"https://example.invalid/image.png"},
                {"kind":"file","path":"C:\\folder\\message.pdf"},
                {"kind":"placeholder","text":"[表情]"},{"kind":"text","text":"after"}]});
        let before = item.clone();
        let Some(ContentProjection::Mixed(parts)) = compact_projection(&item) else {
            panic!("QQ compact projection");
        };
        assert_eq!(
            parts,
            vec![
                MixedPart::Text("before".into()),
                MixedPart::Image { path: None },
                MixedPart::File {
                    name: "message.pdf".into(),
                    path: "C:\\folder\\message.pdf".into()
                },
                MixedPart::Placeholder("[表情]".into()),
                MixedPart::Text("after".into())
            ]
        );
        assert_eq!(item, before);
    }

    #[test]
    fn steps_preserve_line_order_icons_disabled_suffix_and_stats() {
        let item = json!({"kind":"StepClipItem","bodyText":"original StepData", "stepPresentation":{
            "primaryIcon":"fa:Light_ListOl", "previewText":"preview", "statsLabel":"2 steps",
            "previewLines":[{"title":"first","summarySuffix":": value","disabled":true,"icon":"fa:Light_Star"},
                {"title":"second","summary":"summary"}]}});
        let before = item.clone();
        let Some(ContentProjection::Steps(steps)) = compact_projection(&item) else {
            panic!("step projection");
        };
        assert_eq!(
            steps
                .lines
                .iter()
                .map(|line| line.title.as_str())
                .collect::<Vec<_>>(),
            ["first", "second"]
        );
        assert!(steps.lines[0].disabled);
        assert_eq!(steps.lines[0].icon, "fa:Light_Star");
        assert_eq!(steps.lines[0].suffix, ": value");
        assert_eq!(steps.lines[1].icon, "fa:Light_ListOl");
        assert_eq!(steps.lines[1].suffix, ": summary");
        assert_eq!(steps.stats, "2 steps");
        assert_eq!(item, before);
    }

    #[test]
    fn html_and_sparse_qq_stay_in_original_preview_fallbacks() {
        assert!(
            compact_projection(&json!({"kind":"HtmlClipItem","htmlData":"<b>original</b>"}))
                .is_none()
        );
        assert!(compact_projection(
            &json!({"kind":"QQRichTextItem","summary":"kept", "imRichSegments":[]})
        )
        .is_none());
        let Some(ContentProjection::Steps(steps)) = compact_projection(
            &json!({"kind":"StepClipItem", "stepPresentation":{"statsLabel":"3 disabled"}}),
        ) else {
            panic!("stats-only remains visible");
        };
        assert!(steps.lines.is_empty());
        assert_eq!(steps.stats, "3 disabled");
    }
}

struct ToastImage {
    image: Arc<gpui::Image>,
    dimensions: (u32, u32),
    _image_holder: ContentImageHolderLease,
}

/// Use the existing local-image decoder on a worker, including its allocation
/// and retained-resource limits. Drawing receives an owned image rather than a
/// path that GPUI would read later on the UI thread.
fn load_toast_image(
    host: &HostState,
    source: RasterSource,
    foreground: &str,
    identity: &CardReplyIdentity,
) -> Option<ToastImage> {
    if !identity.live() {
        return None;
    }
    let path = match source {
        RasterSource::Item(id) => host.hover_image_file(&id, false).ok()?.to_str()?.to_owned(),
        RasterSource::Local(path) => path,
    };
    if !identity.live() {
        return None;
    }
    let projection = ContentProjection::Mixed(vec![native_content::model::MixedPart::Image {
        path: Some(path.clone()),
    }]);
    let mut resources = load_content_resources(host, &projection, foreground);
    resources.limit_retained_bytes(
        native_content::CONTENT_RESOURCE_BUDGET_BYTES
            .saturating_sub(ContentImageHolderLease::estimate_retained_bytes(1)),
    );
    if !identity.live() {
        return None;
    }
    let image = resources.images.get(&path)?.as_ref()?;
    Some(ToastImage {
        image: image.image.clone(),
        dimensions: (image.width as u32, image.height as u32),
        _image_holder: native_content::retain_content_images(&resources),
    })
}

fn thumbnail_caption(item: &Value, dimensions: (u32, u32)) -> String {
    if item["kind"] != "ImageClipItem" {
        if let Some(text) = ["displayText", "summary", "title"]
            .into_iter()
            .filter_map(|name| item[name].as_str())
            .find(|text| !text.is_empty())
        {
            return text.split_whitespace().collect::<Vec<_>>().join(" ");
        }
        if let Some(path) = item["filePaths"][0].as_str() {
            return path.rsplit(['\\', '/']).next().unwrap_or(path).to_string();
        }
    }
    format!("{} × {}", dimensions.0, dimensions.1)
}

#[cfg(test)]
mod raster_preview_tests {
    use super::*;

    #[test]
    fn local_raster_cards_keep_the_path_caption_and_original_payload() {
        for item in [
            json!({"kind":"FileClipItem", "fileCount":1,
                "filePaths":["C:\\pics\\capture.PNG"], "displayText":"capture.PNG", "bodyText":"original"}),
            json!({"kind":"TextClipItem", "displayText":"C:\\pics\\capture.PNG", "bodyText":"original"}),
        ] {
            let before = item.clone();
            assert_eq!(
                image_source(&item),
                Some(RasterSource::Local("C:\\pics\\capture.PNG".into()))
            );
            assert_eq!(
                thumbnail_caption(&item, (1200, 800)),
                item["displayText"].as_str().unwrap()
            );
            assert_eq!(item, before);
        }
    }

    #[test]
    fn excluded_paths_stay_in_existing_previews_and_image_items_keep_dimensions() {
        for item in [
            json!({"kind":"FileClipItem", "fileCount":2,"filePaths":["C:\\pics\\capture.png"]}),
            json!({"kind":"FileClipItem", "fileCount":1,"filePaths":["C:\\docs\\note.pdf"]}),
            json!({"kind":"TextClipItem","displayText":"https://example.invalid/capture.png"}),
        ] {
            assert!(image_source(&item).is_none());
        }
        let item = json!({"kind":"ImageClipItem","id":"image-id","displayText":"image"});
        assert_eq!(
            image_source(&item),
            Some(RasterSource::Item("image-id".into()))
        );
        assert_eq!(thumbnail_caption(&item, (1200, 800)), "1200 × 800");
    }
}

fn thumbnail_size(dimensions: (u32, u32), max_width: f32, max_height: f32) -> (f32, f32) {
    let width = dimensions.0.max(1) as f32;
    let height = dimensions.1.max(1) as f32;
    let scale = (max_width / width).min(max_height / height).min(1.0);
    (width * scale, height * scale)
}

/// Web Toast uses quickPasteRowText for both HTML and ordinary text, followed
/// by the passive color-swatch renderer. Its links have no hover Open control.
/// Keep this display string separate from the original copy/edit payload.
fn passive_toast_projection(item: &Value) -> TextProjection {
    let raw = ["displayText", "summary", "title"]
        .into_iter()
        .filter_map(|name| item[name].as_str())
        .find(|text| !text.is_empty())
        .unwrap_or_default();
    let text = raw
        .split(|ch| {
            matches!(ch, '\u{0009}'..='\u{000d}' | ' ' | '\u{00a0}' | '\u{1680}'
            | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}'
            | '\u{205f}' | '\u{3000}' | '\u{feff}')
        })
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    TextProjection {
        text,
        links: Vec::new(),
        hit_ranges: Vec::new(),
    }
}

#[cfg(test)]
mod passive_preview_tests {
    use super::*;

    #[test]
    fn html_labels_and_urls_remain_passive_without_parsing_the_payload() {
        let item = json!({"kind":"HtmlClipItem", "displayText":"label\r\nhttps://example.invalid #ff0000",
            "htmlData":"<a href='https://other.invalid'>label</a>", "bodyText":"original"});
        let before = item.clone();
        let projection = passive_toast_projection(&item);
        assert_eq!(projection.text, "label https://example.invalid #ff0000");
        assert!(projection.links.is_empty());
        assert!(projection.hit_ranges.is_empty());
        assert_eq!(
            native_content::colors::collect_color_tokens(&projection.text)[0].css,
            "#ff0000"
        );
        assert_eq!(item, before);
    }

    #[test]
    fn text_normalization_matches_web_whitespace_and_primary_choice() {
        let item = json!({"kind":"TextClipItem","displayText":"\u{feff} one\t#f00\r\n two\u{0085}three\u{00a0}",
            "summary":"not selected"});
        let projection = passive_toast_projection(&item);
        assert_eq!(projection.text, "one #f00 two\u{0085}three");
        assert!(projection.links.is_empty());
        assert_eq!(
            passive_toast_projection(&json!({"displayText":" ","summary":"not selected"})).text,
            ""
        );
        assert_eq!(
            passive_toast_projection(&json!({"displayText":"","summary":" kept\n summary "})).text,
            "kept summary"
        );
    }
}

/// Native mouse-leave callbacks may be missed when this no-activate window is
/// resized or reused. Query geometry without the synchronous HWND hit testing
/// that would re-enter GPUI while its App is borrowed.
#[cfg(windows)]
fn pointer_inside(source_hwnd: isize) -> Option<bool> {
    use windows::Win32::{
        Foundation::{HWND, POINT, RECT},
        UI::WindowsAndMessaging::{GetCursorPos, GetWindowRect, IsWindowVisible},
    };
    if source_hwnd == 0 {
        return None;
    }
    unsafe {
        let hwnd = HWND(source_hwnd as *mut _);
        if !IsWindowVisible(hwnd).as_bool() {
            return Some(false);
        }
        let mut point = POINT::default();
        let mut rect = RECT::default();
        GetCursorPos(&mut point).ok()?;
        GetWindowRect(hwnd, &mut rect).ok()?;
        Some(
            point.x >= rect.left
                && point.x < rect.right
                && point.y >= rect.top
                && point.y < rect.bottom,
        )
    }
}

#[cfg(not(windows))]
fn pointer_inside(_: isize) -> Option<bool> {
    None
}

impl Drop for Card {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
    }
}

impl Card {
    fn clip_id(&self) -> Option<&str> {
        self.detail["clipItem"]["id"]
            .as_str()
            .filter(|id| !id.is_empty())
    }
}

/// Newest first; the oldest card leaves once the stack is full.
fn push_card(cards: &mut Vec<Card>, card: Card) {
    cards.insert(0, card);
    cards.truncate(MAX_CARDS);
}

/// Each card leaves on its own timer; hover/menu/busy pauses that card only.
fn expire_cards(cards: &mut Vec<Card>, now: Instant) -> bool {
    let before = cards.len();
    let mut changed = false;
    for card in cards.iter_mut().filter(|card| card.timer.expired(now)) {
        card.alive.store(false, Ordering::Release);
        card.timer.begin_exit(now, EXIT);
        changed = true;
    }
    cards.retain(|card| !card.timer.finished(now));
    changed || before != cards.len()
}

fn clear_hover(hovered: &mut bool, cards: &mut [Card]) -> bool {
    let mut changed = std::mem::replace(hovered, false);
    for card in cards {
        changed |= std::mem::replace(&mut card.hovered, false);
    }
    changed
}

fn pause_cards(cards: &mut [Card], hovered: bool, menu_key: Option<u64>, now: Instant) {
    for card in cards {
        let pause = hovered || card.hovered || card.busy || menu_key == Some(card.key);
        card.timer.pause(pause, now);
    }
}

type StackLayoutObserver = std::rc::Rc<dyn Fn(Bounds<Pixels>, &Window, &mut App) -> MotionFrame>;

/// Move the entire card, including its real hover/actions/close hitboxes. The
/// outer layout id remains raw, so the HWND does not chase painted transforms.
struct MotionCardElement {
    key: u64,
    child: AnyElement,
    observe: StackLayoutObserver,
}

impl IntoElement for MotionCardElement {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for MotionCardElement {
    type RequestLayoutState = ();
    type PrepaintState = ();
    fn id(&self) -> Option<ElementId> {
        Some(("toast-stack-motion", self.key).into())
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
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let frame = (self.observe)(bounds, window, cx);
        if frame.active {
            window.request_animation_frame();
        }
        window.with_element_offset(gpui::point(px(0.0), px(frame.offset_y)), |window| {
            self.child.prepaint(window, cx);
        });
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
        // Drawable already saved the shifted child bounds in prepaint. A
        // second offset here would move paint away from its accepted hitboxes.
        self.child.paint(window, cx);
    }
}

pub struct CopyToastOverlay {
    host: Arc<HostState>,
    tokens: DesignTokens,
    dark: bool,
    theme: OverlayTheme,
    cards: Vec<Card>,
    next_key: u64,
    hovered: bool,
    explicit_last_dismiss: bool,
    source_hwnd: isize,
    surface_alive: Arc<AtomicBool>,
    reported_height: f32,
    reported_scale: f32,
    layout_revision: u64,
    stack_motion: StackMotion,
    /// The HWND has been shown for the current stack.
    revealed: bool,
    /// Hidden HWND staged at the core rect, waiting for a fitted frame.
    reveal_pending: Option<Instant>,
    menu: Option<WindowHandle<ContextMenuPopup>>,
    menu_lease: Option<(u64, u64)>,
    displayed_language: LanguageMode,
    _ticker: Option<Task<()>>,
}

impl Drop for CopyToastOverlay {
    fn drop(&mut self) {
        self.surface_alive.store(false, Ordering::Release);
        if let Some((lease, _)) = self.menu_lease.take() {
            self.host.release_native_context_menu(lease);
        }
    }
}

impl CopyToastOverlay {
    /// Handle `ShowToast`: add the pushed card (if any) and re-place the window.
    pub fn show(
        existing: &mut Option<WindowHandle<Self>>,
        host: Arc<HostState>,
        tokens: DesignTokens,
        dark: bool,
        cx: &mut App,
    ) -> Result<(), String> {
        let Some(rect) = host.toast_overlay_place() else {
            return Ok(());
        };
        if existing
            .as_ref()
            .is_some_and(|handle| handle.read(cx).is_err())
        {
            *existing = None;
        }
        let push = host.take_toast_push();
        let handle = match existing {
            Some(handle) => *handle,
            None => {
                let window_host = host.clone();
                let handle = cx
                    .open_window(
                        WindowOptions {
                            titlebar: None,
                            focus: false,
                            show: false,
                            is_resizable: false,
                            is_minimizable: false,
                            window_background: gpui::WindowBackgroundAppearance::Transparent,
                            ..Default::default()
                        },
                        |window, cx| {
                            window.set_window_title(tr(
                                "ClipHost GPUI · 复制提示",
                                "ClipHost GPUI · Copy notification",
                            ));
                            cx.new(|cx| Self::new(window_host, tokens.clone(), dark, window, cx))
                        },
                    )
                    .map_err(|error| error.to_string())?;
                *existing = Some(handle);
                handle
            }
        };
        handle
            .update(cx, |toast, window, cx| {
                let system_dark = matches!(
                    window.appearance(),
                    gpui::WindowAppearance::Dark | gpui::WindowAppearance::VibrantDark
                );
                let (tokens, dark) = toast
                    .theme
                    .read(&host, system_dark, true)
                    .unwrap_or((tokens, dark));
                toast.layout_revision = toast.layout_revision.wrapping_add(1);
                // The core may restore its legacy initial bounds on re-show.
                // Measure again even if the same cards keep the same height.
                toast.reported_height = 0.0;
                toast.reported_scale = 0.0;
                #[cfg(windows)]
                let source_hwnd =
                    platform::hwnd(window).map_err(|error| error.to_string())?.0 as isize;
                #[cfg(not(windows))]
                let source_hwnd = 0;
                if toast.source_hwnd != source_hwnd || !toast.surface_alive.load(Ordering::Acquire)
                {
                    toast.clear(cx);
                    toast.source_hwnd = source_hwnd;
                    toast.surface_alive = Arc::new(AtomicBool::new(true));
                }
                host.register_copy_toast_native(source_hwnd, toast.surface_alive.clone())?;
                toast.set_design_tokens(tokens, dark, cx);
                if let Some(detail) = push {
                    toast.add(detail, cx);
                }
                toast.refresh_resources(cx);
                toast.sync_deadline();
                // A hidden toast is sized and drawn while still hidden and is
                // revealed by the first frame whose height the core accepts,
                // so the legacy 452x140 strip or the last frame of the
                // previous stack is never composed on screen.
                if !toast.revealed {
                    toast.reveal_pending.get_or_insert_with(Instant::now);
                }
                platform::toast_place(window, host.host_hwnd(), rect, toast.revealed)
            })
            .map_err(|error| error.to_string())?
    }

    /// Handle `HideToast`: the core already cleared its visible state.
    pub fn hide(handle: WindowHandle<Self>, cx: &mut App) {
        let _ = handle.update(cx, |toast, window, cx| {
            toast.clear(cx);
            toast.revealed = false;
            toast.reveal_pending = None;
            cx.notify();
            platform::hide(window)
        });
    }

    pub fn diagnostics(&self) -> Value {
        json!({ "cards": self.cards.len(), "hovered": self.hovered,
            "paused": self.cards.iter().any(|card| card.timer.paused()), "menuLease": self.menu_lease.as_ref().map(|(lease, _)| *lease),
            "busyCards":self.cards.iter().filter(|card|card.busy).count(),
            "errorCards":self.cards.iter().filter(|card|card.error.is_some()).count(),
            "sourceHwnd":self.source_hwnd,
            "reportedHeight": self.reported_height })
    }

    fn new(
        host: Arc<HostState>,
        tokens: DesignTokens,
        dark: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let ticker = cx.spawn_in(window, async move |this, cx| loop {
            Timer::after(Duration::from_millis(100)).await;
            if this
                .update_in(cx, |this, window, cx| this.tick(window, cx))
                .is_err()
            {
                break;
            }
        });
        Self {
            host,
            theme: OverlayTheme::new(&tokens),
            tokens,
            dark,
            cards: Vec::new(),
            next_key: 0,
            hovered: false,
            explicit_last_dismiss: false,
            source_hwnd: 0,
            surface_alive: Arc::new(AtomicBool::new(true)),
            reported_height: 0.0,
            reported_scale: 0.0,
            layout_revision: 0,
            stack_motion: StackMotion::default(),
            revealed: false,
            reveal_pending: None,
            menu: None,
            menu_lease: None,
            displayed_language: displayed_language(),
            _ticker: Some(ticker),
        }
    }

    fn add(&mut self, detail: Value, cx: &mut Context<Self>) {
        self.layout_revision = self.layout_revision.wrapping_add(1);
        self.explicit_last_dismiss = false;
        let duration = detail["durationMs"]
            .as_i64()
            .map(|ms| (ms > 0).then(|| Duration::from_millis(ms as u64)))
            .unwrap_or(Some(DEFAULT_DURATION));
        self.next_key += 1;
        let key = self.next_key;
        let raster = image_source(&detail["clipItem"]);
        let initial_actions = auto_row_actions(&row_item(&detail["clipItem"]), None);
        let projection = compact_projection(&detail["clipItem"]);
        let foreground = self.tokens.theme(self.dark).text.clone();
        if self.cards.len() >= MAX_CARDS {
            if let Some(card) = self.cards.last() {
                Self::release_resources(&card.resources, cx);
            }
        }
        push_card(
            &mut self.cards,
            Card {
                key,
                detail,
                timer: CardTimer::new(duration, Instant::now()),
                image: None,
                alive: Arc::new(AtomicBool::new(true)),
                hovered: false,
                busy: false,
                error: None,
                actions: initial_actions,
                projection,
                resources: ContentResources::default(),
                resource_holder: None,
                resource_foreground: String::new(),
                resource_pending: Some(foreground.clone()),
                resource_revision: 1,
            },
        );
        if self
            .menu_lease
            .is_some_and(|(_, key)| !self.cards.iter().any(|card| card.key == key))
        {
            self.close_menu(cx);
        }
        let item = self.cards[0].detail["clipItem"].clone();
        let identity = self
            .card_reply_identity(key)
            .expect("new card belongs to the current presentation");
        if item.is_object() {
            let host = self.host.clone();
            let identity = identity.clone();
            let work_identity = identity.clone();
            let projection = self.cards[0].projection.clone();
            let work_foreground = foreground.clone();
            cx.spawn(async move |this, cx| {
                let (actions, resources) = cx
                    .background_executor()
                    .spawn(async move {
                        if !work_identity.live() {
                            return (Vec::new(), ContentResources::default());
                        }
                        let status = host.pin_overlay_status();
                        let provider = if status["snipasteAvailable"] == true {
                            Some("snipaste")
                        } else if status["pixPinAvailable"] == true {
                            Some("pixpin")
                        } else {
                            None
                        };
                        let actions = auto_row_actions(&row_item(&item), provider);
                        if !work_identity.live() {
                            return (actions, ContentResources::default());
                        }
                        let resources = projection
                            .as_ref()
                            .map(|projection| {
                                load_content_resources(&host, projection, &work_foreground)
                            })
                            .unwrap_or_default();
                        (actions, resources)
                    })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    if !this.card_reply_current(&identity) {
                        return;
                    }
                    if let Some(card) = this
                        .cards
                        .iter_mut()
                        .find(|card| card.key == key && !card.timer.exiting())
                    {
                        card.actions = actions;
                        if card.resource_revision == 1 {
                            card.resource_pending = None;
                            if this.tokens.theme(this.dark).text == foreground {
                                card.resource_holder =
                                    Some(native_content::retain_content_images(&resources));
                                Self::release_resources(&card.resources, cx);
                                card.resources = resources;
                                card.resource_foreground = foreground;
                            }
                        }
                        this.layout_revision = this.layout_revision.wrapping_add(1);
                        cx.notify();
                    }
                    this.refresh_resources(cx);
                });
            })
            .detach();
        }
        if let Some(source) = raster {
            let host = self.host.clone();
            let work_identity = identity.clone();
            let foreground = self.tokens.theme(self.dark).text.clone();
            cx.spawn(async move |this, cx| {
                let image = cx
                    .background_executor()
                    .spawn(
                        async move { load_toast_image(&host, source, &foreground, &work_identity) },
                    )
                    .await;
                let _ = this.update(cx, |this, cx| {
                    if !this.card_reply_current(&identity) {
                        return;
                    }
                    if let Some(card) = this.cards.iter_mut().find(|card| card.key == key) {
                        card.image = image;
                        if card.image.is_none() {
                            card.error = Some(
                                tr("图片预览加载失败。", "Could not load the image preview.")
                                    .into(),
                            );
                        }
                        this.layout_revision = this.layout_revision.wrapping_add(1);
                        cx.notify();
                    }
                });
            })
            .detach();
        }
        self.update_pause(cx);
    }

    fn tick(&mut self, window: &Window, cx: &mut Context<Self>) {
        let language = displayed_language();
        if self.displayed_language != language {
            self.displayed_language = language;
            // Entries are translated when opened; release the old-language
            // popup, retaining the cards and their existing expiration times.
            self.close_menu(cx);
            cx.notify();
        }
        if self
            .menu_lease
            .as_ref()
            .is_some_and(|(lease, _)| !self.host.native_context_menu_is_current(*lease))
        {
            self.close_menu(cx);
        }
        if !self.surface_alive.load(Ordering::Acquire) {
            return;
        }
        if !self.cards.is_empty() {
            let system_dark = matches!(
                window.appearance(),
                gpui::WindowAppearance::Dark | gpui::WindowAppearance::VibrantDark
            );
            if let Some((tokens, dark)) = self.theme.read(&self.host, system_dark, false) {
                self.set_design_tokens(tokens, dark, cx);
            }
        }
        // Windows reports leaving the HWND independently of element mouse-move
        // events. Reconcile both levels so a stale card hover cannot keep the
        // timers paused after the pointer has left the notification.
        match pointer_inside(self.source_hwnd) {
            Some(true) => self.set_hovered(true, cx),
            Some(false) => {
                if clear_hover(&mut self.hovered, &mut self.cards) {
                    self.host.toast_web_action(&json!({ "action": "unhover" }));
                    self.update_pause(cx);
                }
            }
            None => {}
        }
        let now = Instant::now();
        if !self.cards.is_empty()
            && self
                .reveal_pending
                .is_some_and(|since| now.saturating_duration_since(since) >= REVEAL_FALLBACK)
        {
            self.reveal(window);
        }
        for card in self.cards.iter().filter(|card| card.timer.finished(now)) {
            Self::release_resources(&card.resources, cx);
        }
        let changed = expire_cards(&mut self.cards, now);
        if changed {
            self.layout_revision = self.layout_revision.wrapping_add(1);
        }
        self.sync_deadline();
        if self.cards.is_empty() {
            // The core defers this while hovered or while its menu is open.
            let action = if self.explicit_last_dismiss {
                "dismiss"
            } else {
                "idle"
            };
            self.host.toast_web_action(&json!({ "action": action }));
        }
        if changed {
            cx.notify();
        }
    }

    fn set_hovered(&mut self, hovered: bool, cx: &mut Context<Self>) {
        if self.hovered == hovered {
            return;
        }
        self.hovered = hovered;
        let action = if hovered { "hover" } else { "unhover" };
        self.host.toast_web_action(&json!({ "action": action }));
        self.update_pause(cx);
    }

    /// Entering the notification region pauses all cards, including new ones.
    /// A menu outside that region keeps only its associated card paused.
    fn update_pause(&mut self, cx: &mut Context<Self>) {
        let now = Instant::now();
        pause_cards(
            &mut self.cards,
            self.hovered,
            self.menu_lease.map(|(_, key)| key),
            now,
        );
        self.sync_deadline();
        cx.notify();
    }

    fn sync_deadline(&self) {
        if self.cards.is_empty() {
            return;
        }
        let now = Instant::now();
        let remaining = if self
            .cards
            .iter()
            .any(|card| card.timer.paused() || card.timer.remaining(now).is_none())
        {
            None
        } else {
            self.cards
                .iter()
                .filter_map(|card| card.timer.remaining(now))
                .max()
                .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
        };
        self.host
            .sync_copy_toast_native_deadline(self.source_hwnd, remaining);
    }

    fn clear(&mut self, cx: &mut Context<Self>) {
        self.surface_alive.store(false, Ordering::Release);
        self.close_menu(cx);
        for card in &self.cards {
            Self::release_resources(&card.resources, cx);
        }
        self.cards.clear();
        self.stack_motion.clear();
        self.hovered = false;
        self.explicit_last_dismiss = false;
        self.reported_height = 0.0;
        self.reported_scale = 0.0;
        self.layout_revision = self.layout_revision.wrapping_add(1);
    }

    fn release_resources(resources: &ContentResources, cx: &mut App) {
        for image in resources.images_for_release() {
            native_content::release_content_image(image, cx);
        }
    }

    /// Update retained cards without changing their presentation or deadlines.
    pub(super) fn set_design_tokens(
        &mut self,
        tokens: DesignTokens,
        dark: bool,
        cx: &mut Context<Self>,
    ) {
        if self.dark == dark && self.tokens.theme(self.dark) == tokens.theme(dark) {
            return;
        }
        self.tokens = tokens;
        self.dark = dark;
        let foreground = self.tokens.theme(self.dark).text.as_str();
        for card in &mut self.cards {
            invalidate_obsolete_resource_request(
                &mut card.resource_pending,
                &mut card.resource_revision,
                foreground,
            );
        }
        self.refresh_resources(cx);
        cx.notify();
    }

    /// Step icons retain the theme foreground they were prepared with. A theme
    /// change rereads resources, and an old-theme reply cannot replace them.
    fn refresh_resources(&mut self, cx: &mut Context<Self>) {
        let foreground = self.tokens.theme(self.dark).text.clone();
        let loads: Vec<_> = self
            .cards
            .iter()
            .filter_map(|card| {
                let projection = card.projection.as_ref()?;
                if card.timer.exiting()
                    || card.resource_foreground == foreground
                    || card.resource_pending.as_deref() == Some(&foreground)
                {
                    return None;
                }
                Some((
                    card.key,
                    projection.clone(),
                    self.card_reply_identity(card.key)?,
                ))
            })
            .collect();
        for (key, projection, identity) in loads {
            let Some(card) = self.cards.iter_mut().find(|card| card.key == key) else {
                continue;
            };
            card.resource_revision = card.resource_revision.wrapping_add(1);
            let revision = card.resource_revision;
            card.resource_pending = Some(foreground.clone());
            let (host, foreground) = (self.host.clone(), foreground.clone());
            let work_foreground = foreground.clone();
            let work_identity = identity.clone();
            cx.spawn(async move |this, cx| {
                let resources = cx
                    .background_executor()
                    .spawn(async move {
                        if work_identity.live() {
                            load_content_resources(&host, &projection, &work_foreground)
                        } else {
                            ContentResources::default()
                        }
                    })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    if !this.card_reply_current(&identity)
                        || this.tokens.theme(this.dark).text != foreground
                    {
                        return;
                    }
                    let Some(card) = this
                        .cards
                        .iter_mut()
                        .find(|card| card.key == key && card.resource_revision == revision)
                    else {
                        return;
                    };
                    card.resource_holder = Some(native_content::retain_content_images(&resources));
                    Self::release_resources(&card.resources, cx);
                    card.resources = resources;
                    card.resource_foreground = foreground;
                    card.resource_pending = None;
                    this.layout_revision = this.layout_revision.wrapping_add(1);
                    cx.notify();
                });
            })
            .detach();
        }
    }

    fn surface_reply_identity(&self) -> SurfaceReplyIdentity {
        SurfaceReplyIdentity {
            source_hwnd: self.source_hwnd,
            alive: self.surface_alive.clone(),
        }
    }

    fn card_reply_identity(&self, key: u64) -> Option<CardReplyIdentity> {
        if !self.surface_alive.load(Ordering::Acquire) {
            return None;
        }
        let card = self.cards.iter().find(|card| {
            card.key == key && card.alive.load(Ordering::Acquire) && !card.timer.exiting()
        })?;
        Some(CardReplyIdentity {
            surface: self.surface_reply_identity(),
            key,
            alive: card.alive.clone(),
        })
    }

    fn card_reply_current(&self, identity: &CardReplyIdentity) -> bool {
        identity
            .surface
            .current(self.source_hwnd, &self.surface_alive)
            && self
                .cards
                .iter()
                .any(|card| identity.current_card(card.key, &card.alive) && !card.timer.exiting())
    }

    fn menu_reply_current(&self, identity: &CardReplyIdentity, lease: u64) -> bool {
        self.card_reply_current(identity)
            && self.menu_lease == Some((lease, identity.key))
            && self.host.native_context_menu_is_current(lease)
    }

    fn hover_card(&mut self, key: u64, hovered: bool, cx: &mut Context<Self>) {
        if let Some(card) = self.cards.iter_mut().find(|card| card.key == key) {
            card.hovered = hovered;
        }
        self.update_pause(cx);
    }

    fn dismiss_card(&mut self, key: u64, cx: &mut Context<Self>) {
        self.layout_revision = self.layout_revision.wrapping_add(1);
        if self.menu_lease.is_some_and(|(_, current)| current == key) {
            self.close_menu(cx);
        }
        if let Some(card) = self.cards.iter_mut().find(|card| card.key == key) {
            card.alive.store(false, Ordering::Release);
            card.busy = false;
            card.timer.begin_exit(Instant::now(), EXIT);
        }
        self.explicit_last_dismiss = self.cards.iter().all(|card| card.timer.exiting());
        self.sync_deadline();
        cx.notify();
    }

    fn reveal(&mut self, window: &Window) {
        if self.reveal_pending.take().is_none() {
            return;
        }
        // The core may have hidden the toast meanwhile; never show stale.
        if let Some(rect) = self.host.toast_overlay_place() {
            self.revealed = true;
            if let Err(error) = platform::toast_place(window, self.host.host_hwnd(), rect, true) {
                tracing::warn!(%error, "copy toast reveal failed");
            }
        }
    }

    fn report_height(&mut self, height: f32, scale: f32, window: &Window) {
        if !height_report_needed(height, scale, self.reported_height, self.reported_scale) {
            return;
        }
        let viewport_height = f32::from(window.viewport_size().height);
        // Keep the current viewport only while a live reposition would be
        // clipped by shrinking it. The final animation frame submits the raw
        // stack height once; translated bounds never become resize input.
        if self.stack_motion.defer_shrink(
            height + 2.0 * LANE_PADDING_Y,
            viewport_height,
            Instant::now(),
        ) {
            return;
        }
        self.reported_height = height;
        self.reported_scale = scale;
        // Bounds changes push ShowToast, which re-places the retained window.
        let reply = self.host.toast_web_action(&json!({
            "action": "contentBounds",
            "fitContent": true,
            "width": LANE_WIDTH,
            "height": height + 2.0 * LANE_PADDING_Y,
            "devicePixelRatio": scale,
        }));
        // A resize pushes ShowToast, which stages the fitted rect and draws
        // again; only a frame the core accepts as-is may reveal the window.
        if reply["resized"] != true {
            self.reveal(window);
        }
    }

    fn open_menu(&mut self, key: u64, window: &Window, cx: &mut Context<Self>) {
        if !self.surface_alive.load(Ordering::Acquire) {
            return;
        }
        let Some(card) = self.cards.iter().find(|card| card.key == key) else {
            return;
        };
        if card.clip_id().is_none() || card.busy || card.timer.exiting() {
            return;
        }
        let Some(identity) = self.card_reply_identity(key) else {
            return;
        };
        let kind = card.detail["clipItem"]["kind"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let actions = card.actions.clone();
        let Some(point) = platform::cursor_position() else {
            return;
        };
        #[cfg(windows)]
        let Ok(owner) = platform::hwnd(window).map(|hwnd| hwnd.0 as isize) else {
            return;
        };
        #[cfg(not(windows))]
        let owner = {
            let _ = window;
            0
        };
        self.close_menu(cx);
        let lease = crate::menu::next_lease();
        if !self.host.acquire_native_context_menu(lease, "copy-toast") {
            return;
        }
        self.menu_lease = Some((lease, key));
        self.update_pause(cx);
        let mut entries = vec![
            MenuAction::tile("paste", tr("粘贴", "Paste")),
            MenuAction::tile("paste-enter", tr("回车粘贴", "Paste and Enter")),
            MenuAction::tile("copy-only", tr("复制", "Copy")),
            MenuEntry::Separator,
            MenuAction::row("edit", tr("编辑", "Edit")),
            MenuAction::row("favorite", tr("收藏", "Favorite")),
        ];
        for action in &actions {
            let label = self.action_label(&action.kind);
            let menu_key = match &action.kind {
                RowActionKind::Pin { .. } => "pin",
                RowActionKind::Simulate => "simulate",
                RowActionKind::Open { .. } => "open",
            };
            entries.push(MenuAction::row(menu_key, label));
        }
        if matches!(kind.as_str(), "FileClipItem" | "ImageClipItem") {
            entries.push(MenuAction::row(
                "reveal-folder",
                tr("打开所在文件夹", "Open containing folder"),
            ));
        }
        let view = cx.weak_entity();
        let selection_identity = identity.clone();
        let on_select: SelectHandler = std::rc::Rc::new(move |lease, key, cx| {
            let _ = view.update(cx, |this, cx| {
                if this.menu_reply_current(&selection_identity, lease) {
                    this.menu_selected(lease, key, cx);
                }
            });
        });
        let (tokens, dark, host) = (self.tokens.clone(), self.dark, self.host.clone());
        let view = cx.weak_entity();
        cx.defer(move |cx| {
            let _ = view.update(cx, |this, cx| {
                if !this.menu_reply_current(&identity, lease) || this.source_hwnd != owner {
                    return;
                }
                let mut menu = this.menu;
                let result = ContextMenuPopup::show(
                    &mut menu, &host, owner, lease, point, entries, tokens, dark, on_select, cx,
                );
                this.menu = menu;
                if !this.menu_reply_current(&identity, lease) {
                    if let Some(menu) = menu {
                        ContextMenuPopup::hide(menu, cx);
                    }
                    this.host.release_native_context_menu(lease);
                    this.menu_lease
                        .take_if(|current| *current == (lease, identity.key));
                    return;
                }
                if let Err(error) = result {
                    this.close_menu(cx);
                    this.show_card_error(key, error, cx);
                }
            });
        });
    }

    fn close_menu(&mut self, cx: &mut Context<Self>) {
        if let Some((lease, _)) = self.menu_lease.take() {
            self.host.release_native_context_menu(lease);
            self.update_pause(cx);
        }
        if let Some(menu) = self.menu {
            ContextMenuPopup::hide(menu, cx);
        }
    }

    fn menu_selected(&mut self, lease: u64, key: &'static str, cx: &mut Context<Self>) {
        let Some((_, card_key)) = self.menu_lease.take_if(|(current, _)| *current == lease) else {
            return;
        };
        if let Some(menu) = self.menu {
            ContextMenuPopup::hide(menu, cx);
        }
        let released = self.host.release_native_context_menu(lease);
        self.update_pause(cx);
        if !released {
            return;
        }
        self.run_action(card_key, key, cx);
    }

    fn show_card_error(&mut self, key: u64, error: String, cx: &mut Context<Self>) {
        self.layout_revision = self.layout_revision.wrapping_add(1);
        if let Some(card) = self
            .cards
            .iter_mut()
            .find(|card| card.key == key && !card.timer.exiting())
        {
            card.error = Some(error);
            card.busy = false;
            card.timer.keep_for(DEFAULT_DURATION, Instant::now());
        }
        self.update_pause(cx);
    }

    fn action_label(&self, action: &RowActionKind) -> String {
        match action {
            RowActionKind::Pin { provider } => format!(
                "{} {}",
                if provider == "snipaste" {
                    "Snipaste"
                } else {
                    "PixPin"
                },
                tr("贴图", "Pin image")
            ),
            RowActionKind::Simulate => tr("模拟输入", "Simulate typing").into(),
            RowActionKind::Open { .. } => tr("打开", "Open").into(),
        }
    }

    fn run_action(&mut self, key: u64, action: &str, cx: &mut Context<Self>) {
        if !self.surface_alive.load(Ordering::Acquire) {
            return;
        }
        let Some(identity) = self.card_reply_identity(key) else {
            return;
        };
        let Some(card) = self
            .cards
            .iter_mut()
            .find(|card| card.key == key && !card.busy && !card.timer.exiting())
        else {
            return;
        };
        let Some(id) = card.clip_id().map(str::to_string) else {
            return;
        };
        if action == "edit" {
            self.close_menu(cx);
            let result = self
                .host
                .toast_web_action(&json!({"action":"openEdit", "itemId":id}));
            if result["ok"].as_bool() != Some(true) {
                self.show_card_error(
                    key,
                    result["message"]
                        .as_str()
                        .unwrap_or_else(|| tr("无法打开编辑器。", "Could not open the editor."))
                        .into(),
                    cx,
                );
            }
            return;
        }
        let mut params = json!({"action": action, "id":id});
        if matches!(action, "pin" | "simulate" | "open") {
            let Some(row) = card.actions.iter().find(|row| row.kind.name() == action) else {
                self.show_card_error(
                    key,
                    tr(
                        "该快捷操作当前不可用。",
                        "This shortcut action is unavailable.",
                    )
                    .into(),
                    cx,
                );
                return;
            };
            match &row.kind {
                RowActionKind::Pin { provider } => params["provider"] = provider.clone().into(),
                RowActionKind::Open { target_kind, value } => {
                    params["targetKind"] = (*target_kind).into();
                    params["value"] = value.clone().into();
                }
                RowActionKind::Simulate => {}
            }
        }
        let input = matches!(action, "paste" | "paste-enter" | "simulate");
        let request =
            match self
                .host
                .capture_copy_toast_action(self.source_hwnd, card.alive.clone(), input)
            {
                Ok(request) => request,
                Err(error) => {
                    self.show_card_error(key, error, cx);
                    return;
                }
            };
        card.busy = true;
        card.error = None;
        self.layout_revision = self.layout_revision.wrapping_add(1);
        self.update_pause(cx);
        let host = self.host.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { host.run_copy_toast_native_action(&params, &request) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if !this.card_reply_current(&identity) {
                    return;
                }
                match result {
                    Ok(result) if result["ok"].as_bool() == Some(true) => {
                        if let Some(warning) = result["warning"].as_str() {
                            this.host.show_host_toast(
                                warning,
                                "warning",
                                Some(tr("复制提示", "Copy notification")),
                                Some(4_200),
                                None,
                            );
                        }
                        if result["consumed"].as_bool() == Some(true) {
                            this.dismiss_card(key, cx);
                        } else {
                            if let Some(card) = this.cards.iter_mut().find(|card| card.key == key) {
                                card.busy = false;
                            }
                            this.layout_revision = this.layout_revision.wrapping_add(1);
                            this.update_pause(cx);
                        }
                    }
                    Ok(result) => this.show_card_error(
                        key,
                        result["message"]
                            .as_str()
                            .unwrap_or_else(|| tr("操作失败。", "The action failed."))
                            .into(),
                        cx,
                    ),
                    Err(error) => this.show_card_error(key, error, cx),
                }
            });
        })
        .detach();
    }

    fn accent(&self, kind: &str) -> Hsla {
        let t = self.tokens.theme(self.dark);
        match (kind, self.dark) {
            ("success", false) => color("#2e9d5b"),
            ("success", true) => color("#4cc38a"),
            ("warning", false) => color("#c27c0e"),
            ("warning", true) => color("#f0b34c"),
            ("error", _) => color(&t.error),
            _ => color(&t.accent),
        }
    }

    fn icon(&self, key: &str, size: f32, tint: Hsla) -> Option<crate::crisp_svg::CrispSvg> {
        self.tokens.icons.get(key).map(|path| {
            svg()
                .path(path.clone())
                .size(px(size))
                .flex_none()
                .text_color(tint)
        })
    }

    fn preview(&self, card: &Card) -> gpui::AnyElement {
        let t = self.tokens.theme(self.dark);
        let item = &card.detail["clipItem"];
        let text = |name: &str| item[name].as_str().unwrap_or("").trim().to_string();
        if let Some(image) = &card.image {
            let (width, height) =
                thumbnail_size(image.dimensions, IMAGE_PREVIEW_WIDTH, IMAGE_PREVIEW_HEIGHT);
            return native_content::render_resource_image(
                image.image.clone(),
                img(image.image.clone())
                    .w(px(width))
                    .h(px(height))
                    .flex_none()
                    .object_fit(gpui::ObjectFit::Contain)
                    .rounded(px(4.0))
                    .with_fallback(|| {
                        div()
                            .child(tr("图片加载失败", "Image failed to load"))
                            .into_any_element()
                    })
                    .into_any_element(),
            );
        }
        if let Some(projection) = &card.projection {
            // compact_projection produces only passive Mixed/Steps content;
            // the mandatory text-link handler is not exposed by these variants.
            return rsx! {
                <div w_full min_w_0 max_h={px(PREVIEW_HEIGHT)} overflow_hidden
                    text_size={px(11.0)} text_color={color(&t.text)} />
            }
                .child(native_content::render_content(
                    card.clip_id().unwrap_or(""),
                    projection,
                    &card.resources,
                    ContentMetrics {
                        width: PREVIEW_WIDTH,
                        font_size: 11.0,
                        line_height: 1.35,
                        max_lines: 2,
                    },
                    &self.tokens,
                    self.dark,
                    std::rc::Rc::new(|_, _, _| {}),
                ))
                .into_any_element();
        }
        match item["kind"].as_str().unwrap_or("") {
            "ImageClipItem" => div()
                .text_color(color(&t.text_muted))
                .child(tr("图片", "Image"))
                .into_any_element(),
            "FileClipItem" => {
                let paths: Vec<&str> = item["filePaths"]
                    .as_array()
                    .map(|paths| paths.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                let count = item["fileCount"].as_u64().unwrap_or(paths.len() as u64);
                let mut files = rsx! {
                    <div flex flex_col gap={px(2.0)} min_w_0 />
                };
                for path in paths.iter().take(3) {
                    files = files.child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(5.0))
                            .children(self.icon("file", 13.0, color(&t.text_muted)))
                            .child(
                                rsx! {
                                    <div min_w_0 text_ellipsis overflow_hidden whitespace_nowrap>
                                        {path.rsplit(['\\', '/']).next().unwrap_or(path).to_string()}
                                    </div>
                                },
                            ),
                    );
                }
                if paths.is_empty() {
                    files = files.child(text("displayText"));
                }
                if count > 3 {
                    files = files.child(
                        div()
                            .text_color(color(&t.text_muted))
                            .child(format!("+{}", count - 3)),
                    );
                }
                files.into_any_element()
            }
            _ => {
                let mut projection = passive_toast_projection(item);
                if projection.text.is_empty()
                    && matches!(
                        item["kind"].as_str(),
                        Some("TextClipItem" | "HtmlClipItem" | "QQRichTextItem")
                    )
                {
                    projection.text = tr("空文本", "Empty text").into();
                }
                if let Some(token) =
                    native_content::colors::resolve_pure_color_token(&projection.text)
                {
                    let fill = token
                        .color
                        .map(|value| {
                            gpui::rgba(u32::from_be_bytes([
                                value.red,
                                value.green,
                                value.blue,
                                value.alpha,
                            ]))
                        })
                        .unwrap_or_else(|| gpui::rgba(0));
                    return rsx! {
                        <div flex items_center gap={px(6.0)} min_w_0>
                            <div size={px(22.0)} flex_none rounded={px(5.0)} border_1
                                border_color={color(&t.border)} bg={fill} />
                            <div flex_1 min_w_0 text_size={px(13.0)} whitespace_nowrap
                                overflow_hidden text_ellipsis>
                                {token.css}
                            </div>
                        </div>
                    }
                        .into_any_element();
                }

                rsx! {
                    <div w_full min_w_0 max_h={px(13.0 * 1.4 * 2.0)} overflow_hidden />
                }
                    .child(native_content::render_text_content(
                        card.clip_id().unwrap_or(""),
                        native_content::ContentRole::Primary,
                        &projection,
                        ContentMetrics {
                            width: PREVIEW_WIDTH,
                            font_size: 13.0,
                            line_height: 1.4,
                            max_lines: 2,
                        },
                        &self.tokens,
                        self.dark,
                        // The Web Toast exposes only its separate action chips.
                        // This projection has no inline activation targets.
                        std::rc::Rc::new(|_, _, _| {}),
                    ))
                    .into_any_element()
            }
        }
    }

    /// Leading glyph: the clip kind for clipboard cards, otherwise the status.
    fn badge(&self, card: &Card, accent: Hsla) -> gpui::AnyElement {
        let key = match card.detail["clipItem"]["kind"].as_str() {
            Some("ImageClipItem") => "image",
            Some("FileClipItem") => "file",
            Some(_) => "text",
            None => "clipboard",
        };
        div()
            .size(px(24.0))
            .flex_none()
            .rounded(px(6.0))
            .bg(accent.opacity(if self.dark { 0.2 } else { 0.12 }))
            .flex()
            .items_center()
            .justify_center()
            .children(self.icon(key, 13.0, accent))
            .into_any_element()
    }

    fn card(&self, card: &Card, cx: &mut Context<Self>) -> gpui::AnyElement {
        let t = self.tokens.theme(self.dark);
        let kind = card.detail["kind"].as_str().unwrap_or("info");
        let accent = self.accent(kind);
        let title = card.detail["title"]
            .as_str()
            .unwrap_or("")
            .trim()
            .to_string();
        let message = card.detail["message"]
            .as_str()
            .unwrap_or("")
            .trim()
            .to_string();
        let has_clip = card.detail["clipItem"].is_object();
        let has_thumbnail = card.image.is_some();
        let key = card.key;
        let identity = self.card_reply_identity(key);
        let group: gpui::SharedString = format!("toast-card-{key}").into();
        let mut content = rsx! {
            <div flex_1 min_w_0 min_h={px(24.0)} pr={px(16.0)} flex flex_col justify_center
                gap={px(2.0)} />
        };
        if !title.is_empty() {
            content = content.child(
                rsx! {
                    <div text_size={px(12.0)} font_weight={gpui::FontWeight::SEMIBOLD}
                        text_color={color(&t.text)}>
                        {title}
                    </div>
                },
            );
        }
        if has_clip {
            let item = &card.detail["clipItem"];
            let kind = match item["kind"].as_str().unwrap_or("") {
                "ImageClipItem" => tr("图片", "Image"),
                "FileClipItem" => tr("文件", "Files"),
                "HtmlClipItem" => tr("富文本", "Rich text"),
                "QQRichTextItem" => tr("聊天富文本", "Chat rich text"),
                "StepClipItem" => tr("步骤", "Steps"),
                "LinkClipItem" => tr("链接", "Link"),
                _ => tr("文本", "Text"),
            };
            let source = item["sourceProcessName"].as_str().unwrap_or("");
            content = content.child(
                rsx! {
                    <div text_size={px(11.0)} text_ellipsis overflow_hidden whitespace_nowrap
                        text_color={color(&t.text_muted)} />
                }
                    .child(if source.is_empty() {
                        kind.to_string()
                    } else {
                        format!("{kind} · {source}")
                    }),
            );
            if has_thumbnail {
                let dimensions = card.image.as_ref().unwrap().dimensions;
                content = content.child(
                    rsx! {
                        <div text_size={px(11.0)} text_color={color(&t.text_secondary)}
                            text_ellipsis overflow_hidden whitespace_nowrap>
                            {thumbnail_caption(item, dimensions)}
                        </div>
                    },
                );
            } else {
                content = content.child(self.preview(card));
            }
        }
        if !message.is_empty() {
            content = content.child(
                div()
                    .text_size(px(12.0))
                    .line_height(relative(1.45))
                    .line_clamp(3)
                    .text_color(color(&t.text_secondary))
                    .child(message),
            );
        }
        if let Some(error) = &card.error {
            content = content.child(
                rsx! {
                    <div text_size={px(11.0)} text_color={color(&t.error)}
                        line_height={relative(1.35)}>
                        {error.clone()}
                    </div>
                },
            );
        }
        if has_clip {
            let mut buttons = rsx! {
                <div flex flex_wrap gap={px(5.0)} mt={px(3.0)} />
            };
            let actions = std::iter::once(("paste", tr("粘贴", "Paste").to_string(), None)).chain(
                card.actions.iter().map(|action| {
                    let icon = match &action.kind {
                        RowActionKind::Pin { .. } => "pin",
                        RowActionKind::Simulate => "keyboard",
                        RowActionKind::Open { target_kind, .. } => {
                            if *target_kind == "uri" {
                                "link"
                            } else {
                                "folder-open"
                            }
                        }
                    };
                    (
                        action.kind.name(),
                        self.action_label(&action.kind),
                        Some(icon),
                    )
                }),
            );
            let disabled = card.busy || card.timer.exiting();
            for (action, label, icon) in actions {
                let identity = identity.clone();
                buttons = buttons.child(
                    rsx! {
                        <div id={(action, key as usize)} flex items_center gap={px(4.0)}
                            px={px(7.0)} py={px(3.0)} rounded={px(5.0)} border_1
                            border_color={color(&t.border)} text_size={px(11.0)} />
                    }
                        .bg(color(if action == "paste" {
                            &t.selected
                        } else {
                            &t.surface
                        }))
                        .when(disabled, |button| button.opacity(0.5))
                        .when(!disabled, |button| {
                            button
                                .cursor_pointer()
                                .hover(|style| style.bg(color(&t.tab_active)))
                        })
                        .children(icon.and_then(|icon| self.icon(icon, 12.0, color(&t.text_muted))))
                        .child(label)
                        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            if !disabled
                                && identity
                                    .as_ref()
                                    .is_some_and(|identity| this.card_reply_current(identity))
                            {
                                this.run_action(key, action, cx);
                            }
                        })),
                );
            }
            content = content.child(buttons);
        }
        let shadow = |alpha: f32, y: f32, blur: f32| gpui::BoxShadow {
            color: gpui::black().opacity(if self.dark { alpha * 3.0 } else { alpha }),
            offset: gpui::point(px(0.0), px(y)),
            blur_radius: px(blur),
            spread_radius: px(0.0),
        };
        let hover_identity = identity.clone();
        let edit_identity = identity.clone();
        let menu_identity = identity.clone();
        let element = div()
            .id(("toast-card", key as usize))
            .group(group.clone())
            .relative()
            .flex_none()
            .w_full()
            .flex()
            .items_start()
            .gap(px(8.0))
            .p(px(8.0))
            .rounded(px(9.0))
            .border_1()
            .border_color(color(&t.border).opacity(0.7))
            .bg(color(&t.surface))
            .shadow(vec![shadow(0.06, 1.0, 2.0), shadow(0.1, 4.0, 10.0)])
            .text_size(px(13.0))
            .text_color(color(&t.text))
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                if hover_identity
                    .as_ref()
                    .is_some_and(|identity| this.card_reply_current(identity))
                {
                    this.hover_card(key, *hovered, cx);
                }
            }))
            .when(has_clip && !card.timer.exiting(), |card| {
                card.on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                        if event.click_count == 2
                            && edit_identity
                                .as_ref()
                                .is_some_and(|identity| this.card_reply_current(identity))
                        {
                            this.run_action(key, "edit", cx);
                        }
                    }),
                )
                .on_mouse_down(
                    gpui::MouseButton::Right,
                    cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        if menu_identity
                            .as_ref()
                            .is_some_and(|identity| this.card_reply_current(identity))
                        {
                            this.open_menu(key, window, cx);
                        }
                    }),
                )
            })
            .child(if has_thumbnail {
                self.preview(card)
            } else {
                self.badge(card, accent)
            })
            .child(content)
            .child(
                rsx! {
                    <div id={("toast-close", key as usize)} absolute top={px(6.0)}
                        right={px(6.0)} size={px(20.0)} rounded={px(6.0)} flex items_center
                        justify_center cursor_pointer opacity={0.0} />
                }
                    .group_hover(group, |style| style.opacity(1.0))
                    .hover(|style| style.bg(color(&t.tab_active)))
                    .children(self.icon("close", 12.0, color(&t.text_muted)))
                    .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        if identity
                            .as_ref()
                            .is_some_and(|identity| this.card_reply_current(identity))
                        {
                            this.dismiss_card(key, cx);
                        }
                    })),
            );
        if card.timer.exiting() {
            element
                .with_animation(
                    ("toast-exit", key as usize),
                    Animation::new(EXIT).with_easing(gpui::ease_out_quint()),
                    |card, delta| card.opacity(1.0 - delta),
                )
                .into_any_element()
        } else {
            element
                .with_animation(
                    ("toast-enter", key as usize),
                    Animation::new(ENTER).with_easing(gpui::ease_out_quint()),
                    |card, delta| card.opacity(delta),
                )
                .into_any_element()
        }
    }
}

impl Render for CopyToastOverlay {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        window.set_window_title(tr(
            "ClipHost GPUI · 复制提示",
            "ClipHost GPUI · Copy notification",
        ));
        let placement = self
            .cards
            .first()
            .and_then(|card| card.detail["placement"].as_str());
        let anchor = StackAnchor::for_placement(placement);
        let bottom = anchor == StackAnchor::Bottom;
        let scale = window.scale_factor();
        // Every placement pins one screen edge; slide survivors relative to it.
        self.stack_motion
            .set_mode(!crate::motion::reduced_motion(), anchor, scale);
        self.stack_motion
            .retain(&self.cards.iter().map(|card| card.key).collect::<Vec<_>>());
        let identity = self.surface_reply_identity();
        let hover_identity = identity.clone();
        let layout_identity = LayoutReplyIdentity {
            surface: identity,
            revision: self.layout_revision,
            scale,
        };
        let cards: Vec<AnyElement> = self
            .cards
            .iter()
            .map(|card| {
                let child = self.card(card, cx);
                let key = card.key;
                let card_live = card.alive.clone();
                let expected = layout_identity.clone();
                let view = cx.weak_entity();
                MotionCardElement {
                    key,
                    child,
                    observe: std::rc::Rc::new(move |bounds, window, cx| {
                        let actual_scale = window.scale_factor();
                        let anchored_y = anchor.anchored_y(
                            f32::from(bounds.top()),
                            f32::from(bounds.bottom()),
                            f32::from(window.viewport_size().height),
                        );
                        view.update(cx, |this, _| {
                            if !expected.current(
                                this.source_hwnd,
                                &this.surface_alive,
                                this.layout_revision,
                                actual_scale,
                            ) {
                                return MotionFrame::default();
                            }
                            let Some(card) = this.cards.iter().find(|card| {
                                card.key == key && Arc::ptr_eq(&card.alive, &card_live)
                            }) else {
                                return MotionFrame::default();
                            };
                            // A current exiting card still owns its displayed
                            // geometry. Retarget it when another row disappears;
                            // its retired action token remains retired.
                            if card.alive.load(Ordering::Acquire) || card.timer.exiting() {
                                this.stack_motion.observe(key, anchored_y, Instant::now())
                            } else {
                                MotionFrame::default()
                            }
                        })
                        .unwrap_or_default()
                    }),
                }
                .into_any_element()
            })
            .collect();
        let view = cx.weak_entity();
        let lane = div()
            .size_full()
            .flex()
            .flex_col()
            .when(bottom, |root| root.justify_end())
            .px(px(LANE_PADDING_X))
            .py(px(LANE_PADDING_Y))
            .on_children_prepainted(move |bounds, window, cx| {
                let actual_scale = window.scale_factor();
                let height = bounds
                    .first()
                    .map(|b| f32::from(b.size.height))
                    .unwrap_or(0.0);
                let _ = view.update(cx, |this, _| {
                    if layout_identity.current(
                        this.source_hwnd,
                        &this.surface_alive,
                        this.layout_revision,
                        actual_scale,
                    ) {
                        this.report_height(height, scale, window);
                    }
                });
            })
            .child(
                rsx! {
                    <div flex_none flex flex_col gap={px(8.0)} />
                }
                    .children(cards),
            );
        rsx! {
            <div id={"copy-toast"} size_full bg={gpui::transparent_black()}
                font_family={self.tokens.typography.font_family.clone()} />
        }
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                if hover_identity.current(this.source_hwnd, &this.surface_alive) {
                    this.set_hovered(*hovered, cx);
                }
            }))
            .child(lane)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(key: u64, deadline: Instant) -> Card {
        Card {
            key,
            detail: Value::Null,
            timer: CardTimer::new(Some(Duration::ZERO), deadline),
            image: None,
            alive: Arc::new(AtomicBool::new(true)),
            hovered: false,
            busy: false,
            error: None,
            actions: Vec::new(),
            projection: None,
            resources: ContentResources::default(),
            resource_holder: None,
            resource_foreground: String::new(),
            resource_pending: None,
            resource_revision: 0,
        }
    }

    #[test]
    fn cards_stack_newest_first_and_drop_the_oldest() {
        let now = Instant::now();
        let mut cards = Vec::new();
        for key in 1..=7 {
            push_card(&mut cards, card(key, now));
        }
        let keys: Vec<u64> = cards.iter().map(|card| card.key).collect();
        assert_eq!(keys, [7, 6, 5, 4, 3]);
    }

    #[test]
    fn each_card_expires_on_its_own_deadline() {
        let now = Instant::now();
        let mut cards = vec![
            card(2, now + Duration::from_millis(500)),
            card(1, now - Duration::from_millis(1)),
        ];
        assert!(expire_cards(&mut cards, now));
        assert_eq!(cards.len(), 2);
        assert!(cards[1].timer.exiting());
        assert!(!cards[1].alive.load(Ordering::Acquire));
        assert!(!expire_cards(&mut cards, now));
        assert!(expire_cards(&mut cards, now + EXIT));
        assert_eq!(cards.len(), 1);
        assert!(expire_cards(&mut cards, now + Duration::from_secs(1)));
        assert!(expire_cards(
            &mut cards,
            now + Duration::from_secs(1) + EXIT
        ));
        assert!(cards.is_empty());
    }

    #[test]
    fn dropping_overflow_card_cancels_its_in_flight_request() {
        let now = Instant::now();
        let oldest = card(1, now);
        let lifetime = oldest.alive.clone();
        let mut cards = vec![oldest];
        for key in 2..=6 {
            push_card(&mut cards, card(key, now));
        }
        assert!(!lifetime.load(Ordering::Acquire));
        assert_eq!(cards.len(), MAX_CARDS);
    }

    #[test]
    fn leaving_window_clears_both_hover_levels_and_resumes_remaining_time() {
        let now = Instant::now();
        for root_hover in [true, false] {
            let mut hovered = root_hover;
            let mut cards = vec![card(1, now)];
            cards[0].timer = CardTimer::new(Some(Duration::from_secs(3)), now);
            cards[0].hovered = true;
            pause_cards(&mut cards, hovered, None, now + Duration::from_secs(1));
            assert!(clear_hover(&mut hovered, &mut cards));
            pause_cards(&mut cards, hovered, None, now + Duration::from_secs(10));
            assert!(!cards[0].timer.paused());
            assert_eq!(
                cards[0].timer.remaining(now + Duration::from_secs(10)),
                Some(Duration::from_secs(2))
            );
            assert!(!expire_cards(
                &mut cards,
                now + Duration::from_millis(11_999)
            ));
            assert!(expire_cards(&mut cards, now + Duration::from_secs(12)));
        }
    }

    #[test]
    fn leaving_window_preserves_only_menu_and_action_pauses() {
        let now = Instant::now();
        let mut cards = vec![card(1, now), card(2, now), card(3, now)];
        let mut hovered = true;
        for card in &mut cards {
            card.timer = CardTimer::new(Some(Duration::from_secs(3)), now);
            card.hovered = true;
        }
        cards[0].busy = true;
        pause_cards(&mut cards, hovered, Some(2), now);
        assert!(clear_hover(&mut hovered, &mut cards));
        pause_cards(&mut cards, hovered, Some(2), now + Duration::from_secs(5));
        assert!(cards[0].timer.paused());
        assert!(cards[1].timer.paused());
        assert!(!cards[2].timer.paused());
    }

    #[test]
    fn large_and_portrait_images_fit_thumbnail_without_stretching_or_upscaling() {
        assert_eq!(thumbnail_size((4000, 1000), 112.0, 64.0), (112.0, 28.0));
        assert_eq!(thumbnail_size((1000, 4000), 112.0, 64.0), (16.0, 64.0));
        assert_eq!(thumbnail_size((20, 12), 112.0, 64.0), (20.0, 12.0));
    }
}
