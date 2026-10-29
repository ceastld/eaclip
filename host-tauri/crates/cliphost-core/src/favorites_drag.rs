//! Relationship-aware favorite drag commands. The config journal bridges the
//! SQLite commit and legacy RootChildren file; it is recovered before capture.
use super::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const STATE_KEY: &str = "favorite-drag.state.v1";
const JOURNAL_FILE: &str = "clip-host-favorite-drag-journal.json";
const CONFLICT: &str = "WRITE_CONFLICT:收藏已在另一窗口中更改，请刷新后重试。";
const MAX_RECEIPTS: usize = 128;

#[derive(Default, Serialize, Deserialize)]
struct DragState {
    receipts: Vec<Receipt>,
    undo: Option<Undo>,
}

#[derive(Serialize, Deserialize)]
struct Receipt {
    operation_id: String,
    request_hash: String,
    result: Value,
}

#[derive(Clone, Serialize, Deserialize)]
struct Undo {
    id: String,
    after_revision: String,
    before: Vec<FavorRecord>,
    created: Vec<String>,
    roots: Vec<String>,
    #[serde(default)]
    selected: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct RootJournal {
    operation_id: String,
    before: Vec<String>,
    after: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DragRequest {
    #[serde(default = "favorite_source")]
    source_kind: String,
    source_parent_id: Option<String>,
    item_ids: Vec<String>,
    target_parent_id: Option<String>,
    anchor_id: Option<String>,
    placement: String,
    #[serde(default)]
    copy: bool,
    expected_revision: String,
    operation_id: String,
}

fn favorite_source() -> String {
    "favorite".into()
}

/// Order and content are both covered, including original document revisions.
/// Sorting by ID makes SQL iteration order irrelevant to the concurrency token.
pub(super) fn snapshot_revision(items: &[FavorRecord], roots: &[String]) -> Result<String, String> {
    if !items.is_empty() && items.iter().all(|item| item.storage_revision.as_deref().is_some_and(|r| r.starts_with("core2:"))) {
        // Core tokens cover complete content/structure, independent of display
        // fallbacks, resolved copy paths and the in-memory DTO's shape.
        let revisions: BTreeMap<_, _> = items.iter()
            .map(|item| (&item.id, item.storage_revision.as_deref().unwrap()))
            .collect();
        return Ok(crate::model::sha256_hex(&serde_json::to_vec(
            &json!({"schema":2,"items":revisions,"roots":roots})
        ).map_err(|e| e.to_string())?));
    }
    legacy_snapshot_revision(items, roots)
}

pub(super) fn legacy_snapshot_revision(items: &[FavorRecord], roots: &[String]) -> Result<String, String> {
    let sorted: BTreeMap<_, _> = items
        .iter()
        .map(|item| {
            (
                &item.id,
                json!({"record":item,"storageRevision":item.storage_revision}),
            )
        })
        .collect();
    let value = json!({"schema":1,"items":sorted,"roots":roots});
    Ok(crate::model::sha256_hex(
        &serde_json::to_vec(&value).map_err(|e| e.to_string())?,
    ))
}

fn state(db: &AppStore) -> Result<DragState, String> {
    db.kv_get(STATE_KEY)
        .map_err(|e| e.to_string())?
        .map(|raw| serde_json::from_str(&raw).map_err(|e| format!("收藏拖动记录无效：{e}")))
        .transpose()
        .map(Option::unwrap_or_default)
}

fn save_state(db: &AppStore, state: &DragState) -> Result<(), String> {
    if let Some(receipt) = state.receipts.last() {
        db.kv_set(
            &receipt_key(&receipt.operation_id),
            &serde_json::to_string(receipt).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
    }
    db.kv_set(
        STATE_KEY,
        &serde_json::to_string(state).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())
}

fn receipt_key(operation_id: &str) -> String {
    format!(
        "favorite-drag.receipt.{}",
        crate::model::sha256_hex(operation_id.as_bytes())
    )
}

fn remember(state: &mut DragState, operation_id: &str, request_hash: &str, result: Value) {
    state.receipts.push(Receipt {
        operation_id: operation_id.into(),
        request_hash: request_hash.into(),
        result,
    });
    if state.receipts.len() > MAX_RECEIPTS {
        state.receipts.remove(0);
    }
}

fn previous_result(
    db: &AppStore,
    state: &DragState,
    operation_id: &str,
    request_hash: &str,
) -> Result<Option<Value>, String> {
    let persisted: Option<Receipt> = db
        .kv_get(&receipt_key(operation_id))
        .map_err(|e| e.to_string())?
        .map(|raw| serde_json::from_str(&raw).map_err(|e| format!("收藏拖动回执无效：{e}")))
        .transpose()?;
    match persisted.as_ref().or_else(|| {
        state
            .receipts
            .iter()
            .find(|receipt| receipt.operation_id == operation_id)
    }) {
        Some(receipt) if receipt.request_hash == request_hash => Ok(Some(receipt.result.clone())),
        Some(_) => Err("该拖动操作编号已用于不同请求。".into()),
        None => Ok(None),
    }
}

pub(super) fn has_pending_recovery(paths: &AppPaths) -> bool {
    paths.data_root.join(JOURNAL_FILE).exists()
}

/// A committed receipt means roll forward; otherwise roll back the root file.
/// Unknown roots mean another process intervened: keep the journal and refuse
/// startup instead of overwriting its work. Other preferences are preserved.
pub(super) fn recover_pending(
    db: &AppStore,
    paths: &AppPaths,
    favor: &mut crate::extra_settings::FavorSettings,
) -> Result<(), String> {
    if db.document_recovery_status().map_err(|e| e.to_string())?["readOnly"] == true {
        return Err("DATABASE_RECOVERY_READ_ONLY: 收藏恢复已暂缓，原始数据库与操作日志已保留。".into());
    }
    let path = paths.data_root.join(JOURNAL_FILE);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("无法读取收藏拖动恢复记录：{error}")),
    };
    let journal: RootJournal = serde_json::from_slice(&bytes)
        .map_err(|error| format!("收藏拖动恢复记录损坏，已保留原文件：{error}"))?;
    let committed = db
        .kv_get(&receipt_key(&journal.operation_id))
        .map_err(|e| e.to_string())?
        .is_some()
        || state(db)?
            .receipts
            .iter()
            .any(|r| r.operation_id == journal.operation_id);
    let config_path = paths.configs.join("FavorConfig.json");
    let mut current: crate::extra_settings::FavorSettings =
        crate::config_compat::load(&config_path)?;
    if current.root_children != journal.before && current.root_children != journal.after {
        return Err("收藏顶层在未完成的拖动后又被修改；恢复记录已保留，请先处理冲突。".into());
    }
    let wanted = if committed {
        journal.after
    } else {
        journal.before
    };
    if current.root_children != wanted {
        current.root_children = wanted;
        crate::config_file::save(&config_path, &current)?;
    }
    *favor = current;
    std::fs::remove_file(&path).map_err(|e| format!("无法完成收藏拖动恢复：{e}"))
}

/// The effective graph matches favor_tree: preserve explicit edges, then append
/// ParentId-only compatibility edges. Missing raw references are never repaired.
struct Graph {
    rows: BTreeMap<String, FavorRecord>,
    children: BTreeMap<String, Vec<String>>,
    roots: Vec<String>,
}

impl Graph {
    fn new(items: &[FavorRecord], root_order: &[String]) -> Self {
        let rows: BTreeMap<_, _> = items
            .iter()
            .map(|item| (item.id.clone(), item.clone()))
            .collect();
        let children = crate::panel::effective_children(items);
        let mut fallback: Vec<_> = items.iter().collect();
        fallback.sort_by(|a, b| {
            a.sub_index
                .cmp(&b.sub_index)
                .then(a.create_time.cmp(&b.create_time))
                .then(a.id.cmp(&b.id))
        });
        let referenced: HashSet<_> = children.values().flatten().collect();
        let mut roots = normalized_root_order(items, root_order);
        roots.retain(|id| !referenced.contains(id));
        for item in fallback {
            if item.parent_id.as_ref().is_none_or(String::is_empty)
                && !referenced.contains(&item.id)
                && !roots.contains(&item.id)
            {
                roots.push(item.id.clone());
            }
        }
        Self {
            rows,
            children,
            roots,
        }
    }

    fn members(&self, parent: Option<&str>) -> Result<Vec<String>, String> {
        match parent {
            None => Ok(self.roots.clone()),
            Some(id) if self.rows.get(id).is_some_and(|row| row.is_folder) => {
                Ok(self.children[id].clone())
            }
            Some(_) => Err("目标或来源收藏夹已不存在。".into()),
        }
    }

    fn reaches(&self, start: &str, target: &str) -> bool {
        let mut todo = vec![start];
        let mut visited = HashSet::new();
        while let Some(id) = todo.pop() {
            if id == target {
                return true;
            }
            if visited.insert(id) {
                if let Some(edges) = self.children.get(id) {
                    todo.extend(edges.iter().map(String::as_str));
                }
            }
        }
        false
    }

    fn subtree(&self, roots: &[String]) -> Result<Vec<String>, String> {
        // Explicit color DFS rejects existing cycles before preparing any assets.
        let mut colors = HashMap::<String, u8>::new();
        let mut ordered = Vec::new();
        for root in roots {
            let mut stack = vec![(root.clone(), false)];
            while let Some((id, leave)) = stack.pop() {
                if leave {
                    colors.insert(id, 2);
                    continue;
                }
                match colors.get(&id) {
                    Some(1) => return Err("收藏结构存在循环，无法复制。".into()),
                    Some(2) => continue,
                    _ => {}
                }
                if !self.rows.contains_key(&id) {
                    return Err("收藏夹包含缺失条目，无法完整复制。".into());
                }
                colors.insert(id.clone(), 1);
                ordered.push(id.clone());
                stack.push((id.clone(), true));
                if let Some(edges) = self.children.get(&id) {
                    stack.extend(edges.iter().rev().map(|child| (child.clone(), false)));
                }
            }
        }
        Ok(ordered)
    }
}

fn parent_value(parent: &Option<String>) -> Result<Option<&str>, String> {
    match parent.as_deref() {
        None => Ok(None),
        Some(id)
            if !id.is_empty()
                && id.trim() == id
                && !crate::panel::is_root_folder_id(id)
                && !crate::panel::is_orphans_folder_id(id) =>
        {
            Ok(Some(id))
        }
        _ => Err("收藏目录身份无效；顶层必须使用 null。".into()),
    }
}

fn insert_members(
    mut members: Vec<String>,
    ids: &[String],
    anchor: Option<&str>,
    placement: &str,
) -> Result<Vec<String>, String> {
    let offset = match placement {
        "into" if anchor.is_none() => members.len(),
        "before" | "after" => {
            let anchor = anchor.ok_or("缺少放置位置。")?;
            members
                .iter()
                .position(|id| id == anchor)
                .ok_or("放置位置已不存在。")?
                + usize::from(placement == "after")
        }
        _ => return Err("拖动放置方式无效。".into()),
    };
    members.splice(offset..offset, ids.iter().cloned());
    Ok(members)
}

struct PreparedAssets {
    paths: Vec<PathBuf>,
    committed: bool,
    isolated_core: bool,
}
impl PreparedAssets {
    fn new(isolated_core: bool) -> Self {
        Self {
            paths: Vec::new(),
            committed: false,
            isolated_core,
        }
    }
    fn copy(&mut self, source: &str, library: &Path) -> Result<String, String> {
        let source = local_path(source)?;
        let extension = source
            .extension()
            .and_then(|s| s.to_str())
            .filter(|s| s.len() <= 12)
            .unwrap_or("img");
        let preferred = format!(
            "drag-copy-{}.{}",
            Uuid::new_v4().simple(),
            extension
        );
        let publish = if self.isolated_core { crate::storage_core::resources::publish_copy }
            else { crate::storage_core::resources::publish_copy_compatible };
        let destination = publish(&source, library, &preferred)
            .map_err(|error| format!("副本图片不可用：{error}"))?;
        self.paths.push(destination.clone());
        Ok(destination.to_string_lossy().into_owned())
    }
}
impl Drop for PreparedAssets {
    fn drop(&mut self) {
        if !self.committed {
            for path in &self.paths {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

fn local_path(path: &str) -> Result<PathBuf, String> {
    if path.starts_with("file:") {
        return url::Url::parse(path)
            .ok()
            .and_then(|url| url.to_file_path().ok())
            .ok_or_else(|| "收藏图片路径无效。".into());
    }
    Ok(PathBuf::from(path))
}

fn retain_assets(
    item: &mut FavorRecord,
    library: &Path,
    assets: &mut PreparedAssets,
) -> Result<(), String> {
    if let Some(path) = item.image_path.as_deref().filter(|path| !path.is_empty()) {
        item.image_path = Some(assets.copy(path, library)?);
    }
    if let Some(raw) = item.qq_rich_text.as_deref().filter(|raw| !raw.is_empty()) {
        use crate::im_rich::document::{ImRichDocument, RichNodeKind, RichOrigin};
        let origin = match crate::im_rich::resolve_origin(&item.source_process_name, raw) {
            crate::im_rich::ImRichOrigin::Qq => RichOrigin::Qq,
            crate::im_rich::ImRichOrigin::WeChat => RichOrigin::Wechat,
            crate::im_rich::ImRichOrigin::Wework => RichOrigin::Wework,
            crate::im_rich::ImRichOrigin::Unknown => RichOrigin::Unknown,
        };
        let mut document =
            ImRichDocument::parse(raw, origin).map_err(|e| format!("无法完整复制富文本：{e}"))?;
        let images: Vec<_> = document
            .nodes()
            .iter()
            .filter(|node| node.kind() == RichNodeKind::Image && !node.path().is_empty())
            .map(|node| (node.id(), node.path().to_string()))
            .collect();
        for (id, path) in images {
            let copied = assets.copy(&path, library)?;
            document.set_path(id, &copied).map_err(|e| e.to_string())?;
        }
        item.qq_rich_text = Some(document.serialize());
    }
    // Original CF_HTML stays byte-for-byte intact. Its local asset references
    // are retained by the shared library; deletion never eagerly removes them.
    Ok(())
}

impl HostState {
    pub fn favorite_drag_revision(&self) -> Result<String, String> {
        let _operation = self.settings_operation.lock();
        let (store, paths, mut favor) = {
            let inner = self.inner.lock();
            (
                inner.store.clone(),
                inner.paths.clone(),
                inner.extras.favor.clone(),
            )
        };
        let db = store.lock();
        if !db.document_recovery_status().map_err(|e| e.to_string())?["readOnly"].as_bool().unwrap_or(true) {
            recover_pending(&db, &paths, &mut favor)?;
        }
        let favor: crate::extra_settings::FavorSettings =
            crate::config_compat::load(&paths.configs.join("FavorConfig.json"))?;
        snapshot_revision(
            &db.list_favors().map_err(|e| e.to_string())?,
            &favor.root_children,
        )
    }

    pub fn favorite_drag_apply(&self, params: &Value) -> Result<Value, String> {
        self.favorite_drag_apply_internal(params, false)
    }

    /// Only the native star's explicit root-save command may allow historical
    /// content at the top level. Ordinary drag/drop keeps its existing rules.
    pub(super) fn favorite_drag_apply_internal(&self, params: &Value, allow_history_root: bool) -> Result<Value, String> {
        self.require_writable_document_view()?;
        let request: DragRequest =
            serde_json::from_value(params.clone()).map_err(|e| format!("拖动请求无效：{e}"))?;
        if request.operation_id.trim().is_empty() || request.operation_id.len() > 160 {
            return Err("缺少有效的拖动操作编号。".into());
        }
        if request.item_ids.is_empty() || request.item_ids.len() > 500 {
            return Err("请选择 1 到 500 个收藏项目。".into());
        }
        if !matches!(request.placement.as_str(), "before" | "after" | "into")
            || (request.placement == "into" && request.anchor_id.is_some())
        {
            return Err("拖动放置方式无效。".into());
        }
        if !matches!(request.source_kind.as_str(), "favorite" | "history") {
            return Err("未知拖动来源。".into());
        }
        let source_parent = parent_value(&request.source_parent_id)?;
        let target_parent = parent_value(&request.target_parent_id)?;
        if allow_history_root && !(request.source_kind == "history" && source_parent.is_none()
            && target_parent.is_none() && request.placement == "into" && request.anchor_id.is_none() && request.copy)
        {
            return Err("总目录收藏请求无效。".into());
        }
        // Keep existing receipt hashes unchanged for ordinary structural calls;
        // the native exception has a separate hash domain for the same op ID.
        let hash_params = if allow_history_root { json!({ "nativeHistoryRoot": params }) } else { params.clone() };
        let request_hash =
            crate::model::sha256_hex(&serde_json::to_vec(&hash_params).map_err(|e| e.to_string())?);
        let _operation = self.settings_operation.lock();
        let (store, paths, mut favor) = {
            let inner = self.inner.lock();
            (
                inner.store.clone(),
                inner.paths.clone(),
                inner.extras.favor.clone(),
            )
        };
        let db = store.lock();
        recover_pending(&db, &paths, &mut favor)?;
        favor = crate::config_compat::load(&paths.configs.join("FavorConfig.json"))?;
        let mut drag_state = state(&db)?;
        if let Some(result) =
            previous_result(&db, &drag_state, &request.operation_id, &request_hash)?
        {
            return Ok(result);
        }
        let transaction = db
            .connection()
            .unchecked_transaction()
            .map_err(|e| e.to_string())?;
        let items = db.list_favors().map_err(|e| e.to_string())?;
        let revision = snapshot_revision(&items, &favor.root_children)?;
        if request.expected_revision != revision {
            return Err(CONFLICT.into());
        }
        let graph = Graph::new(&items, &favor.root_children);
        let target_members = graph.members(target_parent)?;
        let mut selected = Vec::<String>::new();
        let mut undo_selection = Vec::<String>::new();
        let mut changed = BTreeMap::<String, FavorRecord>::new();
        let mut created = Vec::<FavorRecord>::new();
        let mut created_sources = HashMap::<String, (&str, String)>::new();
        let mut assets = PreparedAssets::new(db.is_core_store());
        let mut roots = graph.roots.clone();
        let is_copy = request.copy || request.source_kind == "history";
        if request.source_kind == "history" {
            if source_parent.is_some() {
                return Err("历史来源不能指定收藏目录。".into());
            }
            let mut seen = HashSet::new();
            for id in &request.item_ids {
                if !seen.insert(id) {
                    continue;
                }
                let clip = db.get_clip(id).map_err(|_| "来源历史已不存在。")?;
                let mut item = FavorRecord::from_clip(&clip, request.target_parent_id.clone());
                item = db.core_output_favor(&item).map_err(|e| e.to_string())?;
                retain_assets(&mut item, &paths.image_library, &mut assets)?;
                created_sources.insert(item.id.clone(), ("clip_item", id.clone()));
                selected.push(item.id.clone());
                created.push(item);
            }
            if allow_history_root {
                // RootChildren stores folders only. Ordinary root items are
                // projected by SubIndex/CreateTime/ID; timestamps round to ms
                // in storage, so a fast batch must not rely on UUID tie breaks.
                let existing: Vec<_> = graph.roots.iter().filter_map(|id| graph.rows.get(id))
                    .filter(|item| !item.is_folder).collect();
                let last_offset = i32::try_from(created.len().saturating_sub(1))
                    .map_err(|_| "收藏根目录条目过多，无法继续排序。")?;
                let start = existing.iter().map(|item| item.sub_index).max()
                    .map_or(Some(0), |index| index.checked_add(1))
                    .filter(|start| start.checked_add(last_offset).is_some());
                let start = if let Some(start) = start { start } else {
                    // Exhausted legacy indices are compacted in their visible
                    // order inside this same transaction and included in undo.
                    let start = i32::try_from(existing.len())
                        .map_err(|_| "收藏根目录条目过多，无法继续排序。")?;
                    start.checked_add(last_offset).ok_or("收藏根目录条目过多，无法继续排序。")?;
                    for (index, original) in existing.iter().enumerate() {
                        let mut row = (*original).clone();
                        row.sub_index = index as i32;
                        changed.insert(row.id.clone(), row);
                    }
                    start
                };
                for (offset, item) in created.iter_mut().enumerate() {
                    item.sub_index = start + offset as i32;
                }
            }
        } else {
            let source_members = graph.members(source_parent)?;
            let requested: HashSet<_> = request.item_ids.iter().collect();
            for id in &request.item_ids {
                if !graph.rows.contains_key(id) || !source_members.contains(id) {
                    return Err("来源项目已移动或不属于原收藏夹。".into());
                }
            }
            for id in source_members {
                if requested.contains(&id) && !selected.contains(&id) {
                    selected.push(id);
                }
            }
            // A selected ancestor already carries this occurrence's descendants.
            let all_selected = selected.clone();
            selected.retain(|id| {
                !all_selected
                    .iter()
                    .any(|other| other != id && graph.reaches(other, id))
            });
            if selected.is_empty() {
                return Err("来源收藏结构存在循环，无法拖动。".into());
            }
            undo_selection = selected.clone();
            if let Some(target) = target_parent {
                if selected.iter().any(|id| graph.reaches(id, target)) {
                    return Err("不能将收藏夹移动或复制到自身或子文件夹中。".into());
                }
            }
            if is_copy {
                let subtree = graph.subtree(&selected)?;
                let ids: HashMap<_, _> = subtree
                    .iter()
                    .map(|id| (id.clone(), Uuid::new_v4().simple().to_string()))
                    .collect();
                let top: HashSet<_> = selected.iter().collect();
                for id in &subtree {
                    let mut item = graph.rows[id].clone();
                    item.storage_revision = None;
                    item.id = ids[id].clone();
                    item.children = graph.children[id]
                        .iter()
                        .map(|child| ids[child].clone())
                        .collect();
                    item.parent_id = if top.contains(id) {
                        request.target_parent_id.clone()
                    } else {
                        item.parent_id
                            .as_ref()
                            .and_then(|parent| ids.get(parent).cloned())
                            .or_else(|| {
                                subtree
                                    .iter()
                                    .find(|parent| graph.children[*parent].contains(id))
                                    .map(|parent| ids[parent].clone())
                            })
                    };
                    item.create_time = Utc::now();
                    item.update_time = item.create_time;
                    item = db.core_output_favor(&item).map_err(|e| e.to_string())?;
                    retain_assets(&mut item, &paths.image_library, &mut assets)?;
                    created_sources.insert(item.id.clone(), ("favor_item", id.clone()));
                    created.push(item);
                }
                selected = selected.iter().map(|id| ids[id].clone()).collect();
            } else {
                let mut source_members = graph.members(source_parent)?;
                source_members.retain(|id| !selected.contains(id));
                if let Some(parent) = source_parent {
                    let mut row = graph.rows[parent].clone();
                    row.children = source_members;
                    changed.insert(row.id.clone(), row);
                } else {
                    roots = source_members;
                }
                for id in &selected {
                    let mut row = graph.rows[id].clone();
                    if row.parent_id.as_deref().filter(|id| !id.is_empty()) == source_parent {
                        row.parent_id = request.target_parent_id.clone();
                        changed.insert(row.id.clone(), row);
                    }
                }
            }
        }
        if target_parent.is_none() {
            let kinds: HashMap<_, _> = created
                .iter()
                .map(|item| (item.id.as_str(), item.is_folder))
                .collect();
            if !allow_history_root && selected.iter().any(|id| {
                !kinds
                    .get(id.as_str())
                    .copied()
                    .unwrap_or_else(|| graph.rows.get(id).is_some_and(|item| item.is_folder))
            }) {
                return Err("顶层当前仅支持收藏夹，请放到具体收藏夹中。".into());
            }
            if !is_copy
                && selected.iter().any(|id| {
                    graph.children.iter().any(|(parent, children)| {
                        Some(parent.as_str()) != source_parent && children.contains(id)
                    })
                })
            {
                return Err("该收藏夹仍被其他目录引用，不能移到顶层。".into());
            }
        }
        let mut destination = if !is_copy && source_parent == target_parent {
            target_members
                .into_iter()
                .filter(|id| !selected.contains(id))
                .collect()
        } else {
            target_members
        };
        let mut unchanged_move = false;
        // Dropping before/after one of the dragged members is an unchanged move.
        if !is_copy
            && source_parent == target_parent
            && request
                .anchor_id
                .as_ref()
                .is_some_and(|id| selected.contains(id))
        {
            unchanged_move = true;
            changed.clear();
            roots = graph.roots.clone();
        } else {
            if selected.iter().any(|id| destination.contains(id)) {
                return Err("目标目录已包含所选项目；未改变现有引用。".into());
            }
            destination = insert_members(
                destination,
                &selected,
                request.anchor_id.as_deref(),
                &request.placement,
            )?;
            if !is_copy
                && source_parent == target_parent
                && destination == graph.members(target_parent)?
            {
                unchanged_move = true;
                changed.clear();
            } else if let Some(parent) = target_parent {
                let mut row = changed
                    .remove(parent)
                    .unwrap_or_else(|| graph.rows[parent].clone());
                row.children = destination.clone();
                changed.insert(row.id.clone(), row);
            } else {
                roots = destination;
            }
        }
        let root_touched =
            !unchanged_move && (target_parent.is_none() || (!is_copy && source_parent.is_none()));
        let mut new_favor = favor.clone();
        if root_touched {
            new_favor.root_children = roots
                .into_iter()
                .filter(|id| {
                    graph.rows.get(id).is_some_and(|item| item.is_folder)
                        || created.iter().any(|item| &item.id == id && item.is_folder)
                })
                .collect();
            // Preserve unrelated stale config references rather than repairing them.
            for id in &favor.root_children {
                if !graph.roots.contains(id)
                    && !selected.contains(id)
                    && !new_favor.root_children.contains(id)
                {
                    new_favor.root_children.push(id.clone());
                }
            }
        }
        changed.retain(|id, item| {
            serde_json::to_value(item).ok()
                != graph
                    .rows
                    .get(id)
                    .and_then(|old| serde_json::to_value(old).ok())
        });
        let before: Vec<_> = changed.keys().map(|id| graph.rows[id].clone()).collect();
        // Raw inserts deliberately do not mutate parent Children: the complete
        // selected edges below are written once, inside this same transaction.
        for item in &created {
            let mut detached = item.clone();
            detached.parent_id = None;
            db.insert_favor(&detached).map_err(|e| e.to_string())?;
        }
        for item in &created {
            if item.parent_id.is_some() {
                let mut stored = db.get_favor(&item.id).map_err(|e| e.to_string())?;
                stored.parent_id = item.parent_id.clone();
                db.save_favor(&stored).map_err(|e| e.to_string())?;
            }
        }
        for item in changed.values() {
            db.save_favor(item).map_err(|e| e.to_string())?;
        }
        for item in &created {
            let (table, source_id) = &created_sources[&item.id];
            db.connection()
                .inherit_favorite_document(table, source_id, item)
                .map_err(|e| e.to_string())?;
        }
        db.validate_favorite_graph().map_err(|e| e.to_string())?;
        let after = db.list_favors().map_err(|e| e.to_string())?;
        let after_revision = snapshot_revision(&after, &new_favor.root_children)?;
        let created_ids: Vec<_> = created.iter().map(|item| item.id.clone()).collect();
        let mut result = json!({"ok":true,"createdIds":created_ids,"selectedIds":selected,"revision":after_revision});
        if after_revision != revision {
            let undo_id = Uuid::new_v4().simple().to_string();
            drag_state.undo = Some(Undo {
                id: undo_id.clone(),
                after_revision: after_revision.clone(),
                before,
                created: created_ids.clone(),
                roots: favor.root_children.clone(),
                selected: undo_selection,
            });
            result["undoId"] = json!(undo_id);
        }
        remember(
            &mut drag_state,
            &request.operation_id,
            &request_hash,
            result.clone(),
        );
        save_state(&db, &drag_state)?;
        commit_with_roots(
            transaction,
            &paths,
            &favor,
            &new_favor,
            &request.operation_id,
        )?;
        assets.committed = true;
        drop(db);
        self.inner.lock().extras.favor = new_favor;
        let mut upserts: Vec<_> = changed.keys().cloned().collect();
        upserts.extend(created_ids);
        if !upserts.is_empty() || root_touched {
            self.emit_favorites_changed(&upserts, &[]);
        }
        Ok(result)
    }

    pub fn favorite_drag_undo(&self, params: &Value) -> Result<Value, String> {
        self.require_writable_document_view()?;
        let undo_id = req_str(params, "undoId")?;
        let operation_id = format!("undo:{undo_id}");
        let _operation = self.settings_operation.lock();
        let (store, paths, mut favor) = {
            let inner = self.inner.lock();
            (
                inner.store.clone(),
                inner.paths.clone(),
                inner.extras.favor.clone(),
            )
        };
        let db = store.lock();
        recover_pending(&db, &paths, &mut favor)?;
        favor = crate::config_compat::load(&paths.configs.join("FavorConfig.json"))?;
        let mut drag_state = state(&db)?;
        if let Some(result) = previous_result(&db, &drag_state, &operation_id, &operation_id)? {
            return Ok(result);
        }
        let undo = drag_state
            .undo
            .as_ref()
            .filter(|undo| undo.id == undo_id)
            .cloned()
            .ok_or("该操作已无法撤销。")?;
        let transaction = db
            .connection()
            .unchecked_transaction()
            .map_err(|e| e.to_string())?;
        let revision = snapshot_revision(
            &db.list_favors().map_err(|e| e.to_string())?,
            &favor.root_children,
        )?;
        if revision != undo.after_revision
            || params
                .get("expectedRevision")
                .and_then(Value::as_str)
                .is_some_and(|expected| expected != revision)
        {
            return Err(CONFLICT.into());
        }
        for original in &undo.before {
            let current = db.get_favor(&original.id).map_err(|e| e.to_string())?;
            let mut row = if db.is_core_store() {
                // Drag undo restores structure only. The flattened saved view
                // never becomes the authority for full core payload/overrides.
                let mut row = current.clone();
                row.parent_id = original.parent_id.clone();
                row.children = original.children.clone();
                row.sub_index = original.sub_index;
                row
            } else {
                original.clone()
            };
            row.storage_revision = current.storage_revision;
            db.save_favor(&row).map_err(|e| e.to_string())?;
        }
        db.delete_favors_exact(&undo.created)
            .map_err(|e| e.to_string())?;
        db.validate_favorite_graph().map_err(|e| e.to_string())?;
        let mut new_favor = favor.clone();
        new_favor.root_children = undo.roots;
        let after_revision = snapshot_revision(
            &db.list_favors().map_err(|e| e.to_string())?,
            &new_favor.root_children,
        )?;
        let result = json!({"ok":true,"createdIds":[],"selectedIds":undo.selected,"revision":after_revision});
        drag_state.undo = None;
        remember(
            &mut drag_state,
            &operation_id,
            &operation_id,
            result.clone(),
        );
        save_state(&db, &drag_state)?;
        commit_with_roots(transaction, &paths, &favor, &new_favor, &operation_id)?;
        drop(db);
        self.inner.lock().extras.favor = new_favor;
        self.emit_favorites_changed(
            &undo
                .before
                .iter()
                .map(|item| item.id.clone())
                .collect::<Vec<_>>(),
            &undo.created,
        );
        Ok(result)
    }
}

fn commit_with_roots(
    transaction: rusqlite::Transaction<'_>,
    paths: &AppPaths,
    before: &crate::extra_settings::FavorSettings,
    after: &crate::extra_settings::FavorSettings,
    operation_id: &str,
) -> Result<(), String> {
    if before.root_children == after.root_children {
        return transaction.commit().map_err(|e| e.to_string());
    }
    let config_path = paths.configs.join("FavorConfig.json");
    let current: crate::extra_settings::FavorSettings = crate::config_compat::load(&config_path)?;
    if current.root_children != before.root_children {
        return Err(CONFLICT.into());
    }
    let journal_path = paths.data_root.join(JOURNAL_FILE);
    crate::config_file::save(
        &journal_path,
        &RootJournal {
            operation_id: operation_id.into(),
            before: before.root_children.clone(),
            after: after.root_children.clone(),
        },
    )?;
    let mut current = current;
    current.root_children = after.root_children.clone();
    if let Err(error) = crate::config_file::save(&config_path, &current) {
        // config_file preserves its previous bytes on failure. The uncommitted
        // journal is recovered on the next request/start, never silently ignored.
        return Err(error);
    }
    if let Err(error) = transaction.commit() {
        let restore = (|| {
            let mut latest: crate::extra_settings::FavorSettings =
                crate::config_compat::load(&config_path)?;
            if latest.root_children != after.root_children {
                return Err("顶层在提交失败后被另一程序修改，恢复记录已保留。".into());
            }
            latest.root_children = before.root_children.clone();
            crate::config_file::save(&config_path, &latest)
        })();
        return match restore {
            Ok(()) => {
                let _ = std::fs::remove_file(journal_path);
                Err(error.to_string())
            }
            Err(restore) => Err(format!("{error}; 顶层配置等待恢复：{restore}")),
        };
    }
    // A cleanup failure leaves a committed journal; replay is idempotent.
    if let Err(error) = std::fs::remove_file(journal_path) {
        tracing::warn!(%error, "favorite drag journal cleanup pending");
    }
    Ok(())
}

#[cfg(test)]
#[path = "favorites_drag_tests.rs"]
mod tests;
