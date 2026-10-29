//! 2026-10-28 top-edge file line: an entry beside the Super menu's file
//! category. It keeps its own menu session, so dropping the line never
//! publishes, replaces or closes the Super menu, and the Super menu never
//! closes the line. Nodes and invocation reuse the file menu unchanged.

use serde_json::Value;

use super::HostState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileLineChange {
    Added,
    AlreadyPresent,
}

/// A single removed entry, not a copy of the whole configuration. Restoring it
/// must not revert changes made through Settings or the other file menu.
#[derive(Debug, Clone)]
pub struct FileLineRemoval {
    pub path: String,
    index: usize,
    previous: Option<(String, usize)>,
    next: Option<(String, usize)>,
}

fn path_text(path: &str) -> &str {
    path.trim().trim_matches('"').trim()
}

fn same_path(left: &str, right: &str) -> bool {
    path_text(left).trim_end_matches(['\\', '/'])
        .eq_ignore_ascii_case(path_text(right).trim_end_matches(['\\', '/']))
}

fn path_entry(line: &str) -> bool {
    let line = line.trim();
    !line.is_empty() && !line.starts_with('#') && !crate::file_menu::is_separator_line(line)
}

fn position(lines: &[String], path: &str) -> Option<usize> {
    lines.iter().position(|line| path_entry(line) && same_path(line, path))
}

fn directory_path(path: &str) -> Result<&str, String> {
    let path = path_text(path);
    if path.is_empty() || path.contains(['\0', '\r', '\n'])
        || !(crate::explorer::is_shell_path(path) || std::path::Path::new(path).is_dir())
    {
        return Err(crate::i18n::tr("绳上只挂文件夹，请选择可访问的文件夹", "Only accessible folders can be added to the line").into());
    }
    Ok(path)
}

impl HostState {
    /// Drag the file menu's original paths as COPY data without reading or
    /// replacing the clipboard. GPUI supplies the source-window UI runner.
    pub fn file_line_drag_files(&self, files: Vec<String>) -> Result<Value, String> {
        self.require_external_effects("start external OLE drag")?;
        if files.is_empty() {
            return Ok(serde_json::json!({ "ok": true, "started": false }));
        }
        {
            let mut inner = self.inner.lock();
            if inner.ole_drag_active {
                return Err("A clipboard drag is already in progress.".into());
            }
            inner.super_menu.native_keyboard.retire_binding();
            inner.native_fifo_keyboard.retire_binding();
            inner.native_context_menu_keyboard.retire();
            inner.native_file_line_keyboard.retire();
            inner.ole_drag_active = true;
        }
        struct DragSession<'a>(&'a HostState);
        impl Drop for DragSession<'_> {
            fn drop(&mut self) { self.0.inner.lock().ole_drag_active = false; }
        }
        let _drag = DragSession(self);
        let count = files.len();
        let payload = crate::ole_drag::OleDragPayload { files, ..Default::default() };
        let runner = self.ole_drag_ui.lock().clone();
        let used_runner = runner.is_some();
        if let Some(run) = runner {
            run(payload)?;
        } else if !crate::clipboard::is_headless() {
            crate::ole_drag::begin_ole_drag(&payload)?;
        }
        Ok(serde_json::json!({
            "ok": true,
            "started": used_runner || !crate::clipboard::is_headless(),
            "count": count,
        }))
    }

    /// Hang a directory in the existing file-menu list. Dragging never moves
    /// files on disk. Checking the directory stays outside the host lock.
    pub fn file_line_hang(&self, path: &str, before_path: Option<&str>) -> Result<FileLineChange, String> {
        let path = directory_path(path)?;
        self.file_menu_update(|paths| {
            if position(&paths.lines, path).is_some() {
                return FileLineChange::AlreadyPresent;
            }
            let index = before_path.and_then(|before| position(&paths.lines, before)).unwrap_or(paths.lines.len());
            paths.lines.insert(index, path.to_string());
            FileLineChange::Added
        }).map(|(change, _)| change)
    }

    /// An Explorer drop may contain several folders. Validate the whole drop
    /// first, then commit it once so a late invalid path or failed save cannot
    /// leave a partially added batch.
    pub fn file_line_hang_many(&self, folders: &[String], before_path: Option<&str>) -> Result<usize, String> {
        let folders: Vec<_> = folders.iter().map(|path| directory_path(path)).collect::<Result<_, _>>()?;
        if folders.is_empty() { return Ok(0); }
        self.file_menu_update(|paths| {
            let mut index = before_path.and_then(|before| position(&paths.lines, before)).unwrap_or(paths.lines.len());
            let mut added = 0;
            for path in folders {
                if position(&paths.lines, path).is_none() {
                    paths.lines.insert(index, path.to_string());
                    index += 1;
                    added += 1;
                }
            }
            added
        }).map(|(added, _)| added)
    }

    /// Reorder a saved entry without rereading a possibly offline directory.
    /// A stale destination is ignored instead of moving an unrelated entry.
    pub fn file_line_reorder(&self, path: &str, before_path: Option<&str>) -> Result<bool, String> {
        self.file_menu_update(|paths| {
            let Some(from) = position(&paths.lines, path) else { return false };
            let destination = match before_path {
                Some(before) => match position(&paths.lines, before) {
                    Some(index) => index,
                    None => return false,
                },
                None => paths.lines.len(),
            };
            let to = destination.saturating_sub(usize::from(from < destination));
            if to == from { return false; }
            let saved = paths.lines.remove(from);
            paths.lines.insert(to, saved);
            true
        }).map(|(changed, _)| changed)
    }

    /// Remove only a configured shortcut, including one whose folder is gone.
    /// The token is returned only after its removal has been committed to disk.
    pub fn file_line_take_down(&self, path: &str) -> Result<Option<FileLineRemoval>, String> {
        self.file_menu_update(|paths| {
            let index = position(&paths.lines, path)?;
            let previous = paths.lines[..index].iter().enumerate().rev()
                .find(|(_, line)| path_entry(line))
                .map(|(at, line)| (line.clone(), index - at));
            let next = paths.lines.iter().enumerate().skip(index + 1)
                .find(|(_, line)| path_entry(line))
                .map(|(at, line)| (line.clone(), at - index));
            let path = paths.lines.remove(index);
            Some(FileLineRemoval { path, index, previous, next })
        }).map(|(removed, _)| removed)
    }

    /// Restore one shortcut beside its surviving neighbors. Separators and
    /// comments are retained; other concurrent edits are never rolled back.
    /// The view owns the proposal's six-second lifetime of this token.
    pub fn file_line_restore(&self, removed: &FileLineRemoval) -> Result<bool, String> {
        self.file_menu_update(|paths| {
            if position(&paths.lines, &removed.path).is_some() { return false; }
            let next = removed.next.as_ref().and_then(|(path, distance)| {
                position(&paths.lines, path).map(|index| index.saturating_sub(distance - 1))
            });
            let previous = removed.previous.as_ref().and_then(|(path, distance)| {
                position(&paths.lines, path).map(|index| index.saturating_add(*distance))
            });
            let index = next.or(previous).unwrap_or(removed.index).min(paths.lines.len());
            paths.lines.insert(index, removed.path.clone());
            true
        }).map(|(restored, _)| restored)
    }

    /// Off unless the user turns it on (Settings → Super menu → File menu).
    pub fn file_line_enabled(&self) -> bool {
        self.inner.lock().extras.file_menu.top_edge_line
    }

    /// The window a click on the line acts on: the foreground window when the
    /// line drops, or the last outside window if one of ours is in front.
    pub fn file_line_target(&self) -> i64 {
        let foreground = (!crate::clipboard::is_headless())
            .then(crate::clipboard::resolved_foreground_root_hwnd).flatten();
        let inner = self.inner.lock();
        foreground
            .filter(|&hwnd| hwnd != 0
                && !crate::input_target::belongs_to_host(hwnd)
                && !crate::host_hit_test::host_related_hwnd(inner.host_hwnd, hwnd))
            .unwrap_or(inner.last_non_host_fg) as i64
    }

    /// A detached file-menu session aimed at `target`. The line does not
    /// offer "add the clipboard path", so the clipboard is not read here.
    /// Close it with `menu_close_session`.
    pub fn file_line_open_session(&self, target: i64) -> Value {
        let mut ctx = self.menu_build_context_with("file", "", |ctx| {
            ctx.opened = crate::explorer::list_opened_folders();
        });
        ctx.target_window_handle = target;
        ctx.target = crate::window_menu::query_target(target);
        let style = crate::menu_api::style_from_config(&ctx.menu_cfg);
        self.menus.create_from_context("file", style, &ctx)
    }
}

#[cfg(test)]
mod tests {
    use crate::{host::HostState, paths::AppPaths};
    use super::FileLineChange;

    fn fixture() -> (tempfile::TempDir, HostState) {
        crate::clipboard::force_headless(true);
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp");
        std::fs::create_dir_all(&root).unwrap();
        let dir = tempfile::Builder::new().prefix("file-line-host-").tempdir_in(root).unwrap();
        let host = HostState::open(AppPaths::from_data_root(dir.path().join("data")), 29065).unwrap();
        (dir, host)
    }

    #[test]
    fn file_line_drag_preserves_paths_and_excludes_concurrent_drag() {
        let (_dir, host) = fixture();
        let files = vec![r"C:\fixture\报告.txt".to_string(), r"\\fixture\share\second.txt".to_string()];
        let expected = files.clone();
        let observer = host.clone();
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let called = calls.clone();
        let dragged = host.with_ole_drag_ui_runner(move |payload| {
            called.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            assert_eq!(payload.files, expected);
            assert!(payload.text.is_empty() && payload.html.is_none() && payload.dib.is_none());
            assert!(payload.internal_drag_token.is_none());
            assert!(observer.inner.lock().ole_drag_active);
            assert!(observer.file_line_drag_files(vec!["another.txt".into()]).unwrap_err().contains("already in progress"));
            Ok(())
        });
        assert_eq!(dragged.file_line_drag_files(Vec::new()).unwrap()["started"], false);
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 0);
        let result = dragged.file_line_drag_files(files).unwrap();
        assert_eq!(result["started"], true);
        assert_eq!(result["count"], 2);
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert!(!host.inner.lock().ole_drag_active);
    }

    #[test]
    fn file_line_drag_failure_releases_guard_but_does_not_revive_keyboard() {
        let (_dir, host) = fixture();
        host.set_window_runtime_ready(true);
        {
            let mut inner = host.inner.lock();
            inner.extras.file_menu.top_edge_line = true;
            inner.last_fg = 200;
            inner.ui.keyboard_focus_host = None;
        }
        assert!(host.register_native_file_line_keyboard(101, 1, true, &[(0, 0, 800, 74)]));
        let dragged = host.with_ole_drag_ui_runner(|_| Err("fixture OLE failure".into()));
        for _ in 0..2 {
            assert_eq!(dragged.file_line_drag_files(vec!["fixture.txt".into()]).unwrap_err(), "fixture OLE failure");
            assert!(!host.inner.lock().ole_drag_active);
        }
        assert!(!host.register_native_file_line_keyboard(101, 1, true, &[(0, 0, 800, 74)]));
    }

    #[test]
    fn file_line_drag_qa_gate_precedes_runner_and_input_changes() {
        let (_dir, mut host) = fixture();
        host.keyboard_input_qa = true;
        host.set_ole_drag_ui_runner(|_| panic!("QA drag must not reach the runner"));
        assert!(host.file_line_drag_files(vec!["fixture.txt".into()]).unwrap_err().starts_with("KEYBOARD_QA_SIDE_EFFECT:"));
        assert!(!host.inner.lock().ole_drag_active);
    }

    #[test]
    fn line_session_leaves_the_super_menu_untouched() {
        let (_dir, host) = fixture();
        let before = host.super_menu_state_detail();
        let session = host.file_line_open_session(0);
        let id = session["sessionId"].as_str().expect("line session id").to_string();
        assert_eq!(host.super_menu_state_detail(), before, "the line must not publish a Super menu");
        let root = host.menu_nodes_ex(&id, "", "children", None).expect("line root nodes");
        assert!(root["items"].as_array().is_some_and(|items| !items.is_empty()));
        assert!(host.menu_close_session(&id));
        assert_eq!(host.super_menu_state_detail(), before);
    }

    #[test]
    fn line_stays_off_until_the_user_turns_it_on() {
        let (_dir, host) = fixture();
        assert!(!host.file_line_enabled());
        host.inner.lock().extras.file_menu.top_edge_line = true;
        assert!(host.file_line_enabled());
    }

    #[test]
    fn hanging_accepts_only_directories_and_keeps_existing_lines_on_disk() {
        let (dir, host) = fixture();
        let folder = dir.path().join("Folder");
        std::fs::create_dir(&folder).unwrap();
        let folder = folder.to_string_lossy().into_owned();
        let file = dir.path().join("file.txt");
        std::fs::write(&file, "unchanged").unwrap();
        host.file_menu_replace(vec!["old".into(), "---".into(), "# retained".into(), "tail".into()]).unwrap();
        let before = host.file_menu_mutation_body();
        for invalid in [file.to_string_lossy().into_owned(), dir.path().join("missing").to_string_lossy().into_owned(), "".into()] {
            assert!(host.file_line_hang(&invalid, None).is_err());
            assert_eq!(host.file_menu_mutation_body(), before);
        }
        assert_eq!(host.file_line_hang(&format!("\"{folder}\""), Some("TAIL")).unwrap(), FileLineChange::Added);
        assert_eq!(host.file_line_hang(&format!("{}\\", folder.to_uppercase()), None).unwrap(), FileLineChange::AlreadyPresent);
        let expected = serde_json::json!(["old", "---", "# retained", folder, "tail"]);
        assert_eq!(host.file_menu_mutation_body()["lines"], expected);
        assert_eq!(std::fs::read_to_string(file).unwrap(), "unchanged");
        drop(host);
        let reopened = HostState::open(AppPaths::from_data_root(dir.path().join("data")), 29065).unwrap();
        assert_eq!(reopened.file_menu_mutation_body()["lines"], expected);
    }

    #[test]
    fn reordering_offline_shortcuts_preserves_knots_and_ignores_stale_targets() {
        let (_dir, host) = fixture();
        host.file_menu_replace(vec!["a".into(), "---".into(), "b".into(), "# keep".into(), "c".into()]).unwrap();
        assert!(host.file_line_reorder("C", Some("A")).unwrap());
        assert_eq!(host.file_menu_mutation_body()["lines"], serde_json::json!(["c", "a", "---", "b", "# keep"]));
        assert!(!host.file_line_reorder("c", Some("C")).unwrap());
        assert!(!host.file_line_reorder("c", Some("removed elsewhere")).unwrap());
        assert!(!host.file_line_reorder("missing", None).unwrap());
        assert!(host.file_line_reorder("c", None).unwrap());
        assert_eq!(host.file_menu_mutation_body()["lines"], serde_json::json!(["a", "---", "b", "# keep", "c"]));
    }

    #[test]
    fn dropped_batch_is_validated_together_and_keeps_order_without_duplicates() {
        let (dir, host) = fixture();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        let first = first.to_string_lossy().into_owned();
        let second = second.to_string_lossy().into_owned();
        host.file_menu_replace(vec!["a".into(), "---".into(), "b".into()]).unwrap();
        let before = host.file_menu_mutation_body();
        let missing = dir.path().join("missing").to_string_lossy().into_owned();
        assert!(host.file_line_hang_many(&[first.clone(), missing], Some("b")).is_err());
        assert_eq!(host.file_menu_mutation_body(), before);
        assert_eq!(host.file_line_hang_many(&[first.clone(), second.clone(), first.clone()], Some("b")).unwrap(), 2);
        assert_eq!(host.file_menu_mutation_body()["lines"], serde_json::json!(["a", "---", first, second, "b"]));
        assert_eq!(host.file_line_hang_many(&[first, second], None).unwrap(), 0);
    }

    #[test]
    fn undo_restores_one_shortcut_and_preserves_concurrent_changes() {
        let (_dir, host) = fixture();
        host.file_menu_replace(vec!["a".into(), "---".into(), "b".into(), "---".into(), "c".into()]).unwrap();
        let removed = host.file_line_take_down("B").unwrap().unwrap();
        assert_eq!(removed.path, "b");
        assert_eq!(host.file_menu_mutation_body()["lines"], serde_json::json!(["a", "---", "---", "c"]));
        host.file_menu_replace(vec!["new".into(), "a".into(), "---".into(), "---".into(), "c".into(), "tail".into()]).unwrap();
        assert!(host.file_line_restore(&removed).unwrap());
        assert_eq!(host.file_menu_mutation_body()["lines"], serde_json::json!(["new", "a", "---", "b", "---", "c", "tail"]));
        assert!(!host.file_line_restore(&removed).unwrap(), "a repeated undo cannot duplicate the shortcut");
        assert!(host.file_line_take_down("not configured").unwrap().is_none());
    }

    #[test]
    fn undo_preserves_leading_and_trailing_knots_and_survives_removed_neighbors() {
        let (_dir, host) = fixture();
        for entry in ["a", "b"] {
            let expected = vec!["---".to_string(), "a".into(), "---".into(), "b".into(), "---".into()];
            host.file_menu_replace(expected.clone()).unwrap();
            let removed = host.file_line_take_down(entry).unwrap().unwrap();
            assert!(host.file_line_restore(&removed).unwrap());
            assert_eq!(host.file_menu_mutation_body()["lines"], serde_json::json!(expected));
        }
        let removed = host.file_line_take_down("a").unwrap().unwrap();
        host.file_menu_replace(vec!["new-only".into()]).unwrap();
        assert!(host.file_line_restore(&removed).unwrap());
        assert_eq!(host.file_menu_mutation_body()["lines"], serde_json::json!(["new-only", "a"]));
    }

    #[test]
    fn failed_line_mutations_leave_memory_and_disk_unchanged_and_can_retry() {
        for operation in ["hang", "hang_many", "reorder", "take_down", "restore"] {
            let (dir, host) = fixture();
            let folder = dir.path().to_string_lossy().into_owned();
            let second_folder = dir.path().join("second");
            std::fs::create_dir(&second_folder).unwrap();
            let second_folder = second_folder.to_string_lossy().into_owned();
            host.file_menu_replace(vec!["a".into(), "---".into(), "b".into(), "c".into()]).unwrap();
            let removed = host.file_line_take_down("c").unwrap().unwrap();
            let mutate = || match operation {
                "hang" => host.file_line_hang(&folder, Some("b")).map(|_| ()),
                "hang_many" => host.file_line_hang_many(&[folder.clone(), second_folder.clone()], Some("b")).map(|_| ()),
                "reorder" => host.file_line_reorder("b", Some("a")).map(|_| ()),
                "take_down" => host.file_line_take_down("b").map(|_| ()),
                "restore" => host.file_line_restore(&removed).map(|_| ()),
                _ => unreachable!(),
            };
            let before = host.file_menu_mutation_body();
            let config = AppPaths::from_data_root(dir.path().join("data")).configs.join("FileMenuConfig.json");
            let backup = config.with_extension("saved.json");
            let before_bytes = std::fs::read(&config).unwrap();
            std::fs::rename(&config, &backup).unwrap();
            std::fs::create_dir(&config).unwrap();
            assert!(mutate().is_err(), "{operation} must report failure");
            assert_eq!(host.file_menu_mutation_body(), before, "{operation} changed memory before saving");
            assert_eq!(std::fs::read(&backup).unwrap(), before_bytes);
            std::fs::remove_dir(&config).unwrap();
            std::fs::rename(&backup, &config).unwrap();
            mutate().unwrap();
            assert_ne!(host.file_menu_mutation_body(), before, "{operation} must be retryable");
        }
    }
}
