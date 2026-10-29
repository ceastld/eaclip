//! Bind the existing native popup to the actual painted row/image elements.
//! One persistent row controller chooses a thumbnail before its row text. No
//! independent competing row/image mouse handlers, inferred boxes, or I/O.
use std::{cell::RefCell, collections::HashMap, rc::Rc, sync::Arc};

use gpui::{
    AnyElement, App, Bounds, DispatchPhase, Element, ElementId, GlobalElementId, Hitbox,
    HitboxBehavior, ImageSource, InspectorElementId, IntoElement, LayoutId, MouseExitEvent,
    MouseMoveEvent, Pixels, Point, Window,
};

use crate::hover::{Content, HoverOwner, HoverThumbnailTarget, Request, RowHoverContent, Ticket};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct RowKey {
    id: String,
    index: usize,
    generation: u64,
    session: u64,
}

/// `clear` retires retained callbacks as well as their currently scheduled
/// tickets. A stale element can neither dismiss nor restart a replacement.
#[derive(Default)]
pub(crate) struct HoverBindings(RefCell<HashMap<RowKey, HoverBinding>>);
impl HoverBindings {
    pub(crate) fn row(
        &self,
        id: &str,
        index: usize,
        generation: u64,
        session: u64,
        plan: RowHoverContent,
        images_enabled: bool,
    ) -> HoverBinding {
        let key = RowKey {
            id: id.into(),
            index,
            generation,
            session,
        };
        let mut entries = self.0.borrow_mut();
        if entries
            .get(&key)
            .is_some_and(|binding| !binding.matches(&plan))
        {
            if let Some(old) = entries.remove(&key) {
                old.retire();
            }
        }
        let binding = entries
            .entry(key)
            .or_insert_with(|| HoverBinding::new(plan))
            .clone();
        binding.0.borrow_mut().images_enabled = images_enabled;
        binding
    }
    pub(crate) fn clear(&self) {
        for (_, binding) in self.0.borrow_mut().drain() {
            binding.retire();
        }
    }
}
impl Drop for HoverBindings {
    fn drop(&mut self) {
        self.clear();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Target {
    Row,
    Thumbnail(HoverThumbnailTarget),
}

#[derive(Clone, Debug, PartialEq)]
struct Selection {
    target: Target,
    bounds: Bounds<Pixels>,
    content: Content,
}

struct Lease {
    owner: Arc<HoverOwner>,
    ticket: Ticket,
}
impl Lease {
    fn leave(self) {
        self.owner.leave(self.ticket);
    }
}

struct State {
    plan: RowHoverContent,
    images_enabled: bool,
    retired: bool,
    epoch: u64,
    row_bounds: Option<Bounds<Pixels>>,
    thumbnails: Vec<(HoverThumbnailTarget, Bounds<Pixels>)>,
    selected: Option<Selection>,
    lease: Option<Lease>,
}
impl Drop for State {
    fn drop(&mut self) {
        if let Some(lease) = self.lease.take() {
            lease.leave();
        }
    }
}

/// A render callback retains this exact controller, not a lookup by reused id.
#[derive(Clone)]
pub(crate) struct HoverBinding(Rc<RefCell<State>>);
impl HoverBinding {
    fn new(plan: RowHoverContent) -> Self {
        Self(Rc::new(RefCell::new(State {
            plan,
            images_enabled: true,
            retired: false,
            epoch: 0,
            row_bounds: None,
            thumbnails: Vec::new(),
            selected: None,
            lease: None,
        })))
    }
    fn matches(&self, plan: &RowHoverContent) -> bool {
        let state = self.0.borrow();
        !state.retired && state.plan == *plan
    }
    pub(crate) fn retire(&self) {
        let lease = {
            let mut state = self.0.borrow_mut();
            state.retired = true;
            state.selected = None;
            state.row_bounds = None;
            state.thumbnails.clear();
            state.lease.take()
        };
        if let Some(lease) = lease {
            lease.leave();
        }
    }
    fn begin_paint(&self) -> u64 {
        let mut state = self.0.borrow_mut();
        state.epoch = state.epoch.wrapping_add(1);
        state.row_bounds = None;
        state.thumbnails.clear();
        state.epoch
    }
    fn record_thumbnail(&self, target: HoverThumbnailTarget, bounds: Bounds<Pixels>) {
        let mut state = self.0.borrow_mut();
        if !state.retired && state.plan.thumbnail(target).is_some() && positive(bounds) {
            state.thumbnails.push((target, bounds));
        }
    }
    fn finish_paint(&self, epoch: u64, bounds: Bounds<Pixels>) {
        let lease = {
            let mut state = self.0.borrow_mut();
            if state.retired || state.epoch != epoch {
                return;
            }
            state.row_bounds = positive(bounds).then_some(bounds);
            let still_painted =
                state
                    .selected
                    .as_ref()
                    .is_none_or(|selected| match selected.target {
                        Target::Row => state.row_bounds == Some(selected.bounds),
                        Target::Thumbnail(target) => {
                            state.images_enabled
                                && state.thumbnails.iter().any(|(current, bounds)| {
                                    *current == target && *bounds == selected.bounds
                                })
                        }
                    });
            if still_painted {
                None
            } else {
                state.selected = None;
                state.lease.take()
            }
        };
        if let Some(lease) = lease {
            lease.leave();
        }
    }
    fn change(&self, epoch: u64, point: Option<Point<Pixels>>) -> Option<HoverEvent> {
        let mut state = self.0.borrow_mut();
        if state.retired || state.epoch != epoch {
            return None;
        }
        let selected = point.and_then(|point| selection(&state, point));
        let owner_retired = state
            .lease
            .as_ref()
            .is_some_and(|lease| !lease.owner.is_current());
        if state.selected == selected && !owner_retired {
            return None;
        }
        state.selected = selected.clone();
        Some(HoverEvent {
            binding: self.clone(),
            epoch,
            selected,
        })
    }
}

fn selection(state: &State, point: Point<Pixels>) -> Option<Selection> {
    let bounds = state.row_bounds.filter(|bounds| bounds.contains(&point))?;
    if state.images_enabled {
        for (target, thumbnail_bounds) in state.thumbnails.iter().rev() {
            if thumbnail_bounds.contains(&point) {
                if let Some(thumbnail) = state.plan.thumbnail(*target) {
                    let content = thumbnail.content();
                    match (&state.plan.row_media, *target) {
                        // A file chip focuses that file inside the row's file card.
                        (
                            Some(Content::Files(card)),
                            HoverThumbnailTarget::FileImage(index)
                            | HoverThumbnailTarget::FileMedia(index),
                        ) => {
                            return Some(Selection {
                                target: Target::Thumbnail(*target),
                                bounds: *thumbnail_bounds,
                                content: Content::Files(crate::hover::FileCard {
                                    paths: card.paths.clone(),
                                    focus: Some(index),
                                }),
                            });
                        }
                        // The whole row already previews this image or card;
                        // crossing the thumbnail must not restart it.
                        (Some(media), _) if *media == content || matches!(media, Content::Rich(_)) => break,
                        _ => {
                            return Some(Selection {
                                target: Target::Thumbnail(*target),
                                bounds: *thumbnail_bounds,
                                content,
                            })
                        }
                    }
                }
            }
        }
    }
    Some(Selection {
        target: Target::Row,
        bounds,
        content: if state.images_enabled {
            state
                .plan
                .row_media
                .as_ref()
                .unwrap_or(&state.plan.row)
                .clone()
        } else {
            state.plan.row.clone()
        },
    })
}

/// Carries the original binding and the actual paint epoch. The parent verifies
/// its original row/session before allowing this event to reach the popup.
#[derive(Clone)]
pub(crate) struct HoverEvent {
    binding: HoverBinding,
    epoch: u64,
    selected: Option<Selection>,
}
impl HoverEvent {
    pub(crate) fn apply(
        self,
        owner: Arc<HoverOwner>,
        scale: f64,
        dark: bool,
    ) -> Result<(), String> {
        let old = {
            let mut state = self.binding.0.borrow_mut();
            if state.retired || state.epoch != self.epoch || state.selected != self.selected {
                return Ok(());
            }
            state.lease.take()
        };
        if let Some(old) = old {
            old.leave();
        }
        let Some(selected) = self.selected else {
            return Ok(());
        };
        let bounds = selected.bounds;
        let ticket = owner.schedule(Request {
            anchor: [
                f64::from(f32::from(bounds.left())),
                f64::from(f32::from(bounds.top())),
                f64::from(f32::from(bounds.size.width)),
                f64::from(f32::from(bounds.size.height)),
            ],
            scale,
            dark,
            content: selected.content.clone(),
        })?;
        if let Some(ticket) = ticket {
            let lease = Lease { owner, ticket };
            let mut state = self.binding.0.borrow_mut();
            if state.retired || state.epoch != self.epoch || state.selected != Some(selected) {
                drop(state);
                lease.leave();
            } else {
                state.lease = Some(lease);
            }
        }
        Ok(())
    }
    /// Invalid rows retire their own controller. They never cancel the current
    /// owner's newer row/thumbnail ticket by item id.
    pub(crate) fn reject(self) {
        self.binding.retire();
    }
}

pub(crate) type HoverHandler = Rc<dyn Fn(HoverEvent, &mut Window, &mut App)>;

pub(crate) fn wrap_hover_row(
    child: AnyElement,
    binding: Option<HoverBinding>,
    handler: HoverHandler,
) -> AnyElement {
    match binding {
        Some(binding) => RowElement {
            child,
            binding,
            handler,
        }
        .into_any_element(),
        None => child,
    }
}
pub(crate) fn wrap_hover_thumbnail(
    child: AnyElement,
    binding: Option<HoverBinding>,
    target: HoverThumbnailTarget,
    source: ImageSource,
) -> AnyElement {
    match binding {
        Some(binding) => ThumbnailElement {
            child,
            binding,
            target,
            source: Some(source),
        }
        .into_any_element(),
        None => child,
    }
}

/// A file chip is a real painted target even before a raster thumbnail exists.
/// It registers bounds without loading or probing the media during rendering.
pub(crate) fn wrap_hover_target(
    child: AnyElement,
    binding: Option<HoverBinding>,
    target: HoverThumbnailTarget,
) -> AnyElement {
    match binding {
        Some(binding) => ThumbnailElement {
            child,
            binding,
            target,
            source: None,
        }
        .into_any_element(),
        None => child,
    }
}

struct RowElement {
    child: AnyElement,
    binding: HoverBinding,
    handler: HoverHandler,
}
impl IntoElement for RowElement {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}
impl Element for RowElement {
    type RequestLayoutState = ();
    type PrepaintState = (u64, Hitbox);
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
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let epoch = self.binding.begin_paint();
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        self.child.prepaint(window, cx);
        (epoch, hitbox)
    }
    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        state: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(window, cx);
        let (epoch, hitbox) = state.clone();
        self.binding
            .finish_paint(epoch, bounds.intersect(&window.content_mask().bounds));
        let binding = self.binding.clone();
        let handler = self.handler.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble {
                return;
            }
            let point = hitbox.is_hovered(window).then_some(event.position);
            if let Some(event) = binding.change(epoch, point) {
                handler(event, window, cx);
            }
        });
        let binding = self.binding.clone();
        let handler = self.handler.clone();
        window.on_mouse_event(move |_: &MouseExitEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble {
                if let Some(event) = binding.change(epoch, None) {
                    handler(event, window, cx);
                }
            }
        });
    }
}

struct ThumbnailElement {
    child: AnyElement,
    binding: HoverBinding,
    target: HoverThumbnailTarget,
    source: Option<ImageSource>,
}
impl IntoElement for ThumbnailElement {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}
impl Element for ThumbnailElement {
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
        // `get_asset` reads only the result requested by the real img child.
        // Custom sources cannot be read passively, so they never register here.
        let ready = match &self.source {
            None => true,
            Some(ImageSource::Resource(resource)) => window
                .get_asset::<gpui::ImgResourceLoader>(resource, cx)
                .is_some_and(|image| image.is_ok()),
            Some(ImageSource::Image(image)) => image.clone().get_render_image(window, cx).is_some(),
            Some(ImageSource::Render(_)) => true,
            Some(ImageSource::Custom(_)) => false,
        };
        if ready {
            self.binding
                .record_thumbnail(self.target, bounds.intersect(&window.content_mask().bounds));
        }
    }
}

fn positive(bounds: Bounds<Pixels>) -> bool {
    [
        bounds.origin.x,
        bounds.origin.y,
        bounds.size.width,
        bounds.size.height,
    ]
    .into_iter()
    .all(|value| f32::from(value).is_finite())
        && bounds.size.width > gpui::px(0.)
        && bounds.size.height > gpui::px(0.)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hover::{HoverThumbnail, ImageSource as HoverImageSource};
    fn bounds(x: f32, width: f32) -> Bounds<Pixels> {
        Bounds::new(
            gpui::point(gpui::px(x), gpui::px(0.)),
            gpui::size(gpui::px(width), gpui::px(40.)),
        )
    }
    fn plan() -> RowHoverContent {
        RowHoverContent {
            row: Content::Text("full text".into()),
            row_media: None,
            im_rich_segments: Vec::new(),
            thumbnails: vec![1, 4]
                .into_iter()
                .map(|index| HoverThumbnail {
                    target: HoverThumbnailTarget::ImImage(index),
                    source: HoverImageSource::Local("same.png".into()),
                    meta_line: String::new(),
                })
                .collect(),
        }
    }
    fn painted(binding: &HoverBinding) -> u64 {
        let epoch = binding.begin_paint();
        binding.record_thumbnail(HoverThumbnailTarget::ImImage(1), bounds(20., 20.));
        binding.record_thumbnail(HoverThumbnailTarget::ImImage(4), bounds(60., 20.));
        binding.finish_paint(epoch, bounds(0., 100.));
        epoch
    }
    #[test]
    fn each_actual_thumbnail_keeps_its_original_index_and_bounds() {
        let binding = HoverBinding::new(plan());
        let epoch = painted(&binding);
        let event = binding
            .change(epoch, Some(gpui::point(gpui::px(65.), gpui::px(10.))))
            .unwrap();
        let selected = event.selected.unwrap();
        assert_eq!(
            selected.target,
            Target::Thumbnail(HoverThumbnailTarget::ImImage(4))
        );
        assert_eq!(selected.bounds, bounds(60., 20.));
        let event = binding
            .change(epoch, Some(gpui::point(gpui::px(25.), gpui::px(10.))))
            .unwrap();
        assert_eq!(
            event.selected.unwrap().target,
            Target::Thumbnail(HoverThumbnailTarget::ImImage(1))
        );
    }
    #[test]
    fn disabling_images_preserves_row_text_and_unpainted_plan_does_not_hit() {
        let binding = HoverBinding::new(plan());
        let epoch = binding.begin_paint();
        binding.finish_paint(epoch, bounds(0., 100.));
        let event = binding
            .change(epoch, Some(gpui::point(gpui::px(65.), gpui::px(10.))))
            .unwrap();
        assert_eq!(event.selected.unwrap().target, Target::Row);
        binding.0.borrow_mut().images_enabled = false;
        let epoch = painted(&binding);
        let event = binding.change(epoch, Some(gpui::point(gpui::px(25.), gpui::px(10.))));
        assert!(event.is_none());
        assert_eq!(
            binding.0.borrow().selected.as_ref().unwrap().content,
            Content::Text("full text".into())
        );
    }
    #[test]
    fn single_file_row_media_respects_preview_setting_and_retains_text_fallback() {
        let mut plan = plan();
        plan.row_media = Some(Content::Video {
            path: "C:\\clip.mp4".into(),
        });
        let binding = HoverBinding::new(plan);
        let epoch = painted(&binding);
        let at = Some(gpui::point(gpui::px(5.), gpui::px(10.)));
        let selected = binding.change(epoch, at).unwrap().selected.unwrap();
        assert_eq!(selected.target, Target::Row);
        assert_eq!(
            selected.content,
            Content::Video {
                path: "C:\\clip.mp4".into()
            }
        );
        binding.0.borrow_mut().images_enabled = false;
        let selected = binding.change(epoch, at).unwrap().selected.unwrap();
        assert_eq!(selected.content, Content::Text("full text".into()));
    }
    #[test]
    fn painted_media_chip_routes_original_file_index_and_repaint_retires_it() {
        let mut plan = plan();
        plan.thumbnails.push(HoverThumbnail {
            target: HoverThumbnailTarget::FileMedia(7),
            source: HoverImageSource::Local("C:\\clip.mp4".into()),
            meta_line: String::new(),
        });
        let binding = HoverBinding::new(plan);
        let epoch = binding.begin_paint();
        binding.record_thumbnail(HoverThumbnailTarget::FileMedia(7), bounds(20., 20.));
        binding.finish_paint(epoch, bounds(0., 100.));
        let selected = binding
            .change(epoch, Some(gpui::point(gpui::px(25.), gpui::px(10.))))
            .unwrap()
            .selected
            .unwrap();
        assert_eq!(
            selected.target,
            Target::Thumbnail(HoverThumbnailTarget::FileMedia(7))
        );
        assert_eq!(
            selected.content,
            Content::Video {
                path: "C:\\clip.mp4".into()
            }
        );
        let next = binding.begin_paint();
        binding.finish_paint(next, bounds(0., 100.));
        assert!(binding.0.borrow().selected.is_none());
    }
    #[test]
    fn old_paint_callback_and_retained_binding_cannot_restart_after_clear() {
        let registry = HoverBindings::default();
        let old = registry.row("id", 0, 1, 2, plan(), true);
        let old_epoch = painted(&old);
        let current_epoch = painted(&old);
        assert!(old
            .change(old_epoch, Some(gpui::point(gpui::px(25.), gpui::px(10.))))
            .is_none());
        registry.clear();
        assert!(old
            .change(
                current_epoch,
                Some(gpui::point(gpui::px(25.), gpui::px(10.)))
            )
            .is_none());
        let current = registry.row("id", 0, 1, 2, plan(), true);
        let epoch = painted(&current);
        assert!(current
            .change(epoch, Some(gpui::point(gpui::px(25.), gpui::px(10.))))
            .is_some());
    }
    #[test]
    fn repaint_retires_selected_thumbnail_when_its_real_element_disappears() {
        let binding = HoverBinding::new(plan());
        let epoch = painted(&binding);
        binding
            .change(epoch, Some(gpui::point(gpui::px(25.), gpui::px(10.))))
            .unwrap();
        let epoch = binding.begin_paint();
        binding.finish_paint(epoch, bounds(0., 100.));
        assert!(binding.0.borrow().selected.is_none());
        let event = binding
            .change(epoch, Some(gpui::point(gpui::px(25.), gpui::px(10.))))
            .unwrap();
        assert_eq!(event.selected.unwrap().target, Target::Row);
    }
}
