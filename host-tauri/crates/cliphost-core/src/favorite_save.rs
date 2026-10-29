//! Save historical clipboard items into a real, ordered favorite-folder occurrence.
//! Recent destinations are local presentation state and never enter the drag revision.
use super::*;
use serde::{Deserialize, Serialize};

const STATE_FILE: &str = "clip-host-favorite-save-state.json";
const RECENT_LIMIT: usize = 3;
const MAX_PATH_DEPTH: usize = 128;
const MAX_STATE_BYTES: u64 = 64 * 1024;

#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SaveState {
    #[serde(default)]
    recent: Vec<RecentFolder>,
}

#[derive(Clone, Serialize, Deserialize)]
struct RecentFolder {
    id: String,
    path: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SaveRequest {
    ids: Vec<String>,
    parent_id: String,
    #[serde(default)]
    path: Option<Vec<String>>,
    expected_revision: String,
    operation_id: String,
}

/// Separate from folder saving so an invalid/missing folder can never silently
/// fall back to the top level. This is a native-only API, not a Bridge method.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RootSaveRequest {
    ids: Vec<String>,
    expected_revision: String,
    operation_id: String,
}

fn state_path(host: &HostState) -> PathBuf {
    host.inner.lock().paths.data_root.join(STATE_FILE)
}

fn read_state(path: &Path) -> Result<SaveState, String> {
    match std::fs::metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SaveState::default());
        }
        Err(error) => return Err(error.to_string()),
        Ok(metadata) if metadata.len() > MAX_STATE_BYTES => {
            return Err(tr(
                "最近收藏位置记录过大。",
                "Recent favorite destinations are too large.",
            )
            .into());
        }
        Ok(_) => {}
    }
    let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
    serde_json::from_slice(&bytes).map_err(|error| error.to_string())
}

fn is_folder(tree: &Value, id: &str) -> bool {
    !crate::panel::is_root_folder_id(id)
        && !crate::panel::is_orphans_folder_id(id)
        && tree["items"][id]["isFolder"].as_bool() == Some(true)
}

fn contains_id(values: &Value, id: &str) -> bool {
    values
        .as_array()
        .is_some_and(|values| values.iter().any(|value| value.as_str() == Some(id)))
}

fn project_folder(tree: &Value, recent: &RecentFolder) -> Option<Value> {
    if recent.path.is_empty()
        || recent.path.len() > MAX_PATH_DEPTH
        || recent.path.last() != Some(&recent.id)
    {
        return None;
    }
    let mut seen = HashSet::new();
    let mut labels = Vec::new();
    let mut previous: Option<&str> = None;
    for id in &recent.path {
        if !is_folder(tree, id) || !seen.insert(id) {
            return None;
        }
        let members = previous.map_or(&tree["orderedRootItems"], |parent| {
            &tree["childrenById"][parent]
        });
        if !contains_id(members, id) {
            return None;
        }
        labels.push(tree["items"][id]["title"].as_str().unwrap_or(""));
        previous = Some(id);
    }
    Some(json!({
        "id": recent.id,
        "path": recent.path,
        "title": labels.last().copied().unwrap_or(""),
        "pathLabel": labels.join(" / "),
    }))
}

fn valid_recents(tree: &Value, state: SaveState) -> Vec<RecentFolder> {
    let mut seen = HashSet::new();
    state
        .recent
        .into_iter()
        .filter(|entry| project_folder(tree, entry).is_some() && seen.insert(entry.path.clone()))
        .take(RECENT_LIMIT)
        .collect()
}

fn first_path(tree: &Value, target: &str) -> Option<Vec<String>> {
    let mut pending = Vec::new();
    for root in tree["orderedRootItems"].as_array()?.iter().rev() {
        if let Some(id) = root.as_str().filter(|id| is_folder(tree, id)) {
            pending.push(vec![id.to_string()]);
        }
    }
    let mut visited = HashSet::new();
    while let Some(path) = pending.pop() {
        let id = path.last()?;
        if !visited.insert(id.clone()) {
            continue;
        }
        if id == target {
            return Some(path);
        }
        if path.len() >= MAX_PATH_DEPTH {
            continue;
        }
        if let Some(children) = tree["childrenById"][id].as_array() {
            for child in children.iter().rev().filter_map(Value::as_str) {
                if is_folder(tree, child) && !path.iter().any(|ancestor| ancestor == child) {
                    let mut next = path.clone();
                    next.push(child.into());
                    pending.push(next);
                }
            }
        }
    }
    None
}

impl HostState {
    /// The explicit star click saves a whole selection into the top level.
    /// Reuses the structural transaction/receipt/undo without recording a recent
    /// folder, since the top level is a synthetic location rather than a folder.
    pub fn favorite_save_history_to_root(&self, params: &Value) -> Result<Value, String> {
        let request: RootSaveRequest = serde_json::from_value(params.clone()).map_err(|error| {
            format!(
                "{}: {error}",
                tr("总目录收藏请求无效", "Invalid root favorite request")
            )
        })?;
        let mut result = self.favorite_drag_apply_internal(
            &json!({
                "sourceKind": "history",
                "sourceParentId": null,
                "itemIds": request.ids,
                "targetParentId": null,
                "anchorId": null,
                "placement": "into",
                "copy": true,
                "expectedRevision": request.expected_revision,
                "operationId": request.operation_id,
            }),
            true,
        )?;
        let label = tr("总目录", "Root");
        result["savedFolder"] = json!({"id":"root", "path":[], "title":label, "pathLabel":label});
        Ok(result)
    }

    /// The same ordered graph/revision as `favor_tree`, plus up to three recent
    /// real folder occurrences. Labels always come from the current graph.
    pub fn favorite_save_choices(&self) -> Result<Value, String> {
        let mut tree = self.favor_tree()?;
        let _operation = self.settings_operation.lock();
        match read_state(&state_path(self)) {
            Ok(state) => {
                tree["recentFolders"] = json!(valid_recents(&tree, state)
                    .iter()
                    .filter_map(|entry| project_folder(&tree, entry))
                    .collect::<Vec<_>>());
            }
            Err(error) => {
                tree["recentFolders"] = json!([]);
                tree["recentWarning"] = json!(error);
            }
        }
        Ok(tree)
    }

    /// Save the whole history selection once. The existing structural command
    /// owns payload retention, atomic persistence, conflict checks and undo.
    /// `path` is an optional ordered occurrence path, including the target ID.
    pub fn favorite_save_history(&self, params: &Value) -> Result<Value, String> {
        let request: SaveRequest = serde_json::from_value(params.clone()).map_err(|error| {
            format!(
                "{}: {error}",
                tr("收藏请求无效", "Invalid favorite request")
            )
        })?;
        let parent = request.parent_id.trim();
        if parent.is_empty()
            || crate::panel::is_root_folder_id(parent)
            || crate::panel::is_orphans_folder_id(parent)
        {
            return Err(tr("请选择具体收藏夹。", "Choose a favorite folder.").into());
        }
        let tree = self.favor_tree()?;
        let path = request.path.or_else(|| first_path(&tree, parent));
        let recent = RecentFolder {
            id: parent.into(),
            path: path.unwrap_or_default(),
        };
        // An old revision may be a replay of an already committed operation.
        // Let the structural command check its receipt before rejecting it.
        if tree["dragRevision"].as_str() == Some(request.expected_revision.as_str())
            && project_folder(&tree, &recent).is_none()
        {
            return Err(tr(
                "收藏位置已失效，请重新选择收藏夹。",
                "The favorite destination changed. Choose a folder again.",
            )
            .into());
        }
        let mut result = self.favorite_drag_apply(&json!({
            "sourceKind": "history",
            "sourceParentId": null,
            "itemIds": request.ids,
            "targetParentId": parent,
            "anchorId": null,
            "placement": "into",
            "copy": true,
            "expectedRevision": request.expected_revision,
            "operationId": request.operation_id,
        }))?;
        // Recents are best-effort presentation state. Failure here must never
        // turn an already committed save into a failure that invites a retry.
        let current = match self.favor_tree() {
            Ok(tree) => tree,
            Err(error) => {
                result["recentWarning"] = json!(error);
                return Ok(result);
            }
        };
        if let Some(folder) = project_folder(&current, &recent) {
            result["savedFolder"] = folder;
            let _operation = self.settings_operation.lock();
            let path = state_path(self);
            let mut recents = valid_recents(&current, read_state(&path).unwrap_or_default());
            recents.retain(|entry| entry.path != recent.path);
            recents.insert(0, recent);
            recents.truncate(RECENT_LIMIT);
            if let Err(error) = crate::config_file::save(&path, &SaveState { recent: recents }) {
                result["recentWarning"] = json!(error);
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, HostState) {
        crate::clipboard::force_headless(true);
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp/favorite-save-tests");
        std::fs::create_dir_all(&root).unwrap();
        let dir = tempfile::Builder::new()
            .prefix("save-")
            .tempdir_in(root)
            .unwrap();
        let host = HostState::open(
            AppPaths::from_data_root(dir.path().join("BetterClipTauri")),
            28965,
        )
        .unwrap();
        (dir, host)
    }

    fn folder(host: &HostState, name: &str, parent: Option<&str>) -> String {
        host.favor_folder_create(&json!({"title": name, "parentId": parent}))
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .into()
    }

    fn history(host: &HostState, text: &str) -> String {
        let clip = ClipRecord::new_text(text, "favorite-save-fixture");
        host.store().lock().insert_clip(&clip, true).unwrap();
        clip.id
    }

    fn request(host: &HostState, ids: &[&str], path: &[&str]) -> Value {
        json!({"ids":ids,"parentId":path.last(),"path":path,
            "expectedRevision":host.favorite_drag_revision().unwrap(),
            "operationId":Uuid::new_v4().simple().to_string()})
    }

    #[test]
    fn favorite_save_batch_atomic_failure_and_retry_keeps_recent_and_history() {
        let (_dir, host) = fixture();
        let target = folder(&host, "Target", None);
        let a = history(&host, "first");
        let b = history(&host, "second");
        let params = request(&host, &[&a, &b], &[&target]);
        let before = host.favorite_save_choices().unwrap();
        host.store().lock().connection().execute_batch("CREATE TRIGGER reject_save_second BEFORE INSERT ON favor_item WHEN NEW.body_text='second' BEGIN SELECT RAISE(ABORT, 'save batch fixture'); END;").unwrap();
        assert!(host
            .favorite_save_history(&params)
            .unwrap_err()
            .contains("save batch fixture"));
        assert_eq!(host.favorite_save_choices().unwrap(), before);
        assert!(host
            .store()
            .lock()
            .get_favor(&target)
            .unwrap()
            .children
            .is_empty());
        host.store()
            .lock()
            .connection()
            .execute_batch("DROP TRIGGER reject_save_second;")
            .unwrap();
        let result = host.favorite_save_history(&params).unwrap();
        assert_eq!(result["createdIds"].as_array().unwrap().len(), 2);
        assert_eq!(
            host.store()
                .lock()
                .get_favor(&target)
                .unwrap()
                .children
                .len(),
            2
        );
        assert!(host.store().lock().get_clip(&a).is_ok());
        assert!(host.store().lock().get_clip(&b).is_ok());
        let replay = host.favorite_save_history(&params).unwrap();
        assert_eq!(replay["createdIds"], result["createdIds"]);
        assert_eq!(
            host.store()
                .lock()
                .get_favor(&target)
                .unwrap()
                .children
                .len(),
            2
        );
        let choices = host.favorite_save_choices().unwrap();
        assert_eq!(choices["recentFolders"][0]["path"], json!([target]));
        assert_eq!(choices["dragRevision"], result["revision"]);
        host.favorite_drag_undo(&json!({"undoId":result["undoId"]}))
            .unwrap();
        assert!(host
            .store()
            .lock()
            .get_favor(&target)
            .unwrap()
            .children
            .is_empty());
    }

    #[test]
    fn favorite_save_recents_keep_shared_occurrences_and_project_rename() {
        let (_dir, host) = fixture();
        let left = folder(&host, "Left", None);
        let right = folder(&host, "Right", None);
        let shared = folder(&host, "Shared", Some(&left));
        let mut row = host.store().lock().get_favor(&right).unwrap();
        row.children.push(shared.clone());
        host.store().lock().save_favor(&row).unwrap();
        let id = history(&host, "payload");
        host.favorite_save_history(&request(&host, &[&id], &[&left, &shared]))
            .unwrap();
        host.favorite_save_history(&request(&host, &[&id], &[&right, &shared]))
            .unwrap();
        host.favor_update(&json!({"id":right,"title":"Renamed"}))
            .unwrap();
        let choices = host.favorite_save_choices().unwrap();
        assert_eq!(choices["recentFolders"].as_array().unwrap().len(), 2);
        assert_eq!(choices["recentFolders"][0]["pathLabel"], "Renamed / Shared");
        assert_eq!(choices["recentFolders"][1]["pathLabel"], "Left / Shared");
        let mut row = host.store().lock().get_favor(&right).unwrap();
        row.children.clear();
        host.store().lock().save_favor(&row).unwrap();
        let choices = host.favorite_save_choices().unwrap();
        assert_eq!(choices["recentFolders"].as_array().unwrap().len(), 1);
        assert_eq!(choices["recentFolders"][0]["path"], json!([left, shared]));
    }

    #[test]
    fn favorite_save_rejects_invalid_destination_and_stale_tree_without_partial_save() {
        let (_dir, host) = fixture();
        let home = folder(&host, "Home", None);
        let gone = folder(&host, "Gone", None);
        let id = history(&host, "payload");
        let old = request(&host, &[&id], &[&gone]);
        host.favor_batch_delete(&json!({"ids":[gone]})).unwrap();
        assert!(host.favorite_save_history(&old).is_err());
        let mut invalid = request(&host, &[&id], &[&home]);
        invalid["path"] = json!(["gone", home]);
        assert!(host.favorite_save_history(&invalid).is_err());
        invalid["parentId"] = json!("root");
        assert!(host.favorite_save_history(&invalid).is_err());
        assert!(host
            .store()
            .lock()
            .get_favor(&home)
            .unwrap()
            .children
            .is_empty());
        assert_eq!(
            host.favorite_save_choices().unwrap()["recentFolders"],
            json!([])
        );
    }

    #[test]
    fn favorite_save_root_batch_is_atomic_idempotent_undoable_and_keeps_recent() {
        let (_dir, host) = fixture();
        let home = folder(&host, "Home", None);
        let recent_clip = history(&host, "recent fixture");
        host.favorite_save_history(&request(&host, &[&recent_clip], &[&home]))
            .unwrap();
        let recent_bytes = std::fs::read(state_path(&host)).unwrap();
        let a = history(&host, "root first");
        let b = history(&host, "root second");
        let before = host.favorite_save_choices().unwrap();
        let params = json!({"ids":[a,b], "expectedRevision":before["dragRevision"],
            "operationId":Uuid::new_v4().simple().to_string()});
        host.store().lock().connection().execute_batch("CREATE TRIGGER reject_root_second BEFORE INSERT ON favor_item WHEN NEW.body_text='root second' BEGIN SELECT RAISE(ABORT, 'root batch fixture'); END;").unwrap();
        host.drain_push_events();
        assert!(host
            .favorite_save_history_to_root(&params)
            .unwrap_err()
            .contains("root batch fixture"));
        assert_eq!(host.favorite_save_choices().unwrap(), before);
        assert_eq!(std::fs::read(state_path(&host)).unwrap(), recent_bytes);
        assert!(!host
            .drain_push_events()
            .iter()
            .any(|(name, _)| name == "favoritesChanged"));
        host.store()
            .lock()
            .connection()
            .execute_batch("DROP TRIGGER reject_root_second;")
            .unwrap();
        let result = host.favorite_save_history_to_root(&params).unwrap();
        assert_eq!(result["createdIds"].as_array().unwrap().len(), 2);
        for (created, source) in result["createdIds"]
            .as_array()
            .unwrap()
            .iter()
            .zip([&a, &b])
        {
            let item = host
                .store()
                .lock()
                .get_favor(created.as_str().unwrap())
                .unwrap();
            assert!(item.parent_id.is_none());
            assert_eq!(item.source_clip_id.as_ref(), Some(source));
            assert!(host.store().lock().get_clip(source).is_ok());
        }
        assert_eq!(result["savedFolder"]["id"], "root");
        assert_eq!(result["savedFolder"]["path"], json!([]));
        assert_eq!(host.favorite_save_history_to_root(&params).unwrap(), result);
        let after = host.favorite_save_choices().unwrap();
        let roots = after["orderedRootItems"].as_array().unwrap();
        let first = roots
            .iter()
            .position(|id| id == &result["createdIds"][0])
            .unwrap();
        let second = roots
            .iter()
            .position(|id| id == &result["createdIds"][1])
            .unwrap();
        assert!(first < second);
        assert_eq!(after["recentFolders"], before["recentFolders"]);
        assert_eq!(std::fs::read(state_path(&host)).unwrap(), recent_bytes);
        host.favorite_drag_undo(&json!({"undoId":result["undoId"]}))
            .unwrap();
        let undone = host.favorite_save_choices().unwrap();
        assert_eq!(undone["items"], before["items"]);
        assert_eq!(undone["orderedRootItems"], before["orderedRootItems"]);
        assert_eq!(undone["recentFolders"], before["recentFolders"]);
    }

    #[test]
    fn favorite_save_root_same_millisecond_keeps_batch_order_after_existing_items() {
        let (_dir, host) = fixture();
        let old_clip = ClipRecord::new_text("existing root", "favorite-save-fixture");
        let mut existing = FavorRecord::from_clip(&old_clip, None);
        existing.sub_index = 17;
        host.store().lock().insert_favor_checked(&existing).unwrap();
        let a = history(&host, "same ms first");
        let b = history(&host, "same ms second");
        let result = host
            .favorite_save_history_to_root(&json!({"ids":[a,b],
            "expectedRevision":host.favorite_drag_revision().unwrap(),
            "operationId":Uuid::new_v4().simple().to_string()}))
            .unwrap();
        let created = result["createdIds"].as_array().unwrap();
        let fixed_time = DateTime::<Utc>::from_timestamp_millis(1_700_000_000_000).unwrap();
        // Force the exact persistence tie which previously exposed random UUID
        // ordering. This is a regression precondition, not a timing/sleep race.
        for (offset, id) in created.iter().enumerate() {
            let id = id.as_str().unwrap();
            let store = host.store();
            let store = store.lock();
            let item = store.get_favor(id).unwrap();
            assert_eq!(item.sub_index, 18 + offset as i32);
            // Ordinary edits preserve creation time; set this test-only tie directly.
            assert_eq!(store.connection().execute(
                "UPDATE favor_item SET create_time=?1 WHERE id=?2",
                rusqlite::params![fixed_time.timestamp_millis(), id],
            ).unwrap(), 1);
            assert_eq!(store.get_favor(id).unwrap().create_time, fixed_time,
                "the equal-time regression precondition must be persisted");
        }
        let tree = host.favorite_save_choices().unwrap();
        assert_eq!(
            tree["orderedRootItems"],
            json!([existing.id, created[0], created[1]])
        );
        let first_time = host
            .store()
            .lock()
            .get_favor(created[0].as_str().unwrap())
            .unwrap()
            .create_time;
        let second_time = host
            .store()
            .lock()
            .get_favor(created[1].as_str().unwrap())
            .unwrap()
            .create_time;
        assert_eq!(first_time, second_time);
    }

    #[test]
    fn favorite_save_root_exhausted_order_indices_compact_atomically_and_undo() {
        let (_dir, host) = fixture();
        let mut original =
            FavorRecord::from_clip(&ClipRecord::new_text("legacy max", "fixture"), None);
        original.sub_index = i32::MAX;
        host.store().lock().insert_favor_checked(&original).unwrap();
        let a = history(&host, "after max first");
        let b = history(&host, "after max second");
        let before = host.favorite_save_choices().unwrap();
        let result = host
            .favorite_save_history_to_root(&json!({"ids":[a,b],
            "expectedRevision":before["dragRevision"],
            "operationId":Uuid::new_v4().simple().to_string()}))
            .unwrap();
        let created = result["createdIds"].as_array().unwrap();
        assert_eq!(
            host.favorite_save_choices().unwrap()["orderedRootItems"],
            json!([original.id, created[0], created[1]])
        );
        assert_eq!(
            host.store()
                .lock()
                .get_favor(&original.id)
                .unwrap()
                .sub_index,
            0
        );
        host.favorite_drag_undo(&json!({"undoId":result["undoId"]}))
            .unwrap();
        assert_eq!(
            host.store()
                .lock()
                .get_favor(&original.id)
                .unwrap()
                .sub_index,
            i32::MAX
        );
        let undone = host.favorite_save_choices().unwrap();
        assert_eq!(undone["items"], before["items"]);
        assert_eq!(undone["orderedRootItems"], before["orderedRootItems"]);
    }

    #[test]
    fn favorite_save_root_exception_does_not_relax_folder_or_public_drag_guards() {
        let (_dir, host) = fixture();
        let home = folder(&host, "Home", None);
        let id = history(&host, "guard fixture");
        let before = host.favorite_save_choices().unwrap();
        let revision = before["dragRevision"].clone();
        let operation = Uuid::new_v4().simple().to_string();
        let public_drag = json!({"sourceKind":"history", "sourceParentId":null,"itemIds":[id],
            "targetParentId":null,"anchorId":null,"placement":"into","copy":true,
            "expectedRevision":revision,"operationId":operation});
        assert!(host
            .favorite_drag_apply(&public_drag)
            .unwrap_err()
            .contains("顶层当前仅支持收藏夹"));
        let mut invalid_folder = request(&host, &[&id], &[&home]);
        invalid_folder["parentId"] = json!("missing-folder");
        invalid_folder["path"] = json!(["missing-folder"]);
        assert!(host.favorite_save_history(&invalid_folder).is_err());
        invalid_folder["parentId"] = json!("root");
        assert!(host.favorite_save_history(&invalid_folder).is_err());
        // Explicit root calls do not accept a folder field or silently repair it.
        assert!(host.favorite_save_history_to_root(&invalid_folder).is_err());
        let mut invalid_native = public_drag.clone();
        invalid_native["targetParentId"] = json!(home);
        assert!(host
            .favorite_drag_apply_internal(&invalid_native, true)
            .is_err());
        assert_eq!(host.favorite_save_choices().unwrap(), before);
        assert!(!state_path(&host).exists());
    }

    #[test]
    fn favorite_save_root_missing_history_and_stale_revision_leave_no_partial_batch() {
        let (_dir, host) = fixture();
        let home = folder(&host, "Home", None);
        let id = history(&host, "root guard fixture");
        let initial = host.favorite_save_choices().unwrap();
        let mut params = json!({"ids":[id,"missing-history"],"expectedRevision":initial["dragRevision"],
            "operationId":Uuid::new_v4().simple().to_string()});
        assert!(host.favorite_save_history_to_root(&params).is_err());
        assert_eq!(host.favorite_save_choices().unwrap(), initial);
        host.favor_update(&json!({"id":home,"title":"Renamed"}))
            .unwrap();
        let before_stale = host.favorite_save_choices().unwrap();
        params["ids"] = json!([id]);
        assert!(host
            .favorite_save_history_to_root(&params)
            .unwrap_err()
            .contains("WRITE_CONFLICT"));
        assert_eq!(host.favorite_save_choices().unwrap(), before_stale);
        assert!(!state_path(&host).exists());
    }

    #[test]
    fn favorite_save_recents_limit_persist_and_do_not_change_business_revision() {
        let (_dir, host) = fixture();
        let folders: Vec<_> = (0..4)
            .map(|n| folder(&host, &format!("Folder {n}"), None))
            .collect();
        let id = history(&host, "payload");
        for folder in &folders {
            host.favorite_save_history(&request(&host, &[&id], &[folder]))
                .unwrap();
        }
        let revision = host.favorite_drag_revision().unwrap();
        let choices = host.favorite_save_choices().unwrap();
        assert_eq!(
            choices["recentFolders"].as_array().unwrap().len(),
            RECENT_LIMIT
        );
        assert_eq!(choices["recentFolders"][0]["id"], folders[3]);
        assert_eq!(choices["recentFolders"][2]["id"], folders[1]);
        assert_eq!(host.favorite_drag_revision().unwrap(), revision);
        let state = read_state(&state_path(&host)).unwrap();
        assert_eq!(state.recent.len(), RECENT_LIMIT);
        assert_eq!(state.recent[0].id, folders[3]);
    }
}
