//! Read-only editor geometry captured by actual GPUI prepaint/paint callbacks.
//! The snapshot contains public control IDs and session metadata, never content.

use gpui::{prelude::*, Bounds, Pixels, Size, Window};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};

#[derive(Clone)]
pub(super) struct PaintedControl {
    pub bounds: Bounds<Pixels>,
    pub enabled: bool,
}

#[derive(Clone, Default)]
pub(super) struct EditorPaintSnapshot {
    pub frame_id: u64,
    pub generation: u64,
    pub document_kind: String,
    pub tab: &'static str,
    pub hwnd: isize,
    pub scale_factor: f32,
    pub bounds: Option<Bounds<Pixels>>,
    pub paint_completed: bool,
    pub controls: BTreeMap<&'static str, PaintedControl>,
}

impl EditorPaintSnapshot {
    pub fn viewport_matches(&self, size: Size<Pixels>, scale_factor: f32) -> bool {
        self.scale_factor == scale_factor && self.bounds.is_some_and(|bounds| bounds.size == size)
    }
}

#[derive(Clone, Default)]
pub(super) struct EditorPaintMetadata {
    pub clock: Arc<AtomicU64>,
    state: Arc<Mutex<EditorPaintSnapshot>>,
}

pub(super) fn rect(bounds: Bounds<Pixels>) -> serde_json::Value {
    serde_json::json!({"x":f32::from(bounds.left()),"y":f32::from(bounds.top()),
        "width":f32::from(bounds.size.width),"height":f32::from(bounds.size.height)})
}

pub(super) fn fixture_content_allowed(
    history_target: bool,
    id: &str,
    fixture_ids: &BTreeSet<String>,
    debug: bool,
    headless: bool,
) -> bool {
    debug && headless && history_target && fixture_ids.contains(id)
}

impl EditorPaintMetadata {
    pub fn snapshot(&self) -> Option<EditorPaintSnapshot> {
        self.state.lock().ok().map(|state| state.clone())
    }

    pub fn begin(
        &self,
        generation: u64,
        kind: String,
        tab: &'static str,
    ) -> impl IntoElement + use<> {
        let state = self.state.clone();
        let clock = self.clock.clone();
        gpui::canvas(
            move |bounds, window, _| {
                let frame_id = clock.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
                #[cfg(windows)]
                let hwnd = crate::platform::hwnd(window)
                    .map(|hwnd| hwnd.0 as isize)
                    .unwrap_or(0);
                #[cfg(not(windows))]
                let hwnd = 0;
                if let Ok(mut state) = state.lock() {
                    *state = EditorPaintSnapshot {
                        frame_id,
                        generation,
                        document_kind: kind.clone(),
                        tab,
                        hwnd,
                        scale_factor: window.scale_factor(),
                        bounds: Some(bounds),
                        paint_completed: false,
                        controls: BTreeMap::new(),
                    };
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
    }

    pub fn finish(&self) -> impl IntoElement + use<> {
        let state = self.state.clone();
        let clock = self.clock.clone();
        gpui::canvas(
            move |_, _, _| clock.load(Ordering::Acquire),
            move |_, frame, _, _| {
                if let Ok(mut state) = state.lock() {
                    if state.frame_id == frame && frame != 0 {
                        state.paint_completed = true;
                    }
                }
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
    }

    pub fn control(&self, id: &'static str, enabled: bool) -> impl IntoElement + use<> {
        let state = self.state.clone();
        let clock = self.clock.clone();
        gpui::canvas(
            |bounds, window: &mut Window, _| bounds.intersect(&window.content_mask().bounds),
            move |_, clipped, _, _| {
                if clipped.size.width <= gpui::px(0.) || clipped.size.height <= gpui::px(0.) {
                    return;
                }
                if let Ok(mut state) = state.lock() {
                    if state.frame_id == clock.load(Ordering::Acquire) {
                        state.controls.insert(
                            id,
                            PaintedControl {
                                bounds: clipped,
                                enabled,
                            },
                        );
                    }
                }
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
    }

    pub fn diagnostics(&self) -> serde_json::Value {
        let Some(state) = self.snapshot() else {
            return serde_json::json!({"paintCompleted":false});
        };
        serde_json::json!({"frameId":state.frame_id,"paintFrameId":state.frame_id,"paintCompleted":state.paint_completed,
            "hwnd":state.hwnd,"generation":state.generation,"documentKind":state.document_kind,"visibleTab":state.tab,
            "coordinateSpace":"window-client-logical-pixels","scaleFactor":state.scale_factor,
            "bounds":state.bounds.map(rect),"controls":state.controls.into_iter().map(|(id,control)|{
                (id.to_string(),serde_json::json!({"bounds":rect(control.bounds),"enabled":control.enabled,"frameId":state.frame_id}))
            }).collect::<serde_json::Map<_,_>>()})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn window_resize_or_dpi_change_invalidates_the_painted_viewport() {
        let mut paint = EditorPaintSnapshot {
            bounds: Some(Bounds::new(
                gpui::point(gpui::px(0.), gpui::px(0.)),
                gpui::size(gpui::px(800.), gpui::px(600.)),
            )),
            scale_factor: 1.5,
            ..Default::default()
        };
        assert!(paint.viewport_matches(gpui::size(gpui::px(800.), gpui::px(600.)), 1.5));
        assert!(!paint.viewport_matches(gpui::size(gpui::px(801.), gpui::px(600.)), 1.5));
        assert!(!paint.viewport_matches(gpui::size(gpui::px(800.), gpui::px(601.)), 1.5));
        assert!(!paint.viewport_matches(gpui::size(gpui::px(800.), gpui::px(600.)), 2.));
        paint.bounds = None;
        assert!(!paint.viewport_matches(gpui::size(gpui::px(800.), gpui::px(600.)), 1.5));
    }
    #[test]
    fn exact_content_requires_all_debug_history_fixture_guards() {
        let fixture_ids = BTreeSet::from(["authored-fixture".to_string()]);
        for history_target in [false, true] {
            for debug in [false, true] {
                for headless in [false, true] {
                    assert_eq!(
                        fixture_content_allowed(
                            history_target,
                            "authored-fixture",
                            &fixture_ids,
                            debug,
                            headless
                        ),
                        history_target && debug && headless
                    );
                }
            }
        }
        assert!(!fixture_content_allowed(
            true,
            "production-row",
            &fixture_ids,
            true,
            true
        ));
        assert!(!fixture_content_allowed(
            true,
            "authored-fixture",
            &BTreeSet::new(),
            true,
            true
        ));
    }
}
