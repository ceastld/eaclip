//! Exact, precomputed image identities. No filesystem or decoder runs here.
use super::{AppStore, StoreError};
use crate::{
    clip_dedupe::RECENT_DUPLICATE_SCAN_COUNT,
    model::{ClipKind, ClipRecord},
};
use std::collections::HashSet;

const CACHE_PREFIX: &str = "image-identity:v2:sha256:";
const PIXEL_PREFIX: &str = "image-pixels-v1:";

/// A preparation attempt also records inaccessible/unsupported candidates so
/// they do not force endless retries. The token includes the row revision and
/// payload locator; an edited or newly inserted external row must be prepared
/// again unless its verified file SHA already has a cached identity.
#[derive(Debug, Default, Clone)]
pub(crate) struct PreparedImageIdentities {
    checked: HashSet<String>,
}

impl PreparedImageIdentities {
    pub(crate) fn checked(&mut self, item: &ClipRecord) {
        self.checked.insert(candidate_token(item));
    }

    fn contains(&self, item: &ClipRecord) -> bool {
        self.checked.contains(&candidate_token(item))
    }
}

fn candidate_token(item: &ClipRecord) -> String {
    serde_json::json!([
        item.id,
        item.storage_revision,
        item.update_time,
        item.content_hash.to_ascii_lowercase(),
        item.image_path,
    ])
    .to_string()
}

fn sha(value: &str) -> Option<String> {
    (value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| value.to_ascii_lowercase())
}

fn pixel_key(value: &str) -> Option<String> {
    sha(value.strip_prefix(PIXEL_PREFIX)?).map(|hash| format!("{PIXEL_PREFIX}{hash}"))
}

impl AppStore {
    pub(crate) fn cached_image_identity(
        &self,
        file_sha: &str,
    ) -> Result<Option<String>, StoreError> {
        let Some(file_sha) = sha(file_sha) else {
            return Ok(None);
        };
        Ok(self
            .kv_get(&format!("{CACHE_PREFIX}{file_sha}"))?
            .and_then(|value| pixel_key(&value)))
    }

    /// Call only after the bytes' actual SHA and decoded identity are verified.
    pub(crate) fn cache_image_identity(
        &self,
        file_sha: &str,
        identity: &str,
    ) -> Result<(), StoreError> {
        let file_sha = sha(file_sha)
            .ok_or_else(|| StoreError::Core("Invalid image identity file SHA.".into()))?;
        let identity = pixel_key(identity)
            .ok_or_else(|| StoreError::Core("Invalid exact image pixel identity.".into()))?;
        if let Some(previous) = self.cached_image_identity(&file_sha)? {
            if previous != identity {
                return Err(StoreError::Core(
                    "Conflicting exact identities for the same image SHA.".into(),
                ));
            }
            return Ok(());
        }
        self.kv_set(&format!("{CACHE_PREFIX}{file_sha}"), &identity)
    }

    pub(super) fn images_equal_exact(
        &self,
        a: &ClipRecord,
        b: &ClipRecord,
    ) -> Result<bool, StoreError> {
        if a.kind != ClipKind::ImageClipItem || b.kind != ClipKind::ImageClipItem {
            return Ok(false);
        }
        if !a.content_hash.is_empty() && a.content_hash.eq_ignore_ascii_case(&b.content_hash) {
            return Ok(true);
        }
        Ok(
            match (
                self.cached_image_identity(&a.content_hash)?,
                self.cached_image_identity(&b.content_hash)?,
            ) {
                (Some(a), Some(b)) => a == b,
                _ => false,
            },
        )
    }

    pub(super) fn guard_prepared_image_candidates(
        &self,
        incoming: &ClipRecord,
        recent: &[ClipRecord],
        prepared: &PreparedImageIdentities,
        hidden: &impl Fn(&str) -> bool,
    ) -> Result<(), StoreError> {
        if incoming.kind != ClipKind::ImageClipItem
            || self
                .cached_image_identity(&incoming.content_hash)?
                .is_none()
        {
            return Ok(());
        }
        for candidate in recent
            .iter()
            .take(RECENT_DUPLICATE_SCAN_COUNT)
            .filter(|candidate| candidate.kind == ClipKind::ImageClipItem && !hidden(&candidate.id))
        {
            if candidate
                .content_hash
                .eq_ignore_ascii_case(&incoming.content_hash)
                || self
                    .cached_image_identity(&candidate.content_hash)?
                    .is_some()
                || prepared.contains(candidate)
            {
                continue;
            }
            return Err(StoreError::ImageIdentityPreparationNeeded);
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "store_image_identity_tests.rs"]
mod tests;
