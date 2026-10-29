//! Folder fixture for testing favor sync rules and `cea.favor` / `cea.favor.{id}` slots.
//! This is not Quicker cloud storage. Production HostState sync entry points are
//! disabled with the C# / SPA product gate; only tests call this folder transport.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::model::FavorRecord;

pub const FAVOR_SLOT: &str = "cea.favor";
pub const FAVOR_ITEM_PREFIX: &str = "cea.favor.";
pub const TOMBSTONE_SCHEMA: &str = "FavorItemTombstone/1";
pub const FAVOR_ITEM_SCHEMA: &str = "FavorItem/2";
pub const FAVOR_AGGREGATE_SCHEMA: &str = "favor-v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloudSyncStatusKind {
    UpToDate,
    CloudAhead,
    LocalAhead,
    Conflict,
    Unavailable,
}

impl CloudSyncStatusKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UpToDate => "UpToDate",
            Self::CloudAhead => "CloudAhead",
            Self::LocalAhead => "LocalAhead",
            Self::Conflict => "Conflict",
            Self::Unavailable => "Unavailable",
        }
    }

    fn message(self, local_count: i32, local_tombstone: bool, cloud_tombstone: bool) -> &'static str {
        match self {
            Self::UpToDate => "已同步",
            Self::CloudAhead if local_count > 0 && cloud_tombstone => "云端已删除，可拉取同步",
            Self::CloudAhead => "云端较新，可拉取",
            Self::LocalAhead if local_tombstone && local_count == 0 => "本地已删除，可上传",
            Self::LocalAhead => "本地较新，可上传",
            Self::Conflict => "本地与云端均有变更，请手动选择",
            Self::Unavailable => "云端尚无此同步项",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudSyncSlot {
    pub rev: i32,
    pub hash: String,
    pub updated_at_utc: DateTime<Utc>,
    pub device_id: String,
    pub item_count: i32,
    pub schema: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CloudSyncManifest {
    pub v: i32,
    pub slots: HashMap<String, CloudSyncSlot>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CloudSyncSlotLocalMeta {
    pub rev: i32,
    pub hash: String,
    pub last_check_at_utc: Option<DateTime<Utc>>,
    pub last_pull_at_utc: Option<DateTime<Utc>>,
    pub last_push_at_utc: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CloudSyncLocalStore {
    pub device_id: String,
    pub last_check_all_at_utc: Option<DateTime<Utc>>,
    pub slots: HashMap<String, CloudSyncSlotLocalMeta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FavorTombstone {
    #[serde(default = "tombstone_v")]
    v: i32,
    deleted: bool,
    id: String,
    #[serde(default)]
    deleted_at_utc: Option<DateTime<Utc>>,
}

fn tombstone_v() -> i32 {
    1
}

#[derive(Debug, Clone, Default)]
pub struct SessionReport {
    pub pulled_deletions: Vec<Value>,
    pub pulled_items: Vec<Value>,
    pub pushed_deletions: Vec<Value>,
    pub pushed_items: Vec<Value>,
}

impl SessionReport {
    fn entry(kind: &str, slot_id: &str, label: &str) -> Value {
        json!({
            "kind": kind,
            "slotId": slot_id,
            "favorId": parse_favor_item_id(slot_id).unwrap_or_default(),
            "label": label,
        })
    }

    pub fn has_changes(&self) -> bool {
        !self.pulled_deletions.is_empty()
            || !self.pulled_items.is_empty()
            || !self.pushed_deletions.is_empty()
            || !self.pushed_items.is_empty()
    }

    pub fn summary(&self) -> String {
        if !self.has_changes() {
            return "本次同步无收藏变更".into();
        }
        let mut lines = Vec::new();
        append_section(&mut lines, "从云端删除", &self.pulled_deletions);
        append_section(&mut lines, "从云端合并", &self.pulled_items);
        append_section(&mut lines, "上传删除到云端", &self.pushed_deletions);
        append_section(&mut lines, "上传到云端", &self.pushed_items);
        lines.join("\n")
    }

    pub fn to_json(&self) -> Value {
        json!({
            "pulledDeletions": self.pulled_deletions,
            "pulledItems": self.pulled_items,
            "pushedDeletions": self.pushed_deletions,
            "pushedItems": self.pushed_items,
            "summary": self.summary(),
            "hasChanges": self.has_changes(),
        })
    }
}

fn append_section(lines: &mut Vec<String>, heading: &str, entries: &[Value]) {
    if entries.is_empty() {
        return;
    }
    lines.push(format!("{heading} {} 条：", entries.len()));
    for entry in entries {
        if let Some(label) = entry.get("label").and_then(|v| v.as_str()) {
            lines.push(format!("  {label}"));
        }
    }
}

#[derive(Debug, Clone)]
pub enum FavorApply {
    ReplaceAll(Vec<FavorRecord>),
    Upsert(FavorRecord),
    Delete(String),
}

pub fn try_apply_slot_update(
    manifest: &mut CloudSyncManifest,
    slot_id: &str,
    update: CloudSyncSlot,
    base_cloud_rev: i32,
    force: bool,
) -> Result<(), String> {
    let current_rev = manifest.slots.get(slot_id).map(|s| s.rev).unwrap_or(0);
    if !force && current_rev > base_cloud_rev {
        return Err(format!(
            "云端已被其他设备更新 (cloudRev={current_rev}, baseRev={base_cloud_rev})"
        ));
    }
    if update.rev <= current_rev {
        return Err(format!(
            "版本未前进 (updateRev={}, cloudRev={current_rev})",
            update.rev
        ));
    }
    manifest.slots.insert(slot_id.to_string(), update);
    Ok(())
}

pub fn is_favor_aggregate(slot_id: &str) -> bool {
    slot_id == FAVOR_SLOT
}

pub fn is_favor_item_slot(slot_id: &str) -> bool {
    slot_id.starts_with(FAVOR_ITEM_PREFIX) && slot_id.len() > FAVOR_ITEM_PREFIX.len()
}

pub fn favor_item_slot(favor_id: &str) -> String {
    format!("{FAVOR_ITEM_PREFIX}{}", favor_id.trim())
}

pub fn parse_favor_item_id(slot_id: &str) -> Option<String> {
    slot_id
        .strip_prefix(FAVOR_ITEM_PREFIX)
        .filter(|id| !id.is_empty())
        .map(|id| id.to_string())
}

pub fn compare(
    slot_id: &str,
    local: Option<&CloudSyncSlotLocalMeta>,
    cloud: Option<&CloudSyncSlot>,
    local_count: i32,
    current_hash: &str,
    local_tombstone: bool,
    cloud_tombstone: bool,
) -> Value {
    let local = local.cloned().unwrap_or_default();
    let pending = has_pending_local_change(&local, current_hash, cloud);
    let (kind, cloud_rev, cloud_count, cloud_updated) = if let Some(cloud) = cloud {
        let deletion = resolve_deletion_status(
            &local,
            cloud,
            local_count,
            pending,
            local_tombstone,
            cloud_tombstone,
        );
        let kind = deletion.unwrap_or_else(|| resolve_status(&local, cloud, pending, current_hash));
        (
            kind,
            cloud.rev,
            cloud.item_count,
            Some(cloud.updated_at_utc),
        )
    } else {
        let has_local = pending || local_count > 0 || local.rev > 0;
        let kind = if has_local {
            CloudSyncStatusKind::LocalAhead
        } else {
            CloudSyncStatusKind::UpToDate
        };
        return json!({
            "slotId": slot_id,
            "status": kind.as_str(),
            "localRev": local.rev,
            "cloudRev": 0,
            "localCount": local_count,
            "cloudCount": 0,
            "cloudUpdatedAtUtc": Value::Null,
            "message": if has_local { "云端尚无此同步项，可上传" } else { "云端尚无此同步项" },
        });
    };
    json!({
        "slotId": slot_id,
        "status": kind.as_str(),
        "localRev": local.rev,
        "cloudRev": cloud_rev,
        "localCount": local_count,
        "cloudCount": cloud_count,
        "cloudUpdatedAtUtc": cloud_updated,
        "message": kind.message(local_count, local_tombstone, cloud_tombstone),
    })
}

fn has_pending_local_change(
    local: &CloudSyncSlotLocalMeta,
    current_hash: &str,
    cloud: Option<&CloudSyncSlot>,
) -> bool {
    if current_hash.is_empty() {
        return false;
    }
    if fingerprints_equal(current_hash, &local.hash) {
        return false;
    }
    if let Some(cloud) = cloud {
        if fingerprints_equal(current_hash, &cloud.hash) {
            return false;
        }
    }
    true
}

fn has_prior_sync(local: &CloudSyncSlotLocalMeta) -> bool {
    local.rev > 0 || !local.hash.is_empty() || local.last_pull_at_utc.is_some() || local.last_push_at_utc.is_some()
}

fn resolve_deletion_status(
    local: &CloudSyncSlotLocalMeta,
    cloud: &CloudSyncSlot,
    local_count: i32,
    pending: bool,
    local_tombstone: bool,
    cloud_tombstone: bool,
) -> Option<CloudSyncStatusKind> {
    if cloud_tombstone && local_count == 0 && !local_tombstone && !has_prior_sync(local) {
        return Some(CloudSyncStatusKind::UpToDate);
    }
    if local_tombstone && local_count == 0 {
        if cloud_tombstone {
            return Some(if pending {
                CloudSyncStatusKind::LocalAhead
            } else {
                CloudSyncStatusKind::UpToDate
            });
        }
        if has_prior_sync(local) {
            return Some(CloudSyncStatusKind::LocalAhead);
        }
        return Some(if cloud.item_count > 0 {
            CloudSyncStatusKind::CloudAhead
        } else {
            CloudSyncStatusKind::UpToDate
        });
    }
    if !local_tombstone && local_count > 0 && cloud_tombstone {
        return Some(if cloud.rev >= local.rev {
            CloudSyncStatusKind::CloudAhead
        } else {
            CloudSyncStatusKind::LocalAhead
        });
    }
    None
}

fn resolve_status(
    local: &CloudSyncSlotLocalMeta,
    cloud: &CloudSyncSlot,
    pending: bool,
    current_hash: &str,
) -> CloudSyncStatusKind {
    if cloud.rev > local.rev {
        if !pending && local.rev == 0 && local.hash.is_empty() && !current_hash.is_empty() {
            return CloudSyncStatusKind::UpToDate;
        }
        return if pending {
            CloudSyncStatusKind::Conflict
        } else {
            CloudSyncStatusKind::CloudAhead
        };
    }
    if cloud.rev < local.rev {
        if fingerprints_equal(current_hash, &cloud.hash) {
            return CloudSyncStatusKind::UpToDate;
        }
        return CloudSyncStatusKind::LocalAhead;
    }
    if fingerprints_equal(current_hash, &cloud.hash) {
        return CloudSyncStatusKind::UpToDate;
    }
    if fingerprints_equal(&cloud.hash, &local.hash) {
        return if pending {
            CloudSyncStatusKind::LocalAhead
        } else {
            CloudSyncStatusKind::UpToDate
        };
    }
    if pending {
        CloudSyncStatusKind::Conflict
    } else {
        CloudSyncStatusKind::UpToDate
    }
}

fn fingerprints_equal(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

#[derive(Debug, Clone)]
pub struct CloudFolder {
    root: PathBuf,
    device_id: String,
}

impl CloudFolder {
    pub fn open(data_root: &Path, device_id: impl Into<String>) -> Self {
        let root = data_root.join("cloud");
        let _ = std::fs::create_dir_all(root.join("slots"));
        Self {
            root,
            device_id: device_id.into(),
        }
    }

    pub fn status(&self, local_count: i32, local_hash: &str) -> Value {
        let manifest = self.load_manifest();
        let local = self.load_local();
        let cloud = manifest.slots.get(FAVOR_SLOT);
        let local_meta = local.slots.get(FAVOR_SLOT);
        let dto = compare(
            FAVOR_SLOT,
            local_meta,
            cloud,
            local_count,
            local_hash,
            false,
            self.slot_is_tombstone(FAVOR_SLOT),
        );
        json!({
            "rpcConnected": false,
            "hint": "Tauri isolated folder transport (not Quicker cloud RPC).",
            "error": Value::Null,
            "cloudSyncHostEnabled": true,
            "cloudSyncDisabledReason": Value::Null,
            "autoSyncEnabled": false,
            "lastAutoSyncUtc": local.last_check_all_at_utc,
            "backend": "tauri-folder",
            "status": dto,
        })
    }

    pub fn check(&self, slot: Option<&str>, all: bool, favors: &[FavorRecord]) -> Value {
        let manifest = self.load_manifest();
        let local = self.load_local();
        let slots = self.resolve_slot_ids(slot, all, favors, &manifest, &local);
        let mut items = Vec::new();
        for id in &slots {
            items.push(self.check_slot(id, favors, &manifest, &local));
        }
        json!({
            "ok": true,
            "slots": items,
            "status": items.first().cloned().unwrap_or_else(|| compare(FAVOR_SLOT, None, None, 0, "", false, false)),
        })
    }

    pub fn list_slots(&self, favors: &[FavorRecord]) -> Value {
        let manifest = self.load_manifest();
        let local = self.load_local();
        let ids = self.resolve_slot_ids(None, true, favors, &manifest, &local);
        json!({
            "ok": true,
            "slots": ids,
            "message": format!("已注册同步项:\n{}", ids.join("\n")),
        })
    }

    pub fn probe(&self) -> Value {
        let manifest = self.load_manifest();
        let path = self.manifest_path();
        let length = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        json!({
            "ok": true,
            "rpcConnected": false,
            "folderReady": true,
            "backend": "tauri-folder",
            "cloudSyncSupported": true,
            "manifestFound": path.exists(),
            "manifestLength": length,
            "manifestKey": "folder:manifest.json",
            "hint": "Tauri isolated folder transport; Quicker cloud RPC stays on C# ClipHost.",
            "registeredSlots": manifest.slots.keys().cloned().collect::<Vec<_>>(),
        })
    }

    pub fn pull(
        &self,
        slot: Option<&str>,
        all: bool,
        force: bool,
        favors: &[FavorRecord],
    ) -> Result<(Value, Vec<FavorApply>, SessionReport), String> {
        let manifest = self.load_manifest();
        let mut local = self.load_local();
        local.device_id = self.device_id.clone();
        local.last_check_all_at_utc = Some(Utc::now());
        let slots = self.resolve_slot_ids(slot, all, favors, &manifest, &local);
        let mut applies = Vec::new();
        let mut report = SessionReport::default();
        let mut last_status = Value::Null;
        for id in &slots {
            let dto = self.check_slot(id, favors, &manifest, &local);
            last_status = dto.clone();
            let kind = last_status
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if kind != "CloudAhead" && !(force && (kind == "Conflict" || kind == "LocalAhead")) {
                continue;
            }
            if kind == "Conflict" && !force {
                return Err(format!("本地与云端均有变更，请使用 --force 拉取 {id}"));
            }
            if is_favor_aggregate(id) {
                let items = self.load_slot_items()?;
                let hash = hash_favors(&items);
                applies.push(FavorApply::ReplaceAll(items.clone()));
                report.pulled_items.push(SessionReport::entry(
                    "pulledItem",
                    id,
                    &format!("cea.favor ({})", items.len()),
                ));
                self.touch_local_meta(&mut local, id, manifest.slots.get(id).map(|s| s.rev).unwrap_or(0), &hash, true);
            } else if let Some(favor_id) = parse_favor_item_id(id) {
                if self.slot_is_tombstone(id) {
                    applies.push(FavorApply::Delete(favor_id.clone()));
                    report
                        .pulled_deletions
                        .push(SessionReport::entry("pulledDelete", id, &favor_id));
                } else if let Some(item) = self.load_item_record(id)? {
                    let label = item.name.clone();
                    applies.push(FavorApply::Upsert(item));
                    report
                        .pulled_items
                        .push(SessionReport::entry("pulledItem", id, &label));
                }
                if let Some(cloud) = manifest.slots.get(id) {
                    self.touch_local_meta(&mut local, id, cloud.rev, &cloud.hash, true);
                }
            }
        }
        self.save_local(&local)?;
        Ok((
            json!({
                "slotId": slot.unwrap_or(FAVOR_SLOT),
                "success": true,
                "status": last_status,
                "changeReport": report.to_json(),
                "message": report.summary(),
            }),
            applies,
            report,
        ))
    }

    pub fn push(
        &self,
        slot: Option<&str>,
        all: bool,
        force: bool,
        favors: &[FavorRecord],
    ) -> Result<(Value, SessionReport), String> {
        let mut manifest = self.load_manifest();
        let mut local = self.load_local();
        local.device_id = self.device_id.clone();
        local.last_check_all_at_utc = Some(Utc::now());
        let slots = self.resolve_slot_ids(slot, all, favors, &manifest, &local);
        let mut report = SessionReport::default();
        let mut last_status = Value::Null;
        let favor_by_id: HashMap<String, &FavorRecord> =
            favors.iter().map(|item| (item.id.clone(), item)).collect();
        for id in &slots {
            let dto = self.check_slot(id, favors, &manifest, &local);
            last_status = dto.clone();
            let kind = last_status
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if kind != "LocalAhead" && kind != "Conflict" && kind != "Unavailable" {
                if kind == "UpToDate" {
                    continue;
                }
                if kind == "CloudAhead" && !force {
                    return Err(format!("云端较新，请先拉取或使用 --force 上传 {id}"));
                }
                if kind == "CloudAhead" && force {
                    // continue to overwrite
                } else {
                    continue;
                }
            }
            if kind == "Conflict" && !force {
                return Err(format!("本地与云端均有变更，请使用 --force 上传 {id}"));
            }
            let cloud_rev = manifest.slots.get(id).map(|s| s.rev).unwrap_or(0);
            if is_favor_aggregate(id) {
                let hash = hash_favors(favors);
                self.save_slot_items(favors)?;
                let next = CloudSyncSlot {
                    rev: local.slots.get(id).map(|m| m.rev).unwrap_or(0).max(cloud_rev) + 1,
                    hash: hash.clone(),
                    updated_at_utc: Utc::now(),
                    device_id: self.device_id.clone(),
                    item_count: favors.len() as i32,
                    schema: FAVOR_AGGREGATE_SCHEMA.into(),
                };
                try_apply_slot_update(&mut manifest, id, next.clone(), cloud_rev, force || cloud_rev == 0)?;
                self.touch_local_meta(&mut local, id, next.rev, &hash, false);
                report.pushed_items.push(SessionReport::entry(
                    "pushedItem",
                    id,
                    &format!("cea.favor ({})", favors.len()),
                ));
            } else if let Some(favor_id) = parse_favor_item_id(id) {
                if let Some(item) = favor_by_id.get(&favor_id) {
                    let payload = serde_json::to_string_pretty(item).map_err(|e| e.to_string())?;
                    let hash = hash_bytes(payload.as_bytes());
                    self.write_slot_text(id, &payload)?;
                    let next = CloudSyncSlot {
                        rev: local.slots.get(id).map(|m| m.rev).unwrap_or(0).max(cloud_rev) + 1,
                        hash: hash.clone(),
                        updated_at_utc: Utc::now(),
                        device_id: self.device_id.clone(),
                        item_count: 1,
                        schema: FAVOR_ITEM_SCHEMA.into(),
                    };
                    try_apply_slot_update(&mut manifest, id, next.clone(), cloud_rev, force || cloud_rev == 0)?;
                    self.touch_local_meta(&mut local, id, next.rev, &hash, false);
                    report
                        .pushed_items
                        .push(SessionReport::entry("pushedItem", id, &item.name));
                } else {
                    let tombstone = FavorTombstone {
                        v: 1,
                        deleted: true,
                        id: favor_id.clone(),
                        deleted_at_utc: Some(Utc::now()),
                    };
                    let payload = serde_json::to_string_pretty(&tombstone).map_err(|e| e.to_string())?;
                    let hash = hash_bytes(payload.as_bytes());
                    self.write_slot_text(id, &payload)?;
                    let next = CloudSyncSlot {
                        rev: local.slots.get(id).map(|m| m.rev).unwrap_or(0).max(cloud_rev) + 1,
                        hash: hash.clone(),
                        updated_at_utc: Utc::now(),
                        device_id: self.device_id.clone(),
                        item_count: 0,
                        schema: TOMBSTONE_SCHEMA.into(),
                    };
                    try_apply_slot_update(&mut manifest, id, next.clone(), cloud_rev, force || cloud_rev == 0)?;
                    self.touch_local_meta(&mut local, id, next.rev, &hash, false);
                    report
                        .pushed_deletions
                        .push(SessionReport::entry("pushedDelete", id, &favor_id));
                }
            }
        }
        self.save_manifest(&manifest)?;
        self.save_local(&local)?;
        Ok((
            json!({
                "slotId": slot.unwrap_or(FAVOR_SLOT),
                "success": true,
                "status": last_status,
                "changeReport": report.to_json(),
                "message": report.summary(),
            }),
            report,
        ))
    }

    pub fn sync_favors(&self, favors: &[FavorRecord]) -> Result<Value, String> {
        let check = self.check(Some(FAVOR_SLOT), false, favors);
        let kind = check["status"]["status"].as_str().unwrap_or("");
        if kind == "CloudAhead" || kind == "Conflict" {
            let _ = self.pull(Some(FAVOR_SLOT), false, kind == "Conflict", favors)?;
        }
        if kind == "LocalAhead" || kind == "Unavailable" || kind == "UpToDate" || kind.is_empty() {
            let _ = self.push(Some(FAVOR_SLOT), false, true, favors)?;
        }
        Ok(self.status(favors.len() as i32, &hash_favors(favors)))
    }

    pub fn cleanup(&self) -> Result<Value, String> {
        let mut local = self.load_local();
        let mut manifest = self.load_manifest();
        let before = local.slots.len() as i32;
        let mut orphans = 0i32;
        let mut deletions = 0i32;
        local.slots.retain(|id, _| {
            if is_favor_aggregate(id) || is_favor_item_slot(id) {
                true
            } else {
                orphans += 1;
                false
            }
        });
        let live: Vec<String> = manifest.slots.keys().cloned().collect();
        for id in live {
            if !is_favor_item_slot(&id) || !self.slot_is_tombstone(&id) {
                continue;
            }
            let cloud_hash = manifest.slots.get(&id).map(|slot| slot.hash.clone());
            let local_hash = local.slots.get(&id).map(|meta| meta.hash.clone());
            if let (Some(local_hash), Some(cloud_hash)) = (local_hash, cloud_hash) {
                if fingerprints_equal(&local_hash, &cloud_hash) {
                    manifest.slots.remove(&id);
                    let _ = std::fs::remove_file(self.slot_path(&id));
                    local.slots.remove(&id);
                    deletions += 1;
                }
            }
        }
        self.save_manifest(&manifest)?;
        self.save_local(&local)?;
        let purged = (before - local.slots.len() as i32).max(0);
        Ok(json!({
            "slotId": FAVOR_SLOT,
            "success": true,
            "legacyPurged": false,
            "deletionsPushed": deletions,
            "orphansRemoved": orphans,
            "orphanDataRemoved": 0,
            "orphansRemoveFailed": 0,
            "liveRefreshed": 0,
            "localMetaPurged": purged,
            "message": "Cleaned isolated Tauri cloud folder meta and synced tombstones.",
        }))
    }

    pub fn purge_legacy_aggregate(&self) -> Result<Value, String> {
        let mut manifest = self.load_manifest();
        let mut local = self.load_local();
        let had_items = manifest.slots.keys().any(|id| is_favor_item_slot(id));
        let mut purged = false;
        if had_items && manifest.slots.remove(FAVOR_SLOT).is_some() {
            let _ = std::fs::remove_file(self.slot_path(FAVOR_SLOT));
            local.slots.remove(FAVOR_SLOT);
            purged = true;
        }
        self.save_manifest(&manifest)?;
        self.save_local(&local)?;
        Ok(json!({
            "slotId": FAVOR_SLOT,
            "success": true,
            "legacyPurged": purged,
            "message": if purged { "Removed legacy cea.favor aggregate after per-id slots exist." } else { "No legacy aggregate to purge." },
        }))
    }

    pub fn purge_orphan_slots(&self, refs: &[String]) -> Result<Value, String> {
        if refs.is_empty() {
            return Err("请指定 cea.favor.{id} 或 cea/states/sync/data/cea.favor.{id}".into());
        }
        let mut manifest = self.load_manifest();
        let mut local = self.load_local();
        let mut removed = 0i32;
        for raw in refs {
            let id = normalize_slot_ref(raw);
            if !is_favor_item_slot(&id) {
                continue;
            }
            if manifest.slots.remove(&id).is_some() {
                removed += 1;
            }
            local.slots.remove(&id);
            let _ = std::fs::remove_file(self.slot_path(&id));
        }
        self.save_manifest(&manifest)?;
        self.save_local(&local)?;
        Ok(json!({
            "slotId": FAVOR_SLOT,
            "success": true,
            "orphanDataRemoved": removed,
            "message": format!("Removed {removed} orphan favor data slot(s)."),
        }))
    }

    pub fn purge(&self) -> Result<Value, String> {
        let manifest = self.load_manifest();
        let local = self.load_local();
        let slots = manifest.slots.len() as i32;
        let local_meta = local.slots.len() as i32;
        let _ = std::fs::remove_dir_all(self.root.join("slots"));
        let _ = std::fs::remove_file(self.manifest_path());
        let _ = std::fs::create_dir_all(self.root.join("slots"));
        self.save_manifest(&CloudSyncManifest { v: 1, slots: HashMap::new() })?;
        let _ = std::fs::remove_file(self.root.join("local.json"));
        Ok(json!({
            "slotId": FAVOR_SLOT,
            "success": true,
            "manifestSlotsRemoved": slots,
            "dataKeysDeleted": slots,
            "dataKeysDeleteFailed": 0,
            "localMetaSlotsRemoved": local_meta,
            "message": "Purged isolated Tauri cloud folder.",
        }))
    }

    pub fn load_slot_items(&self) -> Result<Vec<FavorRecord>, String> {
        let path = self.slot_path(FAVOR_SLOT);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        serde_json::from_str(&text).map_err(|e| e.to_string())
    }

    fn save_slot_items(&self, favors: &[FavorRecord]) -> Result<(), String> {
        let path = self.slot_path(FAVOR_SLOT);
        std::fs::write(path, serde_json::to_string_pretty(favors).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())
    }

    fn check_slot(
        &self,
        slot_id: &str,
        favors: &[FavorRecord],
        manifest: &CloudSyncManifest,
        local: &CloudSyncLocalStore,
    ) -> Value {
        if is_favor_aggregate(slot_id) {
            let meta = local.slots.get(slot_id).cloned().unwrap_or_default();
            let current_hash = if favors.is_empty() && !has_prior_sync(&meta) {
                String::new()
            } else {
                hash_favors(favors)
            };
            return compare(
                slot_id,
                local.slots.get(slot_id),
                manifest.slots.get(slot_id),
                favors.len() as i32,
                &current_hash,
                false,
                self.slot_is_tombstone(slot_id),
            );
        }
        let favor_id = parse_favor_item_id(slot_id).unwrap_or_default();
        let item = favors.iter().find(|f| f.id == favor_id);
        let local_tombstone = item.is_none() && has_prior_sync(local.slots.get(slot_id).unwrap_or(&CloudSyncSlotLocalMeta::default()));
        let current_hash = if let Some(item) = item {
            hash_bytes(serde_json::to_string(item).unwrap_or_default().as_bytes())
        } else if local_tombstone {
            hash_bytes(
                serde_json::to_string(&FavorTombstone {
                    v: 1,
                    deleted: true,
                    id: favor_id.clone(),
                    deleted_at_utc: None,
                })
                .unwrap_or_default()
                .as_bytes(),
            )
        } else {
            String::new()
        };
        compare(
            slot_id,
            local.slots.get(slot_id),
            manifest.slots.get(slot_id),
            i32::from(item.is_some()),
            &current_hash,
            local_tombstone,
            self.slot_is_tombstone(slot_id),
        )
    }

    fn resolve_slot_ids(
        &self,
        slot: Option<&str>,
        all: bool,
        favors: &[FavorRecord],
        manifest: &CloudSyncManifest,
        local: &CloudSyncLocalStore,
    ) -> Vec<String> {
        if all {
            let mut ids = Vec::new();
            ids.push(FAVOR_SLOT.to_string());
            let mut seen = HashSet::from([FAVOR_SLOT.to_string()]);
            for item in favors {
                let id = favor_item_slot(&item.id);
                if seen.insert(id.clone()) {
                    ids.push(id);
                }
            }
            for key in manifest.slots.keys().chain(local.slots.keys()) {
                if seen.insert(key.clone()) {
                    ids.push(key.clone());
                }
            }
            return ids;
        }
        vec![normalize_slot_ref(slot.unwrap_or(FAVOR_SLOT))]
    }

    fn touch_local_meta(
        &self,
        local: &mut CloudSyncLocalStore,
        slot_id: &str,
        rev: i32,
        hash: &str,
        pull: bool,
    ) {
        let now = Some(Utc::now());
        local.slots.insert(
            slot_id.into(),
            CloudSyncSlotLocalMeta {
                rev,
                hash: hash.to_string(),
                last_check_at_utc: now,
                last_pull_at_utc: if pull { now } else { local.slots.get(slot_id).and_then(|m| m.last_pull_at_utc) },
                last_push_at_utc: if pull { local.slots.get(slot_id).and_then(|m| m.last_push_at_utc) } else { now },
            },
        );
    }

    fn slot_is_tombstone(&self, slot_id: &str) -> bool {
        let Ok(text) = std::fs::read_to_string(self.slot_path(slot_id)) else {
            return false;
        };
        serde_json::from_str::<FavorTombstone>(&text)
            .map(|t| t.deleted)
            .unwrap_or(false)
    }

    fn load_item_record(&self, slot_id: &str) -> Result<Option<FavorRecord>, String> {
        let path = self.slot_path(slot_id);
        if !path.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        if serde_json::from_str::<FavorTombstone>(&text)
            .map(|t| t.deleted)
            .unwrap_or(false)
        {
            return Ok(None);
        }
        Ok(Some(serde_json::from_str(&text).map_err(|e| e.to_string())?))
    }

    fn write_slot_text(&self, slot_id: &str, text: &str) -> Result<(), String> {
        std::fs::write(self.slot_path(slot_id), text).map_err(|e| e.to_string())
    }

    fn slot_path(&self, slot_id: &str) -> PathBuf {
        let name = slot_id.replace(['/', '\\', ':'], "_");
        self.root.join("slots").join(format!("{name}.json"))
    }

    fn load_manifest(&self) -> CloudSyncManifest {
        std::fs::read_to_string(self.manifest_path())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(CloudSyncManifest { v: 1, slots: HashMap::new() })
    }

    fn save_manifest(&self, manifest: &CloudSyncManifest) -> Result<(), String> {
        std::fs::write(
            self.manifest_path(),
            serde_json::to_string_pretty(manifest).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())
    }

    fn load_local(&self) -> CloudSyncLocalStore {
        std::fs::read_to_string(self.root.join("local.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    fn save_local(&self, local: &CloudSyncLocalStore) -> Result<(), String> {
        std::fs::write(
            self.root.join("local.json"),
            serde_json::to_string_pretty(local).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())
    }

    fn manifest_path(&self) -> PathBuf {
        self.root.join("manifest.json")
    }
}

fn normalize_slot_ref(raw: &str) -> String {
    let trimmed = raw.trim();
    if let Some(id) = trimmed.strip_prefix("cea/states/sync/data/") {
        return id.to_string();
    }
    if is_favor_item_slot(trimmed) || is_favor_aggregate(trimmed) {
        return trimmed.to_string();
    }
    if trimmed.starts_with("cea.") {
        return trimmed.to_string();
    }
    favor_item_slot(trimmed)
}

pub fn hash_favors(favors: &[FavorRecord]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"cea.favor:");
    for item in favors {
        hasher.update(item.id.as_bytes());
        hasher.update(item.name.as_bytes());
        hasher.update(item.body_text.as_deref().unwrap_or("").as_bytes());
        hasher.update(&[if item.is_folder { 1 } else { 0 }]);
    }
    hasher.finalize().to_hex().to_string()
}

fn hash_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn sample(id: &str, name: &str) -> FavorRecord {
        FavorRecord {
            storage_revision: None,
            id: id.into(),
            parent_id: None,
            is_folder: false,
            name: name.into(),
            kind: crate::model::ClipKind::TextClipItem,
            body_text: Some("hi".into()),
            file_paths: Vec::new(),
            image_path: None,
            children: Vec::new(),
            create_time: Utc::now(),
            update_time: Utc::now(),
            source_clip_id: None,
            html_data: None,
            html_source_url: None,
            qq_rich_text: None,
            source_process_name: String::new(),
            sub_index: 0,
            icon: String::new(),
            execute_param: String::new(),
        }
    }

    #[test]
    fn compare_cloud_ahead_when_cloud_rev_higher() {
        let local = CloudSyncSlotLocalMeta {
            rev: 1,
            hash: "aa".into(),
            ..Default::default()
        };
        let cloud = CloudSyncSlot {
            rev: 2,
            hash: "bb".into(),
            updated_at_utc: Utc::now(),
            device_id: "other".into(),
            item_count: 3,
            schema: FAVOR_AGGREGATE_SCHEMA.into(),
        };
        let dto = compare(FAVOR_SLOT, Some(&local), Some(&cloud), 2, "aa", false, false);
        assert_eq!(dto["status"], "CloudAhead");
        assert_eq!(dto["cloudCount"], 3);
    }

    #[test]
    fn compare_conflict_when_cloud_ahead_and_local_changed() {
        let local = CloudSyncSlotLocalMeta {
            rev: 1,
            hash: "local".into(),
            ..Default::default()
        };
        let cloud = CloudSyncSlot {
            rev: 2,
            hash: "cloud".into(),
            updated_at_utc: Utc::now(),
            device_id: "other".into(),
            item_count: 2,
            schema: FAVOR_AGGREGATE_SCHEMA.into(),
        };
        let dto = compare(FAVOR_SLOT, Some(&local), Some(&cloud), 2, "current", false, false);
        assert_eq!(dto["status"], "Conflict");
    }

    #[test]
    fn reject_stale_rev_and_accept_force() {
        let mut manifest = CloudSyncManifest { v: 1, slots: HashMap::new() };
        let slot = CloudSyncSlot {
            rev: 2,
            hash: "a".into(),
            updated_at_utc: Utc::now(),
            device_id: "d".into(),
            item_count: 1,
            schema: "favor-v1".into(),
        };
        try_apply_slot_update(&mut manifest, FAVOR_SLOT, slot.clone(), 0, false).unwrap();
        let stale = CloudSyncSlot { rev: 2, ..slot.clone() };
        assert!(try_apply_slot_update(&mut manifest, FAVOR_SLOT, stale, 2, false).is_err());
        let next = CloudSyncSlot { rev: 3, hash: "b".into(), ..slot };
        try_apply_slot_update(&mut manifest, FAVOR_SLOT, next, 1, true).unwrap();
        assert_eq!(manifest.slots[FAVOR_SLOT].rev, 3);
    }

    #[test]
    fn folder_sync_pushes_favor_slot() {
        let dir = tempdir().unwrap();
        let folder = CloudFolder::open(dir.path(), "dev1");
        let status = folder.sync_favors(&[sample("f1", "note")]).unwrap();
        assert_eq!(status["status"]["slotId"], FAVOR_SLOT);
        assert_eq!(status["status"]["localCount"], 1);
        assert_eq!(status["status"]["status"], "UpToDate");
    }

    #[test]
    fn empty_local_with_cloud_slot_is_cloud_ahead() {
        let dir = tempdir().unwrap();
        let folder = CloudFolder::open(dir.path(), "dev1");
        let mut manifest = CloudSyncManifest { v: 1, slots: HashMap::new() };
        manifest.slots.insert(
            FAVOR_SLOT.into(),
            CloudSyncSlot {
                rev: 2,
                hash: "remote-hash".into(),
                updated_at_utc: Utc::now(),
                device_id: "other".into(),
                item_count: 1,
                schema: FAVOR_AGGREGATE_SCHEMA.into(),
            },
        );
        folder.save_manifest(&manifest).unwrap();
        let checked = folder.check(Some(FAVOR_SLOT), false, &[]);
        assert_eq!(checked["status"]["status"], "CloudAhead");
    }

    #[test]
    fn check_without_cloud_is_local_ahead() {
        let dir = tempdir().unwrap();
        let folder = CloudFolder::open(dir.path(), "dev1");
        let checked = folder.check(Some(FAVOR_SLOT), false, &[sample("a", "local")]);
        assert_eq!(checked["status"]["status"], "LocalAhead");
    }

    #[test]
    fn pull_item_tombstone_emits_delete() {
        let dir = tempdir().unwrap();
        let folder = CloudFolder::open(dir.path(), "dev1");
        let slot = favor_item_slot("gone");
        let tomb = serde_json::json!({ "v": 1, "deleted": true, "id": "gone" });
        std::fs::write(folder.slot_path(&slot), tomb.to_string()).unwrap();
        let mut manifest = CloudSyncManifest { v: 1, slots: HashMap::new() };
        manifest.slots.insert(
            slot.clone(),
            CloudSyncSlot {
                rev: 3,
                hash: "t".into(),
                updated_at_utc: Utc::now(),
                device_id: "other".into(),
                item_count: 0,
                schema: TOMBSTONE_SCHEMA.into(),
            },
        );
        folder.save_manifest(&manifest).unwrap();
        let local_item = sample("gone", "still-here");
        let (result, applies, report) = folder.pull(Some(&slot), false, false, &[local_item]).unwrap();
        assert_eq!(result["success"], true);
        assert!(matches!(applies.first(), Some(FavorApply::Delete(id)) if id == "gone"));
        assert_eq!(report.pulled_deletions.len(), 1);
    }

    #[test]
    fn push_conflict_requires_force() {
        let dir = tempdir().unwrap();
        let folder = CloudFolder::open(dir.path(), "dev1");
        folder.sync_favors(&[sample("a", "one")]).unwrap();
        let mut manifest = folder.load_manifest();
        if let Some(slot) = manifest.slots.get_mut(FAVOR_SLOT) {
            slot.hash = "other-device".into();
            slot.rev = slot.rev; // same rev, different hash
        }
        folder.save_manifest(&manifest).unwrap();
        let err = folder
            .push(Some(FAVOR_SLOT), false, false, &[sample("a", "changed")])
            .unwrap_err();
        assert!(err.contains("--force"), "{err}");
    }
}
