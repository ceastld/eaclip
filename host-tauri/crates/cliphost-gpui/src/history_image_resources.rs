//! Display-only primary history images. Run both probe and load on workers.
//!
//! Probe reads bounded headers, not decoded pixels. A visible-row load checks
//! that exact source version again and publishes ContentResources to the normal
//! ListContentCache (which owns ImageHolder leases and shared byte admission).
//! Never pass these resized bytes to copy, edit, export or native hover.
//! The 32 MiB encoded-input cap is a display fallback only; Core's original
//! image admission/read APIs and payloads are deliberately unchanged.

use std::{
    fs::{self, File, Metadata, OpenOptions},
    io::{Cursor, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::{Instant, UNIX_EPOCH},
};

use cliphost_core::{dto::ClipboardItemDto, host::HostState};

use crate::native_content::{
    self,
    model::{ContentProjection, MixedPart},
    ContentResources,
};

pub const PRIMARY_IMAGE_RESOURCE: &str = "history-primary-image";
const MAX_SOURCE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_PROBE_BYTES: usize = 256 * 1024;
const UNSAFE_ATTRIBUTES: u32 = 0x400 | 0x1000 | 0x40000 | 0x400000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryImageSource {
    pub source_version: String,
    pub width: u32,
    pub height: u32,
    pub size_bytes: u64,
    header_bytes_read: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryImageFailure {
    TemporarilyUnavailable,
    SourceChanged,
    InvalidSource,
    DecodeOrLimit,
}

impl HistoryImageFailure {
    pub fn is_retryable(self) -> bool {
        matches!(self, Self::TemporarilyUnavailable | Self::SourceChanged)
    }

    fn from_code(code: &str) -> Self {
        match code {
            "HISTORY_IMAGE_SOURCE_CHANGED" => Self::SourceChanged,
            "HISTORY_IMAGE_PATH" | "HISTORY_IMAGE_UNSAFE_PATH" | "HISTORY_IMAGE_SIZE" => Self::InvalidSource,
            "HISTORY_IMAGE_DECODE" => Self::DecodeOrLimit,
            _ => Self::TemporarilyUnavailable,
        }
    }
}

pub struct HistoryImageLoad {
    pub resources: ContentResources,
    pub thumbnail: Option<crate::preview::PreviewThumbnail>,
    pub failure: Option<HistoryImageFailure>,
}

/// Pure cache identity. The caller supplies the background probe's version;
/// unknown reloads must advance the page rather than retain a previous image.
pub fn primary_image_projection(
    item: &ClipboardItemDto,
    source_version: &str,
) -> Option<ContentProjection> {
    if item.kind != "ImageClipItem" {
        return None;
    }
    let mut key = blake3::Hasher::new();
    key.update(b"history-primary-image-v2:1024x640\0");
    key.update(item.id.as_bytes());
    key.update(b"\0");
    key.update(source_version.as_bytes());
    Some(ContentProjection::Mixed(vec![MixedPart::Image {
        path: Some(format!("@history-primary:{}", key.finalize().to_hex())),
    }]))
}

/// Resolve through Core's existing managed-image admission on a worker. The
/// file is still independently checked before this module opens its data.
pub fn load_primary_image_resources(
    host: &HostState,
    id: &str,
    expected_source_version: &str,
) -> ContentResources {
    if expected_source_version.is_empty() {
        return unavailable();
    }
    host.hover_image_file(id, false)
        .map_err(|_| "HISTORY_IMAGE_UNAVAILABLE".to_string())
        .and_then(|path| load_cached_path_resources(&host.data_root(), &path, expected_source_version, None)
            .map(|(resources, _)| resources))
        .unwrap_or_else(|_| unavailable())
}

/// A visible-row retry must resolve and probe again. In particular, an initial
/// failed header probe must not permanently bind the row to an empty version.
pub fn load_primary_image(host: &HostState, id: &str) -> HistoryImageLoad {
    load_primary_image_prepared(host, id, None)
}

fn load_primary_image_prepared(host: &HostState, id: &str, display_height: Option<u32>) -> HistoryImageLoad {
    let began = Instant::now();
    let result = host.hover_image_file(id, false)
        .map_err(|_| "HISTORY_IMAGE_UNAVAILABLE".to_string())
        .and_then(|path| {
            let (resources, source) = load_cached_path_resources(&host.data_root(), &path, "", display_height)?;
            let thumbnail = crate::preview::PreviewThumbnail {
                asset: path.to_string_lossy().into_owned(),
                width: source.width,
                height: source.height,
                label: path.file_name().unwrap_or_default().to_string_lossy().into_owned(),
                source_version: source.source_version,
            };
            Ok((resources, thumbnail))
        });
    match result {
        Ok((resources, thumbnail)) => {
            log::debug!("History thumbnail ready in {}ms", began.elapsed().as_millis());
            HistoryImageLoad { resources, thumbnail: Some(thumbnail), failure: None }
        }
        Err(code) => {
            let failure = HistoryImageFailure::from_code(&code);
            // Never log a source path, image bytes, item ID or clipboard text.
            log::warn!("History thumbnail unavailable: {code}, {}ms", began.elapsed().as_millis());
            HistoryImageLoad { resources: unavailable(), thumbnail: None, failure: Some(failure) }
        }
    }
}

/// Visible-row loader: prepare the bounded thumbnail at the list's image height
/// (physical pixels). Disk hits reuse their validated decode for the raster.
pub fn load_primary_display_image(host: &HostState, id: &str, max_height_px: u32) -> HistoryImageLoad {
    load_primary_image_prepared(host, id, Some(max_height_px))
}

fn load_cached_path_resources(
    data_root: &Path,
    path: &Path,
    expected: &str,
    display_height: Option<u32>,
) -> Result<(ContentResources, HistoryImageSource), String> {
    let mut source = GuardedSource::open(path, check_metadata)?;
    let version = probe_open_source(&mut source)?;
    if !expected.is_empty() && expected != version.source_version {
        return Err("HISTORY_IMAGE_SOURCE_CHANGED".into());
    }
    let mut guarded = Some(source);
    let generate = || {
        let resources = decode_open_source(guarded.take().unwrap(), &version, |file, bytes| {
            file.take(MAX_SOURCE_BYTES + 1).read_to_end(bytes).map_err(io_error)?;
            Ok(())
        })?;
        resources.images.get(PRIMARY_IMAGE_RESOURCE).and_then(Clone::clone)
            .ok_or_else(|| "HISTORY_IMAGE_DECODE".into())
    };
    let display = match display_height {
        Some(height) => crate::history_thumbnail_cache::load_or_generate_display(
            data_root, &version.source_version, height, generate,
        ),
        None => crate::history_thumbnail_cache::load_or_generate(
            data_root, &version.source_version, generate,
        ),
    }?;
    // A disk-cache hit still belongs to the source we admitted for this load.
    if let Some(source) = guarded { source.check_current()?; }
    let mut resources = unavailable();
    resources.images.insert(PRIMARY_IMAGE_RESOURCE.into(), Some(display));
    resources.limit_retained_bytes(native_content::CONTENT_RESOURCE_BUDGET_BYTES);
    Ok((resources, version))
}

/// Lightweight metadata/header probe for NativeHost query workers. It never
/// calls image::decode, reads an entire large image or returns a render path.
pub fn probe_source(path: &Path) -> Result<HistoryImageSource, String> {
    let mut source = GuardedSource::open(path, check_metadata)?;
    probe_open_source(&mut source)
}

/// Visible-worker API; `expected_source_version` must come from probe_source.
/// A stale version, a concurrent source change or unavailable file fails closed.
pub fn load_path_resources(
    path: &Path,
    expected_source_version: &str,
) -> Result<ContentResources, String> {
    if expected_source_version.is_empty() {
        return Err("HISTORY_IMAGE_SOURCE_CHANGED".into());
    }
    load_path_with(path, expected_source_version, check_metadata, |file, bytes| {
        file.take(MAX_SOURCE_BYTES + 1)
            .read_to_end(bytes)
            .map_err(|_| "HISTORY_IMAGE_READ".to_string())?;
        Ok(())
    })
}

fn load_path_with(
    path: &Path,
    expected: &str,
    inspect: impl Fn(&Metadata) -> Result<(), String>,
    read: impl FnOnce(&mut File, &mut Vec<u8>) -> Result<(), String>,
) -> Result<ContentResources, String> {
    let mut source = GuardedSource::open(path, inspect)?;
    let version = probe_open_source(&mut source)?;
    if expected.is_empty() || version.source_version != expected {
        return Err("HISTORY_IMAGE_SOURCE_CHANGED".into());
    }
    decode_open_source(source, &version, read)
}

fn decode_open_source(
    mut source: GuardedSource,
    version: &HistoryImageSource,
    read: impl FnOnce(&mut File, &mut Vec<u8>) -> Result<(), String>,
) -> Result<ContentResources, String> {
    source.file.seek(SeekFrom::Start(0)).map_err(io_error)?;
    let mut bytes = Vec::with_capacity(version.size_bytes as usize);
    read(&mut source.file, &mut bytes)?;
    if bytes.len() as u64 != version.size_bytes || bytes.len() as u64 > MAX_SOURCE_BYTES {
        return Err("HISTORY_IMAGE_SOURCE_CHANGED".into());
    }
    source.check_current()?;
    // Recheck the exact header used for this version, not just length/mtime.
    if source.version(&bytes[..version.header_bytes_read]) != version.source_version {
        return Err("HISTORY_IMAGE_SOURCE_CHANGED".into());
    }
    // The byte snapshot is stable. Do not hold filesystem read guards during
    // CPU decoding/encoding: editing may replace the original immediately.
    drop(source);
    let mut resources = unavailable();
    resources.images.insert(
        PRIMARY_IMAGE_RESOURCE.into(),
        native_content::list_image(bytes),
    );
    resources.limit_retained_bytes(native_content::CONTENT_RESOURCE_BUDGET_BYTES);
    Ok(resources)
}

fn unavailable() -> ContentResources {
    let mut resources = ContentResources::default();
    resources.images.insert(PRIMARY_IMAGE_RESOURCE.into(), None);
    resources
}

fn probe_open_source(source: &mut GuardedSource) -> Result<HistoryImageSource, String> {
    let mut header = Vec::new();
    let mut target = 512usize.min(source.stamp.bytes as usize);
    loop {
        let already_read = header.len();
        header.resize(target, 0);
        source.file.read_exact(&mut header[already_read..])
            .map_err(io_error)?;
        let dimensions = image::ImageReader::new(Cursor::new(&header))
            .with_guessed_format()
            .ok()
            .and_then(|mut reader| {
                let mut limits = image::Limits::default();
                limits.max_alloc = Some(8 * 1024 * 1024);
                reader.limits(limits);
                reader.into_dimensions().ok()
            });
        if let Some((width, height)) = dimensions.filter(|(w, h)| *w != 0 && *h != 0) {
            source.check_current()?;
            return Ok(HistoryImageSource {
                source_version: source.version(&header), width, height,
                size_bytes: source.stamp.bytes, header_bytes_read: header.len(),
            });
        }
        if target == MAX_PROBE_BYTES || target as u64 == source.stamp.bytes {
            return Err("HISTORY_IMAGE_HEADER".into());
        }
        target = target.saturating_mul(4).min(MAX_PROBE_BYTES).min(source.stamp.bytes as usize);
    }
}

#[derive(Debug, PartialEq, Eq)]
struct FileStamp {
    bytes: u64,
    modified: Option<u128>,
    created: Option<u128>,
    attributes: u32,
    identity: (u64, u64),
}

impl FileStamp {
    fn of(metadata: &Metadata) -> Self {
        let nanos = |time: std::io::Result<std::time::SystemTime>| {
            time.ok().and_then(|time| time.duration_since(UNIX_EPOCH).ok()).map(|time| time.as_nanos())
        };
        #[cfg(unix)]
        let identity = {
            use std::os::unix::fs::MetadataExt;
            (metadata.dev(), metadata.ino())
        };
        #[cfg(not(unix))]
        let identity = (0, 0);
        Self { bytes: metadata.len(), modified: nanos(metadata.modified()),
            created: nanos(metadata.created()), attributes: attributes(metadata), identity }
    }
}

struct GuardedSource {
    path: PathBuf,
    file: File,
    stamp: FileStamp,
    // Windows metadata-only handles deny mutation/deletion of every route before
    // data access. They also pin a leaf while checking cloud placeholder flags.
    _route_guards: Vec<File>,
}

impl GuardedSource {
    fn open(path: &Path, inspect: impl Fn(&Metadata) -> Result<(), String>) -> Result<Self, String> {
        let raw = path.to_string_lossy();
        if !path.is_absolute() || raw.split(['/', '\\']).any(|s| s == "..")
            || raw.starts_with(r"\\.\") || raw.to_ascii_lowercase().starts_with(r"\\?\globalroot\") {
            return Err("HISTORY_IMAGE_PATH".into());
        }
        let mut guards = Vec::new();
        let mut leaf = None;
        for part in path.ancestors().collect::<Vec<_>>().into_iter().rev() {
            let metadata = fs::symlink_metadata(part).map_err(io_error)?;
            inspect(&metadata)?;
            if part != path && !metadata.is_dir() {
                return Err("HISTORY_IMAGE_PATH".into());
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::OpenOptionsExt;
                let guard = OpenOptions::new().access_mode(0x80).share_mode(1)
                    .custom_flags(0x0020_0000 | 0x0200_0000 | 0x0010_0000)
                    .open(part).map_err(io_error)?;
                let pinned = guard.metadata().map_err(io_error)?;
                inspect(&pinned)?;
                if FileStamp::of(&metadata) != FileStamp::of(&pinned) {
                    return Err("HISTORY_IMAGE_SOURCE_CHANGED".into());
                }
                guards.push(guard);
            }
            if part == path { leaf = Some(metadata); }
        }
        let metadata = leaf.ok_or("HISTORY_IMAGE_PATH")?;
        if metadata.is_file() && metadata.len() == 0 {
            return Err("HISTORY_IMAGE_EMPTY".into());
        }
        if !metadata.is_file() || metadata.len() > MAX_SOURCE_BYTES {
            return Err("HISTORY_IMAGE_SIZE".into());
        }
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(1).custom_flags(0x0020_0000 | 0x0010_0000);
        }
        let file = options.open(path).map_err(io_error)?;
        let opened = file.metadata().map_err(io_error)?;
        inspect(&opened)?;
        let stamp = FileStamp::of(&metadata);
        if stamp != FileStamp::of(&opened) { return Err("HISTORY_IMAGE_SOURCE_CHANGED".into()); }
        Ok(Self { path: path.to_owned(), file, stamp, _route_guards: guards })
    }

    fn check_current(&self) -> Result<(), String> {
        let opened = self.file.metadata().map_err(io_error)?;
        let named = fs::symlink_metadata(&self.path).map_err(io_error)?;
        check_metadata(&opened)?;
        check_metadata(&named)?;
        if self.stamp != FileStamp::of(&opened) || self.stamp != FileStamp::of(&named) {
            return Err("HISTORY_IMAGE_SOURCE_CHANGED".into());
        }
        Ok(())
    }

    fn version(&self, header: &[u8]) -> String {
        let mut hash = blake3::Hasher::new();
        hash.update(b"history-image-file-v1\0");
        hash.update(self.path.to_string_lossy().as_bytes());
        hash.update(format!("{:?}", self.stamp).as_bytes());
        hash.update(header);
        hash.finalize().to_hex().to_string()
    }
}

fn attributes(metadata: &Metadata) -> u32 {
    #[cfg(windows)]
    { use std::os::windows::fs::MetadataExt; metadata.file_attributes() }
    #[cfg(not(windows))]
    { let _ = metadata; 0 }
}

fn check_metadata(metadata: &Metadata) -> Result<(), String> {
    if metadata.file_type().is_symlink() { return Err("HISTORY_IMAGE_UNSAFE_PATH".into()); }
    check_attributes(attributes(metadata))
}

fn check_attributes(flags: u32) -> Result<(), String> {
    if flags & UNSAFE_ATTRIBUTES != 0 { return Err("HISTORY_IMAGE_UNSAFE_PATH".into()); }
    Ok(())
}

fn io_error(error: std::io::Error) -> String {
    match (error.kind(), error.raw_os_error()) {
        (std::io::ErrorKind::NotFound, _) => "HISTORY_IMAGE_MISSING",
        (_, Some(32 | 33)) => "HISTORY_IMAGE_BUSY",
        (std::io::ErrorKind::PermissionDenied, _) => "HISTORY_IMAGE_ACCESS",
        _ => "HISTORY_IMAGE_READ",
    }.into()
}

#[cfg(test)]
#[path = "history_image_resources_tests.rs"]
mod tests;
