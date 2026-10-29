use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    blob_digest, canonical_json, valid_entry_key, valid_id, ConflictRecord, PreparedSync,
    RecoveryKey, RemoteFile, RemoteStore, Snapshot, SyncError, SyncReport, CHUNK_BYTES,
    MAX_ENCRYPTED_OBJECT_BYTES, MAX_ENTRIES, MAX_METADATA_BYTES, MAX_SNAPSHOT_BYTES,
};

const FORMAT: u32 = 1;
const CAS_ATTEMPTS: usize = 6;
const MAX_ANCESTORS: usize = 2048;
const MAX_BLOBS: usize = 20_000;
const LOCAL_REPLACE_RETRIES: usize = 8;
const LOCAL_REPLACE_DELAY: std::time::Duration = std::time::Duration::from_millis(50);

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Vault {
    format: u32,
    collection_id: String,
    key_check: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Head {
    format: u32,
    collection_id: String,
    commit_id: String,
}

#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Commit {
    format: u32,
    parent: Option<String>,
    entries: BTreeMap<String, String>,
    blobs: BTreeMap<String, BlobManifest>,
    conflicts: BTreeMap<String, WireConflict>,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Revision {
    key: String,
    value: Option<Value>,
    parents: Vec<String>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BlobManifest {
    bytes: usize,
    chunks: Vec<Chunk>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Chunk {
    id: String,
    bytes: usize,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WireConflict {
    key: String,
    base: Option<String>,
    left: String,
    right: String,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LocalInput {
    entries: BTreeMap<String, Value>,
    blobs: BTreeMap<String, BlobManifest>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Pending {
    token: String,
    commit_id: String,
    report: SyncReport,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct State {
    format: u32,
    replica_id: String,
    collection_id: Option<String>,
    initializing: bool,
    base_commit: Option<String>,
    outbox: Option<LocalInput>,
    pending: Option<Pending>,
}

/// One owner per state directory. The host serializes this engine together with
/// snapshot/apply; no UI or network callback may concurrently reuse the state.
pub struct Engine {
    root: PathBuf,
    key: RecoveryKey,
    state: State,
    uploaded: usize,
    downloaded: usize,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("initialized", &self.state.collection_id.is_some())
            .field("pending_apply", &self.state.pending.is_some())
            .finish_non_exhaustive()
    }
}

impl Engine {
    pub fn open(state_dir: &Path, key: RecoveryKey) -> Result<Self, SyncError> {
        fs::create_dir_all(state_dir.join("objects"))?;
        let path = state_dir.join("state.enc");
        let state = if path.exists() {
            let bytes = read_bounded(&path, MAX_ENCRYPTED_OBJECT_BYTES)?;
            let plaintext = key.open("local", "state", "state", &bytes)?;
            let state: State = serde_json::from_slice(&plaintext)?;
            validate_state(&state)?;
            state
        } else {
            State {
                format: FORMAT,
                replica_id: uuid::Uuid::new_v4().simple().to_string(),
                collection_id: None,
                initializing: false,
                base_commit: None,
                outbox: None,
                pending: None,
            }
        };
        let engine = Self {
            root: state_dir.to_path_buf(),
            key,
            state,
            uploaded: 0,
            downloaded: 0,
        };
        engine.persist()?;
        Ok(engine)
    }

    pub fn collection_id(&self) -> Option<&str> {
        self.state.collection_id.as_deref()
    }
    pub fn replica_id(&self) -> &str {
        &self.state.replica_id
    }
    pub fn has_pending_apply(&self) -> bool {
        self.state.pending.is_some()
    }

    /// Lets the host distinguish first-join defaults (an absent local root
    /// ordering) from a deliberate empty ordering after a previous sync.
    pub fn has_acknowledged_baseline(&self) -> bool {
        self.state.base_commit.is_some()
    }

    /// Explicitly create a collection, or finish this replica's interrupted
    /// initialization. A foreign, empty/missing collection is never inferred.
    pub fn create(&mut self, remote: &impl RemoteStore) -> Result<String, SyncError> {
        if self.state.collection_id.is_some() && !self.state.initializing {
            return Err(SyncError::AlreadyInitialized);
        }
        if self.state.collection_id.is_none() {
            self.state.collection_id = Some(uuid::Uuid::new_v4().simple().to_string());
            self.state.initializing = true;
            self.persist()?;
        }
        let candidate = self.collection()?.to_string();
        let vault = Vault {
            format: FORMAT,
            collection_id: candidate.clone(),
            key_check: self.key.keyed_id("vault-check", candidate.as_bytes()),
        };
        let bytes = serde_json::to_vec(&vault)?;
        let created = match remote.create("vault.json", &bytes) {
            Ok(value) => value,
            Err(error) => {
                // The first response can be lost after a successful create.
                if let Ok(Some(file)) = remote.get("vault.json") {
                    let actual = self.validate_vault(&file.bytes)?;
                    if actual.collection_id != candidate {
                        return Err(error);
                    }
                    false
                } else {
                    return Err(error);
                }
            }
        };
        if !created {
            let actual = self.read_vault(remote)?;
            if actual.collection_id != candidate {
                // The concurrent winner must have the same recovery key and a
                // complete collection; do not initialize somebody else's vault.
                self.read_head_for(remote, &actual.collection_id)?;
                self.state.collection_id = Some(actual.collection_id);
                self.state.initializing = false;
                self.persist()?;
                return Ok(self.collection()?.to_string());
            }
        }
        match remote.get("head.json")? {
            Some(file) => {
                self.validate_head(&file)?;
            }
            None => {
                let commit = Commit {
                    format: FORMAT,
                    ..Commit::default()
                };
                let commit_id = self.put_json(remote, "commit", &commit)?;
                let head = Head {
                    format: FORMAT,
                    collection_id: candidate.clone(),
                    commit_id,
                };
                let bytes = serde_json::to_vec(&head)?;
                match remote.create("head.json", &bytes) {
                    Ok(true) => {}
                    Ok(false) => {
                        self.read_head(remote)?;
                    }
                    Err(error) => {
                        if self.read_head(remote).is_err() {
                            return Err(error);
                        }
                    }
                }
            }
        }
        self.state.initializing = false;
        self.persist()?;
        Ok(candidate)
    }

    /// Joining verifies a complete vault/head with the supplied key. It does not
    /// treat remote contents as a baseline for local deletions on first use.
    pub fn join(&mut self, remote: &impl RemoteStore) -> Result<String, SyncError> {
        let vault = self.read_vault(remote)?;
        if let Some(existing) = self.state.collection_id.as_deref() {
            if existing != vault.collection_id {
                return Err(SyncError::InvalidData("collection_changed"));
            }
        }
        self.read_head_for(remote, &vault.collection_id)?;
        self.state.collection_id = Some(vault.collection_id.clone());
        self.state.initializing = false;
        self.persist()?;
        Ok(vault.collection_id)
    }

    /// Publish/receive a complete merged commit and persist its local apply
    /// ticket. All attachment bytes are verified before this method succeeds.
    pub fn prepare(
        &mut self,
        remote: &impl RemoteStore,
        snapshot: &Snapshot,
    ) -> Result<PreparedSync, SyncError> {
        self.prepare_scoped(remote, snapshot, &[String::new()])
    }

    /// A scope ending in `/` selects a namespace; other nonempty scopes select
    /// exactly one key. Empty selects all keys. The host MUST use an explicit
    /// scope when some remote datasets/settings are not subscribed.
    pub fn prepare_scoped(
        &mut self,
        remote: &impl RemoteStore,
        snapshot: &Snapshot,
        scopes: &[String],
    ) -> Result<PreparedSync, SyncError> {
        self.prepare_scoped_validated(remote, snapshot, scopes, |_| Ok(()))
    }

    /// A conflict-free merged closure must satisfy business invariants before
    /// its head is published. Explicit conflicts instead publish both originals
    /// as a recovery ticket: their deterministic projection is NOT necessarily
    /// a valid business graph and MUST NOT be applied by the host. The final
    /// resolution must pass this validator before it can become applicable.
    /// Rejection retains the outbox and all local/remote originals, and leaves
    /// only harmless unpublished immutable objects on the server.
    pub fn prepare_scoped_validated(
        &mut self,
        remote: &impl RemoteStore,
        snapshot: &Snapshot,
        scopes: &[String],
        mut validator: impl FnMut(&Snapshot) -> Result<(), SyncError>,
    ) -> Result<PreparedSync, SyncError> {
        if self.state.pending.is_some() {
            return Err(SyncError::PendingApply);
        }
        self.collection()?;
        self.verify_remote_identity(remote)?;
        validate_snapshot(snapshot)?;
        if scopes.is_empty() || snapshot.entries.keys().any(|key| !in_scope(key, scopes)) {
            return Err(SyncError::InvalidData("subscription_scope"));
        }
        self.uploaded = 0;
        self.downloaded = 0;

        let mut input = LocalInput {
            entries: snapshot.entries.clone(),
            blobs: BTreeMap::new(),
        };
        for (digest, bytes) in &snapshot.blobs {
            let mut chunks = Vec::new();
            // Cache stable ciphertext before any network operation. Empty files
            // have one authenticated zero-length chunk.
            if bytes.is_empty() {
                let id = self.cache_plaintext("chunk", &[])?;
                chunks.push(Chunk { id, bytes: 0 });
            } else {
                for block in bytes.chunks(CHUNK_BYTES) {
                    let id = self.cache_plaintext("chunk", block)?;
                    chunks.push(Chunk {
                        id,
                        bytes: block.len(),
                    });
                }
            }
            input.blobs.insert(
                digest.clone(),
                BlobManifest {
                    bytes: bytes.len(),
                    chunks,
                },
            );
        }
        self.state.outbox = Some(input.clone());
        self.persist()?;

        let baseline = if let Some(id) = self.state.base_commit.clone() {
            Some(self.read_commit(remote, &id)?)
        } else {
            None
        };
        let base_values = if let Some(base) = baseline.as_ref() {
            self.entry_values(remote, base)?
        } else {
            BTreeMap::new()
        };

        for _ in 0..CAS_ATTEMPTS {
            let (head, etag) = self.read_head(remote)?;
            if let Some(base) = self.state.base_commit.clone() {
                self.require_ancestor(remote, &head.commit_id, &base)?;
            }
            let current = self.read_commit(remote, &head.commit_id)?;
            if baseline.as_ref().is_some_and(|base| {
                base.entries
                    .keys()
                    .any(|key| !current.entries.contains_key(key))
            }) {
                return Err(SyncError::InvalidData("missing_tombstone"));
            }
            let remote_values = self.entry_values(remote, &current)?;
            let mut merged = current.clone();
            merged.parent = Some(head.commit_id.clone());

            for (digest, manifest) in &input.blobs {
                if merged.blobs.get(digest) == Some(manifest) {
                    continue;
                }
                for chunk in &manifest.chunks {
                    self.publish_cached(remote, "chunk", &chunk.id)?;
                }
                if let Some(previous) = merged.blobs.get(digest) {
                    if previous != manifest {
                        return Err(SyncError::InvalidData("blob_manifest_collision"));
                    }
                }
                merged.blobs.insert(digest.clone(), manifest.clone());
            }

            let keys: BTreeSet<String> = base_values
                .keys()
                .chain(input.entries.keys())
                .chain(remote_values.keys())
                .cloned()
                .collect();
            for key in keys {
                if !in_scope(&key, scopes) {
                    continue;
                }
                let local = input.entries.get(&key);
                let base = base_values.get(&key).and_then(|v| v.as_ref());
                let remote_value = remote_values.get(&key).and_then(|v| v.as_ref());
                let base_revision = baseline.as_ref().and_then(|b| b.entries.get(&key)).cloned();
                let remote_revision = current.entries.get(&key).cloned();
                if local == base {
                    // Unchanged locally, including new remote records which did
                    // not exist in this replica's acknowledged baseline.
                    continue;
                }
                if local == remote_value {
                    continue;
                }
                let local_revision = Revision {
                    key: key.clone(),
                    value: local.cloned(),
                    parents: base_revision.iter().cloned().collect(),
                };
                let local_id = self.put_json(remote, "revision", &local_revision)?;
                // An explicit resolution is a child of both alternatives. If
                // this unchanged local edit is already one of its ancestors,
                // accepting the resolution must not reopen the old conflict.
                // A genuinely newer local edit produces another revision ID
                // and still conflicts, preserving work done during resolution.
                if remote_revision != base_revision {
                    if let Some(remote_id) = remote_revision.as_deref() {
                        if self.revision_has_ancestor(remote, &key, remote_id, &local_id)? {
                            continue;
                        }
                    }
                }
                if remote_revision == base_revision || remote_revision.is_none() {
                    merged.entries.insert(key, local_id);
                } else {
                    let remote_id =
                        remote_revision.ok_or(SyncError::InvalidData("missing_revision"))?;
                    let (left, right) = if local_id < remote_id {
                        (local_id, remote_id)
                    } else {
                        (remote_id, local_id)
                    };
                    let wire = WireConflict {
                        key: key.clone(),
                        base: base_revision,
                        left: left.clone(),
                        right,
                    };
                    let conflict_id = self.key.keyed_id("conflict", &json_bytes(&wire)?);
                    merged.conflicts.insert(conflict_id, wire);
                    // A deterministic projection makes every replica converge;
                    // both originals remain explicit unresolved conflicts.
                    merged.entries.insert(key, left);
                }
            }
            validate_commit(&merged)?;
            let (materialized, materialized_conflicts) = self.materialize(remote, &merged)?;
            if materialized_conflicts.is_empty() {
                validator(&materialized)?;
            }
            let unchanged = merged.entries == current.entries
                && merged.blobs == current.blobs
                && merged.conflicts == current.conflicts;
            let commit_id = if unchanged {
                head.commit_id.clone()
            } else {
                self.put_json(remote, "commit", &merged)?
            };
            let published = if unchanged {
                false
            } else {
                let next = Head {
                    format: FORMAT,
                    collection_id: self.collection()?.to_string(),
                    commit_id: commit_id.clone(),
                };
                let bytes = serde_json::to_vec(&next)?;
                match remote.replace("head.json", &etag, &bytes) {
                    Ok(true) => true,
                    Ok(false) => continue,
                    Err(error) => {
                        // A lost successful response is acknowledged only after
                        // reading the exact commit or a descendant containing it.
                        match self.read_head(remote) {
                            Ok((observed, _)) if observed.commit_id == commit_id => true,
                            Ok((observed, _))
                                if self
                                    .require_ancestor(remote, &observed.commit_id, &commit_id)
                                    .is_ok() =>
                            {
                                continue
                            }
                            _ => return Err(error),
                        }
                    }
                }
            };
            return self.prepare_materialized(
                &commit_id,
                materialized,
                materialized_conflicts,
                published,
            );
        }
        Err(SyncError::Conflict)
    }

    /// Reconstruct an interrupted apply entirely from authenticated local cache.
    pub fn pending(&mut self) -> Result<Option<PreparedSync>, SyncError> {
        let Some(pending) = self.state.pending.clone() else {
            return Ok(None);
        };
        let commit = self.read_commit(&CacheOnly, &pending.commit_id)?;
        let (snapshot, conflicts) = self.materialize(&CacheOnly, &commit)?;
        Ok(Some(PreparedSync {
            token: pending.token,
            snapshot,
            conflicts,
            report: pending.report,
        }))
    }

    /// The caller must durably apply BOTH business state and its own apply
    /// receipt before acknowledging. Repeated acknowledgement is not guessed.
    pub fn acknowledge(&mut self, token: &str) -> Result<(), SyncError> {
        let pending = self
            .state
            .pending
            .as_ref()
            .ok_or(SyncError::InvalidAcknowledgement)?;
        if pending.token != token {
            return Err(SyncError::InvalidAcknowledgement);
        }
        self.state.base_commit = Some(pending.commit_id.clone());
        self.state.pending = None;
        self.state.outbox = None;
        self.persist()
    }

    /// Leave all published and local versions intact while abandoning only an
    /// unapplied ticket (for example to explicitly resolve a reported conflict).
    pub fn abandon_pending(&mut self, token: &str) -> Result<(), SyncError> {
        if self.state.pending.as_ref().map(|p| p.token.as_str()) != Some(token) {
            return Err(SyncError::InvalidAcknowledgement);
        }
        self.state.pending = None;
        self.persist()
    }

    /// Explicitly select one retained conflict revision, creating a new child
    /// revision of BOTH alternatives. The host presents the alternatives first.
    pub fn resolve(
        &mut self,
        remote: &impl RemoteStore,
        conflict_id: &str,
        selected_revision: &str,
    ) -> Result<PreparedSync, SyncError> {
        self.resolve_validated(remote, conflict_id, selected_revision, |_| Ok(()))
    }

    /// A partial resolution retains a non-applicable recovery ticket while
    /// other explicit conflicts remain. The last choice must validate the
    /// complete resolved business graph before publishing an applicable head;
    /// rejection leaves the original unresolved head and alternatives intact.
    pub fn resolve_validated(
        &mut self,
        remote: &impl RemoteStore,
        conflict_id: &str,
        selected_revision: &str,
        mut validator: impl FnMut(&Snapshot) -> Result<(), SyncError>,
    ) -> Result<PreparedSync, SyncError> {
        if self.state.pending.is_some() {
            return Err(SyncError::PendingApply);
        }
        self.verify_remote_identity(remote)?;
        self.uploaded = 0;
        self.downloaded = 0;
        for _ in 0..CAS_ATTEMPTS {
            let (head, etag) = self.read_head(remote)?;
            if let Some(base) = self.state.base_commit.clone() {
                self.require_ancestor(remote, &head.commit_id, &base)?;
            }
            let mut commit = self.read_commit(remote, &head.commit_id)?;
            let conflict = commit
                .conflicts
                .get(conflict_id)
                .cloned()
                .ok_or(SyncError::InvalidData("unknown_conflict"))?;
            if selected_revision != conflict.left && selected_revision != conflict.right {
                return Err(SyncError::InvalidData("unknown_conflict_revision"));
            }
            let chosen: Revision = self.read_json(remote, "revision", selected_revision)?;
            if chosen.key != conflict.key {
                return Err(SyncError::InvalidData("revision_key"));
            }
            let revision = Revision {
                key: conflict.key.clone(),
                value: chosen.value,
                parents: vec![conflict.left.clone(), conflict.right.clone()],
            };
            let revision_id = self.put_json(remote, "revision", &revision)?;
            commit.conflicts.remove(conflict_id);
            let projection = commit.entries.get(&conflict.key).cloned();
            if let Some(current_id) =
                projection.filter(|id| id != &conflict.left && id != &conflict.right)
            {
                let current: Revision = self.read_json(remote, "revision", &current_id)?;
                if current.key != conflict.key {
                    return Err(SyncError::InvalidData("revision_key"));
                }
                if current.value != revision.value {
                    // An ordinary edit may have advanced the active projection
                    // while the earlier alternatives were awaiting a choice.
                    // Keep that later edit as a new explicit conflict rather
                    // than silently overwriting it with the old selection.
                    let (left, right) = if current_id < revision_id {
                        (current_id, revision_id)
                    } else {
                        (revision_id, current_id)
                    };
                    let next = WireConflict {
                        key: conflict.key.clone(),
                        base: None,
                        left: left.clone(),
                        right,
                    };
                    let id = self.key.keyed_id("conflict", &json_bytes(&next)?);
                    commit.conflicts.insert(id, next);
                    commit.entries.insert(conflict.key.clone(), left);
                } else {
                    let mut resolved = revision;
                    resolved.parents.push(current_id);
                    resolved.parents.sort();
                    resolved.parents.dedup();
                    let id = self.put_json(remote, "revision", &resolved)?;
                    commit.entries.insert(conflict.key.clone(), id);
                }
            } else {
                commit.entries.insert(conflict.key.clone(), revision_id);
            }
            commit.parent = Some(head.commit_id);
            validate_commit(&commit)?;
            let (materialized, materialized_conflicts) = self.materialize(remote, &commit)?;
            if materialized_conflicts.is_empty() {
                validator(&materialized)?;
            }
            let commit_id = self.put_json(remote, "commit", &commit)?;
            let next = Head {
                format: FORMAT,
                collection_id: self.collection()?.to_string(),
                commit_id: commit_id.clone(),
            };
            match remote.replace("head.json", &etag, &serde_json::to_vec(&next)?) {
                Ok(true) => {
                    return self.prepare_materialized(
                        &commit_id,
                        materialized,
                        materialized_conflicts,
                        true,
                    )
                }
                Ok(false) => continue,
                Err(error) => {
                    if let Ok((observed, _)) = self.read_head(remote) {
                        if self
                            .require_ancestor(remote, &observed.commit_id, &commit_id)
                            .is_ok()
                        {
                            let actual = self.read_commit(remote, &observed.commit_id)?;
                            let (materialized, conflicts) = self.materialize(remote, &actual)?;
                            if conflicts.is_empty() {
                                validator(&materialized)?;
                            }
                            return self.prepare_materialized(
                                &observed.commit_id,
                                materialized,
                                conflicts,
                                true,
                            );
                        }
                    }
                    return Err(error);
                }
            }
        }
        Err(SyncError::Conflict)
    }

    fn prepare_materialized(
        &mut self,
        commit_id: &str,
        snapshot: Snapshot,
        conflicts: Vec<ConflictRecord>,
        published: bool,
    ) -> Result<PreparedSync, SyncError> {
        let report = SyncReport {
            collection_id: self.collection()?.to_string(),
            commit_id: commit_id.to_string(),
            entries: snapshot.entries.len(),
            blobs: snapshot.blobs.len(),
            conflicts: conflicts.len(),
            uploaded_objects: self.uploaded,
            downloaded_objects: self.downloaded,
            published,
        };
        let token = uuid::Uuid::new_v4().simple().to_string();
        self.state.pending = Some(Pending {
            token: token.clone(),
            commit_id: commit_id.to_string(),
            report: report.clone(),
        });
        self.persist()?;
        Ok(PreparedSync {
            token,
            snapshot,
            conflicts,
            report,
        })
    }

    fn materialize(
        &mut self,
        remote: &impl RemoteStore,
        commit: &Commit,
    ) -> Result<(Snapshot, Vec<ConflictRecord>), SyncError> {
        let mut snapshot = Snapshot::default();
        for (key, value) in self.entry_values(remote, commit)? {
            if let Some(value) = value {
                snapshot.entries.insert(key, value);
            }
        }
        let mut total = 0usize;
        for (digest, manifest) in &commit.blobs {
            total = total
                .checked_add(manifest.bytes)
                .ok_or(SyncError::LimitExceeded("snapshot_bytes"))?;
            if total > MAX_SNAPSHOT_BYTES {
                return Err(SyncError::LimitExceeded("snapshot_bytes"));
            }
            let mut bytes = Vec::with_capacity(manifest.bytes);
            for chunk in &manifest.chunks {
                let block = self.read_object(remote, "chunk", &chunk.id)?;
                if block.len() != chunk.bytes {
                    return Err(SyncError::InvalidData("chunk_length"));
                }
                bytes.extend_from_slice(&block);
            }
            if bytes.len() != manifest.bytes || blob_digest(&bytes) != *digest {
                return Err(SyncError::InvalidData("blob_digest"));
            }
            snapshot.blobs.insert(digest.clone(), bytes);
        }
        let mut conflicts = Vec::new();
        for (id, wire) in &commit.conflicts {
            let left: Revision = self.read_json(remote, "revision", &wire.left)?;
            let right: Revision = self.read_json(remote, "revision", &wire.right)?;
            if left.key != wire.key || right.key != wire.key {
                return Err(SyncError::InvalidData("conflict_key"));
            }
            conflicts.push(ConflictRecord {
                id: id.clone(),
                key: wire.key.clone(),
                base_revision: wire.base.clone(),
                left_revision: wire.left.clone(),
                right_revision: wire.right.clone(),
                left_value: left.value,
                right_value: right.value,
            });
        }
        validate_snapshot(&snapshot)?;
        Ok((snapshot, conflicts))
    }

    fn entry_values(
        &mut self,
        remote: &impl RemoteStore,
        commit: &Commit,
    ) -> Result<BTreeMap<String, Option<Value>>, SyncError> {
        let mut values = BTreeMap::new();
        for (key, id) in &commit.entries {
            let revision: Revision = self.read_json(remote, "revision", id)?;
            if revision.key != *key
                || revision.parents.len() > 8
                || revision.parents.iter().any(|id| !valid_id(id))
            {
                return Err(SyncError::InvalidData("revision"));
            }
            values.insert(key.clone(), revision.value);
        }
        Ok(values)
    }

    fn collection(&self) -> Result<&str, SyncError> {
        self.state
            .collection_id
            .as_deref()
            .ok_or(SyncError::NotInitialized)
    }

    fn persist(&self) -> Result<(), SyncError> {
        let bytes = json_bytes(&self.state)?;
        let sealed = self.key.seal("local", "state", "state", &bytes)?;
        write_atomic(&self.root.join("state.enc"), &sealed)
    }

    fn read_vault(&self, remote: &impl RemoteStore) -> Result<Vault, SyncError> {
        let file = remote.get("vault.json")?.ok_or(SyncError::MissingRemote)?;
        self.validate_vault(&file.bytes)
    }

    fn validate_vault(&self, bytes: &[u8]) -> Result<Vault, SyncError> {
        if bytes.len() > 4096 {
            return Err(SyncError::LimitExceeded("vault_bytes"));
        }
        let vault: Vault = serde_json::from_slice(bytes)?;
        if vault.format != FORMAT || !valid_uuid(&vault.collection_id) {
            return Err(SyncError::InvalidData("vault_format"));
        }
        if self
            .key
            .keyed_id("vault-check", vault.collection_id.as_bytes())
            != vault.key_check
        {
            return Err(SyncError::KeyMismatch);
        }
        Ok(vault)
    }

    fn verify_remote_identity(&self, remote: &impl RemoteStore) -> Result<(), SyncError> {
        let vault = self.read_vault(remote)?;
        if vault.collection_id != self.collection()? {
            return Err(SyncError::InvalidData("collection_changed"));
        }
        Ok(())
    }

    fn read_head_for(
        &self,
        remote: &impl RemoteStore,
        collection: &str,
    ) -> Result<(Head, String), SyncError> {
        let file = remote.get("head.json")?.ok_or(SyncError::MissingRemote)?;
        if file.bytes.len() > 4096 {
            return Err(SyncError::LimitExceeded("head_bytes"));
        }
        require_strong_etag(&file.etag)?;
        let head: Head = serde_json::from_slice(&file.bytes)?;
        if head.format != FORMAT || head.collection_id != collection || !valid_id(&head.commit_id) {
            return Err(SyncError::InvalidData("head_format"));
        }
        Ok((head, file.etag))
    }

    fn read_head(&self, remote: &impl RemoteStore) -> Result<(Head, String), SyncError> {
        self.read_head_for(remote, self.collection()?)
    }

    fn validate_head(&self, file: &RemoteFile) -> Result<Head, SyncError> {
        if file.bytes.len() > 4096 {
            return Err(SyncError::LimitExceeded("head_bytes"));
        }
        require_strong_etag(&file.etag)?;
        let head: Head = serde_json::from_slice(&file.bytes)?;
        if head.format != FORMAT
            || head.collection_id != self.collection()?
            || !valid_id(&head.commit_id)
        {
            return Err(SyncError::InvalidData("head_format"));
        }
        Ok(head)
    }

    fn read_commit(&mut self, remote: &impl RemoteStore, id: &str) -> Result<Commit, SyncError> {
        let commit: Commit = self.read_json(remote, "commit", id)?;
        validate_commit(&commit)?;
        Ok(commit)
    }

    fn require_ancestor(
        &mut self,
        remote: &impl RemoteStore,
        descendant: &str,
        ancestor: &str,
    ) -> Result<(), SyncError> {
        let mut current = descendant.to_string();
        let mut seen = BTreeSet::new();
        for _ in 0..MAX_ANCESTORS {
            if current == ancestor {
                return Ok(());
            }
            if !seen.insert(current.clone()) {
                return Err(SyncError::InvalidData("commit_cycle"));
            }
            let commit = self.read_commit(remote, &current)?;
            current = commit
                .parent
                .ok_or(SyncError::InvalidData("remote_rollback"))?;
        }
        Err(SyncError::LimitExceeded("commit_ancestry"))
    }

    fn revision_has_ancestor(
        &mut self,
        remote: &impl RemoteStore,
        key: &str,
        descendant: &str,
        ancestor: &str,
    ) -> Result<bool, SyncError> {
        let mut pending = vec![descendant.to_string()];
        let mut seen = BTreeSet::new();
        while let Some(id) = pending.pop() {
            if id == ancestor {
                return Ok(true);
            }
            if !seen.insert(id.clone()) {
                continue;
            }
            if seen.len() > MAX_ANCESTORS {
                return Err(SyncError::LimitExceeded("revision_ancestry"));
            }
            let revision: Revision = self.read_json(remote, "revision", &id)?;
            if revision.key != key
                || revision.parents.len() > 8
                || revision.parents.iter().any(|id| !valid_id(id))
            {
                return Err(SyncError::InvalidData("revision_ancestry"));
            }
            pending.extend(revision.parents);
        }
        Ok(false)
    }

    fn put_json<T: Serialize>(
        &mut self,
        remote: &impl RemoteStore,
        kind: &str,
        value: &T,
    ) -> Result<String, SyncError> {
        let bytes = json_bytes(value)?;
        let id = self.cache_plaintext(kind, &bytes)?;
        self.publish_cached(remote, kind, &id)?;
        Ok(id)
    }

    fn read_json<T: for<'a> Deserialize<'a>>(
        &mut self,
        remote: &impl RemoteStore,
        kind: &str,
        id: &str,
    ) -> Result<T, SyncError> {
        let bytes = self.read_object(remote, kind, id)?;
        if bytes.len() > MAX_METADATA_BYTES {
            return Err(SyncError::LimitExceeded("metadata_bytes"));
        }
        Ok(serde_json::from_slice(&bytes)?)
    }

    fn object_id(&self, kind: &str, plaintext: &[u8]) -> Result<String, SyncError> {
        Ok(self
            .key
            .keyed_id(&format!("{}/{kind}", self.collection()?), plaintext))
    }

    fn cache_plaintext(&self, kind: &str, plaintext: &[u8]) -> Result<String, SyncError> {
        let id = self.object_id(kind, plaintext)?;
        let path = self.cache_path(&id)?;
        if path.exists() {
            let existing = read_bounded(&path, MAX_ENCRYPTED_OBJECT_BYTES)?;
            let decoded = self.decrypt_object(kind, &id, &existing)?;
            if decoded != plaintext {
                return Err(SyncError::InvalidData("object_collision"));
            }
        } else {
            let ciphertext = self.key.seal(self.collection()?, kind, &id, plaintext)?;
            write_atomic(&path, &ciphertext)?;
        }
        Ok(id)
    }

    fn publish_cached(
        &mut self,
        remote: &impl RemoteStore,
        kind: &str,
        id: &str,
    ) -> Result<(), SyncError> {
        let bytes = read_bounded(&self.cache_path(id)?, MAX_ENCRYPTED_OBJECT_BYTES)?;
        let expected = self.decrypt_object(kind, id, &bytes)?;
        let path = object_path(kind, id)?;
        match remote.create(&path, &bytes) {
            Ok(true) => {
                self.uploaded += 1;
                Ok(())
            }
            Ok(false) => self.verify_existing(remote, kind, id, &expected),
            Err(error) => {
                if self.verify_existing(remote, kind, id, &expected).is_ok() {
                    Ok(())
                } else {
                    Err(error)
                }
            }
        }
    }

    fn verify_existing(
        &mut self,
        remote: &impl RemoteStore,
        kind: &str,
        id: &str,
        expected: &[u8],
    ) -> Result<(), SyncError> {
        let file = remote
            .get(&object_path(kind, id)?)?
            .ok_or(SyncError::MissingRemote)?;
        let plaintext = self.decrypt_object(kind, id, &file.bytes)?;
        if plaintext != expected {
            return Err(SyncError::InvalidData("immutable_collision"));
        }
        // Reuse the winning envelope on retries rather than generating another
        // nonce for this logical object after process restart.
        write_atomic(&self.cache_path(id)?, &file.bytes)?;
        Ok(())
    }

    fn read_object(
        &mut self,
        remote: &impl RemoteStore,
        kind: &str,
        id: &str,
    ) -> Result<Vec<u8>, SyncError> {
        let cache_path = self.cache_path(id)?;
        if cache_path.exists() {
            if let Ok(bytes) = read_bounded(&cache_path, MAX_ENCRYPTED_OBJECT_BYTES) {
                if let Ok(plaintext) = self.decrypt_object(kind, id, &bytes) {
                    return Ok(plaintext);
                }
            }
        }
        let file = remote
            .get(&object_path(kind, id)?)?
            .ok_or(SyncError::MissingRemote)?;
        let plaintext = self.decrypt_object(kind, id, &file.bytes)?;
        write_atomic(&cache_path, &file.bytes)?;
        self.downloaded += 1;
        Ok(plaintext)
    }

    fn decrypt_object(
        &self,
        kind: &str,
        id: &str,
        ciphertext: &[u8],
    ) -> Result<Vec<u8>, SyncError> {
        let bytes = self.key.open(self.collection()?, kind, id, ciphertext)?;
        if self.object_id(kind, &bytes)? != id {
            return Err(SyncError::InvalidData("object_identity"));
        }
        Ok(bytes)
    }

    fn cache_path(&self, id: &str) -> Result<PathBuf, SyncError> {
        if !valid_id(id) {
            return Err(SyncError::InvalidData("object_id"));
        }
        Ok(self.root.join("objects").join(id))
    }
}

fn json_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>, SyncError> {
    let bytes = canonical_json(&serde_json::to_value(value)?)?;
    if bytes.len() > MAX_METADATA_BYTES {
        return Err(SyncError::LimitExceeded("metadata_bytes"));
    }
    Ok(bytes)
}

fn validate_snapshot(snapshot: &Snapshot) -> Result<(), SyncError> {
    if snapshot.entries.len() > MAX_ENTRIES {
        return Err(SyncError::LimitExceeded("entries"));
    }
    if snapshot.blobs.len() > MAX_BLOBS {
        return Err(SyncError::LimitExceeded("blobs"));
    }
    if snapshot.entries.keys().any(|key| !valid_entry_key(key)) {
        return Err(SyncError::InvalidData("entry_key"));
    }
    if json_bytes(&snapshot.entries)?.len() > MAX_METADATA_BYTES {
        return Err(SyncError::LimitExceeded("metadata_bytes"));
    }
    let mut total = 0usize;
    for (digest, bytes) in &snapshot.blobs {
        if !valid_id(digest) || blob_digest(bytes) != *digest {
            return Err(SyncError::InvalidData("blob_digest"));
        }
        total = total
            .checked_add(bytes.len())
            .ok_or(SyncError::LimitExceeded("snapshot_bytes"))?;
        if total > MAX_SNAPSHOT_BYTES {
            return Err(SyncError::LimitExceeded("snapshot_bytes"));
        }
    }
    Ok(())
}

fn validate_commit(commit: &Commit) -> Result<(), SyncError> {
    if commit.format != FORMAT {
        return Err(SyncError::InvalidData("commit_format"));
    }
    if commit.parent.as_ref().is_some_and(|id| !valid_id(id)) {
        return Err(SyncError::InvalidData("commit_parent"));
    }
    if commit.entries.len() > MAX_ENTRIES
        || commit.conflicts.len() > MAX_ENTRIES
        || commit.blobs.len() > MAX_BLOBS
    {
        return Err(SyncError::LimitExceeded("commit_objects"));
    }
    if commit
        .entries
        .iter()
        .any(|(key, id)| !valid_entry_key(key) || !valid_id(id))
    {
        return Err(SyncError::InvalidData("commit_entry"));
    }
    let mut total = 0usize;
    for (digest, blob) in &commit.blobs {
        if !valid_id(digest)
            || blob.chunks.is_empty()
            || blob.chunks.len() > MAX_SNAPSHOT_BYTES / CHUNK_BYTES + 1
        {
            return Err(SyncError::InvalidData("blob_manifest"));
        }
        let mut bytes = 0usize;
        for chunk in &blob.chunks {
            if !valid_id(&chunk.id) || chunk.bytes > CHUNK_BYTES {
                return Err(SyncError::InvalidData("chunk_manifest"));
            }
            bytes = bytes
                .checked_add(chunk.bytes)
                .ok_or(SyncError::LimitExceeded("blob_bytes"))?;
        }
        if bytes != blob.bytes {
            return Err(SyncError::InvalidData("blob_length"));
        }
        total = total
            .checked_add(bytes)
            .ok_or(SyncError::LimitExceeded("snapshot_bytes"))?;
        if total > MAX_SNAPSHOT_BYTES {
            return Err(SyncError::LimitExceeded("snapshot_bytes"));
        }
    }
    for (id, conflict) in &commit.conflicts {
        if !valid_id(id)
            || !valid_entry_key(&conflict.key)
            || !valid_id(&conflict.left)
            || !valid_id(&conflict.right)
            || conflict.left >= conflict.right
            || conflict.base.as_ref().is_some_and(|id| !valid_id(id))
        {
            return Err(SyncError::InvalidData("conflict"));
        }
    }
    Ok(())
}

fn validate_state(state: &State) -> Result<(), SyncError> {
    if state.format != FORMAT
        || !valid_uuid(&state.replica_id)
        || state
            .collection_id
            .as_ref()
            .is_some_and(|id| !valid_uuid(id))
        || state.base_commit.as_ref().is_some_and(|id| !valid_id(id))
    {
        return Err(SyncError::InvalidData("local_state"));
    }
    if let Some(pending) = &state.pending {
        if !valid_uuid(&pending.token) || !valid_id(&pending.commit_id) {
            return Err(SyncError::InvalidData("pending_state"));
        }
    }
    Ok(())
}

fn valid_uuid(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn in_scope(key: &str, scopes: &[String]) -> bool {
    scopes.iter().any(|scope| {
        scope.is_empty()
            || if scope.ends_with('/') {
                key.starts_with(scope)
            } else {
                key == scope
            }
    })
}

fn require_strong_etag(etag: &str) -> Result<(), SyncError> {
    if etag.len() < 2
        || etag.len() > 1024
        || !etag.starts_with('"')
        || !etag.ends_with('"')
        || etag[1..etag.len() - 1]
            .bytes()
            .any(|b| b == b'"' || b < 0x21 || b == 0x7f)
    {
        return Err(SyncError::InvalidData("strong_etag_required"));
    }
    Ok(())
}

fn object_path(kind: &str, id: &str) -> Result<String, SyncError> {
    if !valid_id(id) {
        return Err(SyncError::InvalidData("object_id"));
    }
    let directory = match kind {
        "commit" => "commits",
        "revision" => "objects",
        "chunk" => "blobs",
        _ => return Err(SyncError::InvalidData("object_kind")),
    };
    Ok(format!("{directory}/{}/{}", &id[..2], id))
}

fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>, SyncError> {
    let file = fs::File::open(path)?;
    if file.metadata()?.len() > limit as u64 {
        return Err(SyncError::LimitExceeded("local_file_bytes"));
    }
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(SyncError::LimitExceeded("local_file_bytes"));
    }
    Ok(bytes)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), SyncError> {
    let parent = path.parent().ok_or(SyncError::InvalidData("state_path"))?;
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".webdav-pending-")
        .tempfile_in(parent)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    persist_with_retry_using(temporary, path, |file, path| file.persist(path).map(|_| ()))
        .map_err(preserve_failed_replace)?;
    Ok(())
}

fn persist_with_retry_using(
    mut file: tempfile::NamedTempFile,
    path: &Path,
    mut persist: impl FnMut(tempfile::NamedTempFile, &Path) -> Result<(), tempfile::PersistError>,
) -> Result<(), tempfile::PersistError> {
    for attempt in 0..=LOCAL_REPLACE_RETRIES {
        match persist(file, path) {
            Ok(()) => return Ok(()),
            Err(error) => {
                // A scanner or reader may briefly deny a Windows replacement.
                // Retain the SAME staged bytes on every bounded retry. A real
                // permission failure still returns an error after the budget.
                let retryable =
                    cfg!(windows) && matches!(error.error.raw_os_error(), Some(5 | 32 | 33));
                if !retryable || attempt == LOCAL_REPLACE_RETRIES {
                    return Err(error);
                }
                file = error.file;
                std::thread::sleep(LOCAL_REPLACE_DELAY);
            }
        }
    }
    unreachable!("final replacement attempt always returns")
}

fn preserve_failed_replace(error: tempfile::PersistError) -> SyncError {
    let tempfile::PersistError { error, file } = error;
    // Do not discard the only fully flushed candidate state when a permanent
    // access denial prevents replacement. The normal destination is unchanged;
    // an identifiable encrypted candidate remains beside it for recovery.
    if let Err(mut keep_error) = file.keep() {
        keep_error.file.disable_cleanup(true);
    }
    SyncError::Io(error)
}

struct CacheOnly;
impl RemoteStore for CacheOnly {
    fn get(&self, _: &str) -> Result<Option<RemoteFile>, SyncError> {
        Err(SyncError::MissingRemote)
    }
    fn create(&self, _: &str, _: &[u8]) -> Result<bool, SyncError> {
        Err(SyncError::MissingRemote)
    }
    fn replace(&self, _: &str, _: &str, _: &[u8]) -> Result<bool, SyncError> {
        Err(SyncError::MissingRemote)
    }
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
