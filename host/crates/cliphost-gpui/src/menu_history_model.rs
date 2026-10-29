//! Pure layout for the history-only menu. All coordinates are client DIPs.
use super::super::{MenuAction, MenuEntry};

pub(super) type Rect = (f32, f32, f32, f32);
pub(super) const SHADOW: f32 = 8.0;
pub(super) const ROOT_WIDTH: f32 = 280.0;
pub(super) const DEVICE_WIDTH: f32 = 224.0;

#[derive(Clone, Default, Debug)]
pub(super) struct Groups {
    pub primary: Vec<MenuAction>,
    pub paste: Vec<MenuAction>,
    pub location: Vec<MenuAction>,
    pub rows: Vec<MenuAction>,
    pub global: Vec<MenuAction>,
}

impl Groups {
    pub fn new(entries: Vec<MenuEntry>) -> Self {
        let mut actions: Vec<_> = entries
            .into_iter()
            .filter_map(|entry| match entry {
                MenuEntry::Action(action) => Some(action),
                _ => None,
            })
            .collect();
        // Native normalized paste is the same capability as plain text.
        if actions
            .iter()
            .any(|action| action.key == "paste-plain-text")
        {
            actions.retain(|action| action.key != "paste-standard-text");
        }
        let take = |actions: &mut Vec<MenuAction>, keys: &[&str]| -> Vec<MenuAction> {
            keys.iter()
                .filter_map(|key| {
                    actions
                        .iter()
                        .position(|action| action.key == *key)
                        .map(|index| actions.remove(index))
                })
                .collect()
        };
        let primary = take(
            &mut actions,
            &["paste", "copy-only", "favorite", "pin-toggle", "delete"],
        );
        let paste = take(
            &mut actions,
            &[
                "paste-plain-text",
                "paste-standard-text",
                "reverse-paste",
                "fifo-paste",
            ],
        );
        let location = take(&mut actions, &["reveal-file", "reveal-folder"]);
        let global = take(&mut actions, &["distinct"]);
        Self {
            primary,
            paste,
            location,
            rows: actions,
            global,
        }
    }

    pub fn actions(&self) -> impl Iterator<Item = &MenuAction> {
        self.primary
            .iter()
            .chain(&self.paste)
            .chain(&self.location)
            .chain(&self.rows)
            .chain(&self.global)
    }

    pub fn height(&self, width: f32, multiple: bool) -> f32 {
        let mut height = 10.0 + 30.0;
        if !self.primary.is_empty() {
            height += 38.0;
        }
        for (index, items) in [&self.paste, &self.location].into_iter().enumerate() {
            if items.is_empty() {
                continue;
            }
            height += 6.0 + if index == 1 { 1.0 } else { 0.0 };
            for pair in items.chunks(2) {
                height += secondary_height(pair, width - 10.0, index == 1 && multiple);
            }
            height += items.len().div_ceil(2).saturating_sub(1) as f32 * 2.0;
        }
        if !self.rows.is_empty() {
            height += 7.0 + 30.0 * self.rows.len() as f32;
        }
        if !self.global.is_empty() {
            height += 4.0 + 30.0 * self.global.len() as f32;
        }
        height
    }
}

pub(super) fn secondary_height(items: &[MenuAction], width: f32, scope: bool) -> f32 {
    let cell = if items.len() == 1 {
        width
    } else {
        (width - 2.0) * 0.5
    };
    items
        .iter()
        .map(|action| {
            let label_width: f32 = action
                .label
                .chars()
                .map(|ch| if ch.is_ascii() { 6.0 } else { 12.0 })
                .sum();
            let lines = (label_width / (cell - 38.0 - if scope { 34.0 } else { 0.0 }).max(20.0))
                .ceil()
                .max(1.0);
            28.0 + (lines - 1.0) * 18.0
        })
        .fold(28.0, f32::max)
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Layout {
    pub window: (i32, i32, i32, i32),
    pub scale: f64,
    pub root: Rect,
    pub device_x: f32,
    pub device_width: f32,
    pub device_height: f32,
    pub work_top: f32,
    pub work_bottom: f32,
    pub opens_left: bool,
}

impl Layout {
    pub fn place(
        point: (i32, i32),
        work: (i32, i32, i32, i32),
        scale: f64,
        root_height: f32,
        peer_count: usize,
    ) -> Self {
        let scale = if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            1.0
        };
        let (ww, wh) = (
            (f64::from(work.2.max(1)) / scale) as f32,
            (f64::from(work.3.max(1)) / scale) as f32,
        );
        let margin = SHADOW.min(ww * 0.25).min(wh * 0.25);
        let width = ROOT_WIDTH.min((ww - margin * 2.0).max(1.0));
        let height = root_height.min((wh - margin * 2.0).max(1.0));
        let device_width = DEVICE_WIDTH.min((ww - margin * 2.0).max(1.0));
        let device_height = (10.0 + 23.0 + 30.0 + 7.0 + peer_count.max(1) as f32 * 30.0)
            .min((wh - margin * 2.0).max(1.0));
        let x = ((f64::from(point.0 - work.0) / scale) as f32)
            .clamp(margin, (ww - width - margin).max(margin));
        let y = ((f64::from(point.1 - work.1) / scale) as f32)
            .clamp(margin, (wh - height - margin).max(margin));
        let (right, left) = (ww - x - width - margin, x - margin);
        let opens_left = right < device_width + 4.0 && left > right;
        let child_x = if opens_left {
            x - device_width - 4.0
        } else {
            x + width + 4.0
        }
        .clamp(margin, (ww - device_width - margin).max(margin));
        // Reserve the possible union up front, so showing a child never moves
        // the root or changes its monitor. Hidden reserved pixels are shaped out.
        let child_top = y.min((wh - device_height - margin).max(margin));
        let child_bottom =
            (y + height).min((wh - device_height - margin).max(margin)) + device_height;
        let ox = (x.min(child_x) - margin).max(0.0);
        let oy = (y.min(child_top) - margin).max(0.0);
        let right = (x + width).max(child_x + device_width) + margin;
        let bottom = (y + height).max(child_bottom) + margin;
        let physical = |v: f32| (f64::from(v) * scale).floor() as i32;
        let px = work.0 + physical(ox);
        let py = work.1 + physical(oy);
        let end_x = work.0 + (f64::from(right.min(ww)) * scale).ceil() as i32;
        let end_y = work.1 + (f64::from(bottom.min(wh)) * scale).ceil() as i32;
        let actual_ox = (f64::from(px - work.0) / scale) as f32;
        let actual_oy = (f64::from(py - work.1) / scale) as f32;
        Self {
            window: (px, py, end_x - px, end_y - py),
            scale,
            root: (x - actual_ox, y - actual_oy, width, height),
            device_x: child_x - actual_ox,
            device_width,
            device_height,
            work_top: margin - actual_oy,
            work_bottom: wh - margin - actual_oy,
            opens_left,
        }
    }

    pub fn devices(&self, row_top: f32) -> Rect {
        (
            self.device_x,
            row_top.clamp(
                self.work_top,
                (self.work_bottom - self.device_height).max(self.work_top),
            ),
            self.device_width,
            self.device_height,
        )
    }
}

pub(super) fn contains(rect: Rect, x: f32, y: f32) -> bool {
    x >= rect.0 && y >= rect.1 && x < rect.0 + rect.2 && y < rect.1 + rect.3
}
pub(super) fn shadow(rect: Rect) -> Rect {
    (
        rect.0 - SHADOW,
        rect.1 - SHADOW,
        rect.2 + 2.0 * SHADOW,
        rect.3 + 2.0 * SHADOW,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn history_menu_groups_merge_native_alias_and_preserve_scope_order() {
        let groups = Groups::new(vec![
            MenuAction::tile("delete", "删除"),
            MenuAction::tile("paste", "粘贴"),
            MenuEntry::Label("旧分组标题".into()),
            MenuAction::tile("paste-standard-text", "规范粘贴"),
            MenuAction::tile("paste-plain-text", "纯文本粘贴"),
            MenuAction::row("lan-share", "局域网"),
            MenuAction::row("distinct", "去重"),
        ]);
        assert_eq!(
            groups.primary.iter().map(|a| a.key).collect::<Vec<_>>(),
            ["paste", "delete"]
        );
        assert_eq!(
            groups.paste.iter().map(|a| a.key).collect::<Vec<_>>(),
            ["paste-plain-text"]
        );
        assert_eq!(groups.rows[0].key, "lan-share");
        assert_eq!(groups.global[0].key, "distinct");
    }
    #[test]
    fn history_menu_geometry_flips_and_keeps_the_root_stable_inside_work_area() {
        for scale in [1.0, 1.25, 1.5, 2.0] {
            let work = (-1600, 0, 1600, 900);
            let layout = Layout::place((-20, 780), work, scale, 300.0, 3);
            assert!(layout.opens_left);
            for rect in [
                layout.root,
                layout.devices(layout.root.1),
                layout.devices(layout.root.1 + layout.root.3),
            ] {
                let outer = shadow(rect);
                assert!(outer.0 >= -0.1 && outer.1 >= -0.1);
                assert!(outer.0 + outer.2 <= layout.window.2 as f32 / scale as f32 + 1.0);
                assert!(outer.1 + outer.3 <= layout.window.3 as f32 / scale as f32 + 1.0);
            }
        }
    }
    #[test]
    fn history_menu_narrow_overlap_respects_work_area_and_distinct_card_hits() {
        let layout = Layout::place((300, 260), (0, 0, 320, 400), 1.0, 290.0, 2);
        let child = layout.devices(layout.root.1 + 190.0);
        let overlap = (layout.root.0.max(child.0) + 10.0, layout.root.1.max(child.1) + 10.0);
        assert!(contains(layout.root, overlap.0, overlap.1));
        assert!(contains(child, overlap.0, overlap.1));
        assert!(child.0 >= 0.0 && child.0 + child.2 <= 320.0);
        assert!(!contains(layout.root, -1.0, -1.0));
        assert!(!contains(child, child.0 + child.2, child.1));
    }
}
