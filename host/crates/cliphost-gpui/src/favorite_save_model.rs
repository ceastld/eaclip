//! Folder occurrences, rather than folder IDs, identify a save destination.
//! Shared children retain their distinct visible paths; cycles never recurse.

use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

pub(super) type Path = Vec<String>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum ChoiceMode {
    Browse,
    QuickSave,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Choice {
    pub folder: Folder,
    pub mode: ChoiceMode,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Folder {
    pub id: String,
    pub path: Path,
    pub title: String,
    pub label: String,
    pub has_children: bool,
}

#[derive(Default)]
pub(super) struct Choices {
    pub revision: String,
    pub folders: Vec<Folder>,
    pub recent: Vec<Folder>,
    pub truncated: bool,
}

impl Choices {
    pub fn parse(value: &Value) -> Result<Self, String> {
        let revision = value["dragRevision"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or("Favorite folders have no revision.")?
            .to_owned();
        let items = value["items"]
            .as_object()
            .ok_or("Invalid favorite folders.")?;
        let adjacency: HashMap<_, Vec<_>> = value["childrenById"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(id, children)| {
                (
                    id.as_str(),
                    children
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .collect(),
                )
            })
            .collect();
        let roots = value["orderedRootItems"]
            .as_array()
            .ok_or("Invalid favorite roots.")?;
        let folder = |id: &str| {
            items
                .get(id)
                .filter(|item| item["isFolder"].as_bool() == Some(true))
        };
        let mut stack: Vec<(Path, Vec<String>)> = roots
            .iter()
            .rev()
            .filter_map(Value::as_str)
            .map(|id| (vec![id.to_owned()], Vec::new()))
            .collect();
        let mut result = Self {
            revision,
            ..Self::default()
        };
        let mut visited = HashSet::new();
        while let Some((path, mut labels)) = stack.pop() {
            if visited.len() >= 20_000 {
                result.truncated = true;
                break;
            }
            if !visited.insert(path.clone()) {
                continue;
            }
            let id = path.last().unwrap();
            let Some(item) = folder(id) else {
                continue;
            };
            let title = item["title"]
                .as_str()
                .filter(|name| !name.trim().is_empty())
                .unwrap_or(id)
                .to_owned();
            labels.push(title.clone());
            let next: Vec<_> = adjacency
                .get(id.as_str())
                .into_iter()
                .flatten()
                .copied()
                .filter(|child| folder(child).is_some() && !path.iter().any(|id| id == child))
                .collect();
            let has_children = !next.is_empty() && path.len() < 128;
            result.folders.push(Folder {
                id: id.clone(),
                path: path.clone(),
                title,
                label: labels.join(" / "),
                has_children,
            });
            if has_children {
                stack.extend(next.into_iter().rev().map(|child| {
                    let mut child_path = path.clone();
                    child_path.push(child.to_owned());
                    (child_path, labels.clone())
                }));
            } else if !next.is_empty() {
                result.truncated = true;
            }
        }
        for recent in value["recentFolders"].as_array().into_iter().flatten() {
            let path: Path = recent["path"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect();
            if let Some(found) = result.folders.iter().find(|folder| {
                folder.path == path && recent["id"].as_str() == Some(folder.id.as_str())
            }) {
                if !result.recent.iter().any(|folder| folder.path == path) {
                    result.recent.push(found.clone());
                    if result.recent.len() == 3 {
                        break;
                    }
                }
            }
        }
        Ok(result)
    }

    pub fn members(&self, parent: &[String]) -> Vec<Folder> {
        self.folders
            .iter()
            .filter(|folder| {
                folder.path.len() == parent.len() + 1 && folder.path.starts_with(parent)
            })
            .cloned()
            .collect()
    }

    pub fn search(&self, query: &str) -> Vec<Folder> {
        let query = query.trim().to_lowercase();
        self.folders
            .iter()
            .filter(|folder| folder.label.to_lowercase().contains(&query))
            .cloned()
            .collect()
    }

    pub fn label(&self, path: &[String]) -> String {
        self.folders
            .iter()
            .find(|folder| folder.path == path)
            .map(|folder| folder.label.clone())
            .unwrap_or_default()
    }

    pub fn destination(&self, path: &[String]) -> Option<Folder> {
        if path.is_empty() {
            return None;
        }
        self.folders
            .iter()
            .find(|folder| folder.path == path)
            .cloned()
    }

    /// Keyboard order mirrors recent shortcuts and this column's real folders.
    pub fn choices(&self, path: &[String], query: &str) -> Vec<Choice> {
        let mut result = Vec::new();
        if !query.trim().is_empty() {
            result.extend(self.search(query).into_iter().map(|folder| Choice {
                folder,
                mode: ChoiceMode::QuickSave,
            }));
        } else {
            if path.is_empty() {
                result.extend(self.recent.iter().cloned().map(|folder| Choice {
                    folder,
                    mode: ChoiceMode::QuickSave,
                }));
            }
            result.extend(self.members(path).into_iter().map(|folder| Choice {
                folder,
                mode: ChoiceMode::Browse,
            }));
        }
        result
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Layout {
    pub rect: (i32, i32, i32, i32),
    pub root_x: f32,
    pub root_width: f32,
    pub height: f32,
    pub direction: f32,
    pub columns: usize,
}

impl Layout {
    /// Reserve one shaped HWND for at most three cards. Transparent reserved
    /// space is excluded by the actual card regions after painting.
    pub fn place(point: (i32, i32), work: (i32, i32, i32, i32), scale: f64) -> Self {
        Self::place_for_trigger(point, work, scale, None)
    }

    pub fn place_for_trigger(
        point: (i32, i32),
        work: (i32, i32, i32, i32),
        scale: f64,
        trigger: Option<(i32, i32, i32, i32)>,
    ) -> Self {
        let scale = scale.max(0.5);
        let width = work.2 as f32 / scale as f32;
        let work_height = work.3 as f32 / scale as f32;
        let root_width = (width - 16.0).min(260.0).max(40.0);
        let trigger_y = (point.1 - work.1) as f32 / scale as f32;
        let below = (work_height - trigger_y - 14.0).max(0.0);
        let trigger_top = trigger.map_or(trigger_y - 28.0, |(_, y, _, _)| {
            (y - work.1) as f32 / scale as f32
        });
        let above = (trigger_top - 14.0).max(0.0);
        let open_below = below >= 240.0 || below >= above;
        let height = (if open_below { below } else { above })
            .min(420.0)
            .max(40.0)
            .min((work_height - 16.0).max(1.0));
        let root_screen = ((point.0 - work.0) as f32 / scale as f32 - root_width)
            .clamp(8.0, (width - root_width - 8.0).max(8.0));
        let left = root_screen - 8.0;
        let right = width - root_screen - root_width - 8.0;
        let direction = if right > left { 1.0 } else { -1.0 };
        let columns = (1 + (left.max(right) / 210.0).floor() as usize).min(3);
        let extra = (columns - 1) as f32 * 210.0;
        let root_x = 8.0 + if direction < 0.0 { extra } else { 0.0 };
        let physical_width = ((root_width + extra + 16.0) as f64 * scale).ceil() as i32;
        let x = (work.0 + ((root_screen - root_x) as f64 * scale).round() as i32)
            .clamp(work.0, work.0 + work.2 - physical_width.min(work.2));
        let physical_height = ((height + 16.0) as f64 * scale).ceil() as i32;
        let card_top = if open_below {
            trigger_y + 6.0
        } else {
            trigger_top - 6.0 - height
        };
        let y = (work.1 + ((card_top - 8.0) as f64 * scale).round() as i32)
            .clamp(work.1, work.1 + work.3 - physical_height.min(work.3));
        Self {
            rect: (
                x,
                y,
                physical_width.min(work.2),
                physical_height.min(work.3),
            ),
            root_x,
            root_width,
            height,
            direction,
            columns,
        }
    }

    pub fn column_x(&self, index: usize) -> f32 {
        if index == 0 {
            self.root_x
        } else if self.direction > 0.0 {
            self.root_x + self.root_width + (index - 1) as f32 * 210.0
        } else {
            self.root_x - index as f32 * 210.0
        }
    }
}

/// Permission to use native input is not editor focus. A list button reached
/// from search must not keep a hover session alive after the pointer leaves.
pub(super) fn text_input_holds_open(
    native_active: bool,
    search_focused: bool,
    composing: bool,
) -> bool {
    native_active && (search_focused || composing)
}

/// Logical row focus is deliberately absent: only real pointer engagement,
/// explicit native text input, and an accepted save suspend hover dismissal.
#[derive(Default)]
pub(super) struct LeaveDeadline {
    started: Option<Instant>,
}
impl LeaveDeadline {
    pub fn observe(
        &mut self,
        inside: bool,
        native_input: bool,
        saving: bool,
        now: Instant,
        delay: Duration,
    ) -> bool {
        if inside || native_input || saving {
            self.started = None;
            return false;
        }
        let started = self.started.get_or_insert(now);
        now.saturating_duration_since(*started) >= delay
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn shared_folders_keep_full_paths_and_cycles_stop() {
        let data = json!({"dragRevision":"rev", "orderedRootItems":["a","b"],
            "items":{"a":{"title":"工作","isFolder":true},"b":{"title":"个人","isFolder":true},
                "same":{"title":"常用","isFolder":true},"text":{"title":"正文","isFolder":false}},
            "childrenById":{"a":["same","text"],"b":["same"],"same":["a"]},
            "recentFolders":[{"id":"same","path":["b","same"]},{"id":"a","path":["bad"]}]});
        let choices = Choices::parse(&data).unwrap();
        let same: Vec<_> = choices
            .folders
            .iter()
            .filter(|folder| folder.id == "same")
            .collect();
        assert_eq!(same.len(), 2);
        assert_ne!(same[0].path, same[1].path);
        assert_eq!(choices.recent[0].label, "个人 / 常用");
        assert_eq!(choices.recent.len(), 1);
        assert!(choices.folders.iter().all(|folder| folder.path.len() <= 3));
        assert_eq!(choices.members(&["a".into()])[0].id, "same");
        assert!(choices
            .search("个人 / 常用")
            .iter()
            .any(|folder| folder.id == "a"));
    }

    #[test]
    fn cascade_cards_stay_on_screen_without_covering_trigger() {
        for width in [320, 420, 760, 1920] {
            for point in [(3, 10), (width - 5, 710)] {
                let layout = Layout::place(point, (0, 0, width, 800), 1.0);
                assert!(layout.rect.0 >= 0 && layout.rect.1 >= 0);
                assert!(layout.rect.0 + layout.rect.2 <= width);
                assert!(layout.rect.1 + layout.rect.3 <= 800);
                assert!(layout.root_width <= 260.0);
                assert!((1..=3).contains(&layout.columns));
                for column in 0..layout.columns {
                    assert!(layout.column_x(column) >= 8.0);
                    let card_width = if column == 0 {
                        layout.root_width
                    } else {
                        210.0
                    };
                    assert!(layout.column_x(column) + card_width <= layout.rect.2 as f32 - 7.0);
                }
                let card_top = layout.rect.1 as f32 + 8.0;
                let card_bottom = card_top + layout.height;
                assert!(card_top >= point.1 as f32 + 6.0 || card_bottom <= point.1 as f32 - 28.0);
            }
        }
    }

    #[test]
    fn cascade_destinations_keep_leaf_paths_and_recent_shortcuts() {
        let data = json!({"dragRevision":"rev", "orderedRootItems":["a"],
            "items":{"a":{"title":"Work","isFolder":true},"leaf":{"title":"Leaf","isFolder":true}},
            "childrenById":{"a":["leaf"]},"recentFolders":[{"id":"leaf","path":["a","leaf"]}]});
        let choices = Choices::parse(&data).unwrap();
        let root = choices.choices(&[], "");
        assert_eq!(
            root.iter().map(|choice| choice.mode).collect::<Vec<_>>(),
            [ChoiceMode::QuickSave, ChoiceMode::Browse]
        );
        assert!(choices.destination(&[]).is_none());
        let leaf = vec!["a".into(), "leaf".into()];
        assert!(choices.choices(&leaf, "").is_empty());
        assert_eq!(choices.destination(&leaf).unwrap().id, "leaf");
        assert_eq!(choices.choices(&[], "Leaf")[0].mode, ChoiceMode::QuickSave);
    }

    #[test]
    fn monitor_scale_and_negative_coordinates_keep_reserved_window_inside_work_area() {
        let work = (-1920, -180, 1920, 1080);
        for scale in [1.25, 1.5, 2.0] {
            let layout = Layout::place((-8, 890), work, scale);
            assert!(layout.rect.0 >= work.0 && layout.rect.1 >= work.1);
            assert!(layout.rect.0 + layout.rect.2 <= work.0 + work.2);
            assert!(layout.rect.1 + layout.rect.3 <= work.1 + work.3);
            assert!(((layout.root_x + layout.root_width) as f64 * scale) <= layout.rect.2 as f64);
        }
    }

    #[test]
    fn upward_opening_uses_the_real_trigger_top_at_monitor_scale() {
        let work = (-1800, -300, 1800, 1000);
        for scale in [1.0, 1.25, 1.5, 2.0] {
            let trigger = (-400, 600, 40, 50);
            let layout = Layout::place_for_trigger((-360, 650), work, scale, Some(trigger));
            let card_bottom = layout.rect.1 as f64 + (8.0 + layout.height as f64) * scale;
            assert!(card_bottom <= trigger.1 as f64 - 5.0 * scale);
        }
    }

    #[test]
    fn hover_leave_waits_for_continuous_outside_time_and_reentry_cancels() {
        let now = Instant::now();
        let delay = Duration::from_millis(360);
        let mut leave = LeaveDeadline::default();
        assert!(!leave.observe(false, false, false, now, delay));
        assert!(!leave.observe(false, false, false, now + Duration::from_millis(359), delay));
        assert!(!leave.observe(true, false, false, now + Duration::from_millis(359), delay));
        assert!(!leave.observe(false, false, false, now + Duration::from_millis(400), delay));
        assert!(!leave.observe(false, false, false, now + Duration::from_millis(759), delay));
        assert!(leave.observe(false, false, false, now + Duration::from_millis(760), delay));
    }

    #[test]
    fn editor_focus_protection_ends_when_keyboard_navigation_focuses_a_row() {
        let now = Instant::now();
        let delay = Duration::from_millis(360);
        let mut leave = LeaveDeadline::default();
        assert!(!leave.observe(
            false,
            text_input_holds_open(true, true, false),
            false,
            now,
            delay
        ));
        // Up/Down moves focus from the editor to the list, while the native
        // text-input lease stays opted in. That lease is deliberately absent.
        assert!(!leave.observe(
            false,
            text_input_holds_open(true, false, false),
            false,
            now + delay,
            delay
        ));
        assert!(leave.observe(
            false,
            text_input_holds_open(true, false, false),
            false,
            now + delay * 2,
            delay
        ));
        assert!(text_input_holds_open(true, false, true));
        assert!(
            !text_input_holds_open(false, true, true),
            "stale marked text in an inactive window cannot retain the popup"
        );
    }

    #[test]
    fn only_explicit_native_input_or_accepted_save_holds_hover_open() {
        let now = Instant::now();
        let delay = Duration::from_millis(360);
        let mut leave = LeaveDeadline::default();
        assert!(!leave.observe(false, false, false, now, delay));
        assert!(!leave.observe(false, true, false, now + delay, delay));
        assert!(!leave.observe(false, false, false, now + delay * 2, delay));
        // A merely focused ordinary row passes native_input=false, so closes.
        assert!(leave.observe(false, false, false, now + delay * 3, delay));
        assert!(!leave.observe(false, false, true, now + delay * 4, delay));
        assert!(!leave.observe(false, false, false, now + delay * 5, delay));
        assert!(leave.observe(false, false, false, now + delay * 6, delay));
    }
}
