//! Content drag preview in its own click-through popup (2026-10-26). GPUI's
//! drag view is painted inside the source window: it is clipped at the window
//! edge and freezes once the OLE loop takes over for other programs. This
//! popup follows the pointer across our windows and into other processes.
//! One hidden window is reused for every history/favorites/panel drag.
//!
//! 2026-10-27 (DESIGN, `ui-design` drag-preview stage): the preview is the
//! dragged row itself, same size, 78% opaque with a bottom fade, held at the
//! grab point. A "＋ 复制到 <app>" hint shows where the target accepts; a
//! cancelled drag slides back to the row, an accepted one fades out. The
//! one-line label remains the fallback when the row cannot be captured.

#[path = "drag_preview_snapshot.rs"]
mod snapshot;

use std::{
    cell::Cell,
    rc::Rc,
    time::{Duration, Instant},
};

use cliphost_core::i18n::tr;
use gpui::{
    canvas, div, img, prelude::*, px, size, App, Bounds, Context, Global, Pixels, SharedString,
    Task, Timer, Window, WindowBounds, WindowHandle, WindowOptions,
};
use gpui_rsx::rsx;

use self::snapshot::{Snapshot, PAD};
use crate::{favorite_drag::ContentDrag, platform};

/// Hint chip offset from the pointer, and the room reserved for it.
const HINT_OFFSET: (f32, f32) = (16.0, 22.0);
const HINT_ROOM: (f32, f32) = (260.0, 28.0);
/// Multi-select stack layers step down-right behind the row.
const STACK_STEP: f32 = 5.0;
/// Longest wait for the row capture before falling back to the label.
const CAPTURE_WAIT: Duration = Duration::from_millis(300);
const SLIDE_BACK: Duration = Duration::from_millis(200);
const FADE_OUT: Duration = Duration::from_millis(120);

/// Logical window size. The label is truncated inside; the rest is transparent.
const WIDTH: f32 = 360.0;
const HEIGHT: f32 = 44.0;
/// Room for the label shadow between the label and the window edge.
const INSET: f32 = 8.0;
/// Label corner from the hotspot, clear of the copy/no-drop cursor badge.
const OFFSET_X: f32 = 14.0;
const OFFSET_Y: f32 = 18.0;
/// Flipped label edge distance from the hotspot.
const FLIP_GAP: f32 = 4.0;
const FOLLOW: Duration = Duration::from_millis(8);

#[cfg(all(windows, debug_assertions))]
#[path = "drag_preview_fixture.rs"]
pub mod fixture;

/// The dark pill shared by the popup and the in-window fallback. GPUI's own
/// ellipsis needs a definite width and sums primary-font glyph widths, so a
/// content-sized label with CJK text never truncates; fit it here instead.
pub(crate) fn label(text: String, font: Option<SharedString>, window: &Window) -> gpui::Div {
    const PADDING_X: f32 = 10.0;
    let size = px(12.);
    let mut run = window.text_style().to_run(0);
    if let Some(font) = &font {
        run.font.family = font.clone();
    }
    let text = fit(&text.replace(['\r', '\n'], " "), WIDTH - 2.0 * INSET - 2.0 * PADDING_X, |text| {
        let run = gpui::TextRun { len: text.len(), ..run.clone() };
        f32::from(window.text_system().shape_line(text.to_owned().into(), size, &[run], None).width)
    });
    rsx! {
        <div flex_none whitespace_nowrap px={px(PADDING_X)} py={px(6.)} rounded={px(6.)}
            shadow_md bg={gpui::rgb(0x353535)} text_color={gpui::rgb(0xffffff)} text_size={size}>
            {text}
        </div>
    }
    .when_some(font, |pill, font| pill.font_family(font))
}

/// The whole text, or its longest char prefix plus an ellipsis within `max`.
fn fit(text: &str, max: f32, width: impl Fn(&str) -> f32) -> String {
    if width(text) <= max {
        return text.to_owned();
    }
    // The first `chars` characters; the whole text already overflows.
    let starts: Vec<usize> = text.char_indices().map(|(index, _)| index).collect();
    let prefix = |chars: usize| &text[..starts[chars]];
    let (mut low, mut high) = (0, starts.len() - 1);
    while low < high {
        let middle = (low + high).div_ceil(2);
        if width(&format!("{}…", prefix(middle))) <= max {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    format!("{}…", prefix(low).trim_end())
}

struct Popup(Option<WindowHandle<DragPreview>>);
impl Global for Popup {}

/// Show the popup for a drag that has just been activated. `source` is the
/// dragged row's bounds in `window`, captured as the preview. False keeps
/// the in-window fallback, e.g. headless or when the popup cannot open.
pub(crate) fn begin(
    drag: &ContentDrag,
    source: Option<Bounds<Pixels>>,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    if !cfg!(windows) || cliphost_core::is_headless() {
        return false;
    }
    let handle = match cx.try_global::<Popup>().and_then(|popup| popup.0) {
        Some(handle) => handle,
        None => match open(cx) {
            Some(handle) => handle,
            None => return false,
        },
    };
    let capturing = source.is_some_and(|bounds| {
        let pointer = window.mouse_position();
        let grab = (f32::from(pointer.x - bounds.origin.x), f32::from(pointer.y - bounds.origin.y));
        snapshot::capture(drag.operation_id.clone(), bounds, grab, window)
    });
    let drag = drag.clone();
    if handle.update(cx, |this, window, cx| this.start(drag, capturing, window, cx)).is_ok() {
        return true;
    }
    cx.set_global(Popup(None));
    false
}

fn open(cx: &mut App) -> Option<WindowHandle<DragPreview>> {
    let bounds = Bounds::centered(None, size(px(WIDTH), px(HEIGHT)), cx);
    let handle = cx
        .open_window(
            WindowOptions {
                titlebar: None,
                focus: false,
                show: false,
                is_resizable: false,
                is_minimizable: false,
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_background: gpui::WindowBackgroundAppearance::Transparent,
                ..Default::default()
            },
            |window, cx| {
                window.set_window_title(tr("ClipHost GPUI · 拖动预览", "ClipHost GPUI · Drag preview"));
                // The app's UI font (GPUI's default would miss CJK widths)
                // and accents for the count badge.
                let tokens = crate::preview::DesignTokens::from_json(crate::preview::TOKENS_JSON).ok();
                let font = tokens.as_ref().map(|tokens| SharedString::from(tokens.typography.font_family.clone()));
                let accent = tokens.map(|tokens| {
                    (crate::view::color(&tokens.themes.light.accent), crate::view::color(&tokens.themes.dark.accent))
                });
                cx.new(|_| DragPreview { font, accent, ..Default::default() })
            },
        )
        .inspect_err(|error| tracing::debug!(%error, "Drag preview window unavailable"))
        .ok()?;
    cx.set_global(Popup(Some(handle)));
    Some(handle)
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Placement {
    origin: (i32, i32),
    size: (i32, i32),
    /// Label aligned to the window's right / bottom edge.
    flip: (bool, bool),
}

/// Physical window rect for a hotspot. The label sits below-right of the
/// pointer and flips to the other side where it would leave the work area
/// (left, top, right, bottom). `label` is the painted logical label size.
fn place(cursor: (i32, i32), work: (i32, i32, i32, i32), scale: f64, label: (f32, f32)) -> Placement {
    let scale = if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };
    let at = |logical: f32| f64::from(logical) * scale;
    let (x, y) = (f64::from(cursor.0), f64::from(cursor.1));
    let flip_x = x + at(OFFSET_X + label.0) > f64::from(work.2);
    let flip_y = y + at(OFFSET_Y + label.1) > f64::from(work.3);
    let left = if flip_x {
        x - at(FLIP_GAP) + at(INSET) - at(WIDTH)
    } else {
        x + at(OFFSET_X) - at(INSET)
    };
    let top = if flip_y {
        y - at(FLIP_GAP) + at(INSET) - at(HEIGHT)
    } else {
        y + at(OFFSET_Y) - at(INSET)
    };
    Placement {
        origin: (left.round() as i32, top.round() as i32),
        size: (at(WIDTH).round() as i32, at(HEIGHT).round() as i32),
        flip: (flip_x, flip_y),
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum Mode {
    /// The one-line label (fallback).
    #[default]
    Label,
    /// Hidden until the row capture arrives or `CAPTURE_WAIT` passes.
    Waiting(Instant),
    Snapshot,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Ending {
    /// Cancelled: glide from `from` back to the row's place, then hide.
    SlideBack { from: (i32, i32), to: (i32, i32), started: Instant },
    /// Accepted: fade the whole popup out, then hide.
    FadeOut { started: Instant },
}

#[derive(Default)]
pub(crate) struct DragPreview {
    drag: Option<ContentDrag>,
    mode: Mode,
    snapshot: Option<Snapshot>,
    /// Selected items in this gesture (stack layers and badge above one).
    count: usize,
    control: bool,
    external: bool,
    /// "＋ 复制到 …" outside, the move/copy/save verb over our own targets.
    hint: Option<String>,
    /// Root window under the pointer and its program name.
    app: Option<(isize, Option<String>)>,
    ending: Option<Ending>,
    flip: (bool, bool),
    origin: Option<(i32, i32)>,
    visible: bool,
    /// Painted logical label size, for flipping near screen edges.
    measured: Rc<Cell<(f32, f32)>>,
    font: Option<SharedString>,
    /// Light and dark accent for the count badge.
    accent: Option<(gpui::Hsla, gpui::Hsla)>,
    follow: Option<Task<()>>,
}

/// Logical popup size for a snapshot: the shaped row plus room for the hint
/// chip beside the pointer.
fn snapshot_window(snapshot: &Snapshot) -> (f32, f32) {
    (
        snapshot.size.0.max(PAD + snapshot.grab.0 + HINT_OFFSET.0 + HINT_ROOM.0),
        snapshot.size.1.max(PAD + snapshot.grab.1 + HINT_OFFSET.1 + HINT_ROOM.1),
    )
}

/// Physical popup origin that keeps the grab point under the pointer.
fn snapshot_origin(cursor: (i32, i32), grab: (f32, f32), scale: f64) -> (i32, i32) {
    let at = |logical: f32| (f64::from(logical) * scale).round() as i32;
    (cursor.0 - at(grab.0 + PAD), cursor.1 - at(grab.1 + PAD))
}

fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3)
}

impl DragPreview {
    fn start(&mut self, drag: ContentDrag, capturing: bool, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.ending.take(), Some(Ending::FadeOut { .. })) {
            let _ = platform::drag_preview_alpha(window, 255);
        }
        // A new gesture never shows the previous one's frame or geometry.
        if std::mem::take(&mut self.visible) {
            let _ = platform::hide(window);
        }
        self.count = drag.item_ids.len();
        self.drag = Some(drag);
        self.snapshot = None;
        self.hint = None;
        self.origin = None;
        self.mode = if capturing { Mode::Waiting(Instant::now()) } else { Mode::Label };
        self.sample(cx);
        self.follow(window, cx);
        cx.notify();
        if self.follow.is_none() {
            self.follow = Some(cx.spawn_in(window, async move |this, cx| loop {
                Timer::after(FOLLOW).await;
                let keep = this.update_in(cx, |this, window, cx| this.tick(window, cx));
                if !matches!(keep, Ok(true)) {
                    return;
                }
            }));
        }
    }

    fn tick(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if let Some(ending) = self.ending {
            return self.animate_end(ending, window, cx);
        }
        let live = cx.has_active_drag()
            && self.drag.as_ref().is_some_and(ContentDrag::preview_live);
        if !live {
            return self.finish(window, cx);
        }
        if let (Mode::Waiting(since), Some(drag)) = (self.mode, &self.drag) {
            match snapshot::take(&drag.operation_id) {
                Some(Ok(snapshot)) => {
                    self.snapshot = Some(snapshot);
                    self.mode = Mode::Snapshot;
                    cx.notify();
                }
                Some(Err(error)) => {
                    tracing::debug!(%error, "Drag row capture unavailable");
                    self.mode = Mode::Label;
                    cx.notify();
                }
                None if since.elapsed() >= CAPTURE_WAIT => {
                    self.mode = Mode::Label;
                    cx.notify();
                }
                None => {}
            }
        }
        let state = (self.control, self.external, self.hint.clone());
        self.sample(cx);
        if state != (self.control, self.external, self.hint.clone()) {
            cx.notify();
        }
        self.follow(window, cx);
        true
    }

    /// Ctrl is read from the keyboard: this popup never receives input, and
    /// the source window stops seeing it once the OLE loop owns the gesture.
    fn sample(&mut self, cx: &App) {
        #[cfg(windows)]
        {
            self.control = unsafe {
                crate::windows_api::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(0x11) < 0
            };
        }
        self.external = !crate::favorite_drag::pointer_over_surface(cx);
        let Some(drag) = self.drag.clone() else {
            self.hint = None;
            return;
        };
        self.hint = if !self.external {
            crate::favorite_drag::offered_here(&drag, cx)
                .then(|| drag.verb(self.control, false).to_owned())
        } else if drag.went_native() && cliphost_core::ole_drag::drop_effect() != 0 {
            Some(match self.app_under_pointer() {
                Some(app) => cliphost_core::trf!("＋ 复制到 {}", "+ Copy to {}", app),
                None => tr("＋ 复制", "+ Copy").to_owned(),
            })
        } else {
            None
        };
    }

    /// Friendly name of the program under the pointer, cached per window.
    fn app_under_pointer(&mut self) -> Option<String> {
        #[cfg(windows)]
        {
            use crate::windows_api::Win32::{
                Foundation::POINT,
                UI::WindowsAndMessaging::{GetAncestor, WindowFromPoint, GA_ROOT},
            };
            let (x, y) = platform::cursor_position()?;
            let root = unsafe { GetAncestor(WindowFromPoint(POINT { x, y }), GA_ROOT) }.0 as isize;
            if self.app.as_ref().is_none_or(|(cached, _)| *cached != root) {
                self.app = Some((root, cliphost_core::window_menu::window_app_name(root)));
            }
            self.app.as_ref().and_then(|(_, name)| name.clone())
        }
        #[cfg(not(windows))]
        None
    }

    fn follow(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(cursor) = platform::cursor_position() {
            self.follow_at(cursor, window, cx);
        }
    }

    fn follow_at(&mut self, cursor: (i32, i32), window: &mut Window, cx: &mut Context<Self>) {
        let Some(area) = cliphost_core::window_place::overlay_work_area_from_point(cursor.0, cursor.1) else {
            return;
        };
        let scale = area.dpi_scale_y;
        let (origin, size) = match (self.mode, &self.snapshot) {
            (Mode::Waiting(_), _) => return,
            (Mode::Snapshot, Some(snapshot)) => {
                let (width, height) = snapshot_window(snapshot);
                let at = |logical: f32| (f64::from(logical) * scale).round() as i32;
                (snapshot_origin(cursor, snapshot.grab, scale), (at(width), at(height)))
            }
            _ => {
                let work = (
                    area.work.left.round() as i32,
                    area.work.top.round() as i32,
                    (area.work.left + area.work.width).round() as i32,
                    (area.work.top + area.work.height).round() as i32,
                );
                let mut label = self.measured.get();
                if label.0 <= 0.0 || label.1 <= 0.0 {
                    label = (WIDTH - 2.0 * INSET, HEIGHT - 2.0 * INSET);
                }
                let placement = place(cursor, work, scale, label);
                if placement.flip != self.flip {
                    self.flip = placement.flip;
                    cx.notify();
                }
                (placement.origin, placement.size)
            }
        };
        self.move_to(origin, size, window);
    }

    fn move_to(&mut self, origin: (i32, i32), size: (i32, i32), window: &mut Window) {
        if self.origin == Some(origin) {
            return;
        }
        self.origin = Some(origin);
        let result = if self.visible {
            platform::set_geometry(window, Some(origin), None)
        } else {
            self.visible = true;
            platform::drag_preview_show(window, (origin.0, origin.1, size.0, size.1))
        };
        if let Err(error) = result {
            tracing::debug!(%error, "Drag preview placement failed");
        }
    }

    /// The gesture ended: fade out when a target took it, otherwise glide
    /// the snapshot back to its row. True while an animation still runs.
    fn finish(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let accepted = self.drag.as_ref().is_some_and(|drag| {
            drag.is_claimed() || (drag.went_native() && cliphost_core::ole_drag::drop_effect() != 0)
        });
        self.drag = None;
        self.hint = None;
        crate::appearance::set_hover_suppressed(false);
        cx.notify();
        let started = Instant::now();
        self.ending = match (&self.snapshot, self.origin) {
            _ if !self.visible || crate::motion::reduced_motion() => None,
            _ if accepted => Some(Ending::FadeOut { started }),
            (Some(snapshot), Some(from)) if self.mode == Mode::Snapshot => {
                let pad = (PAD * snapshot.scale).round() as i32;
                Some(Ending::SlideBack { from, to: (snapshot.origin.0 - pad, snapshot.origin.1 - pad), started })
            }
            _ => None,
        };
        if self.ending.is_none() {
            self.end(window, cx);
            return false;
        }
        true
    }

    fn animate_end(&mut self, ending: Ending, window: &mut Window, cx: &mut Context<Self>) -> bool {
        match ending {
            Ending::SlideBack { from, to, started } => {
                let t = started.elapsed().as_secs_f32() / SLIDE_BACK.as_secs_f32();
                if t >= 1.0 {
                    self.end(window, cx);
                    return false;
                }
                let eased = ease_out(t);
                let at = |a: i32, b: i32| a + ((b - a) as f32 * eased).round() as i32;
                let _ = platform::set_geometry(window, Some((at(from.0, to.0), at(from.1, to.1))), None);
            }
            Ending::FadeOut { started } => {
                let t = started.elapsed().as_secs_f32() / FADE_OUT.as_secs_f32();
                if t >= 1.0 {
                    self.end(window, cx);
                    return false;
                }
                let _ = platform::drag_preview_alpha(window, ((1.0 - t) * 255.0).round() as u8);
            }
        }
        true
    }

    fn end(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.drag = None;
        self.snapshot = None;
        self.mode = Mode::Label;
        self.hint = None;
        self.ending = None;
        self.follow = None;
        self.origin = None;
        crate::appearance::set_hover_suppressed(false);
        if std::mem::take(&mut self.visible) {
            let _ = platform::hide(window);
        }
        cx.notify();
    }

    fn render_snapshot(&self, snapshot: &Snapshot) -> gpui::Div {
        let row = (snapshot.size.0 - 2.0 * PAD, snapshot.size.1 - 2.0 * PAD);
        let [r, g, b] = snapshot.surface;
        let surface = gpui::rgb(u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b));
        let dark = (0.2126 * f32::from(r) + 0.7152 * f32::from(g) + 0.0722 * f32::from(b)) < 128.0;
        let accent = self
            .accent
            .map(|(light, dark_accent)| if dark { dark_accent } else { light })
            .unwrap_or_else(|| gpui::rgb(0x4f7cff).into());
        let layers = (1..self.count.min(3)).rev().map(|depth| {
            let offset = PAD + STACK_STEP * depth as f32;
            let alpha = if depth == 1 { 0.8 } else { 0.55 } * snapshot::OPACITY;
            div()
                .absolute()
                .left(px(offset))
                .top(px(offset))
                .w(px(row.0))
                .h(px(row.1))
                .rounded(px(8.0))
                .border_1()
                .border_color(gpui::black().opacity(0.1 * alpha))
                .bg(gpui::Hsla::from(surface).opacity(alpha))
        });
        let hint = (self.ending.is_none()).then(|| self.hint.clone()).flatten();
        div()
            .size_full()
            .relative()
            .children(layers)
            .child(
                img(gpui::ImageSource::Render(snapshot.image.clone()))
                    .absolute()
                    .top_0()
                    .left_0()
                    .w(px(snapshot.size.0))
                    .h(px(snapshot.size.1)),
            )
            .when(self.count > 1, |root| {
                root.child(rsx! {
                    <div absolute left={px(PAD + row.0 - 12.0)} top={px(PAD - 8.0)} min_w={px(20.0)}
                        h={px(20.0)} px={px(6.0)} rounded_full bg={accent} flex items_center justify_center
                        text_color={gpui::white()} text_size={px(11.0)} font_weight={gpui::FontWeight::SEMIBOLD}
                        shadow_sm>
                        {self.count.to_string()}
                    </div>
                })
            })
            .when_some(hint, |root, hint| {
                root.child(rsx! {
                    <div absolute left={px(PAD + snapshot.grab.0 + HINT_OFFSET.0)}
                        top={px(PAD + snapshot.grab.1 + HINT_OFFSET.1)} px={px(8.0)} py={px(3.0)}
                        rounded={px(4.0)} border_1 border_color={gpui::rgb(0xc9ccd1)} bg={gpui::white()}
                        text_color={gpui::rgb(0x1f2328)} text_size={px(11.5)} whitespace_nowrap shadow_sm>
                        {hint}
                    </div>
                })
            })
    }
}

impl Render for DragPreview {
    fn render(&mut self, window: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        if let (Mode::Snapshot, Some(snapshot)) = (self.mode, &self.snapshot) {
            return self
                .render_snapshot(snapshot)
                .when_some(self.font.clone(), |root, font| root.font_family(font))
                .into_any_element();
        }
        let text = self
            .drag
            .as_ref()
            .filter(|_| !matches!(self.mode, Mode::Waiting(_)))
            .map(|drag| drag.preview_label(self.control, self.external));
        let measured = self.measured.clone();
        div()
            .size_full()
            .p(px(INSET))
            .flex()
            .when(self.flip.0, |root| root.justify_end())
            .when(self.flip.1, |root| root.items_end())
            .when_some(text, |root, text| {
                root.child(
                    label(text, self.font.clone(), window)
                        .relative()
                        .child(
                            canvas(
                                move |bounds, _, _| {
                                    measured.set((f32::from(bounds.size.width), f32::from(bounds.size.height)));
                                },
                                |_, _, _, _| {},
                            )
                            .absolute()
                            .top_0()
                            .left_0()
                            .size_full(),
                        ),
                )
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORK: (i32, i32, i32, i32) = (0, 0, 1920, 1040);

    #[test]
    fn label_sits_below_right_of_the_pointer_at_any_scale() {
        let placement = place((500, 300), WORK, 1.0, (120.0, 28.0));
        assert_eq!(placement.flip, (false, false));
        assert_eq!(placement.origin, (500 + 14 - 8, 300 + 18 - 8));
        assert_eq!(placement.size, (360, 44));
        let scaled = place((500, 300), WORK, 1.5, (120.0, 28.0));
        assert_eq!(scaled.origin, (509, 315));
        assert_eq!(scaled.size, (540, 66));
    }

    #[test]
    fn label_flips_only_on_the_side_that_would_leave_the_work_area() {
        // A short label still fits near the right edge.
        assert_eq!(place((1700, 300), WORK, 1.0, (120.0, 28.0)).flip, (false, false));
        let right = place((1850, 300), WORK, 1.0, (120.0, 28.0));
        assert_eq!(right.flip, (true, false));
        // Right-aligned label: its right edge ends just left of the hotspot.
        assert_eq!(right.origin.0 + right.size.0 - 8, 1850 - 4);
        let corner = place((1850, 1020), WORK, 1.0, (120.0, 28.0));
        assert_eq!(corner.flip, (true, true));
        assert_eq!(corner.origin.1 + corner.size.1 - 8, 1020 - 4);
    }

    #[test]
    fn long_labels_keep_the_longest_prefix_that_fits_with_an_ellipsis() {
        // One unit per char: a stand-in for shaped widths, CJK included.
        let width = |text: &str| text.chars().count() as f32;
        assert_eq!(fit("短标题", 10.0, width), "短标题");
        assert_eq!(fit("很长的剪贴板条目标题", 6.0, width), "很长的剪贴…");
        assert_eq!(fit("ab cdef", 4.0, width), "ab…");
        assert_eq!(fit("长", 0.5, width), "…");
    }

    #[test]
    fn invalid_scale_falls_back_to_logical_pixels() {
        assert_eq!(
            place((10, 10), WORK, f64::NAN, (100.0, 28.0)),
            place((10, 10), WORK, 1.0, (100.0, 28.0))
        );
    }
}
