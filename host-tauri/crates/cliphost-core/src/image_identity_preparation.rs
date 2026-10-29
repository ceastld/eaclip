//! Prepare exact image comparisons without holding a database/history lock.
use std::io::Read;
use std::path::{Component, Path};

use parking_lot::Mutex;

use crate::clip_dedupe::RECENT_DUPLICATE_SCAN_COUNT;
use crate::model::{ClipKind, ClipRecord};
use crate::store::{AppStore, PreparedImageIdentities, StoreError};

// Bound warming on a capture; repeated files use the verified SHA cache.
const MAX_PREPARATION_BYTES: u64 = 64 * 1024 * 1024;
const MAX_PREPARATION_PIXELS: u64 = 80_000_000;

pub(crate) fn prepare(
    store: &Mutex<AppStore>,
    incoming: &ClipRecord,
) -> Result<PreparedImageIdentities, StoreError> {
    let mut prepared = PreparedImageIdentities::default();
    if incoming.kind != ClipKind::ImageClipItem {
        return Ok(prepared);
    }
    let candidates = store.lock().list_recent_by_create_time()?;
    let mut bytes_left = MAX_PREPARATION_BYTES;
    let mut pixels_left = MAX_PREPARATION_PIXELS;
    for item in std::iter::once(incoming).chain(candidates.iter().take(RECENT_DUPLICATE_SCAN_COUNT))
    {
        if item.kind != ClipKind::ImageClipItem {
            continue;
        }
        prepared.checked(item);
        if store
            .lock()
            .cached_image_identity(&item.content_hash)?
            .is_some()
        {
            continue;
        }
        if let Some(identity) = verified_identity(item, &mut bytes_left, &mut pixels_left) {
            store
                .lock()
                .cache_image_identity(&item.content_hash, &identity)?;
        }
    }
    Ok(prepared)
}

fn verified_identity(
    item: &ClipRecord,
    bytes_left: &mut u64,
    pixels_left: &mut u64,
) -> Option<String> {
    if item.content_hash.len() != 64 || !item.content_hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let path = Path::new(item.image_path.as_deref()?);
    if !safe_local_path(path) {
        return None;
    }
    let metadata = std::fs::symlink_metadata(path).ok()?;
    let limit = (crate::clipboard_sync::MAX_IMAGE_BYTES as u64).min(*bytes_left);
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > limit {
        return None;
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    *bytes_left = bytes_left.saturating_sub(bytes.len() as u64);
    if bytes.len() as u64 > limit
        || !crate::model::sha256_hex(&bytes).eq_ignore_ascii_case(&item.content_hash)
    {
        return None;
    }
    let format = image::guess_format(&bytes).ok()?;
    if format != image::ImageFormat::Png || !crate::image_identity::pixel_identity_png(&bytes) {
        return None;
    }
    let (width, height) = image::ImageReader::with_format(std::io::Cursor::new(&bytes), format)
        .into_dimensions()
        .ok()?;
    let pixels = u64::from(width) * u64::from(height);
    if pixels > *pixels_left {
        return None;
    }
    *pixels_left -= pixels;
    let key = crate::image_identity::image_key(&bytes).ok()?;
    key.starts_with("image-pixels-v1:").then_some(key)
}

fn safe_local_path(path: &Path) -> bool {
    let value = path.to_string_lossy();
    if !path.is_absolute() || value.starts_with("\\\\") || value.starts_with("//") {
        return false;
    }
    // Check each ancestor before opening a child: do not traverse junctions or
    // hydrate cloud placeholders while comparing recent image candidates.
    let mut current = std::path::PathBuf::new();
    for component in path.components() {
        if matches!(component, Component::ParentDir) {
            return false;
        }
        let name = component.as_os_str().to_string_lossy().to_ascii_lowercase();
        if name.starts_with("onedrive")
            || ["dropbox", "icloud", "iclouddrive"]
                .iter()
                .any(|cloud| name == *cloud)
        {
            return false;
        }
        current.push(component);
        if matches!(component, Component::Prefix(_)) {
            continue;
        }
        let Ok(metadata) = std::fs::symlink_metadata(&current) else {
            return false;
        };
        if metadata.file_type().is_symlink() {
            return false;
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if metadata.file_attributes() & (0x400 | 0x1000 | 0x40000 | 0x400000) != 0 {
                return false;
            }
        }
    }
    true
}

#[cfg(test)]
#[path = "image_identity_preparation_tests.rs"]
mod tests;
