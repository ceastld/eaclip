//! Disposable, display-only static history thumbnails. Call only from workers.
//!
//! The source owner validates its revision before and after this call; this
//! module never opens originals. Failed/unsafe cache IO cannot block an already
//! generated preview. The file format, size and algorithm identity are private.

use std::{
    collections::HashMap,
    fs::{self, File, Metadata, OpenOptions},
    io::{Cursor, Read},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, Weak},
    time::{SystemTime, UNIX_EPOCH},
};

use gpui::{Image, ImageFormat};

use crate::native_content::ContentImage;

const CACHE_DIRECTORY: &str = "history-v1";
const ALGORITHM: &[u8] = b"history-static-png-triangle-v1\0width=1024\0height=640\0";
const DISPLAY_ALGORITHM: &[u8] = b"history-display-png-triangle-v2\0width=1024\0height=";
const MAGIC: &[u8; 8] = b"CLTHMB01";
const HEADER_BYTES: usize = 88;
const MAX_ENTRY_BYTES: u64 = 4 * 1024 * 1024;
const MAX_THUMB_WIDTH: u32 = 1024;
const MAX_THUMB_HEIGHT: u32 = 640;
const UNSAFE_ATTRIBUTES: u32 = 0x400 | 0x1000 | 0x40000 | 0x400000;
const MISS: &str = "THUMB_CACHE_MISS";

type CacheResult<T> = Result<T, &'static str>;
type SharedResult = Result<ContentImage, String>;
type Gate = Mutex<Option<SharedResult>>;

#[derive(Clone, Copy)]
struct Limits {
    bytes: u64,
    items: usize,
}

const LIMITS: Limits = Limits {
    bytes: 256 * 1024 * 1024,
    items: 1024,
};

/// `root` is the selected data root, never a directory inferred from an image.
/// GIF/WebP payloads retain their existing animated representation in memory.
pub(crate) fn load_or_generate(
    root: &Path,
    source_version: &str,
    generate: impl FnOnce() -> SharedResult,
) -> SharedResult {
    load_or_generate_with(root, source_version, generate, LIMITS)
}

/// The list's thumbnail: stored at the size the row paints (at most 1024 wide
/// and `max_height_px` tall), never the 1024x640 encoded thumbnail. Each display
/// height owns its own disk entry, so a hit decodes only those few pixels.
/// `generate` should already return a PNG within those bounds; larger PNGs are
/// shrunk once here before being stored.
pub(crate) fn load_or_generate_display(
    root: &Path,
    source_version: &str,
    max_height_px: u32,
    generate: impl FnOnce() -> SharedResult,
) -> SharedResult {
    let height = max_height_px.clamp(1, MAX_THUMB_HEIGHT);
    let generate = move || generate().map(|image| shrink_to_display(image, height));
    load_or_generate_prepared(root, source_version, generate, LIMITS, Some(height))
}

/// Static PNGs larger than the display bounds are resized and re-encoded;
/// animations and already-small PNGs are returned unchanged.
fn shrink_to_display(image: ContentImage, height: u32) -> ContentImage {
    if image.render.is_some() || image.image.format() != ImageFormat::Png {
        return image;
    }
    let Ok((width, current)) = png_dimensions(image.image.bytes()) else { return image };
    if width <= MAX_THUMB_WIDTH && current <= height {
        return image;
    }
    let shrunk = png_reader(image.image.bytes()).ok()
        .and_then(|reader| reader.decode().ok())
        .and_then(|decoded| {
            let resized = decoded.thumbnail(decoded.width().min(MAX_THUMB_WIDTH), decoded.height().min(height));
            let mut bytes = Cursor::new(Vec::new());
            resized.write_to(&mut bytes, image::ImageFormat::Png).ok()?;
            Some(bytes.into_inner())
        });
    match shrunk {
        Some(bytes) => ContentImage {
            image: Arc::new(Image::from_bytes(ImageFormat::Png, bytes)),
            ..image
        },
        None => image,
    }
}

fn load_or_generate_with(
    root: &Path,
    source_version: &str,
    generate: impl FnOnce() -> SharedResult,
    limits: Limits,
) -> SharedResult {
    load_or_generate_prepared(root, source_version, generate, limits, None)
}

fn load_or_generate_prepared(
    root: &Path,
    source_version: &str,
    generate: impl FnOnce() -> SharedResult,
    limits: Limits,
    display_height: Option<u32>,
) -> SharedResult {
    if source_version.is_empty() {
        return generate().map(|image| prepare_generated(image, display_height));
    }
    let key = match display_height {
        Some(height) => display_cache_key(source_version, height),
        None => cache_key(source_version),
    };
    let gate = key_gate(root, &key);
    // Only callers for the same root/version wait here. No registry or quota
    // lock is retained across source reads, generation, or thumbnail decoding.
    let mut result = gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(result) = result.as_ref() {
        return result.clone();
    }
    let loaded = match DirectoryGuard::open(root, false)
        .and_then(|directory| read_entry_prepared(&directory.path.join(format!("{key}.thumb")), &key, display_height))
    {
        Ok(image) => Some(image),
        Err(MISS) => None,
        Err(category) => {
            tracing::debug!(category, "History thumbnail cache read skipped");
            None
        }
    };
    let current = loaded.map(Ok).unwrap_or_else(|| {
        let image = generate()?;
        // Static list bitmaps have already been normalized to PNG. Retaining
        // any other format avoids changing animation or codec behavior.
        if image.image.format() == ImageFormat::Png {
            if let Err(category) =
                encode_entry(&image, &key).and_then(|bytes| save_entry(root, &key, &bytes, limits))
            {
                tracing::debug!(category, "History thumbnail cache write skipped");
            }
        }
        Ok(prepare_generated(image, display_height))
    });
    // Concurrent waiters also share a generated result when disk is read-only,
    // full, unsafe, or when the generator returned an admitted animation.
    *result = Some(current.clone());
    current
}

fn prepare_generated(image: ContentImage, display_height: Option<u32>) -> ContentImage {
    match display_height {
        Some(height) => crate::native_content::display_from_list_image(image, height),
        None => image,
    }
}

fn cache_key(source_version: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(ALGORITHM);
    hasher.update(source_version.as_bytes());
    hasher.finalize().to_hex().to_string()
}

fn display_cache_key(source_version: &str, height: u32) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(DISPLAY_ALGORITHM);
    hasher.update(&height.to_le_bytes());
    hasher.update(source_version.as_bytes());
    hasher.finalize().to_hex().to_string()
}

fn key_gate(root: &Path, key: &str) -> Arc<Gate> {
    static GATES: OnceLock<Mutex<HashMap<PathBuf, Weak<Gate>>>> = OnceLock::new();
    let mut identity = normalized_root(root).join(key);
    #[cfg(windows)]
    {
        identity = PathBuf::from(identity.to_string_lossy().to_ascii_lowercase());
    }
    let mut gates = GATES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // Completed keys retain no image or strong reference between calls.
    gates.retain(|_, value| value.strong_count() != 0);
    if let Some(gate) = gates.get(&identity).and_then(Weak::upgrade) {
        return gate;
    }
    let gate = Arc::new(Mutex::new(None));
    gates.insert(identity, Arc::downgrade(&gate));
    gate
}

fn natural_dimensions(width: f32, height: f32) -> CacheResult<()> {
    if !width.is_finite()
        || !height.is_finite()
        || width < 1.0
        || height < 1.0
        || width > 16_384.0
        || height > 16_384.0
    {
        return Err("THUMB_CACHE_DIMENSIONS");
    }
    Ok(())
}

fn png_reader(bytes: &[u8]) -> CacheResult<image::ImageReader<Cursor<&[u8]>>> {
    if bytes.len() as u64 > MAX_ENTRY_BYTES || !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err("THUMB_CACHE_PNG");
    }
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), image::ImageFormat::Png);
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(8 * 1024 * 1024);
    limits.max_image_width = Some(MAX_THUMB_WIDTH);
    limits.max_image_height = Some(MAX_THUMB_HEIGHT);
    reader.limits(limits);
    Ok(reader)
}

fn png_dimensions(bytes: &[u8]) -> CacheResult<(u32, u32)> {
    let (width, height) = png_reader(bytes)?
        .into_dimensions()
        .map_err(|_| "THUMB_CACHE_PNG")?;
    if width == 0 || height == 0 || width > MAX_THUMB_WIDTH || height > MAX_THUMB_HEIGHT {
        return Err("THUMB_CACHE_DIMENSIONS");
    }
    Ok((width, height))
}

fn encode_entry(image: &ContentImage, key: &str) -> CacheResult<Vec<u8>> {
    natural_dimensions(image.width, image.height)?;
    let png = image.image.bytes();
    let total = HEADER_BYTES
        .checked_add(png.len())
        .ok_or("THUMB_CACHE_SIZE")?;
    if total as u64 > MAX_ENTRY_BYTES {
        return Err("THUMB_CACHE_SIZE");
    }
    // The generator owns a valid encoder output. Header inspection is enough
    // here; disk reads validate the complete bounded PNG before admitting it.
    let (width, height) = png_dimensions(png)?;
    if width as f32 > image.width || height as f32 > image.height {
        return Err("THUMB_CACHE_DIMENSIONS");
    }
    let mut bytes = Vec::with_capacity(total);
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&image.width.to_le_bytes());
    bytes.extend_from_slice(&image.height.to_le_bytes());
    bytes.extend_from_slice(&(png.len() as u64).to_le_bytes());
    bytes.extend_from_slice(blake3::hash(png).as_bytes());
    bytes.extend_from_slice(blake3::hash(key.as_bytes()).as_bytes());
    bytes.extend_from_slice(png);
    Ok(bytes)
}

#[cfg(test)]
fn read_entry(path: &Path, key: &str) -> CacheResult<ContentImage> {
    read_entry_prepared(path, key, None)
}

fn read_entry_prepared(path: &Path, key: &str, display_height: Option<u32>) -> CacheResult<ContentImage> {
    let named = fs::symlink_metadata(path).map_err(io_category)?;
    check_metadata(&named)?;
    if !named.is_file() || named.len() < HEADER_BYTES as u64 || named.len() > MAX_ENTRY_BYTES {
        return Err("THUMB_CACHE_SIZE");
    }
    // Pin metadata before a data open so placeholders are rejected without
    // hydrating them. OPEN_REPARSE_POINT also closes the leaf replacement gap.
    let _leaf_guard = pin(path, &named)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options
            .share_mode(1)
            .custom_flags(0x0020_0000 | 0x0010_0000);
    }
    let mut file = options.open(path).map_err(io_category)?;
    let opened = file.metadata().map_err(io_category)?;
    check_metadata(&opened)?;
    if stamp(&opened) != stamp(&named) {
        return Err("THUMB_CACHE_CHANGED");
    }
    let mut bytes = Vec::with_capacity(named.len() as usize);
    Read::by_ref(&mut file)
        .take(MAX_ENTRY_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(io_category)?;
    let after = fs::symlink_metadata(path).map_err(io_category)?;
    check_metadata(&after)?;
    if bytes.len() as u64 != named.len()
        || stamp(&after) != stamp(&named)
        || stamp(&file.metadata().map_err(io_category)?) != stamp(&named)
    {
        return Err("THUMB_CACHE_CHANGED");
    }
    if bytes.get(..8) != Some(MAGIC.as_slice()) {
        return Err("THUMB_CACHE_HEADER");
    }
    let width = f32::from_le_bytes(bytes[8..12].try_into().unwrap());
    let height = f32::from_le_bytes(bytes[12..16].try_into().unwrap());
    natural_dimensions(width, height)?;
    let length = u64::from_le_bytes(bytes[16..24].try_into().unwrap());
    if length != (bytes.len() - HEADER_BYTES) as u64
        || &bytes[56..88] != blake3::hash(key.as_bytes()).as_bytes()
    {
        return Err("THUMB_CACHE_HEADER");
    }
    let png = &bytes[HEADER_BYTES..];
    if &bytes[24..56] != blake3::hash(png).as_bytes() {
        return Err("THUMB_CACHE_DIGEST");
    }
    let (thumb_width, thumb_height) = png_dimensions(png)?;
    if thumb_width as f32 > width || thumb_height as f32 > height {
        return Err("THUMB_CACHE_DIMENSIONS");
    }
    // Fully validate once, then reuse those pixels for the display raster.
    // Merely trusting the digest/header would admit a damaged PNG payload.
    let decoded = png_reader(png)?.decode().map_err(|_| "THUMB_CACHE_PNG")?;
    if let Some(max_height_px) = display_height {
        return crate::native_content::display_image(&decoded, width, height, max_height_px)
            .ok_or("THUMB_CACHE_PNG");
    }
    Ok(ContentImage {
        image: Arc::new(Image::from_bytes(ImageFormat::Png, png.to_vec())),
        width,
        height,
        render: None,
    })
}

struct DirectoryGuard {
    path: PathBuf,
    _guards: Vec<File>,
}

impl DirectoryGuard {
    fn open(root: &Path, create: bool) -> CacheResult<Self> {
        check_path(root)?;
        // Core's strict publisher returns the lexical DOS spelling even when
        // its caller used a canonical verbatim disk path. Keep comparisons and
        // per-root gates on that same spelling; UNC/device paths stay refused.
        let root = normalized_root(root);
        let mut guards = Vec::new();
        for part in root.ancestors().collect::<Vec<_>>().into_iter().rev() {
            pin_directory(part, &mut guards)?;
        }
        let mut path = root;
        for name in ["Image", "Thumbnails", CACHE_DIRECTORY] {
            path.push(name);
            if create {
                match fs::create_dir(&path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(io_category(error)),
                }
            }
            pin_directory(&path, &mut guards)?;
        }
        Ok(Self {
            path,
            _guards: guards,
        })
    }
}

fn check_path(path: &Path) -> CacheResult<()> {
    if !path.is_absolute() {
        return Err("THUMB_CACHE_UNSAFE_PATH");
    }
    for part in path.components() {
        match part {
            Component::Normal(name) => {
                let name = name.to_string_lossy().to_ascii_lowercase();
                if name.starts_with("onedrive")
                    || matches!(name.as_str(), "dropbox" | "icloud")
                    || name.contains(':')
                    || name.ends_with([' ', '.'])
                {
                    return Err("THUMB_CACHE_UNSAFE_PATH");
                }
            }
            Component::RootDir => {}
            #[cfg(windows)]
            Component::Prefix(prefix) => {
                if !matches!(
                    prefix.kind(),
                    std::path::Prefix::Disk(_) | std::path::Prefix::VerbatimDisk(_)
                ) {
                    return Err("THUMB_CACHE_UNSAFE_PATH");
                }
            }
            _ => return Err("THUMB_CACHE_UNSAFE_PATH"),
        }
    }
    Ok(())
}

fn normalized_root(root: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        use std::path::Prefix;
        let mut path = PathBuf::new();
        for part in root.components() {
            match part {
                Component::Prefix(prefix) => match prefix.kind() {
                    Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => {
                        path.push(format!("{}:\\", drive as char));
                    }
                    _ => path.push(part.as_os_str()),
                },
                _ => path.push(part.as_os_str()),
            }
        }
        path
    }
    #[cfg(not(windows))]
    {
        root.to_owned()
    }
}

fn pin_directory(path: &Path, guards: &mut Vec<File>) -> CacheResult<()> {
    let metadata = fs::symlink_metadata(path).map_err(io_category)?;
    check_metadata(&metadata)?;
    if !metadata.is_dir() {
        return Err("THUMB_CACHE_UNSAFE_PATH");
    }
    if let Some(guard) = pin(path, &metadata)? {
        guards.push(guard);
    }
    Ok(())
}

fn pin(path: &Path, named: &Metadata) -> CacheResult<Option<File>> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        let guard = OpenOptions::new()
            .access_mode(0x80)
            .share_mode(1)
            .custom_flags(0x0020_0000 | 0x0200_0000 | 0x0010_0000)
            .open(path)
            .map_err(io_category)?;
        let metadata = guard.metadata().map_err(io_category)?;
        check_metadata(&metadata)?;
        if stamp(&metadata) != stamp(named) {
            return Err("THUMB_CACHE_CHANGED");
        }
        Ok(Some(guard))
    }
    #[cfg(not(windows))]
    {
        let _ = (path, named);
        Ok(None)
    }
}

fn check_metadata(metadata: &Metadata) -> CacheResult<()> {
    if metadata.file_type().is_symlink() {
        return Err("THUMB_CACHE_UNSAFE_PATH");
    }
    check_attributes(attributes(metadata))
}

fn check_attributes(flags: u32) -> CacheResult<()> {
    if flags & UNSAFE_ATTRIBUTES != 0 {
        return Err("THUMB_CACHE_UNSAFE_PATH");
    }
    Ok(())
}

fn attributes(metadata: &Metadata) -> u32 {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes()
    }
    #[cfg(not(windows))]
    {
        let _ = metadata;
        0
    }
}

fn stamp(metadata: &Metadata) -> (u64, Option<SystemTime>, Option<SystemTime>, u32) {
    (
        metadata.len(),
        metadata.modified().ok(),
        metadata.created().ok(),
        attributes(metadata),
    )
}

struct DiskEntry {
    path: PathBuf,
    bytes: u64,
    modified: SystemTime,
    stage: bool,
}

fn save_entry(root: &Path, key: &str, bytes: &[u8], limits: Limits) -> CacheResult<()> {
    // Only short quota enumeration and atomic publication are serialized.
    // Source reads, cache reads and decoding have already completed outside
    // this lock, so every admitted cold thumbnail can persist before returning.
    // Separate data roots maintain their independent quotas independently.
    let maintenance = key_gate(root, "quota-maintenance");
    let _maintenance = maintenance
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if limits.items == 0 || bytes.len() as u64 > limits.bytes {
        return Err("THUMB_CACHE_CAPACITY");
    }
    let directory = DirectoryGuard::open(root, true)?;
    let target = directory.path.join(format!("{key}.thumb"));
    // A previously corrupt/oversized entry is derived data belonging to this
    // key. Never follow or remove links, directories, or placeholder entries.
    remove_owned_file(&target)?;
    reserve_capacity(&directory.path, bytes.len() as u64, limits)?;
    let published = cliphost_core::storage_core::resources::publish_bytes(
        &directory.path,
        &format!("{key}.thumb"),
        bytes,
    )
    .map_err(|_| "THUMB_CACHE_PUBLISH")?;
    // A concurrent process may have won the exact filename. Core publishes
    // without overwriting; do not retain its collision alias as a second item.
    if published != target {
        remove_owned_file(&published)?;
    }
    Ok(())
}

fn remove_owned_file(path: &Path) -> CacheResult<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            check_metadata(&metadata)?;
            if !metadata.is_file() {
                return Err("THUMB_CACHE_UNSAFE_PATH");
            }
            fs::remove_file(path).map_err(io_category)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_category(error)),
    }
}

fn owned_name(name: &str) -> Option<bool> {
    if let Some(stem) = name.strip_suffix(".thumb") {
        let (key, collision) = stem
            .split_once('_')
            .map_or((stem, None), |(a, b)| (a, Some(b)));
        if key.len() == 64
            && key
                .bytes()
                .all(|v| v.is_ascii_hexdigit() && !v.is_ascii_uppercase())
            && collision.is_none_or(|suffix| {
                suffix.len() == 32 && suffix.bytes().all(|v| v.is_ascii_hexdigit())
            })
        {
            return Some(false);
        }
    }
    name.strip_prefix(".clip-image-")
        .and_then(|name| name.strip_suffix(".stage"))
        .filter(|suffix| !suffix.is_empty() && suffix.bytes().all(|v| v.is_ascii_alphanumeric()))
        .map(|_| true)
}

fn reserve_capacity(directory: &Path, incoming: u64, limits: Limits) -> CacheResult<()> {
    let mut entries = Vec::new();
    let mut total = 0u64;
    for (seen, entry) in fs::read_dir(directory).map_err(io_category)?.enumerate() {
        if seen >= 8192 {
            return Err("THUMB_CACHE_CAPACITY");
        }
        let entry = entry.map_err(io_category)?;
        let Some(stage) = entry.file_name().to_str().and_then(owned_name) else {
            continue;
        };
        let metadata = fs::symlink_metadata(entry.path()).map_err(io_category)?;
        check_metadata(&metadata)?;
        if !metadata.is_file() {
            return Err("THUMB_CACHE_UNSAFE_PATH");
        }
        total = total.saturating_add(metadata.len());
        entries.push(DiskEntry {
            path: entry.path(),
            bytes: metadata.len(),
            modified: metadata.modified().unwrap_or(UNIX_EPOCH),
            stage,
        });
        // The normal producer stops at 1024 items. Bound metadata retention if
        // a directory has been populated by an unrelated/external producer.
        if entries.len() > 4096 {
            return Err("THUMB_CACHE_CAPACITY");
        }
    }
    // Hits do not touch mtime. This eviction order avoids write IO on scrolling.
    entries.sort_by_key(|entry| entry.modified);
    let mut count = entries.len();
    for entry in entries {
        if total.saturating_add(incoming) <= limits.bytes && count < limits.items {
            break;
        }
        // Leave a different process's recent atomic stage alone. Crashed
        // stages eventually join the same bounded quota instead of leaking.
        if entry.stage && entry.modified.elapsed().unwrap_or_default().as_secs() < 3600 {
            continue;
        }
        remove_owned_file(&entry.path)?;
        total = total.saturating_sub(entry.bytes);
        count = count.saturating_sub(1);
    }
    if total.saturating_add(incoming) > limits.bytes || count >= limits.items {
        return Err("THUMB_CACHE_CAPACITY");
    }
    Ok(())
}

fn io_category(error: std::io::Error) -> &'static str {
    if error.kind() == std::io::ErrorKind::NotFound {
        MISS
    } else {
        "THUMB_CACHE_IO"
    }
}

#[cfg(test)]
#[path = "history_thumbnail_cache_tests.rs"]
mod tests;
