//! Opt-in development synchronization. The application, rather than this crate,
//! owns business transactions and must acknowledge only after a durable apply.

mod crypto;
mod engine;
pub mod transport;

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

pub use crypto::RecoveryKey;
pub use engine::Engine;

/// All errors deliberately omit addresses, credentials, and object contents.
#[derive(Debug, Error)]
pub enum SyncError {
    #[error("WebDAV transport: {0}")]
    Transport(&'static str),
    #[error("Invalid synchronization data: {0}")]
    InvalidData(&'static str),
    #[error("Synchronization storage IO failed")]
    Io(#[from] std::io::Error),
    #[error("Synchronization data encoding failed")]
    Json(#[from] serde_json::Error),
    #[error("Collection has not been initialized")]
    NotInitialized,
    #[error("Collection is already initialized")]
    AlreadyInitialized,
    #[error("The collection recovery key does not match")]
    KeyMismatch,
    #[error("The expected remote collection or object is missing")]
    MissingRemote,
    #[error("Concurrent updates prevented publishing; local changes are retained")]
    Conflict,
    #[error("Synchronization resource limit exceeded: {0}")]
    LimitExceeded(&'static str),
    #[error("A local apply is pending; recover it before synchronizing again")]
    PendingApply,
    #[error("The pending apply acknowledgement does not match")]
    InvalidAcknowledgement,
}

#[derive(Clone)]
pub struct RemoteFile {
    pub bytes: Vec<u8>,
    pub etag: String,
}

impl fmt::Debug for RemoteFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteFile")
            .field("bytes", &self.bytes.len())
            .finish_non_exhaustive()
    }
}

/// Paths are product-relative, never absolute URLs. False is a failed
/// precondition, not a successful write. Implementations must not follow
/// cross-origin redirects or replace existing immutable keys.
pub trait RemoteStore {
    fn get(&self, path: &str) -> Result<Option<RemoteFile>, SyncError>;
    fn create(&self, path: &str, bytes: &[u8]) -> Result<bool, SyncError>;
    fn replace(&self, path: &str, etag: &str, bytes: &[u8]) -> Result<bool, SyncError>;
}

/// Business objects use stable keys such as `favor/<uuid>`, `roots`, and
/// `setting/<name>`. Blob keys MUST be the lowercase SHA-256 of their bytes.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub entries: BTreeMap<String, Value>,
    pub blobs: BTreeMap<String, Vec<u8>>,
}

impl fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Snapshot")
            .field("entries", &self.entries.len())
            .field("blobs", &self.blobs.len())
            .finish()
    }
}

/// Both original versions (including deletion) remain encrypted in the remote
/// commit and local object cache. Ordering is by revision identity, not clock.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConflictRecord {
    pub id: String,
    pub key: String,
    pub base_revision: Option<String>,
    pub left_revision: String,
    pub right_revision: String,
    pub left_value: Option<Value>,
    pub right_value: Option<Value>,
}

impl fmt::Debug for ConflictRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConflictRecord")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncReport {
    pub collection_id: String,
    pub commit_id: String,
    pub entries: usize,
    pub blobs: usize,
    pub conflicts: usize,
    pub uploaded_objects: usize,
    pub downloaded_objects: usize,
    pub published: bool,
}

#[derive(Clone)]
pub struct PreparedSync {
    pub token: String,
    /// When conflicts is nonempty this is only a deterministic projection for
    /// diagnostics, not an applicable graph. The host must retain its current
    /// business state until every conflict is resolved and validated.
    pub snapshot: Snapshot,
    pub conflicts: Vec<ConflictRecord>,
    pub report: SyncReport,
}

impl fmt::Debug for PreparedSync {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreparedSync")
            .field("report", &self.report)
            .finish_non_exhaustive()
    }
}

pub const MAX_ENTRIES: usize = 20_000;
pub const MAX_METADATA_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_SNAPSHOT_BYTES: usize = 256 * 1024 * 1024;
pub const CHUNK_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_ENCRYPTED_OBJECT_BYTES: usize = MAX_METADATA_BYTES + 64;

pub fn blob_digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    crypto::hex(&Sha256::digest(bytes))
}

pub(crate) fn valid_id(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

pub(crate) fn valid_entry_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 512
        && !key.chars().any(char::is_control)
        && !key
            .split('/')
            .any(|segment| segment == "." || segment == "..")
}

/// Canonical JSON is independent of map insertion order and workspace feature
/// unification (including serde_json/preserve_order).
pub(crate) fn canonical_json(value: &Value) -> Result<Vec<u8>, SyncError> {
    fn append(value: &Value, output: &mut Vec<u8>) -> Result<(), serde_json::Error> {
        match value {
            Value::Object(map) => {
                output.push(b'{');
                let sorted: BTreeMap<_, _> = map.iter().collect();
                for (index, (key, value)) in sorted.into_iter().enumerate() {
                    if index != 0 {
                        output.push(b',');
                    }
                    serde_json::to_writer(&mut *output, key)?;
                    output.push(b':');
                    append(value, output)?;
                }
                output.push(b'}');
            }
            Value::Array(array) => {
                output.push(b'[');
                for (index, value) in array.iter().enumerate() {
                    if index != 0 {
                        output.push(b',');
                    }
                    append(value, output)?;
                }
                output.push(b']');
            }
            _ => serde_json::to_writer(output, value)?,
        }
        Ok(())
    }
    let mut bytes = Vec::new();
    append(value, &mut bytes)?;
    Ok(bytes)
}
