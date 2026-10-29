use super::*;
use serde_json::json;
use std::cell::RefCell;
use std::sync::Mutex;

#[derive(Default)]
struct Memory {
    data: Mutex<MemoryData>,
}

#[derive(Default)]
struct MemoryData {
    files: BTreeMap<String, RemoteFile>,
    generation: u64,
    lose_replace_response: bool,
    lose_object_response: bool,
    fail_initial_head_create: bool,
}

impl RemoteStore for Memory {
    fn get(&self, path: &str) -> Result<Option<RemoteFile>, SyncError> {
        Ok(self.data.lock().unwrap().files.get(path).cloned())
    }

    fn create(&self, path: &str, bytes: &[u8]) -> Result<bool, SyncError> {
        let mut data = self.data.lock().unwrap();
        if data.files.contains_key(path) {
            return Ok(false);
        }
        if path == "head.json" && data.fail_initial_head_create {
            data.fail_initial_head_create = false;
            return Err(SyncError::Transport("interrupted"));
        }
        data.generation += 1;
        let etag = format!("\"{}\"", data.generation);
        data.files.insert(
            path.into(),
            RemoteFile {
                bytes: bytes.to_vec(),
                etag,
            },
        );
        if data.lose_object_response && (path.starts_with("objects/") || path.starts_with("blobs/"))
        {
            data.lose_object_response = false;
            return Err(SyncError::Transport("response_lost"));
        }
        Ok(true)
    }

    fn replace(&self, path: &str, etag: &str, bytes: &[u8]) -> Result<bool, SyncError> {
        let mut data = self.data.lock().unwrap();
        if data.files.get(path).map(|file| file.etag.as_str()) != Some(etag) {
            return Ok(false);
        }
        data.generation += 1;
        let etag = format!("\"{}\"", data.generation);
        data.files.insert(
            path.into(),
            RemoteFile {
                bytes: bytes.to_vec(),
                etag,
            },
        );
        if data.lose_replace_response {
            data.lose_replace_response = false;
            return Err(SyncError::Transport("response_lost"));
        }
        Ok(true)
    }
}

fn engine(path: &Path, key: &RecoveryKey) -> Engine {
    Engine::open(path, key.clone()).unwrap()
}

fn applied(engine: &mut Engine, remote: &impl RemoteStore, snapshot: &Snapshot) -> PreparedSync {
    let prepared = engine.prepare(remote, snapshot).unwrap();
    engine.acknowledge(&prepared.token).unwrap();
    prepared
}

fn initial() -> Snapshot {
    let bytes = b"original\0image\xffbytes".to_vec();
    Snapshot {
        entries: BTreeMap::from([
            (
                "favor/first".into(),
                json!({"name":"first","raw":{"$type":"UnknownFutureFavorite","extra":{"x":1}},"html":"<html><body> original </body></html>"}),
            ),
            (
                "favor/second".into(),
                json!({"name":"second","children":["first"]}),
            ),
            ("roots".into(), json!(["second", "first"])),
        ]),
        blobs: BTreeMap::from([(blob_digest(&bytes), bytes)]),
    }
}

#[test]
fn encrypted_collection_preserves_unknown_fields_and_original_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let remote = Memory::default();
    let key = RecoveryKey::generate().unwrap();
    let mut a = engine(&temp.path().join("a"), &key);
    a.create(&remote).unwrap();
    let source = initial();
    let first = applied(&mut a, &remote, &source);
    assert_eq!(first.snapshot, source);
    let mut b = engine(&temp.path().join("b"), &key);
    b.join(&remote).unwrap();
    let received = applied(&mut b, &remote, &Snapshot::default());
    assert_eq!(received.snapshot, source);
    assert_eq!(received.conflicts.len(), 0);
    for (path, file) in &remote.data.lock().unwrap().files {
        if path != "vault.json" && path != "head.json" {
            assert!(!String::from_utf8_lossy(&file.bytes).contains("UnknownFutureFavorite"));
            assert!(!String::from_utf8_lossy(&file.bytes).contains("original"));
        }
    }
    let local_state = fs::read(temp.path().join("a/state.enc")).unwrap();
    assert!(!String::from_utf8_lossy(&local_state).contains("UnknownFutureFavorite"));
    assert!(!format!("{key:?}").contains(&key.to_hex()));
}

#[test]
fn three_offline_devices_merge_disjoint_edits_without_reuploading_blobs() {
    let temp = tempfile::tempdir().unwrap();
    let remote = Memory::default();
    let key = RecoveryKey::generate().unwrap();
    let mut a = engine(&temp.path().join("a"), &key);
    a.create(&remote).unwrap();
    let baseline = applied(&mut a, &remote, &initial()).snapshot;
    let mut b = engine(&temp.path().join("b"), &key);
    b.join(&remote).unwrap();
    applied(&mut b, &remote, &Snapshot::default());
    let mut c = engine(&temp.path().join("c"), &key);
    c.join(&remote).unwrap();
    applied(&mut c, &remote, &Snapshot::default());
    let mut edit_a = baseline.clone();
    edit_a.entries.get_mut("favor/first").unwrap()["name"] = json!("A offline");
    applied(&mut a, &remote, &edit_a);
    let mut edit_b = baseline.clone();
    edit_b.entries.get_mut("favor/second").unwrap()["name"] = json!("B offline");
    let merged = applied(&mut b, &remote, &edit_b);
    assert!(merged.conflicts.is_empty());
    let received = applied(&mut c, &remote, &baseline);
    assert_eq!(
        received.snapshot.entries["favor/first"]["name"],
        "A offline"
    );
    assert_eq!(
        received.snapshot.entries["favor/second"]["name"],
        "B offline"
    );
    let again = applied(&mut c, &remote, &received.snapshot);
    assert!(!again.report.published);
    assert_eq!(again.report.uploaded_objects, 0);
}

#[test]
fn same_record_conflict_keeps_both_versions_and_explicit_resolution() {
    let temp = tempfile::tempdir().unwrap();
    let remote = Memory::default();
    let key = RecoveryKey::generate().unwrap();
    let mut a = engine(&temp.path().join("a"), &key);
    a.create(&remote).unwrap();
    let baseline = applied(&mut a, &remote, &initial()).snapshot;
    let mut b = engine(&temp.path().join("b"), &key);
    b.join(&remote).unwrap();
    applied(&mut b, &remote, &Snapshot::default());
    let mut left = baseline.clone();
    left.entries.get_mut("favor/first").unwrap()["name"] = json!("left");
    applied(&mut a, &remote, &left);
    let mut right = baseline.clone();
    right.entries.get_mut("favor/first").unwrap()["name"] = json!("right");
    let conflicted = b.prepare(&remote, &right).unwrap();
    assert_eq!(conflicted.conflicts.len(), 1);
    let conflict = &conflicted.conflicts[0];
    let names: BTreeSet<_> = [
        conflict.left_value.as_ref().unwrap()["name"]
            .as_str()
            .unwrap(),
        conflict.right_value.as_ref().unwrap()["name"]
            .as_str()
            .unwrap(),
    ]
    .into();
    assert_eq!(names, BTreeSet::from(["left", "right"]));
    assert!(matches!(
        b.prepare(&remote, &right),
        Err(SyncError::PendingApply)
    ));
    b.abandon_pending(&conflicted.token).unwrap();
    let chosen = if conflict.left_value.as_ref().unwrap()["name"] == "right" {
        &conflict.left_revision
    } else {
        &conflict.right_revision
    };
    let resolved = b.resolve(&remote, &conflict.id, chosen).unwrap();
    assert!(resolved.conflicts.is_empty());
    assert_eq!(resolved.snapshot.entries["favor/first"]["name"], "right");
    b.acknowledge(&resolved.token).unwrap();
}

#[test]
fn deletion_conflicting_with_offline_edit_retains_tombstone_and_edit() {
    let temp = tempfile::tempdir().unwrap();
    let remote = Memory::default();
    let key = RecoveryKey::generate().unwrap();
    let mut a = engine(&temp.path().join("a"), &key);
    a.create(&remote).unwrap();
    let baseline = applied(&mut a, &remote, &initial()).snapshot;
    let mut b = engine(&temp.path().join("b"), &key);
    b.join(&remote).unwrap();
    applied(&mut b, &remote, &Snapshot::default());
    let mut deleted = baseline.clone();
    deleted.entries.remove("favor/first");
    applied(&mut a, &remote, &deleted);
    let mut edited = baseline.clone();
    edited.entries.get_mut("favor/first").unwrap()["name"] = json!("precious offline edit");
    let conflict = b.prepare(&remote, &edited).unwrap();
    assert_eq!(conflict.conflicts.len(), 1);
    let record = &conflict.conflicts[0];
    assert_ne!(record.left_value.is_none(), record.right_value.is_none());
    let retained = record
        .left_value
        .as_ref()
        .or(record.right_value.as_ref())
        .unwrap();
    assert_eq!(retained["name"], "precious offline edit");
}

#[test]
fn resolution_rebase_keeps_new_local_edits_without_reopening_the_resolved_alternative() {
    let temp = tempfile::tempdir().unwrap();
    let remote = Memory::default();
    let key = RecoveryKey::generate().unwrap();
    let mut a = engine(&temp.path().join("a"), &key);
    a.create(&remote).unwrap();
    let baseline = applied(&mut a, &remote, &initial()).snapshot;
    let mut b = engine(&temp.path().join("b"), &key);
    b.join(&remote).unwrap();
    applied(&mut b, &remote, &Snapshot::default());
    let mut remote_edit = baseline.clone();
    remote_edit.entries.get_mut("favor/first").unwrap()["name"] = json!("chosen remote");
    applied(&mut a, &remote, &remote_edit);
    let mut local_edit = baseline;
    local_edit.entries.get_mut("favor/first").unwrap()["name"] =
        json!("original local alternative");
    let conflicted = b.prepare(&remote, &local_edit).unwrap();
    let conflict = conflicted.conflicts[0].clone();
    b.abandon_pending(&conflicted.token).unwrap();
    let selected = if conflict.left_value.as_ref().unwrap()["name"] == "chosen remote" {
        &conflict.left_revision
    } else {
        &conflict.right_revision
    };
    let resolved = b.resolve(&remote, &conflict.id, selected).unwrap();
    b.abandon_pending(&resolved.token).unwrap();
    // The current database still has the original local alternative, plus
    // unrelated edits made while the user was resolving the conflict.
    local_edit.entries.get_mut("favor/second").unwrap()["name"] = json!("new unrelated local edit");
    let rebased = b.prepare(&remote, &local_edit).unwrap();
    assert!(rebased.conflicts.is_empty());
    assert_eq!(
        rebased.snapshot.entries["favor/first"]["name"],
        "chosen remote"
    );
    assert_eq!(
        rebased.snapshot.entries["favor/second"]["name"],
        "new unrelated local edit"
    );
    b.abandon_pending(&rebased.token).unwrap();
    local_edit.entries.get_mut("favor/first").unwrap()["name"] =
        json!("genuinely new edit after resolution");
    let new_conflict = b.prepare(&remote, &local_edit).unwrap();
    assert_eq!(new_conflict.conflicts.len(), 1);
    let conflict = &new_conflict.conflicts[0];
    assert!([&conflict.left_value, &conflict.right_value]
        .iter()
        .any(|value| value.as_ref().unwrap()["name"] == "genuinely new edit after resolution"));
}

#[test]
fn pending_apply_survives_restart_and_recovers_without_network() {
    let temp = tempfile::tempdir().unwrap();
    let remote = Memory::default();
    let key = RecoveryKey::generate().unwrap();
    let path = temp.path().join("a");
    let mut a = engine(&path, &key);
    a.create(&remote).unwrap();
    let prepared = a.prepare(&remote, &initial()).unwrap();
    drop(a);
    let mut a = engine(&path, &key);
    let recovered = a.pending().unwrap().unwrap();
    assert_eq!(recovered.token, prepared.token);
    assert_eq!(recovered.snapshot, prepared.snapshot);
    assert!(matches!(
        a.acknowledge("wrong"),
        Err(SyncError::InvalidAcknowledgement)
    ));
    a.acknowledge(&recovered.token).unwrap();
    drop(a);
    let mut a = engine(&path, &key);
    assert!(a.pending().unwrap().is_none());
    let unchanged = applied(&mut a, &remote, &prepared.snapshot);
    assert!(!unchanged.report.published);
}

#[test]
fn resolving_an_older_conflict_preserves_a_later_edit_of_its_projection() {
    let temp = tempfile::tempdir().unwrap();
    let remote = Memory::default();
    let key = RecoveryKey::generate().unwrap();
    let mut a = engine(&temp.path().join("a"), &key);
    a.create(&remote).unwrap();
    let baseline = applied(&mut a, &remote, &initial()).snapshot;
    let mut b = engine(&temp.path().join("b"), &key);
    b.join(&remote).unwrap();
    applied(&mut b, &remote, &Snapshot::default());
    let mut left = baseline.clone();
    left.entries.get_mut("favor/first").unwrap()["name"] = json!("left original");
    applied(&mut a, &remote, &left);
    let mut right = baseline;
    right.entries.get_mut("favor/first").unwrap()["name"] = json!("right original");
    let first = applied(&mut b, &remote, &right);
    let conflict = first.conflicts[0].clone();
    let mut later = first.snapshot;
    later.entries.get_mut("favor/first").unwrap()["name"] = json!("later valuable edit");
    applied(&mut b, &remote, &later);
    let resolved = b
        .resolve(&remote, &conflict.id, &conflict.left_revision)
        .unwrap();
    assert_eq!(resolved.conflicts.len(), 1);
    let next = &resolved.conflicts[0];
    assert!([&next.left_value, &next.right_value]
        .iter()
        .any(|value| value.as_ref().unwrap()["name"] == "later valuable edit"));
    assert_ne!(next.id, conflict.id);
}

#[test]
fn lost_publish_and_object_responses_are_read_back_without_duplicate_changes() {
    let temp = tempfile::tempdir().unwrap();
    let remote = Memory::default();
    let key = RecoveryKey::generate().unwrap();
    let mut a = engine(temp.path(), &key);
    a.create(&remote).unwrap();
    {
        let mut data = remote.data.lock().unwrap();
        data.lose_replace_response = true;
        data.lose_object_response = true;
    }
    let prepared = applied(&mut a, &remote, &initial());
    assert_eq!(prepared.snapshot, initial());
    assert!(prepared.conflicts.is_empty());
    let count = remote.data.lock().unwrap().files.len();
    let again = applied(&mut a, &remote, &prepared.snapshot);
    assert!(!again.report.published);
    assert_eq!(remote.data.lock().unwrap().files.len(), count);
}

#[test]
fn missing_collection_or_wrong_key_cannot_clear_local_or_create_another_vault() {
    let temp = tempfile::tempdir().unwrap();
    let remote = Memory::default();
    let key = RecoveryKey::generate().unwrap();
    let mut a = engine(&temp.path().join("a"), &key);
    a.create(&remote).unwrap();
    let baseline = applied(&mut a, &remote, &initial()).snapshot;
    let mut wrong = engine(
        &temp.path().join("wrong"),
        &RecoveryKey::generate().unwrap(),
    );
    assert!(matches!(wrong.join(&remote), Err(SyncError::KeyMismatch)));
    assert!(wrong.collection_id().is_none());
    let files = remote.data.lock().unwrap().files.len();
    let empty = Memory::default();
    assert!(matches!(
        a.prepare(&empty, &baseline),
        Err(SyncError::MissingRemote)
    ));
    assert!(empty.data.lock().unwrap().files.is_empty());
    assert_eq!(remote.data.lock().unwrap().files.len(), files);
    assert!(!a.has_pending_apply());
}

#[test]
fn tampered_attachment_never_becomes_a_prepared_local_apply() {
    let temp = tempfile::tempdir().unwrap();
    let remote = Memory::default();
    let key = RecoveryKey::generate().unwrap();
    let mut a = engine(&temp.path().join("a"), &key);
    a.create(&remote).unwrap();
    applied(&mut a, &remote, &initial());
    {
        let mut data = remote.data.lock().unwrap();
        let (_, file) = data
            .files
            .iter_mut()
            .find(|(path, _)| path.starts_with("blobs/"))
            .unwrap();
        *file.bytes.last_mut().unwrap() ^= 1;
    }
    let mut b = engine(&temp.path().join("b"), &key);
    b.join(&remote).unwrap();
    assert!(matches!(
        b.prepare(&remote, &Snapshot::default()),
        Err(SyncError::KeyMismatch)
    ));
    assert!(!b.has_pending_apply());
    assert!(b.state.base_commit.is_none());
}

#[test]
fn scope_preserves_unknown_or_unsubscribed_remote_settings_after_acknowledgement() {
    let temp = tempfile::tempdir().unwrap();
    let remote = Memory::default();
    let key = RecoveryKey::generate().unwrap();
    let mut a = engine(&temp.path().join("a"), &key);
    a.create(&remote).unwrap();
    let mut baseline = initial();
    baseline
        .entries
        .insert("future/dataset".into(), json!({"future":true}));
    baseline
        .entries
        .insert("setting/private".into(), json!("do not delete"));
    applied(&mut a, &remote, &baseline);
    let mut b = engine(&temp.path().join("b"), &key);
    b.join(&remote).unwrap();
    let scopes = vec!["favor/".into(), "roots".into()];
    let first = b
        .prepare_scoped(&remote, &Snapshot::default(), &scopes)
        .unwrap();
    b.acknowledge(&first.token).unwrap();
    let local = Snapshot {
        entries: first
            .snapshot
            .entries
            .iter()
            .filter(|(key, _)| in_scope(key, &scopes))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
        blobs: first.snapshot.blobs,
    };
    let next = b.prepare_scoped(&remote, &local, &scopes).unwrap();
    assert_eq!(
        next.snapshot.entries["future/dataset"],
        json!({"future":true})
    );
    assert_eq!(next.snapshot.entries["setting/private"], "do not delete");
    assert!(!next.report.published);
}

#[test]
fn concurrent_compare_and_swap_retry_merges_the_winning_commit_first() {
    struct Race<'a> {
        remote: &'a Memory,
        callback: RefCell<Option<Box<dyn FnOnce() + 'a>>>,
    }
    impl RemoteStore for Race<'_> {
        fn get(&self, path: &str) -> Result<Option<RemoteFile>, SyncError> {
            self.remote.get(path)
        }
        fn create(&self, path: &str, bytes: &[u8]) -> Result<bool, SyncError> {
            self.remote.create(path, bytes)
        }
        fn replace(&self, path: &str, etag: &str, bytes: &[u8]) -> Result<bool, SyncError> {
            if let Some(callback) = self.callback.borrow_mut().take() {
                callback();
            }
            self.remote.replace(path, etag, bytes)
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let remote = Memory::default();
    let key = RecoveryKey::generate().unwrap();
    let mut a = engine(&temp.path().join("a"), &key);
    a.create(&remote).unwrap();
    let baseline = applied(&mut a, &remote, &initial()).snapshot;
    let mut b = engine(&temp.path().join("b"), &key);
    b.join(&remote).unwrap();
    applied(&mut b, &remote, &Snapshot::default());
    let mut b_snapshot = baseline.clone();
    b_snapshot.entries.get_mut("favor/second").unwrap()["name"] = json!("concurrent winner");
    let race = Race {
        remote: &remote,
        callback: RefCell::new(Some(Box::new(|| {
            applied(&mut b, &remote, &b_snapshot);
        }))),
    };
    let mut a_snapshot = baseline;
    a_snapshot.entries.get_mut("favor/first").unwrap()["name"] = json!("CAS retry");
    let prepared = applied(&mut a, &race, &a_snapshot);
    assert!(prepared.conflicts.is_empty());
    assert_eq!(
        prepared.snapshot.entries["favor/first"]["name"],
        "CAS retry"
    );
    assert_eq!(
        prepared.snapshot.entries["favor/second"]["name"],
        "concurrent winner"
    );
}

#[test]
fn chunks_are_content_addressed_and_invalid_digests_are_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let remote = Memory::default();
    let key = RecoveryKey::generate().unwrap();
    let mut a = engine(temp.path(), &key);
    a.create(&remote).unwrap();
    let mut snapshot = Snapshot::default();
    snapshot
        .entries
        .insert("favor/file".into(), json!({"file":"large"}));
    let mut bytes = vec![19u8; CHUNK_BYTES];
    bytes.extend_from_slice(&vec![22u8; 8192]);
    snapshot.blobs.insert(blob_digest(&bytes), bytes);
    let result = applied(&mut a, &remote, &snapshot);
    assert_eq!(result.snapshot, snapshot);
    let blobs = remote
        .data
        .lock()
        .unwrap()
        .files
        .keys()
        .filter(|path| path.starts_with("blobs/"))
        .count();
    assert_eq!(blobs, 2);
    let mut invalid = snapshot;
    invalid
        .blobs
        .insert("0".repeat(64), b"wrong digest".to_vec());
    assert!(matches!(
        a.prepare(&remote, &invalid),
        Err(SyncError::InvalidData("blob_digest"))
    ));
}

#[test]
fn observed_remote_rollback_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let remote = Memory::default();
    let key = RecoveryKey::generate().unwrap();
    let mut a = engine(temp.path(), &key);
    a.create(&remote).unwrap();
    let old_head = remote.get("head.json").unwrap().unwrap();
    let baseline = applied(&mut a, &remote, &initial()).snapshot;
    remote
        .data
        .lock()
        .unwrap()
        .files
        .insert("head.json".into(), old_head);
    assert!(matches!(
        a.prepare(&remote, &baseline),
        Err(SyncError::InvalidData("remote_rollback"))
    ));
    assert!(!a.has_pending_apply());
}

#[test]
fn interrupted_initialization_resumes_only_its_own_staged_vault() {
    let temp = tempfile::tempdir().unwrap();
    let remote = Memory::default();
    remote.data.lock().unwrap().fail_initial_head_create = true;
    let key = RecoveryKey::generate().unwrap();
    let path = temp.path().join("creator");
    let mut creator = engine(&path, &key);
    assert!(matches!(
        creator.create(&remote),
        Err(SyncError::Transport("interrupted"))
    ));
    let collection = creator.collection_id().unwrap().to_string();
    assert!(remote.get("vault.json").unwrap().is_some());
    assert!(remote.get("head.json").unwrap().is_none());
    let mut other = engine(&temp.path().join("other"), &key);
    assert!(matches!(other.join(&remote), Err(SyncError::MissingRemote)));
    assert!(other.collection_id().is_none());
    drop(creator);
    let mut creator = engine(&path, &key);
    assert_eq!(creator.create(&remote).unwrap(), collection);
    assert!(remote.get("head.json").unwrap().is_some());
    assert_eq!(
        applied(&mut creator, &remote, &initial()).snapshot,
        initial()
    );
}

#[test]
fn weak_head_etag_is_not_silently_downgraded_to_unconditional_put() {
    let temp = tempfile::tempdir().unwrap();
    let remote = Memory::default();
    let key = RecoveryKey::generate().unwrap();
    let mut a = engine(temp.path(), &key);
    a.create(&remote).unwrap();
    remote
        .data
        .lock()
        .unwrap()
        .files
        .get_mut("head.json")
        .unwrap()
        .etag = "W/\"weak\"".into();
    let before = remote.data.lock().unwrap().files.len();
    assert!(matches!(
        a.prepare(&remote, &initial()),
        Err(SyncError::InvalidData("strong_etag_required"))
    ));
    assert!(!a.has_pending_apply());
    assert_eq!(remote.data.lock().unwrap().files.len(), before);
}

#[test]
fn business_validator_rejects_an_invalid_merged_closure_before_publishing_head() {
    fn validate_graph(snapshot: &Snapshot) -> Result<(), SyncError> {
        for (key, value) in &snapshot.entries {
            if !key.starts_with("favor/") {
                continue;
            }
            if let Some(parent) = value.get("parent").and_then(Value::as_str) {
                if !snapshot.entries.contains_key(&format!("favor/{parent}")) {
                    return Err(SyncError::InvalidData("favorite_graph"));
                }
            }
        }
        Ok(())
    }
    let temp = tempfile::tempdir().unwrap();
    let remote = Memory::default();
    let key = RecoveryKey::generate().unwrap();
    let mut a = engine(&temp.path().join("a"), &key);
    a.create(&remote).unwrap();
    let baseline = Snapshot {
        entries: BTreeMap::from([
            ("favor/folder".into(), json!({"parent":null,"folder":true})),
            ("roots".into(), json!(["folder"])),
        ]),
        blobs: BTreeMap::new(),
    };
    applied(&mut a, &remote, &baseline);
    let mut b = engine(&temp.path().join("b"), &key);
    b.join(&remote).unwrap();
    applied(&mut b, &remote, &Snapshot::default());
    let scopes = vec!["favor/".into(), "roots".into()];
    let deleted = Snapshot {
        entries: BTreeMap::from([("roots".into(), json!([]))]),
        blobs: BTreeMap::new(),
    };
    validate_graph(&deleted).unwrap();
    let a_change = a
        .prepare_scoped_validated(&remote, &deleted, &scopes, validate_graph)
        .unwrap();
    a.acknowledge(&a_change.token).unwrap();
    let head_before = remote.get("head.json").unwrap().unwrap();
    let mut child_added = baseline;
    child_added.entries.insert(
        "favor/child".into(),
        json!({"parent":"folder","text":"valuable new child"}),
    );
    validate_graph(&child_added).unwrap();
    assert!(matches!(
        b.prepare_scoped_validated(&remote, &child_added, &scopes, validate_graph),
        Err(SyncError::InvalidData("favorite_graph"))
    ));
    let head_after = remote.get("head.json").unwrap().unwrap();
    assert_eq!(head_after.bytes, head_before.bytes);
    assert_eq!(head_after.etag, head_before.etag);
    assert!(b.state.outbox.is_some());
    assert!(!b.has_pending_apply());
    // Once the user makes the local graph compatible, a normal retry succeeds
    // rather than being stuck behind a remotely published invalid graph.
    let retried = b
        .prepare_scoped_validated(&remote, &deleted, &scopes, validate_graph)
        .unwrap();
    assert!(retried.conflicts.is_empty());
    b.acknowledge(&retried.token).unwrap();
}

#[test]
fn rejected_conflict_choice_does_not_replace_the_unresolved_head() {
    let temp = tempfile::tempdir().unwrap();
    let remote = Memory::default();
    let key = RecoveryKey::generate().unwrap();
    let mut a = engine(&temp.path().join("a"), &key);
    a.create(&remote).unwrap();
    let baseline = applied(&mut a, &remote, &initial()).snapshot;
    let mut b = engine(&temp.path().join("b"), &key);
    b.join(&remote).unwrap();
    applied(&mut b, &remote, &Snapshot::default());
    let mut left = baseline.clone();
    left.entries.get_mut("favor/first").unwrap()["name"] = json!("left");
    applied(&mut a, &remote, &left);
    let mut right = baseline;
    right.entries.get_mut("favor/first").unwrap()["name"] = json!("right");
    let conflict = b.prepare(&remote, &right).unwrap();
    b.abandon_pending(&conflict.token).unwrap();
    let head_before = remote.get("head.json").unwrap().unwrap();
    let original = &conflict.conflicts[0];
    assert!(matches!(
        b.resolve_validated(&remote, &original.id, &original.left_revision, |_| Err(
            SyncError::InvalidData("business_invariant")
        )),
        Err(SyncError::InvalidData("business_invariant"))
    ));
    assert_eq!(
        remote.get("head.json").unwrap().unwrap().bytes,
        head_before.bytes
    );
    assert!(!b.has_pending_apply());
    let chosen = b
        .resolve_validated(&remote, &original.id, &original.left_revision, |_| Ok(()))
        .unwrap();
    assert!(chosen.conflicts.is_empty());
}

#[test]
fn invalid_conflict_projection_stays_exportable_until_the_final_valid_resolution() {
    fn graph(snapshot: &Snapshot) -> Result<(), SyncError> {
        for (key, value) in &snapshot.entries {
            if key.starts_with("favor/") {
                if let Some(parent) = value.get("parent").and_then(Value::as_str) {
                    if !snapshot.entries.contains_key(&format!("favor/{parent}")) {
                        return Err(SyncError::InvalidData("favorite_graph"));
                    }
                }
            }
        }
        if let Some(roots) = snapshot.entries.get("roots").and_then(Value::as_array) {
            for root in roots {
                if !root
                    .as_str()
                    .is_some_and(|id| snapshot.entries.contains_key(&format!("favor/{id}")))
                {
                    return Err(SyncError::InvalidData("favorite_graph"));
                }
            }
        }
        Ok(())
    }
    let temp = tempfile::tempdir().unwrap();
    let remote = Memory::default();
    let key = RecoveryKey::from_hex(&"01".repeat(32)).unwrap();
    let mut a = engine(&temp.path().join("a"), &key);
    // Fixed test collection identity makes the hash ordering deterministic.
    a.state.collection_id = Some("0123456789abcdef0123456789abcdef".into());
    a.state.initializing = true;
    a.persist().unwrap();
    a.create(&remote).unwrap();
    let baseline = Snapshot {
        entries: BTreeMap::from([
            (
                "favor/folder".into(),
                json!({"folder":true,"parent":null,"name":"original"}),
            ),
            ("favor/other".into(), json!({"folder":true,"parent":null})),
            ("roots".into(), json!(["folder", "other"])),
        ]),
        blobs: BTreeMap::new(),
    };
    applied(&mut a, &remote, &baseline);
    let mut b = engine(&temp.path().join("b"), &key);
    b.join(&remote).unwrap();
    applied(&mut b, &remote, &Snapshot::default());
    let scopes = vec!["favor/".into(), "roots".into()];
    let mut deleted = baseline.clone();
    deleted.entries.remove("favor/folder");
    deleted.entries.insert("roots".into(), json!(["other"]));
    let deletion = a
        .prepare_scoped_validated(&remote, &deleted, &scopes, graph)
        .unwrap();
    a.acknowledge(&deletion.token).unwrap();
    let (head, _) = b.read_head(&remote).unwrap();
    let current = b.read_commit(&remote, &head.commit_id).unwrap();
    let tombstone = &current.entries["favor/folder"];
    let base_id = b.state.base_commit.clone().unwrap();
    let old = b.read_commit(&remote, &base_id).unwrap();
    // Select a deterministic edit whose revision sorts after the tombstone, so
    // the conflict projection actually lacks the child's owner.
    let edited = (0..10_000)
        .find_map(|index| {
            let value = json!({"folder":true,"parent":null,"name":format!("edited {index}")});
            let revision = Revision {
                key: "favor/folder".into(),
                value: Some(value.clone()),
                parents: vec![old.entries["favor/folder"].clone()],
            };
            let id = b
                .object_id("revision", &json_bytes(&revision).unwrap())
                .unwrap();
            (id > *tombstone).then_some(value)
        })
        .expect("fixed fixture has a higher edit revision");
    let mut local = baseline;
    local.entries.insert("favor/folder".into(), edited);
    local.entries.insert(
        "favor/child".into(),
        json!({"parent":"folder","text":"valuable new child"}),
    );
    local
        .entries
        .insert("roots".into(), json!(["other", "folder"]));
    graph(&local).unwrap();
    let calls = std::cell::Cell::new(0usize);
    let mut validator = |snapshot: &Snapshot| {
        calls.set(calls.get() + 1);
        graph(snapshot)
    };
    let ticket = b
        .prepare_scoped_validated(&remote, &local, &scopes, &mut validator)
        .unwrap();
    assert_eq!(calls.get(), 0);
    assert_eq!(ticket.conflicts.len(), 2);
    assert!(!ticket.snapshot.entries.contains_key("favor/folder"));
    assert!(ticket.snapshot.entries.contains_key("favor/child"));
    assert!(graph(&ticket.snapshot).is_err());
    // An interrupted host can still export both original folder alternatives.
    let exported = b.pending().unwrap().unwrap();
    let folder = exported
        .conflicts
        .iter()
        .find(|conflict| conflict.key == "favor/folder")
        .unwrap();
    let roots = exported
        .conflicts
        .iter()
        .find(|conflict| conflict.key == "roots")
        .unwrap();
    let desired_roots = json!(["other", "folder"]);
    let root_choice = if roots.left_value.as_ref() == Some(&desired_roots) {
        &roots.left_revision
    } else {
        &roots.right_revision
    };
    b.abandon_pending(&ticket.token).unwrap();
    let partial = b
        .resolve_validated(&remote, &roots.id, root_choice, &mut validator)
        .unwrap();
    assert_eq!(calls.get(), 0);
    assert_eq!(partial.conflicts.len(), 1);
    assert!(graph(&partial.snapshot).is_err());
    b.abandon_pending(&partial.token).unwrap();
    let before = remote.get("head.json").unwrap().unwrap();
    let delete_choice = if folder.left_value.is_none() {
        &folder.left_revision
    } else {
        &folder.right_revision
    };
    assert!(matches!(
        b.resolve_validated(&remote, &folder.id, delete_choice, &mut validator),
        Err(SyncError::InvalidData("favorite_graph"))
    ));
    assert_eq!(
        remote.get("head.json").unwrap().unwrap().bytes,
        before.bytes
    );
    let live_choice = if folder.left_value.is_some() {
        &folder.left_revision
    } else {
        &folder.right_revision
    };
    let final_choice = b
        .resolve_validated(&remote, &folder.id, live_choice, &mut validator)
        .unwrap();
    assert!(final_choice.conflicts.is_empty());
    graph(&final_choice.snapshot).unwrap();
    assert_eq!(calls.get(), 2);
    b.acknowledge(&final_choice.token).unwrap();
}

#[cfg(windows)]
#[test]
fn local_atomic_replace_retries_sharing_violations_without_changing_staged_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("state.enc");
    fs::write(&target, b"old committed state").unwrap();
    let mut staged = tempfile::NamedTempFile::new_in(temp.path()).unwrap();
    staged.write_all(b"new encrypted state").unwrap();
    staged.as_file().sync_all().unwrap();
    let mut attempts = 0;
    persist_with_retry_using(staged, &target, |file, path| {
        attempts += 1;
        assert_eq!(fs::read(file.path()).unwrap(), b"new encrypted state");
        if attempts <= 2 {
            assert_eq!(fs::read(path).unwrap(), b"old committed state");
            Err(tempfile::PersistError {
                error: std::io::Error::from_raw_os_error(32),
                file,
            })
        } else {
            file.persist(path).map(|_| ())
        }
    })
    .unwrap();
    assert_eq!(attempts, 3);
    assert_eq!(fs::read(target).unwrap(), b"new encrypted state");
}

#[cfg(windows)]
#[test]
fn permanent_local_denial_is_bounded_and_retains_the_only_staged_candidate() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("state.enc");
    fs::write(&target, b"old committed state").unwrap();
    let mut staged = tempfile::NamedTempFile::new_in(temp.path()).unwrap();
    staged.write_all(b"new encrypted state").unwrap();
    staged.as_file().sync_all().unwrap();
    let staged_path = staged.path().to_path_buf();
    let mut attempts = 0;
    let denied = persist_with_retry_using(staged, &target, |file, _| {
        attempts += 1;
        Err(tempfile::PersistError {
            error: std::io::Error::from_raw_os_error(5),
            file,
        })
    })
    .unwrap_err();
    assert_eq!(attempts, LOCAL_REPLACE_RETRIES + 1);
    assert!(matches!(preserve_failed_replace(denied), SyncError::Io(_)));
    assert_eq!(fs::read(target).unwrap(), b"old committed state");
    assert_eq!(fs::read(staged_path).unwrap(), b"new encrypted state");
}
