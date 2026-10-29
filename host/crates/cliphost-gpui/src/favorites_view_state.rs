//! Bounded, local favorites presentation memory. No favorite DTO or business
//! database is changed here. Load/flush run on a worker; remember is memory-only.

use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, MutexGuard,
    },
    time::Duration,
};

use serde::{Deserialize, Serialize};

pub(crate) type OccurrencePath = Vec<String>;

const FORMAT_VERSION: u32 = 1;
const FILE_NAME: &str = "clip-host-favorites-view-state.json";
const MAX_FILE_BYTES: usize = 1024 * 1024;
const MAX_CONTEXTS: usize = 32;
const MAX_EXPANDED: usize = 256;
const MAX_SIDEBAR_EXPANDED: usize = 512;
const MAX_PATH_DEPTH: usize = 64;
const MAX_ID_BYTES: usize = 512;
const MAX_PATH_BYTES: usize = 8192;
// Worst-case JSON escaping uses six bytes per source byte. Accounting for that
// here keeps the complete state comfortably below the file cap, even for IDs
// containing quotes or control characters. IDs themselves are never truncated.
const MAX_PATH_JSON_BYTES: usize = 256 * 1024;
const REPLACE_RETRIES: usize = 8;
const REPLACE_RETRY_DELAY: Duration = Duration::from_millis(50);
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum LayoutMode {
    #[default]
    #[serde(alias = "sidebarList", alias = "SidebarList")]
    SidebarList,
    #[serde(alias = "Tree")]
    Tree,
    #[serde(alias = "sidebarTree", alias = "SidebarTree")]
    SidebarTree,
    /// Folders browsed in place without the sidebar (DESIGN 2026-10-14).
    #[serde(alias = "List")]
    List,
}

impl LayoutMode {
    #[cfg(test)]
    pub(crate) const ALL: [Self; 4] = [Self::SidebarList, Self::Tree, Self::SidebarTree, Self::List];

    pub(crate) fn has_sidebar(self) -> bool {
        matches!(self, Self::SidebarList | Self::SidebarTree)
    }

    pub(crate) fn is_tree(self) -> bool {
        matches!(self, Self::Tree | Self::SidebarTree)
    }

    /// Footer switches: the sidebar and list/tree toggle independently.
    pub(crate) fn with_sidebar(self, sidebar: bool) -> Self {
        match (sidebar, self.is_tree()) {
            (true, true) => Self::SidebarTree,
            (true, false) => Self::SidebarList,
            (false, true) => Self::Tree,
            (false, false) => Self::List,
        }
    }

    pub(crate) fn with_tree(self, tree: bool) -> Self {
        match (self.has_sidebar(), tree) {
            (true, true) => Self::SidebarTree,
            (true, false) => Self::SidebarList,
            (false, true) => Self::Tree,
            (false, false) => Self::List,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct ScrollAnchor {
    pub(crate) path: OccurrencePath,
    pub(crate) offset: f32,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct ContextState {
    pub(crate) layout: LayoutMode,
    /// Empty is the all-favorites scope; otherwise this is a complete path from
    /// a root, including the selected folder itself, rather than only its ID.
    pub(crate) folder: OccurrencePath,
    pub(crate) expanded: BTreeSet<OccurrencePath>,
    pub(crate) selected: Option<OccurrencePath>,
    pub(crate) scroll_anchor: Option<ScrollAnchor>,
}

/// Favorites list display (DESIGN 2026-10-14), saved with the local view
/// state and independent of the clipboard list appearance.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct FavoriteDisplay {
    /// 详细: a second content line (body under a title, file folder, size).
    pub(crate) detailed: bool,
    pub(crate) show_time: bool,
    pub(crate) show_count: bool,
    /// `HoverAction::as_str` values; primary copies or pastes per execute mode.
    pub(crate) hover: Vec<String>,
}

impl Default for FavoriteDisplay {
    fn default() -> Self {
        Self {
            detailed: false,
            show_time: false,
            show_count: true,
            hover: vec!["primary".into(), "edit".into(), "delete".into()],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct FavoritesViewState {
    pub(crate) version: u32,
    pub(crate) layout: LayoutMode,
    pub(crate) active_folder: OccurrencePath,
    pub(crate) sidebar_expanded: BTreeSet<OccurrencePath>,
    /// Least recently used first. No search text or content text belongs here.
    pub(crate) contexts: Vec<ContextState>,
    pub(crate) display: FavoriteDisplay,
}

impl Default for FavoritesViewState {
    fn default() -> Self {
        Self {
            version: FORMAT_VERSION,
            layout: LayoutMode::default(),
            active_folder: Vec::new(),
            sidebar_expanded: BTreeSet::new(),
            contexts: Vec::new(),
            display: FavoriteDisplay::default(),
        }
    }
}

impl FavoritesViewState {
    pub(crate) fn context(&self, layout: LayoutMode, folder: &[String]) -> Option<&ContextState> {
        self.contexts
            .iter()
            .rev()
            .find(|context| context.layout == layout && context.folder == folder)
    }

    /// Access a context when entering/leaving a scope, not on every render.
    pub(crate) fn context_mut(
        &mut self,
        layout: LayoutMode,
        folder: &[String],
    ) -> &mut ContextState {
        let context = self
            .contexts
            .iter()
            .position(|context| context.layout == layout && context.folder == folder)
            .map(|index| self.contexts.remove(index))
            .unwrap_or_else(|| ContextState {
                layout,
                folder: folder.to_vec(),
                ..ContextState::default()
            });
        self.contexts.push(context);
        if self.contexts.len() > MAX_CONTEXTS {
            self.contexts.drain(..self.contexts.len() - MAX_CONTEXTS);
        }
        self.contexts
            .last_mut()
            .expect("a context was just inserted")
    }

    /// Call after a successful complete graph refresh, never after a failed or
    /// partial fetch. A moved/deleted occurrence cannot transfer its UI state to
    /// another parent merely because its final item ID is still the same.
    pub(crate) fn prune(&mut self, valid_paths: &BTreeSet<OccurrencePath>) {
        self.active_folder = nearest_valid(&self.active_folder, valid_paths);
        self.sidebar_expanded
            .retain(|path| valid_paths.contains(path));
        self.contexts
            .retain(|context| context.folder.is_empty() || valid_paths.contains(&context.folder));
        for context in &mut self.contexts {
            context.expanded.retain(|path| valid_paths.contains(path));
            if let Some(path) = &mut context.selected {
                *path = nearest_valid(path, valid_paths);
                if path.is_empty() {
                    context.selected = None;
                }
            }
            if let Some(anchor) = &mut context.scroll_anchor {
                anchor.path = nearest_valid(&anchor.path, valid_paths);
                if anchor.path.is_empty() {
                    context.scroll_anchor = None;
                }
            }
        }
        self.limit();
    }

    fn limit(&mut self) {
        self.version = FORMAT_VERSION;
        let mut budget = MAX_PATH_JSON_BYTES;
        if !take_path(&self.active_folder, true, &mut budget) {
            self.active_folder.clear();
        }
        // Traverse newest first so stale directories cannot evict the current
        // context. Duplicate contexts in old/partial JSON keep their last entry.
        let mut contexts: Vec<ContextState> = Vec::new();
        for mut context in std::mem::take(&mut self.contexts).into_iter().rev() {
            if contexts.len() == MAX_CONTEXTS {
                break;
            }
            if contexts
                .iter()
                .any(|entry| entry.layout == context.layout && entry.folder == context.folder)
                || !take_path(&context.folder, true, &mut budget)
            {
                continue;
            }
            if context
                .selected
                .as_ref()
                .is_some_and(|path| !take_path(path, false, &mut budget))
            {
                context.selected = None;
            }
            if let Some(anchor) = &mut context.scroll_anchor {
                if !take_path(&anchor.path, false, &mut budget) {
                    context.scroll_anchor = None;
                } else {
                    anchor.offset = if anchor.offset.is_finite() {
                        anchor.offset.clamp(-16_384.0, 16_384.0)
                    } else {
                        0.0
                    };
                }
            }
            limit_paths(&mut context.expanded, MAX_EXPANDED, &mut budget);
            contexts.push(context);
        }
        contexts.reverse();
        self.contexts = contexts;
        limit_paths(
            &mut self.sidebar_expanded,
            MAX_SIDEBAR_EXPANDED,
            &mut budget,
        );
    }
}

fn nearest_valid(path: &[String], valid_paths: &BTreeSet<OccurrencePath>) -> OccurrencePath {
    let mut path = path.to_vec();
    while !path.is_empty() && !valid_paths.contains(&path) {
        path.pop();
    }
    path
}

fn take_path(path: &[String], allow_empty: bool, budget: &mut usize) -> bool {
    if (!allow_empty && path.is_empty())
        || path.len() > MAX_PATH_DEPTH
        || path
            .iter()
            .any(|id| id.is_empty() || id.len() > MAX_ID_BYTES)
        || path.iter().map(String::len).sum::<usize>() > MAX_PATH_BYTES
    {
        return false;
    }
    let cost = 2 + path.iter().map(|id| id.len() * 6 + 3).sum::<usize>();
    if cost > *budget {
        return false;
    }
    *budget -= cost;
    true
}

fn limit_paths(paths: &mut BTreeSet<OccurrencePath>, maximum: usize, budget: &mut usize) {
    let mut count = 0;
    paths.retain(|path| {
        if count < maximum && take_path(path, false, budget) {
            count += 1;
            true
        } else {
            false
        }
    });
}

#[derive(Clone)]
pub(crate) struct FavoritesViewStore {
    inner: Arc<StoreInner>,
}

struct StoreInner {
    path: Option<PathBuf>,
    pending: Mutex<Pending>,
    writing: Mutex<()>,
}

struct Pending {
    state: FavoritesViewState,
    revision: u64,
    persisted_revision: u64,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl FavoritesViewStore {
    pub(crate) fn file_path(data_root: &Path) -> PathBuf {
        data_root.join(FILE_NAME)
    }

    /// Only NotFound starts with defaults. The caller must report other errors;
    /// it may explicitly use in_memory() to keep the page usable without ever
    /// overwriting an unreadable, damaged or unsupported state file.
    pub(crate) fn load(data_root: &Path) -> Result<Self, String> {
        let path = Self::file_path(data_root);
        let mut state = match File::open(&path) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take(MAX_FILE_BYTES as u64 + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|error| {
                        format!("读取收藏界面状态 {} 失败：{error}", path.display())
                    })?;
                if bytes.len() > MAX_FILE_BYTES {
                    return Err(format!("收藏界面状态 {} 超过大小限制", path.display()));
                }
                let state: FavoritesViewState =
                    serde_json::from_slice(&bytes).map_err(|error| {
                        format!("读取收藏界面状态 {} 失败：{error}", path.display())
                    })?;
                if state.version > FORMAT_VERSION {
                    return Err(format!(
                        "收藏界面状态 {} 的版本 {} 暂不支持",
                        path.display(),
                        state.version
                    ));
                }
                state
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                FavoritesViewState::default()
            }
            Err(error) => return Err(format!("读取收藏界面状态 {} 失败：{error}", path.display())),
        };
        state.limit();
        Ok(Self::new(Some(path), state))
    }

    pub(crate) fn in_memory() -> Self {
        Self::new(None, FavoritesViewState::default())
    }

    fn new(path: Option<PathBuf>, state: FavoritesViewState) -> Self {
        Self {
            inner: Arc::new(StoreInner {
                path,
                pending: Mutex::new(Pending {
                    state,
                    revision: 0,
                    persisted_revision: 0,
                }),
                writing: Mutex::new(()),
            }),
        }
    }

    pub(crate) fn snapshot(&self) -> FavoritesViewState {
        lock(&self.inner.pending).state.clone()
    }

    /// Call synchronously in the accepted UI event, then schedule flush on a
    /// worker. Do not queue independent background tasks carrying old snapshots.
    pub(crate) fn remember(&self, mut state: FavoritesViewState) -> u64 {
        state.limit();
        let mut pending = lock(&self.inner.pending);
        if pending.state != state {
            pending.revision = pending
                .revision
                .checked_add(1)
                .expect("favorites UI revision exhausted");
            pending.state = state;
        }
        pending.revision
    }

    pub(crate) fn has_pending(&self) -> bool {
        if self.inner.path.is_none() {
            return false;
        }
        let pending = lock(&self.inner.pending);
        pending.revision != pending.persisted_revision
    }

    /// Serializes workers and drains newer revisions which arrived during slow
    /// I/O. An older background flush therefore cannot finish after a newer one
    /// and replace the file with stale state. Failed writes remain pending.
    pub(crate) fn flush(&self) -> Result<bool, String> {
        self.flush_with(atomic_save)
    }

    fn flush_with(
        &self,
        mut publish: impl FnMut(&Path, &[u8]) -> Result<(), String>,
    ) -> Result<bool, String> {
        let Some(path) = &self.inner.path else {
            return Ok(false);
        };
        let _writing = lock(&self.inner.writing);
        let mut written = false;
        loop {
            let (revision, state) = {
                let pending = lock(&self.inner.pending);
                if pending.revision == pending.persisted_revision {
                    return Ok(written);
                }
                (pending.revision, pending.state.clone())
            };
            let bytes = serde_json::to_vec_pretty(&state)
                .map_err(|error| format!("保存收藏界面状态失败：{error}"))?;
            if bytes.len() > MAX_FILE_BYTES {
                return Err("收藏界面状态超过大小限制".into());
            }
            publish(path, &bytes)?;
            written = true;
            lock(&self.inner.pending).persisted_revision = revision;
        }
    }
}

struct StagedFile(PathBuf);

impl Drop for StagedFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn atomic_save(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)
        .map_err(|error| format!("保存收藏界面状态 {} 失败：{error}", path.display()))?;
    let (stage, mut file) = (0..32)
        .find_map(|_| {
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let temporary = parent.join(format!(
                ".{FILE_NAME}.{}.{sequence}.tmp",
                std::process::id()
            ));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
            {
                Ok(file) => Some(Ok((StagedFile(temporary), file))),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => None,
                Err(error) => Some(Err(error)),
            }
        })
        .unwrap_or_else(|| {
            Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "无法创建唯一的界面状态临时文件",
            ))
        })
        .map_err(|error| format!("保存收藏界面状态 {} 失败：{error}", path.display()))?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|error| format!("保存收藏界面状态 {} 失败：{error}", path.display()))?;
    // Close our staging handle before Windows replacement. The target remains
    // untouched if staging, flushing or atomic same-directory rename fails.
    drop(file);
    for attempt in 0..=REPLACE_RETRIES {
        match fs::rename(&stage.0, path) {
            Ok(()) => return Ok(()),
            Err(error) => {
                let retryable = cfg!(windows) && matches!(error.raw_os_error(), Some(5 | 32 | 33));
                if !retryable || attempt == REPLACE_RETRIES {
                    return Err(format!("保存收藏界面状态 {} 失败：{error}", path.display()));
                }
                std::thread::sleep(REPLACE_RETRY_DELAY);
            }
        }
    }
    unreachable!("the final replace attempt always returns")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(ids: &[&str]) -> OccurrencePath {
        ids.iter().map(|id| (*id).to_owned()).collect()
    }

    #[test]
    fn favorites_view_state_partial_json_defaults_and_aliases() {
        let dir = tempfile::tempdir().unwrap();
        let target = FavoritesViewStore::file_path(dir.path());
        fs::write(
            &target,
            br#"{"layout":"sidebarTree","contexts":[{"folder":["a"]}]}"#,
        )
        .unwrap();
        let state = FavoritesViewStore::load(dir.path()).unwrap().snapshot();
        assert_eq!(state.layout, LayoutMode::SidebarTree);
        for layout in LayoutMode::ALL {
            assert_eq!(layout.with_sidebar(layout.has_sidebar()), layout);
            assert_eq!(layout.with_tree(layout.is_tree()), layout);
            assert_eq!(layout.with_sidebar(!layout.has_sidebar()).is_tree(), layout.is_tree());
            assert_eq!(layout.with_tree(!layout.is_tree()).has_sidebar(), layout.has_sidebar());
        }
        assert_eq!(state.contexts[0].layout, LayoutMode::SidebarList);
        assert!(state.contexts[0].expanded.is_empty());
        assert!(state.contexts[0].selected.is_none());
        assert_eq!(state.version, FORMAT_VERSION);
        assert!(!serde_json::to_string(&state).unwrap().contains("search"));
    }

    #[test]
    fn favorites_view_state_only_missing_file_uses_writable_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let store = FavoritesViewStore::load(dir.path()).unwrap();
        assert_eq!(store.snapshot(), FavoritesViewState::default());
        let target = FavoritesViewStore::file_path(dir.path());
        let corrupt = b"{invalid json";
        fs::write(&target, corrupt).unwrap();
        assert!(FavoritesViewStore::load(dir.path()).is_err());
        let fallback = FavoritesViewStore::in_memory();
        let mut state = fallback.snapshot();
        state.layout = LayoutMode::Tree;
        fallback.remember(state);
        assert!(!fallback.flush().unwrap());
        assert!(!fallback.has_pending());
        assert_eq!(fs::read(&target).unwrap(), corrupt);
        fs::write(&target, br#"{"version":99}"#).unwrap();
        assert!(FavoritesViewStore::load(dir.path()).is_err());
        fs::remove_file(&target).unwrap();
        fs::create_dir(&target).unwrap();
        assert!(FavoritesViewStore::load(dir.path()).is_err());
    }

    #[test]
    fn favorites_view_state_contexts_isolate_layout_and_parent_occurrences() {
        let mut state = FavoritesViewState::default();
        let a = path(&["a"]);
        let b = path(&["b"]);
        let a_shared = path(&["a", "shared"]);
        let b_shared = path(&["b", "shared"]);
        state
            .context_mut(LayoutMode::Tree, &a)
            .expanded
            .insert(a_shared.clone());
        state.context_mut(LayoutMode::SidebarTree, &b).selected = Some(b_shared.clone());
        state.context_mut(LayoutMode::SidebarList, &a).scroll_anchor = Some(ScrollAnchor {
            path: a_shared.clone(),
            offset: 7.5,
        });
        assert!(state
            .context(LayoutMode::Tree, &a)
            .unwrap()
            .expanded
            .contains(&a_shared));
        assert!(!state
            .context(LayoutMode::Tree, &a)
            .unwrap()
            .expanded
            .contains(&b_shared));
        assert_eq!(
            state.context(LayoutMode::SidebarTree, &b).unwrap().selected,
            Some(b_shared)
        );
        assert_eq!(
            state
                .context(LayoutMode::SidebarList, &a)
                .unwrap()
                .scroll_anchor
                .as_ref()
                .unwrap()
                .offset,
            7.5
        );
    }

    #[test]
    fn favorites_view_state_prunes_deleted_and_moved_occurrences() {
        let mut state = FavoritesViewState::default();
        let a = path(&["a"]);
        let old = path(&["a", "shared"]);
        let moved = path(&["b", "shared"]);
        state.active_folder = path(&["a", "removed", "nested"]);
        state.sidebar_expanded.extend([a.clone(), old.clone()]);
        let context = state.context_mut(LayoutMode::Tree, &[]);
        context.expanded.extend([old.clone(), moved.clone()]);
        context.selected = Some(old.clone());
        context.scroll_anchor = Some(ScrollAnchor {
            path: path(&["unknown"]),
            offset: 3.0,
        });
        state.context_mut(LayoutMode::SidebarTree, &old);
        state.prune(
            &[a.clone(), path(&["b"]), moved.clone()]
                .into_iter()
                .collect(),
        );
        assert_eq!(state.active_folder, a);
        assert_eq!(state.sidebar_expanded.len(), 1);
        let context = state.context(LayoutMode::Tree, &[]).unwrap();
        assert_eq!(context.selected, Some(path(&["a"])));
        assert_eq!(context.expanded, [moved].into_iter().collect());
        assert!(context.scroll_anchor.is_none());
        assert!(state.context(LayoutMode::SidebarTree, &old).is_none());
    }

    #[test]
    fn favorites_view_state_bounds_memory_and_retains_recent_contexts() {
        let store = FavoritesViewStore::in_memory();
        let mut state = store.snapshot();
        for index in 0..MAX_CONTEXTS + 8 {
            state.context_mut(LayoutMode::Tree, &path(&[&index.to_string()]));
        }
        state.context_mut(LayoutMode::Tree, &path(&["8"]));
        let context = state.context_mut(LayoutMode::Tree, &path(&["current"]));
        for index in 0..MAX_EXPANDED + 20 {
            context.expanded.insert(path(&[&index.to_string()]));
        }
        context
            .expanded
            .insert(vec!["too-deep".into(); MAX_PATH_DEPTH + 1]);
        context.scroll_anchor = Some(ScrollAnchor {
            path: path(&["current", "item"]),
            offset: f32::NAN,
        });
        store.remember(state);
        let state = store.snapshot();
        assert_eq!(state.contexts.len(), MAX_CONTEXTS);
        assert!(state.context(LayoutMode::Tree, &path(&["8"])).is_some());
        assert!(state.context(LayoutMode::Tree, &path(&["0"])).is_none());
        let current = state
            .context(LayoutMode::Tree, &path(&["current"]))
            .unwrap();
        assert!(current.expanded.len() <= MAX_EXPANDED);
        assert!(current
            .expanded
            .iter()
            .all(|path| path.len() <= MAX_PATH_DEPTH));
        assert_eq!(current.scroll_anchor.as_ref().unwrap().offset, 0.0);
        assert!(serde_json::to_vec_pretty(&state).unwrap().len() < MAX_FILE_BYTES);
    }

    #[test]
    fn favorites_view_state_roundtrip_never_changes_business_files() {
        let dir = tempfile::tempdir().unwrap();
        let business = dir.path().join("userdata.db");
        let config = dir.path().join("ClipConfig.json");
        fs::write(&business, b"favorite business fixture").unwrap();
        fs::write(&config, b"{\"untouched\":true}").unwrap();
        let store = FavoritesViewStore::load(dir.path()).unwrap();
        let mut state = store.snapshot();
        state.layout = LayoutMode::SidebarTree;
        state.active_folder = path(&["a"]);
        state
            .context_mut(LayoutMode::SidebarTree, &path(&["a"]))
            .selected = Some(path(&["a", "item"]));
        store.remember(state.clone());
        assert!(store.flush().unwrap());
        assert!(!store.has_pending());
        assert!(!store.flush().unwrap());
        assert_eq!(
            FavoritesViewStore::load(dir.path()).unwrap().snapshot(),
            state
        );
        assert_eq!(fs::read(&business).unwrap(), b"favorite business fixture");
        assert_eq!(fs::read(&config).unwrap(), b"{\"untouched\":true}");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 3);
    }

    #[test]
    fn favorites_view_state_failed_publish_keeps_previous_file_and_pending_revision() {
        let dir = tempfile::tempdir().unwrap();
        let store = FavoritesViewStore::load(dir.path()).unwrap();
        let mut state = store.snapshot();
        state.layout = LayoutMode::Tree;
        store.remember(state);
        store.flush().unwrap();
        let target = FavoritesViewStore::file_path(dir.path());
        let before = fs::read(&target).unwrap();
        let mut state = store.snapshot();
        state.layout = LayoutMode::SidebarTree;
        store.remember(state);
        assert!(store
            .flush_with(|_, _| Err("fixture write denied".into()))
            .is_err());
        assert_eq!(fs::read(&target).unwrap(), before);
        assert!(store.has_pending());
        assert!(store.flush().unwrap());
        assert_eq!(
            FavoritesViewStore::load(dir.path())
                .unwrap()
                .snapshot()
                .layout,
            LayoutMode::SidebarTree
        );
    }

    #[test]
    fn favorites_view_state_concurrent_flush_drains_latest_revision_without_stale_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let store = FavoritesViewStore::load(dir.path()).unwrap();
        let mut state = store.snapshot();
        state.layout = LayoutMode::Tree;
        store.remember(state);
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (continue_tx, continue_rx) = std::sync::mpsc::channel();
        let first = store.clone();
        let worker = std::thread::spawn(move || {
            let mut count = 0;
            first
                .flush_with(|path, bytes| {
                    count += 1;
                    if count == 1 {
                        started_tx.send(()).unwrap();
                        continue_rx.recv().unwrap();
                    }
                    atomic_save(path, bytes)
                })
                .unwrap();
            count
        });
        started_rx.recv().unwrap();
        let mut latest = store.snapshot();
        latest.layout = LayoutMode::SidebarTree;
        let revision = store.remember(latest.clone());
        assert_eq!(revision, 2);
        let second = store.clone();
        let other_worker = std::thread::spawn(move || second.flush().unwrap());
        continue_tx.send(()).unwrap();
        assert_eq!(worker.join().unwrap(), 2);
        assert!(!other_worker.join().unwrap());
        assert!(!store.has_pending());
        assert_eq!(
            FavoritesViewStore::load(dir.path()).unwrap().snapshot(),
            latest
        );
    }
}
