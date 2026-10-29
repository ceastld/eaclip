//! Field-local text history for editor-owned native inputs.
//!
//! Matches the local CodeMirror `history()` defaults for ordinary text edits:
//! adjacent typing/deletion events join within 500ms of the previous edit, and
//! branches retain at least 100 entries with CodeMirror's lazy trimming slack.
//! Paste and external replacements are recorded in isolated groups. The native
//! input records composition once, using its first preedit's committed snapshot;
//! that commit is isolated rather than joining a preceding CodeMirror compose
//! event. Live preedit must never be passed to this model.
//!
//! A new item or field context must call `reset`, even when its text is identical.
//! Same-text quiet acknowledgments should keep this model and the live selection.
//! Different quiet replacements must explicitly reset or record `External`.

use std::{
    ops::Range,
    time::{Duration, Instant},
};

const NEW_GROUP_DELAY: Duration = Duration::from_millis(500);
const MIN_DEPTH: usize = 100;
const TRIM_SLACK: usize = 20;

/// All selection positions are UTF-8 bytes in `text`, including reversed ranges.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TextSnapshot {
    pub text: String,
    pub selection: Range<usize>,
    pub reversed: bool,
}

impl TextSnapshot {
    fn normalized(mut self) -> Self {
        let start = floor_boundary(&self.text, self.selection.start);
        let end = floor_boundary(&self.text, self.selection.end);
        self.selection = start.min(end)..start.max(end);
        self.reversed &= !self.selection.is_empty();
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TextEditKind {
    Typing,
    Delete,
    Paste,
    Composition,
    /// An undoable parent-owned change within the current field lifecycle.
    External,
}

impl TextEditKind {
    fn joinable(self) -> bool {
        matches!(self, Self::Typing | Self::Delete)
    }
}

#[derive(Debug)]
struct EditGroup {
    last_edit: Instant,
    /// The grouped inverse change occupies this range in the current text.
    affected: Range<usize>,
}

#[derive(Default, Debug)]
pub(crate) struct EditorTextHistory {
    undo: Vec<TextSnapshot>,
    redo: Vec<TextSnapshot>,
    current: Option<TextSnapshot>,
    group: Option<EditGroup>,
}

impl EditorTextHistory {
    pub fn reset(&mut self, snapshot: TextSnapshot) {
        self.undo.clear();
        self.redo.clear();
        self.group = None;
        self.current = Some(snapshot.normalized());
    }

    /// Record one committed native replacement, never an intermediate preedit.
    /// `range` belongs to `before.text`; `inserted` must exactly yield `after.text`.
    /// An inconsistent caller baseline/range starts a safe new lifecycle instead
    /// of allowing an older whole-text snapshot to overwrite an unrelated field.
    pub fn record(
        &mut self,
        before: TextSnapshot,
        after: TextSnapshot,
        range: Range<usize>,
        inserted: &str,
        kind: TextEditKind,
        now: Instant,
    ) {
        let before = before.normalized();
        let after = after.normalized();
        if self
            .current
            .as_ref()
            .is_some_and(|current| current.text != before.text)
        {
            self.reset(before.clone());
        }
        // A cancelled empty composition or boundary delete must neither consume
        // redo nor extend/cut an existing typing group.
        if before.text == after.text {
            self.current = Some(after);
            return;
        }
        if !is_replacement(&before.text, &after.text, &range, inserted) {
            self.reset(after);
            return;
        }
        let merge = kind.joinable()
            && self.group.as_ref().is_some_and(|group| {
                now.checked_duration_since(group.last_edit)
                    .is_some_and(|elapsed| elapsed < NEW_GROUP_DELAY)
                    && touches(&group.affected, &range)
            });
        let affected = if merge {
            let previous = &self.group.as_ref().expect("merged group").affected;
            let mapped = map_changed_range(previous, &range, inserted.len());
            mapped.start.min(range.start)..mapped.end.max(range.start + inserted.len())
        } else {
            push_branch(&mut self.undo, before);
            range.start..range.start + inserted.len()
        };
        self.redo.clear();
        self.current = Some(after);
        self.group = kind.joinable().then_some(EditGroup {
            last_edit: now,
            affected,
        });
    }

    /// Pure navigation cuts a group, but creates no text undo step and keeps redo.
    pub fn selection_changed(&mut self, snapshot: TextSnapshot) {
        let snapshot = snapshot.normalized();
        let Some(current) = &self.current else {
            self.reset(snapshot);
            return;
        };
        if current.text != snapshot.text {
            self.reset(snapshot);
            return;
        }
        if current.selection != snapshot.selection || current.reversed != snapshot.reversed {
            self.cutoff();
        }
        self.current = Some(snapshot);
    }

    pub fn cutoff(&mut self) {
        self.group = None;
    }

    pub fn undo(&mut self, current: TextSnapshot) -> Option<TextSnapshot> {
        let current = self.accept_current(current)?;
        let previous = self.undo.pop()?;
        push_branch(&mut self.redo, current);
        self.current = Some(previous.clone());
        self.cutoff();
        Some(previous)
    }

    pub fn redo(&mut self, current: TextSnapshot) -> Option<TextSnapshot> {
        let current = self.accept_current(current)?;
        let next = self.redo.pop()?;
        push_branch(&mut self.undo, current);
        self.current = Some(next.clone());
        self.cutoff();
        Some(next)
    }

    pub fn current(&self) -> Option<&TextSnapshot> {
        self.current.as_ref()
    }

    pub fn undo_depth(&self) -> usize {
        self.undo.len()
    }

    pub fn redo_depth(&self) -> usize {
        self.redo.len()
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    fn accept_current(&mut self, current: TextSnapshot) -> Option<TextSnapshot> {
        let current = current.normalized();
        if !self
            .current
            .as_ref()
            .is_some_and(|known| known.text == current.text)
        {
            self.reset(current);
            return None;
        }
        self.current = Some(current.clone());
        Some(current)
    }
}

fn floor_boundary(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

fn is_replacement(before: &str, after: &str, range: &Range<usize>, inserted: &str) -> bool {
    if range.start > range.end
        || range.end > before.len()
        || !before.is_char_boundary(range.start)
        || !before.is_char_boundary(range.end)
    {
        return false;
    }
    let Some(end) = range.start.checked_add(inserted.len()) else {
        return false;
    };
    if end > after.len() || !after.is_char_boundary(range.start) || !after.is_char_boundary(end) {
        return false;
    }
    before[..range.start] == after[..range.start]
        && inserted == &after[range.start..end]
        && before[range.end..] == after[end..]
}

fn touches(a: &Range<usize>, b: &Range<usize>) -> bool {
    b.end >= a.start && b.start <= a.end
}

fn map_changed_range(
    previous: &Range<usize>,
    edit: &Range<usize>,
    inserted: usize,
) -> Range<usize> {
    let map = |offset: usize, end: bool| {
        if offset < edit.start {
            offset
        } else if offset > edit.end {
            offset - (edit.end - edit.start) + inserted
        } else if end {
            edit.start + inserted
        } else {
            edit.start
        }
    };
    map(previous.start, false)..map(previous.end, true)
}

fn push_branch(branch: &mut Vec<TextSnapshot>, snapshot: TextSnapshot) {
    // Local @codemirror/commands updateBranch: minDepth=100, 20-entry slack;
    // when appending entry 121, remove 19 old entries and retain 102.
    if branch.len() >= MIN_DEPTH + TRIM_SLACK {
        let remove = branch.len() - MIN_DEPTH - 1;
        branch.drain(..remove);
    }
    branch.push(snapshot);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(text: &str, selection: Range<usize>, reversed: bool) -> TextSnapshot {
        TextSnapshot {
            text: text.into(),
            selection,
            reversed,
        }
    }
    fn caret(text: &str, offset: usize) -> TextSnapshot {
        snapshot(text, offset..offset, false)
    }
    fn insert(
        history: &mut EditorTextHistory,
        before: &str,
        at: usize,
        text: &str,
        kind: TextEditKind,
        now: Instant,
    ) -> TextSnapshot {
        let mut after = before.to_string();
        after.insert_str(at, text);
        let after = caret(&after, at + text.len());
        history.record(caret(before, at), after.clone(), at..at, text, kind, now);
        after
    }

    #[test]
    fn adjacent_typing_uses_previous_edit_time_instead_of_group_start() {
        let now = Instant::now();
        let mut history = EditorTextHistory::default();
        history.reset(caret("", 0));
        insert(&mut history, "", 0, "A", TextEditKind::Typing, now);
        insert(
            &mut history,
            "A",
            1,
            "B",
            TextEditKind::Typing,
            now + Duration::from_millis(400),
        );
        let after = insert(
            &mut history,
            "AB",
            2,
            "C",
            TextEditKind::Typing,
            now + Duration::from_millis(800),
        );
        assert_eq!(history.undo_depth(), 1);
        assert_eq!(history.undo(after), Some(caret("", 0)));
    }

    #[test]
    fn exact_delay_and_nonadjacent_ranges_start_separate_events() {
        let now = Instant::now();
        let mut history = EditorTextHistory::default();
        history.reset(caret("abcd", 0));
        insert(&mut history, "abcd", 0, "X", TextEditKind::Typing, now);
        insert(
            &mut history,
            "Xabcd",
            1,
            "Y",
            TextEditKind::Typing,
            now + NEW_GROUP_DELAY,
        );
        // Spatially distant changes cannot join even without a navigation event.
        insert(
            &mut history,
            "XYabcd",
            6,
            "Z",
            TextEditKind::Typing,
            now + NEW_GROUP_DELAY + Duration::from_millis(1),
        );
        assert_eq!(history.undo_depth(), 3);
    }

    #[test]
    fn typing_and_adjacent_backward_and_forward_deletes_can_join() {
        let now = Instant::now();
        let mut history = EditorTextHistory::default();
        history.reset(caret("AB", 1));
        let inserted = insert(&mut history, "AB", 1, "X", TextEditKind::Typing, now);
        let deleted = caret("AB", 1);
        history.record(
            inserted,
            deleted.clone(),
            1..2,
            "",
            TextEditKind::Delete,
            now + Duration::from_millis(10),
        );
        let forward = caret("A", 1);
        history.record(
            deleted,
            forward.clone(),
            1..2,
            "",
            TextEditKind::Delete,
            now + Duration::from_millis(20),
        );
        assert_eq!(history.undo_depth(), 1);
        assert_eq!(history.undo(forward), Some(caret("AB", 1)));
    }

    #[test]
    fn explicit_selection_navigation_cuts_typing_without_creating_a_step() {
        let now = Instant::now();
        let mut history = EditorTextHistory::default();
        history.reset(caret("", 0));
        insert(&mut history, "", 0, "AB", TextEditKind::Typing, now);
        history.selection_changed(snapshot("AB", 0..2, true));
        let after = caret("X", 1);
        history.record(
            snapshot("AB", 0..2, true),
            after.clone(),
            0..2,
            "X",
            TextEditKind::Typing,
            now + Duration::from_millis(10),
        );
        assert_eq!(history.undo_depth(), 2);
        assert_eq!(history.undo(after), Some(snapshot("AB", 0..2, true)));
    }

    #[test]
    fn paste_and_external_replacement_are_undoable_and_isolated_on_both_sides() {
        let now = Instant::now();
        let mut history = EditorTextHistory::default();
        history.reset(caret("", 0));
        insert(&mut history, "", 0, "A", TextEditKind::Typing, now);
        insert(&mut history, "A", 1, "B", TextEditKind::Paste, now);
        insert(&mut history, "AB", 2, "C", TextEditKind::Typing, now);
        let external = snapshot("canon", 1..4, true);
        history.record(
            caret("ABC", 3),
            external.clone(),
            0..3,
            "canon",
            TextEditKind::External,
            now,
        );
        history.selection_changed(caret("canon", 5));
        let mut current = insert(&mut history, "canon", 5, "!", TextEditKind::Typing, now);
        assert_eq!(history.undo_depth(), 5);
        for expected in [
            caret("canon", 5),
            caret("ABC", 3),
            caret("AB", 2),
            caret("A", 1),
            caret("", 0),
        ] {
            current = history.undo(current).unwrap();
            assert_eq!(current, expected);
        }
    }

    #[test]
    fn composition_commit_restores_original_reversed_unicode_selection_as_one_step() {
        let now = Instant::now();
        let mut history = EditorTextHistory::default();
        let before = snapshot("A😀B", 1..5, true);
        history.reset(before.clone());
        // Native preedit is not recorded. The sole commit retains its original
        // snapshot, regardless of the number of candidate/preedit replacements.
        let after = snapshot("A中文B", 1..4, false);
        history.record(
            before.clone(),
            after.clone(),
            1..5,
            "中文",
            TextEditKind::Composition,
            now,
        );
        assert_eq!(history.undo_depth(), 1);
        let restored = history.undo(after.clone()).unwrap();
        assert_eq!(restored, before);
        assert_eq!(history.redo(restored), Some(after));
    }

    #[test]
    fn composition_commit_is_isolated_from_typing_on_both_sides() {
        let now = Instant::now();
        let mut history = EditorTextHistory::default();
        history.reset(caret("", 0));
        let before = insert(&mut history, "", 0, "A", TextEditKind::Typing, now);
        let composed = caret("A中", 4);
        history.record(
            before,
            composed.clone(),
            1..1,
            "中",
            TextEditKind::Composition,
            now,
        );
        let after = insert(&mut history, "A中", 4, "B", TextEditKind::Typing, now);
        assert_eq!(history.undo_depth(), 3);
        assert!(history.can_undo());
        assert!(!history.can_redo());
        assert_eq!(history.undo(after), Some(composed));
        assert!(history.can_redo());
    }

    #[test]
    fn no_op_does_not_clear_redo_or_cut_or_extend_a_typing_group() {
        let now = Instant::now();
        let mut history = EditorTextHistory::default();
        history.reset(caret("", 0));
        let after = insert(&mut history, "", 0, "A", TextEditKind::Typing, now);
        history.record(
            after.clone(),
            after.clone(),
            1..1,
            "",
            TextEditKind::Paste,
            now + Duration::from_millis(450),
        );
        insert(
            &mut history,
            "A",
            1,
            "B",
            TextEditKind::Typing,
            now + Duration::from_millis(600),
        );
        assert_eq!(history.undo_depth(), 2); // no-op did not refresh last-edit time
        let restored = history.undo(caret("AB", 2)).unwrap();
        history.record(
            restored.clone(),
            restored.clone(),
            1..1,
            "",
            TextEditKind::External,
            now,
        );
        assert_eq!(history.redo_depth(), 1);
        assert_eq!(history.redo(restored), Some(caret("AB", 2)));
    }

    #[test]
    fn no_op_keeps_adjacent_group_and_identical_selection_notify_does_not_cut() {
        let now = Instant::now();
        let mut history = EditorTextHistory::default();
        history.reset(caret("", 0));
        let after = insert(&mut history, "", 0, "A", TextEditKind::Typing, now);
        history.selection_changed(after.clone());
        history.record(
            after.clone(),
            after,
            1..1,
            "",
            TextEditKind::Composition,
            now,
        );
        insert(
            &mut history,
            "A",
            1,
            "B",
            TextEditKind::Typing,
            now + Duration::from_millis(10),
        );
        assert_eq!(history.undo_depth(), 1);
    }

    #[test]
    fn navigation_preserves_redo_and_redo_restores_full_current_selection() {
        let now = Instant::now();
        let mut history = EditorTextHistory::default();
        let before = snapshot("abcd", 1..3, true);
        history.reset(before.clone());
        let after = snapshot("a😀d", 1..5, true);
        history.record(
            before.clone(),
            after.clone(),
            1..3,
            "😀",
            TextEditKind::Typing,
            now,
        );
        let restored = history.undo(after.clone()).unwrap();
        history.selection_changed(caret("abcd", 0));
        assert_eq!(history.redo_depth(), 1);
        assert_eq!(history.redo(caret("abcd", 0)), Some(after));
        assert_eq!(
            history.undo(snapshot("a😀d", 1..5, true)),
            Some(caret("abcd", 0))
        );
        assert_eq!(restored, before);
    }

    #[test]
    fn new_edit_after_undo_invalidates_redo_without_rejoining_the_old_group() {
        let now = Instant::now();
        let mut history = EditorTextHistory::default();
        history.reset(caret("", 0));
        let after = insert(&mut history, "", 0, "A", TextEditKind::Typing, now);
        history.undo(after).unwrap();
        insert(&mut history, "", 0, "B", TextEditKind::Typing, now);
        assert_eq!(history.undo_depth(), 1);
        assert_eq!(history.redo_depth(), 0);
    }

    #[test]
    fn reset_clears_both_branches_even_for_identical_document_text() {
        let now = Instant::now();
        let mut history = EditorTextHistory::default();
        history.reset(caret("", 0));
        let after = insert(&mut history, "", 0, "A", TextEditKind::Typing, now);
        history.undo(after).unwrap();
        history.reset(caret("", 0));
        assert_eq!(history.undo_depth(), 0);
        assert_eq!(history.redo_depth(), 0);
        assert_eq!(history.undo(caret("", 0)), None);
    }

    #[test]
    fn mismatched_baseline_or_untracked_replacement_cannot_undo_an_old_document() {
        let now = Instant::now();
        let mut history = EditorTextHistory::default();
        history.reset(caret("old", 3));
        insert(&mut history, "old", 3, "!", TextEditKind::Typing, now);
        let after = insert(&mut history, "new", 3, "?", TextEditKind::Typing, now);
        assert_eq!(history.undo_depth(), 1);
        assert_eq!(history.undo(after), Some(caret("new", 3)));
        assert_eq!(history.undo(caret("other", 5)), None);
        assert_eq!(history.redo_depth(), 0);
        assert_eq!(history.current(), Some(&caret("other", 5)));
    }

    #[test]
    fn invalid_utf8_edit_range_or_mismatched_payload_resets_safely() {
        let now = Instant::now();
        for (range, inserted) in [(2..3, "X"), (0..99, "X"), (3..1, "X"), (1..5, "wrong")] {
            let mut history = EditorTextHistory::default();
            let before = caret("A😀B", 1);
            let after = caret("AXB", 2);
            history.reset(before.clone());
            history.record(
                before,
                after.clone(),
                range,
                inserted,
                TextEditKind::Typing,
                now,
            );
            assert_eq!(history.undo_depth(), 0);
            assert_eq!(history.undo(after.clone()), None);
            assert_eq!(history.current(), Some(&after));
        }
    }

    #[test]
    fn snapshot_positions_are_bounded_to_unicode_character_boundaries() {
        let mut history = EditorTextHistory::default();
        history.reset(snapshot("A😀中", 3..usize::MAX, true));
        assert_eq!(history.current(), Some(&snapshot("A😀中", 1..8, true)));
        history.reset(snapshot("😀", 99..99, true));
        assert_eq!(history.current(), Some(&caret("😀", 4)));
    }

    #[test]
    fn branch_depth_matches_codemirror_lazy_min_depth_and_stays_bounded() {
        let now = Instant::now();
        let mut history = EditorTextHistory::default();
        history.reset(caret("0", 1));
        for index in 1..=121 {
            let before = caret(&(index - 1).to_string(), (index - 1).to_string().len());
            let after = caret(&index.to_string(), index.to_string().len());
            let range = 0..before.text.len();
            let inserted = after.text.clone();
            history.record(before, after, range, &inserted, TextEditKind::External, now);
            assert!(history.undo_depth() <= MIN_DEPTH + TRIM_SLACK);
        }
        assert_eq!(history.undo_depth(), 102);
        let mut current = caret("121", 3);
        for _ in 0..102 {
            current = history.undo(current).unwrap();
        }
        assert_eq!(current.text, "19");
        assert_eq!(history.undo(current.clone()), None);
        assert_eq!(history.redo_depth(), 102);
        for _ in 0..102 {
            current = history.redo(current).unwrap();
        }
        assert_eq!(current, caret("121", 3));
    }
}
