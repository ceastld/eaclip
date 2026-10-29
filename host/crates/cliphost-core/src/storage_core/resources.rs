//! File publication and explicit resource-root mapping for core storage.
//!
//! Historical aliases are lexical names, never filesystem search roots. A
//! resolver can only open files below its selected active root. Publication
//! never overwrites an existing file, even when names or bytes are identical.

use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

pub const MAX_RESOURCE_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
enum PublicationPolicy {
    IsolatedLocal,
    ExistingLocation,
}

impl PublicationPolicy {
    fn check_size(self, bytes: u64) -> Result<(), String> {
        if self == Self::IsolatedLocal && bytes > MAX_RESOURCE_BYTES {
            return Err("resource_size_limit".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocatorBase {
    ImageRoot,
    DataRoot,
    Absolute,
}

impl LocatorBase {
    pub fn from_token(value: &str) -> Result<Self, String> {
        match value {
            "image_root" => Ok(Self::ImageRoot),
            "data_root" => Ok(Self::DataRoot),
            "absolute" => Ok(Self::Absolute),
            _ => Err("unsupported_resource_locator_base".into()),
        }
    }
}

/// The selected root must exist. Aliases need not exist and are never opened.
#[derive(Debug, Clone)]
pub struct ResourceResolver {
    active_root: PathBuf,
    historical_roots: Vec<PathBuf>,
}

impl ResourceResolver {
    /// Metadata belongs to the selected candidate tree; historical roots from
    /// the report are still treated only as lexical aliases.
    pub fn from_data_root(active_root: &Path) -> Result<Self, String> {
        let root = absolute_lexical(active_root)?;
        let report = root.join("migration/core-report.json");
        let aliases = if inspect_path(&report)?.is_some() {
            let mut reader = checked_open(&report, Some(&root))?;
            if reader.metadata().map_err(io_error)?.len() > 16 * 1024 * 1024 {
                return Err("resource_context_size_limit".into());
            }
            let mut bytes = Vec::new();
            Read::by_ref(&mut reader).take(16 * 1024 * 1024 + 1).read_to_end(&mut bytes).map_err(io_error)?;
            if bytes.len() > 16 * 1024 * 1024 { return Err("resource_context_size_limit".into()); }
            let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| "invalid_resource_context")?;
            match value.get("sourceAliases") {
                None => Vec::new(),
                Some(serde_json::Value::Array(values)) => values.iter()
                    .map(|value| value.as_str().map(PathBuf::from).ok_or_else(|| "invalid_resource_context_alias".to_string()))
                    .collect::<Result<Vec<_>, _>>()?,
                _ => return Err("invalid_resource_context_aliases".into()),
            }
        } else { Vec::new() };
        Self::new(&root, &aliases)
    }

    pub fn new(active_root: &Path, historical_roots: &[PathBuf]) -> Result<Self, String> {
        let active_root = absolute_lexical(active_root)?;
        let metadata = inspect_path(&active_root)?.ok_or("resource_root_missing")?;
        if !metadata.is_dir() {
            return Err("resource_root_not_directory".into());
        }
        let mut aliases: Vec<PathBuf> = Vec::new();
        for root in historical_roots {
            let spelling = root.to_string_lossy();
            if spelling.starts_with(r"\\?\") || spelling.starts_with(r"\\.\") {
                return Err("resource_alias_device_refused".into());
            }
            let root = absolute_lexical(root)?;
            if root.parent().is_none() || root.file_name().is_none() {
                return Err("resource_alias_volume_root_refused".into());
            }
            if relative_beneath(&root, &active_root).is_some()
                || relative_beneath(&active_root, &root).is_some()
                || aliases.iter().any(|other| {
                    relative_beneath(&root, other).is_some()
                        || relative_beneath(other, &root).is_some()
                })
            {
                return Err("resource_alias_overlap".into());
            }
            aliases.push(root);
        }
        Ok(Self { active_root, historical_roots: aliases })
    }

    pub fn active_root(&self) -> &Path { &self.active_root }
    pub fn historical_roots(&self) -> &[PathBuf] { &self.historical_roots }

    /// No file metadata, canonicalize, URL fetch or alias-root access occurs.
    pub fn resolve(&self, locator: &str, base: LocatorBase) -> Result<PathBuf, String> {
        if locator.is_empty() || locator.contains('\0') {
            return Err("invalid_resource_locator".into());
        }
        let decoded;
        let value = if locator.get(..5).is_some_and(|v| v.eq_ignore_ascii_case("file:")) {
            decoded = local_file_url(locator)?;
            decoded.as_str()
        } else { locator };
        let path = Path::new(value);
        let target = if path.is_absolute() {
            let absolute = absolute_lexical(path)?;
            if let Some(relative) = relative_beneath(&absolute, &self.active_root) {
                self.active_root.join(relative)
            } else if let Some(relative) = self.historical_roots.iter()
                .find_map(|root| relative_beneath(&absolute, root))
            {
                self.active_root.join(relative)
            } else {
                return Err("unmapped_external_resource".into());
            }
        } else {
            let relative = relative_lexical(path)?;
            match base {
                LocatorBase::ImageRoot => self.active_root.join("Image").join(relative),
                LocatorBase::DataRoot => self.active_root.join(relative),
                LocatorBase::Absolute => return Err("absolute_resource_required".into()),
            }
        };
        if relative_beneath(&target, &self.active_root).is_none() {
            return Err("resource_outside_selected_root".into());
        }
        // Also reject named cloud directories lexically, before any metadata.
        reject_cloud_components(&target)?;
        Ok(target)
    }

    pub fn open_local(&self, locator: &str, base: LocatorBase) -> Result<File, String> {
        let path = self.resolve(locator, base)?;
        checked_open(&path, Some(&self.active_root))
    }

    pub fn read_local(&self, locator: &str, base: LocatorBase) -> Result<Vec<u8>, String> {
        let mut file = self.open_local(locator, base)?;
        let size = file.metadata().map_err(io_error)?.len();
        if size > MAX_RESOURCE_BYTES { return Err("resource_size_limit".into()); }
        let mut output = Vec::new();
        Read::by_ref(&mut file).take(MAX_RESOURCE_BYTES + 1).read_to_end(&mut output).map_err(io_error)?;
        if output.len() as u64 > MAX_RESOURCE_BYTES { return Err("resource_size_limit".into()); }
        Ok(output)
    }
}

/// Prepare a record for operations which do not read ordinary file-list inputs
/// (for example, creating a favorite). Canonical records stay byte-identical;
/// the core revision is retained so strict persistence can still recognize it.
/// Actual clipboard or OS output must use `project_clip_for_io` instead.
pub fn project_clip_for_output(
    record: &crate::model::ClipRecord, resolver: &ResourceResolver,
) -> Result<crate::model::ClipRecord, String> {
    use crate::im_rich::document::{ImRichDocument, RichNodeKind, RichOrigin};
    let mut output = record.clone();
    if let Some(locator) = output.image_path.as_deref().filter(|value| !value.is_empty()) {
        output.image_path = Some(resolver.resolve(locator, LocatorBase::ImageRoot)?.to_string_lossy().into_owned());
    }
    // Ordinary file lists are user-owned external references. Copying a record
    // neither reads nor duplicates those files. OS file-output callers must
    // separately enforce the isolated resolver before opening any path.
    if let Some(raw) = output.qq_rich_text.as_deref().filter(|value| !value.trim().is_empty()) {
        let origin = match crate::im_rich::resolve_origin(&output.source_process_name, raw) {
            crate::im_rich::ImRichOrigin::Qq => RichOrigin::Qq,
            crate::im_rich::ImRichOrigin::WeChat => RichOrigin::Wechat,
            crate::im_rich::ImRichOrigin::Wework => RichOrigin::Wework,
            crate::im_rich::ImRichOrigin::Unknown => RichOrigin::Unknown,
        };
        let mut document = ImRichDocument::parse(raw, origin).map_err(|_| "invalid_im_output_document")?;
        let paths: Vec<_> = document.nodes().iter()
            .filter(|node| matches!(node.kind(), RichNodeKind::Image | RichNodeKind::File) && !node.path().is_empty())
            .map(|node| (node.id(), node.path().to_string())).collect();
        for (id, locator) in paths {
            let mapped = resolver.resolve(&locator, LocatorBase::ImageRoot)?;
            document.set_path(id, &mapped.to_string_lossy()).map_err(|_| "invalid_im_output_path")?;
        }
        output.qq_rich_text = Some(document.serialize());
    }
    crate::csharp_import_assets::rewrite_embedded_images(&mut output, &mut |locator| {
        resolver.resolve(locator, LocatorBase::ImageRoot).map(|path| path.to_string_lossy().into_owned())
    })?;
    Ok(output)
}

/// Project and validate every local input before clipboard or OS output. File
/// lists, unlike no-I/O favorite preparation, must also map into this root.
/// Only the final disposable I/O copy loses its core CAS token. This never
/// opens a historical alias or an unmapped external path.
pub fn project_clip_for_io(
    record: &crate::model::ClipRecord, resolver: &ResourceResolver,
) -> Result<crate::model::ClipRecord, String> {
    use crate::im_rich::document::{ImRichDocument, RichNodeKind, RichOrigin};
    let mut output = project_clip_for_output(record, resolver)?;
    if let Some(locator) = output.image_path.as_deref().filter(|value| !value.is_empty()) {
        drop(resolver.open_local(locator, LocatorBase::Absolute)?);
    }
    let mut paths = Vec::with_capacity(output.file_paths.len());
    for locator in &output.file_paths {
        let mapped = resolver.resolve(locator, LocatorBase::DataRoot)?;
        // A file-output consumer can subsequently open these exact paths.
        // Directory output needs a separately scoped, recursive contract;
        // refusing it here avoids exporting unchecked descendant links.
        drop(checked_open(&mapped, Some(resolver.active_root()))?);
        paths.push(mapped.to_string_lossy().into_owned());
    }
    output.file_paths = paths;
    if let Some(raw) = output.qq_rich_text.as_deref().filter(|value| !value.trim().is_empty()) {
        let origin = match crate::im_rich::resolve_origin(&output.source_process_name, raw) {
            crate::im_rich::ImRichOrigin::Qq => RichOrigin::Qq,
            crate::im_rich::ImRichOrigin::WeChat => RichOrigin::Wechat,
            crate::im_rich::ImRichOrigin::Wework => RichOrigin::Wework,
            crate::im_rich::ImRichOrigin::Unknown => RichOrigin::Unknown,
        };
        let document = ImRichDocument::parse(raw, origin).map_err(|_| "invalid_im_output_document")?;
        for node in document.nodes().iter().filter(|node| {
            matches!(node.kind(), RichNodeKind::Image | RichNodeKind::File) && !node.path().is_empty()
        }) {
            drop(resolver.open_local(node.path(), LocatorBase::Absolute)?);
        }
    }
    crate::csharp_import_assets::rewrite_embedded_images(&mut output, &mut |locator| {
        let path = resolver.resolve(locator, LocatorBase::Absolute)?;
        drop(checked_open(&path, Some(resolver.active_root()))?);
        Ok(path.to_string_lossy().into_owned())
    })?;
    output.storage_revision = None;
    Ok(output)
}

/// Validate a local existing path without following links or hydrating files.
pub fn validate_local_file(path: &Path) -> Result<(), String> {
    drop(checked_open(&absolute_lexical(path)?, None)?);
    Ok(())
}

/// Read one explicitly selected local file through the same link, placeholder
/// and opened-handle checks as resource IO, without hydrating cloud files.
pub(crate) fn read_local_file_bounded(path: &Path, max_bytes: u64) -> Result<Vec<u8>, String> {
    let mut file = checked_open(&absolute_lexical(path)?, None)?;
    if file.metadata().map_err(io_error)?.len() > max_bytes {
        return Err("resource_size_limit".into());
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file).take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes).map_err(io_error)?;
    if bytes.len() as u64 > max_bytes { return Err("resource_size_limit".into()); }
    Ok(bytes)
}

/// The preferred filename is retained when vacant; collisions use a fresh name.
pub fn publish_bytes(directory: &Path, preferred_name: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    publish_bytes_with_policy(directory, preferred_name, bytes, PublicationPolicy::IsolatedLocal)
}

/// Atomic publication for an existing application location. This preserves the
/// ordinary capture path contract (including configured UNC/link locations);
/// it is not an isolation check and must never replace the checked core API.
pub fn publish_bytes_compatible(directory: &Path, preferred_name: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    publish_bytes_with_policy(directory, preferred_name, bytes, PublicationPolicy::ExistingLocation)
}

fn publish_bytes_with_policy(directory: &Path, preferred_name: &str, bytes: &[u8], policy: PublicationPolicy) -> Result<PathBuf, String> {
    policy.check_size(bytes.len() as u64)?;
    publish_with_policy(directory, preferred_name, policy, |file| {
        file.write_all(bytes).map_err(io_error)?;
        file.flush().map_err(io_error)?;
        file.seek(SeekFrom::Start(0)).map_err(io_error)?;
        if digest_reader_with_policy(file, policy)?.as_slice() != Sha256::digest(bytes).as_slice() {
            return Err("resource_copy_verification_failed".into());
        }
        Ok(())
    })
}

/// Copy an explicitly selected local input; never reuse a same-name destination.
/// A second streaming source digest detects modifications during the copy.
pub fn publish_copy(source: &Path, directory: &Path, preferred_name: &str) -> Result<PathBuf, String> {
    publish_copy_with_policy(source, directory, preferred_name, PublicationPolicy::IsolatedLocal)
}

/// Existing-location counterpart of `publish_copy`. It follows the same paths
/// the ordinary legacy capture accepted, while still verifying copied bytes
/// and refusing to overwrite another published name.
pub fn publish_copy_compatible(source: &Path, directory: &Path, preferred_name: &str) -> Result<PathBuf, String> {
    publish_copy_with_policy(source, directory, preferred_name, PublicationPolicy::ExistingLocation)
}

fn publish_copy_with_policy(source: &Path, directory: &Path, preferred_name: &str, policy: PublicationPolicy) -> Result<PathBuf, String> {
    let mut reader = if policy == PublicationPolicy::IsolatedLocal {
        checked_open(&absolute_lexical(source)?, None)?
    } else {
        // This metadata deliberately follows the existing location contract;
        // do not apply candidate-only link/cloud/network rules here.
        if !fs::metadata(source).map_err(io_error)?.is_file() { return Err("resource_not_regular_file".into()); }
        let file = File::open(source).map_err(io_error)?;
        if !file.metadata().map_err(io_error)?.is_file() { return Err("resource_not_regular_file".into()); }
        file
    };
    policy.check_size(reader.metadata().map_err(io_error)?.len())?;
    publish_with_policy(directory, preferred_name, policy, |writer| {
        let mut hasher = Sha256::new();
        let mut count = 0u64;
        let mut buffer = [0u8; 65536];
        loop {
            let length = reader.read(&mut buffer).map_err(io_error)?;
            if length == 0 { break; }
            count = count.checked_add(length as u64).ok_or("resource_size_overflow")?;
            policy.check_size(count)?;
            hasher.update(&buffer[..length]);
            writer.write_all(&buffer[..length]).map_err(io_error)?;
        }
        let copied = hasher.finalize();
        reader.seek(SeekFrom::Start(0)).map_err(io_error)?;
        if digest_reader_with_policy(&mut reader, policy)?.as_slice() != copied.as_slice() {
            return Err("resource_source_changed".into());
        }
        writer.flush().map_err(io_error)?;
        writer.seek(SeekFrom::Start(0)).map_err(io_error)?;
        if digest_reader_with_policy(writer, policy)?.as_slice() != copied.as_slice() {
            return Err("resource_copy_verification_failed".into());
        }
        Ok(())
    })
}

#[cfg(test)]
fn publish_with(
    directory: &Path, preferred_name: &str,
    write: impl FnOnce(&mut File) -> Result<(), String>,
) -> Result<PathBuf, String> {
    publish_with_policy(directory, preferred_name, PublicationPolicy::IsolatedLocal, write)
}

fn publication_directory(directory: &Path, policy: PublicationPolicy) -> Result<PathBuf, String> {
    if policy == PublicationPolicy::IsolatedLocal {
        absolute_lexical(directory)
    } else if directory.as_os_str().is_empty() {
        Err("empty_resource_directory".into())
    } else {
        Ok(directory.to_owned())
    }
}

fn ensure_publication_directory(directory: &Path, policy: PublicationPolicy) -> Result<(), String> {
    if policy == PublicationPolicy::IsolatedLocal {
        ensure_directory(directory)
    } else {
        fs::create_dir_all(directory).map_err(io_error)
    }
}

fn publish_with_policy(
    directory: &Path, preferred_name: &str, policy: PublicationPolicy,
    write: impl FnOnce(&mut File) -> Result<(), String>,
) -> Result<PathBuf, String> {
    validate_filename(preferred_name)?;
    let directory = publication_directory(directory, policy)?;
    ensure_publication_directory(&directory, policy)?;
    // tempfile's Windows publication calls need an extended absolute path for
    // long names even when std::fs can already open the same DOS spelling.
    // Canonicalize only this selected destination directory, never a locator
    // or historical alias. Keep the caller's logical path in the return value.
    let io_directory = fs::canonicalize(&directory).map_err(io_error)?;
    if policy == PublicationPolicy::IsolatedLocal {
        let actual = absolute_lexical(&io_directory)?;
        if relative_beneath(&actual, &directory).is_none_or(|remaining| !remaining.as_os_str().is_empty()) {
            return Err("resource_destination_directory_changed".into());
        }
    }
    let mut stage = tempfile::Builder::new().prefix(".clip-image-").suffix(".stage")
        .tempfile_in(&io_directory).map_err(io_error)?;
    if policy == PublicationPolicy::IsolatedLocal {
        let stage_name = stage.path().file_name().ok_or("resource_stage_filename_missing")?;
        // Compare the opened physical file to the original selected location,
        // not to its own canonical path. A redirected directory must not make
        // that check a tautology before any source bytes reach the stage.
        validate_opened_path(stage.as_file(), &directory.join(stage_name))?;
    }
    write(stage.as_file_mut())?;
    stage.as_file().sync_all().map_err(io_error)?;
    let preferred = Path::new(preferred_name);
    for attempt in 0..32 {
        let name = if attempt == 0 {
            preferred_name.to_string()
        } else {
            let stem = preferred.file_stem().and_then(|v| v.to_str()).unwrap_or("image");
            let suffix = preferred.extension().and_then(|v| v.to_str()).map(|v| format!(".{v}")).unwrap_or_default();
            format!("{stem}_{}{suffix}", uuid::Uuid::new_v4().simple())
        };
        let target = directory.join(&name);
        let io_target = io_directory.join(name);
        ensure_publication_directory(&directory, policy)?;
        match stage.persist_noclobber(&io_target) {
            Ok(file) => {
                if policy == PublicationPolicy::IsolatedLocal { validate_opened_path(&file, &target)?; }
                file.sync_all().map_err(io_error)?;
                #[cfg(unix)]
                File::open(&directory).and_then(|dir| dir.sync_all()).map_err(io_error)?;
                return Ok(target);
            }
            Err(error) => {
                let collision = error.error.kind() == std::io::ErrorKind::AlreadyExists;
                stage = error.file;
                if !collision { return Err(io_error(error.error)); }
            }
        }
    }
    Err("resource_name_collision_limit".into())
}

fn digest_reader_with_policy(reader: &mut File, policy: PublicationPolicy) -> Result<Vec<u8>, String> {
    let mut hasher = Sha256::new();
    let mut count = 0u64;
    let mut buffer = [0u8; 65536];
    loop {
        let length = reader.read(&mut buffer).map_err(io_error)?;
        if length == 0 { break; }
        count = count.checked_add(length as u64).ok_or("resource_size_overflow")?;
        policy.check_size(count)?;
        hasher.update(&buffer[..length]);
    }
    Ok(hasher.finalize().to_vec())
}

fn local_file_url(value: &str) -> Result<String, String> {
    let (_, tail) = value.split_once(':').ok_or("invalid_resource_url")?;
    if !tail.starts_with(['/', '\\']) { return Err("relative_file_url_refused".into()); }
    if tail.split(['/', '\\']).any(|part| part == "..") || tail.to_ascii_lowercase().contains("%2e") {
        return Err("resource_relative_traversal_refused".into());
    }
    let url = url::Url::parse(value).map_err(|_| "invalid_resource_url")?;
    if url.scheme() != "file" || url.host_str().is_some_and(|v| !v.eq_ignore_ascii_case("localhost"))
        || url.query().is_some() || url.fragment().is_some()
    { return Err("network_resource_refused".into()); }
    url.to_file_path().map_err(|_| "invalid_resource_url".to_string())?
        .into_os_string().into_string().map_err(|_| "resource_path_encoding".into())
}

fn absolute_lexical(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() { return Err("absolute_resource_path_required".into()); }
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            #[cfg(windows)]
            Component::Prefix(prefix) => {
                use std::path::Prefix;
                match prefix.kind() {
                    Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => output.push(format!("{}:\\", drive as char)),
                    _ => return Err("network_or_device_resource_refused".into()),
                }
            }
            Component::RootDir => output.push(component.as_os_str()),
            Component::Normal(name) => {
                validate_component(name)?;
                output.push(name);
            }
            _ => return Err("resource_traversal_or_device_refused".into()),
        }
    }
    Ok(output)
}

fn relative_lexical(path: &Path) -> Result<PathBuf, String> {
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => { validate_component(name)?; output.push(name); }
            _ => return Err("resource_relative_traversal_refused".into()),
        }
    }
    if output.as_os_str().is_empty() { return Err("empty_resource_path".into()); }
    Ok(output)
}

fn validate_component(name: &std::ffi::OsStr) -> Result<(), String> {
    let value = name.to_str().ok_or("resource_path_encoding")?;
    if value.is_empty() || value.ends_with(['.', ' ']) || value.chars().any(|ch| ch.is_control() || "<>:\"|?*".contains(ch)) {
        return Err("invalid_resource_path_component".into());
    }
    let stem = value.split('.').next().unwrap_or("").to_ascii_uppercase();
    if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && matches!(stem.as_bytes()[3], b'1'..=b'9'))
    { return Err("resource_device_name_refused".into()); }
    Ok(())
}

fn validate_filename(value: &str) -> Result<(), String> {
    let path = relative_lexical(Path::new(value))?;
    if path.components().count() != 1 { return Err("resource_filename_required".into()); }
    Ok(())
}

fn relative_beneath(path: &Path, root: &Path) -> Option<PathBuf> {
    let mut parts = path.components();
    for expected in root.components() {
        let actual = parts.next()?;
        #[cfg(windows)]
        if !actual.as_os_str().to_string_lossy().eq_ignore_ascii_case(&expected.as_os_str().to_string_lossy()) { return None; }
        #[cfg(not(windows))]
        if actual != expected { return None; }
    }
    Some(parts.collect())
}

fn reject_cloud_components(path: &Path) -> Result<(), String> {
    for part in path.components() {
        if let Component::Normal(name) = part {
            let lower = name.to_string_lossy().to_ascii_lowercase();
            if lower.starts_with("onedrive") || lower == "dropbox" || lower.starts_with("icloud") {
                return Err("cloud_resource_refused".into());
            }
        }
    }
    Ok(())
}

fn safe_metadata(metadata: &Metadata) -> Result<(), String> {
    if metadata.file_type().is_symlink() { return Err("resource_link_refused".into()); }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & (0x400 | 0x1000 | 0x40000 | 0x400000) != 0 {
            return Err("resource_link_or_placeholder_refused".into());
        }
    }
    Ok(())
}

fn inspect_path(path: &Path) -> Result<Option<Metadata>, String> {
    reject_cloud_components(path)?;
    let mut current = PathBuf::new();
    let mut parts = path.components().peekable();
    let mut last = None;
    while let Some(part) = parts.next() {
        current.push(part.as_os_str());
        if matches!(part, Component::Prefix(_)) { continue; }
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                safe_metadata(&metadata)?;
                if parts.peek().is_some() && !metadata.is_dir() { return Err("resource_ancestor_not_directory".into()); }
                last = Some(metadata);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io_error(error)),
        }
    }
    Ok(last)
}

fn ensure_directory(path: &Path) -> Result<(), String> {
    reject_cloud_components(path)?;
    let mut current = PathBuf::new();
    for part in path.components() {
        current.push(part.as_os_str());
        if matches!(part, Component::Prefix(_)) { continue; }
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match fs::create_dir(&current) {
                    Ok(()) => {},
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {},
                    Err(error) => return Err(io_error(error)),
                }
                fs::symlink_metadata(&current).map_err(io_error)?
            }
            Err(error) => return Err(io_error(error)),
        };
        safe_metadata(&metadata)?;
        if !metadata.is_dir() { return Err("resource_destination_not_directory".into()); }
    }
    Ok(())
}

fn checked_open(path: &Path, selected_root: Option<&Path>) -> Result<File, String> {
    if selected_root.is_some_and(|root| relative_beneath(path, root).is_none()) {
        return Err("resource_outside_selected_root".into());
    }
    let metadata = inspect_path(path)?.ok_or("resource_missing")?;
    if !metadata.is_file() { return Err("resource_not_regular_file".into()); }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000).share_mode(1);
    }
    let file = options.open(path).map_err(io_error)?;
    safe_metadata(&file.metadata().map_err(io_error)?)?;
    validate_opened_path(&file, path)?;
    Ok(file)
}

fn validate_opened_path(file: &File, expected: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        #[link(name = "kernel32")]
        extern "system" {
            fn GetFinalPathNameByHandleW(handle: *mut std::ffi::c_void, path: *mut u16, size: u32, flags: u32) -> u32;
        }
        let mut buffer = vec![0u16; 512];
        loop {
            let length = unsafe { GetFinalPathNameByHandleW(file.as_raw_handle(), buffer.as_mut_ptr(), buffer.len() as u32, 0) };
            if length == 0 { return Err(io_error(std::io::Error::last_os_error())); }
            if length as usize >= buffer.len() { buffer.resize(length as usize + 1, 0); continue; }
            let actual = PathBuf::from(String::from_utf16(&buffer[..length as usize]).map_err(|_| "resource_path_encoding")?);
            let actual = absolute_lexical(&actual)?;
            let expected = absolute_lexical(expected)?;
            if relative_beneath(&actual, &expected).is_none_or(|remaining| !remaining.as_os_str().is_empty()) {
                return Err("resource_opened_path_changed".into());
            }
            break;
        }
    }
    #[cfg(not(windows))]
    { let _ = (file, expected); }
    Ok(())
}

fn io_error(error: std::io::Error) -> String { format!("resource_io_{:?}", error.kind()) }

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(3).unwrap()
            .join(".tmp/storage-core-resource-tests");
        fs::create_dir_all(&root).unwrap();
        tempfile::tempdir_in(root).unwrap()
    }

    #[test]
    fn equal_timestamp_and_basename_never_overwrite_shared_bytes() {
        let temp = fixture();
        let directory = temp.path().join("Image/Temp");
        let first = publish_bytes(&directory, "20261017_010203_004.png", b"first").unwrap();
        let second = publish_bytes(&directory, "20261017_010203_004.png", b"second").unwrap();
        assert_ne!(first, second);
        assert_eq!(fs::read(first).unwrap(), b"first");
        assert_eq!(fs::read(second).unwrap(), b"second");
    }

    #[test]
    fn copy_collision_preserves_source_and_previous_destination() {
        let temp = fixture();
        let source = temp.path().join("source.png");
        fs::write(&source, b"incoming").unwrap();
        let directory = temp.path().join("Image");
        let previous = publish_bytes(&directory, "same.png", b"shared-old").unwrap();
        let copied = publish_copy(&source, &directory, "same.png").unwrap();
        assert_ne!(copied, previous);
        assert_eq!(fs::read(previous).unwrap(), b"shared-old");
        assert_eq!(fs::read(copied).unwrap(), b"incoming");
        assert_eq!(fs::read(source).unwrap(), b"incoming");
    }

    #[test]
    fn compatible_publication_preserves_normal_paths_and_never_overwrites() {
        let temp = fixture();
        let source = temp.path().join("source.png");
        fs::write(&source, b"incoming").unwrap();
        let directory = temp.path().join("Image/Temp");
        let first = publish_bytes_compatible(&directory, "same.png", b"shared-old").unwrap();
        let second = publish_copy_compatible(&source, &directory, "same.png").unwrap();
        let third = publish_bytes_compatible(&directory, "same.png", b"third").unwrap();
        assert_ne!(first, second);
        assert_ne!(second, third);
        assert_eq!(fs::read(first).unwrap(), b"shared-old");
        assert_eq!(fs::read(second).unwrap(), b"incoming");
        assert_eq!(fs::read(third).unwrap(), b"third");
        assert_eq!(fs::read(source).unwrap(), b"incoming");
    }

    #[test]
    fn publication_preserves_literal_percent_and_html_entity_filenames() {
        let temp = fixture();
        let name = "literal%26amp%3B&amp;+中文.png";
        let source = temp.path().join(name);
        fs::write(&source, b"literal-file-bytes").unwrap();
        for compatible in [false, true] {
            let directory = temp.path().join(if compatible {"compatible"} else {"strict"});
            let result = if compatible {
                publish_copy_compatible(&source, &directory, name)
            } else {
                publish_copy(&source, &directory, name)
            }.unwrap();
            assert_eq!(result.file_name().unwrap(), name);
            assert_eq!(fs::read(result).unwrap(), b"literal-file-bytes");
            assert!(!directory.join("literal&amp;&amp;+中文.png").exists());
        }
        assert_eq!(fs::read(source).unwrap(), b"literal-file-bytes");
    }

    #[cfg(windows)]
    #[test]
    fn long_windows_file_publication_uses_extended_io_paths_without_rewriting_names() {
        let temp = fixture();
        let directory = temp.path().join("long-directory-component-".repeat(3))
            .join("second-long-directory-component-".repeat(3));
        fs::create_dir_all(&directory).unwrap();
        let source = temp.path().join("source%26&amp;.png");
        fs::write(&source, b"long-path-source").unwrap();
        let name = format!("{}%26&amp;.png", "literal-long-file-name-".repeat(5));
        let logical = directory.join(&name);
        assert!(logical.as_os_str().to_string_lossy().encode_utf16().count() > 260);
        let published = publish_copy(&source, &directory, &name).unwrap();
        assert_eq!(published, logical);
        assert_eq!(fs::read(&published).unwrap(), b"long-path-source");
        let copied = publish_copy(&published, &temp.path().join("Image"), &name).unwrap();
        assert_eq!(copied.file_name().unwrap(), name.as_str());
        assert_eq!(fs::read(copied).unwrap(), b"long-path-source");
        let compatible = publish_copy_compatible(&source, &directory, &name).unwrap();
        assert_ne!(compatible, logical);
        assert_eq!(fs::read(compatible).unwrap(), b"long-path-source");
        assert_eq!(fs::read(published).unwrap(), b"long-path-source");
    }

    #[test]
    fn compatible_location_policy_is_separate_from_isolated_local_policy() {
        let temp = fixture();
        // This is an ordinary synthetic directory name, not a cloud provider.
        let directory = temp.path().join("OneDrive-existing-location");
        let source = publish_bytes_compatible(&directory, "source.png", b"legacy").unwrap();
        assert!(publish_bytes(&directory, "strict.png", b"refused").is_err());
        assert!(publish_copy(&source, &temp.path().join("strict"), "strict.png").is_err());
        let target = publish_copy_compatible(&source, &directory, "source.png").unwrap();
        assert_ne!(source, target);
        assert_eq!(fs::read(source).unwrap(), b"legacy");
        assert_eq!(fs::read(target).unwrap(), b"legacy");
        assert!(PublicationPolicy::IsolatedLocal.check_size(MAX_RESOURCE_BYTES + 1).is_err());
        assert!(PublicationPolicy::ExistingLocation.check_size(MAX_RESOURCE_BYTES + 1).is_ok());
        let failed = temp.path().join("compatible-failure");
        assert!(publish_with_policy(&failed, "partial.png", PublicationPolicy::ExistingLocation, |file| {
            file.write_all(b"partial").unwrap();
            Err("injected_compatible_failure".into())
        }).is_err());
        assert_eq!(fs::read_dir(failed).unwrap().count(), 0);
    }

    #[cfg(windows)]
    #[test]
    fn unc_publication_policy_selection_is_purely_lexical() {
        for value in [r"\\never-contacted.invalid\share\Image", r"\\?\UNC\never-contacted.invalid\share\Image"] {
            let directory = Path::new(value);
            assert_eq!(publication_directory(directory, PublicationPolicy::ExistingLocation).unwrap(), directory);
            assert!(publication_directory(directory, PublicationPolicy::IsolatedLocal).is_err());
        }
    }

    #[test]
    fn failed_stage_does_not_publish_partial_resource() {
        let temp = fixture();
        let directory = temp.path().join("Image");
        assert!(publish_with(&directory, "broken.png", |file| {
            file.write_all(b"partial").unwrap();
            Err("injected_write_failure".into())
        }).is_err());
        assert!(!directory.join("broken.png").exists());
        assert_eq!(fs::read_dir(directory).unwrap().count(), 0);
    }

    #[test]
    fn aliases_are_lexical_and_only_clone_bytes_are_read() {
        let temp = fixture();
        let clone = temp.path().join("clone");
        fs::create_dir_all(clone.join("Image")).unwrap();
        fs::write(clone.join("Image/picture.png"), b"clone-only").unwrap();
        let nonexistent = temp.path().join("historical-does-not-exist");
        let resolver = ResourceResolver::new(&clone, &[nonexistent.clone()]).unwrap();
        let old = nonexistent.join("Image/picture.png");
        assert_eq!(resolver.read_local(old.to_str().unwrap(), LocatorBase::Absolute).unwrap(), b"clone-only");
        assert_eq!(resolver.read_local("picture.png", LocatorBase::ImageRoot).unwrap(), b"clone-only");
        let impersonation = nonexistent.with_file_name("historical-does-not-exist-neighbor").join("Image/picture.png");
        assert!(resolver.resolve(impersonation.to_str().unwrap(), LocatorBase::Absolute).is_err());
        assert!(!nonexistent.exists());
    }

    #[test]
    fn existing_original_sentinel_is_never_read_or_copied() {
        let temp = fixture();
        let clone = temp.path().join("clone");
        let original = temp.path().join("original");
        fs::create_dir_all(clone.join("Image")).unwrap();
        fs::create_dir_all(original.join("Image")).unwrap();
        fs::write(clone.join("Image/a.png"), b"clone").unwrap();
        fs::write(original.join("Image/a.png"), b"do-not-read-original").unwrap();
        let resolver = ResourceResolver::new(&clone, &[original.clone()]).unwrap();
        assert_eq!(resolver.read_local(original.join("Image/a.png").to_str().unwrap(), LocatorBase::Absolute).unwrap(), b"clone");
        assert_eq!(fs::read(original.join("Image/a.png")).unwrap(), b"do-not-read-original");
    }

    #[test]
    fn traversal_devices_cloud_network_and_overlapping_aliases_are_refused() {
        let temp = fixture();
        let root = temp.path().join("clone");
        fs::create_dir_all(&root).unwrap();
        let alias = temp.path().join("old");
        assert!(ResourceResolver::new(&root, &[alias.clone(), alias.join("nested")]).is_err());
        assert!(ResourceResolver::new(&root, &[root.clone()]).is_err());
        let resolver = ResourceResolver::new(&root, &[]).unwrap();
        for value in ["../outside.png", "file:relative.png", "https://example.invalid/a.png", "image.png:secret", "NUL.png", "OneDrive/a.png"] {
            assert!(resolver.resolve(value, LocatorBase::ImageRoot).is_err(), "{value}");
        }
        #[cfg(windows)]
        for value in [r"\\server\share\a.png", r"\\.\C:\a.png", r"\\?\GLOBALROOT\Device\a.png"] {
            assert!(resolver.resolve(value, LocatorBase::Absolute).is_err());
        }
        assert!(publish_bytes(&root, "../outside.png", b"bad").is_err());
    }

    #[test]
    fn file_url_and_verbatim_paths_map_without_changing_payload() {
        let temp = fixture();
        fs::create_dir_all(temp.path().join("Image")).unwrap();
        let path = temp.path().join("Image/中文 image.png");
        fs::write(&path, b"image").unwrap();
        let resolver = ResourceResolver::new(temp.path(), &[]).unwrap();
        let url = url::Url::from_file_path(&path).unwrap().to_string();
        assert_eq!(resolver.read_local(&url, LocatorBase::Absolute).unwrap(), b"image");
        #[cfg(windows)]
        assert_eq!(resolver.read_local(fs::canonicalize(path).unwrap().to_str().unwrap(), LocatorBase::Absolute).unwrap(), b"image");
    }

    #[test]
    fn metadata_context_and_rich_output_use_clone_without_rewriting_original() {
        let temp = fixture();
        let root = temp.path().join("clone");
        let old = temp.path().join("never-created-original");
        fs::create_dir_all(root.join("migration")).unwrap();
        fs::create_dir_all(root.join("Image")).unwrap();
        fs::write(root.join("Image/a.png"), b"clone-image").unwrap();
        fs::write(root.join("migration/core-report.json"), serde_json::to_vec(&serde_json::json!({
            "sourceAliases":[old.to_string_lossy()]
        })).unwrap()).unwrap();
        let resolver = ResourceResolver::from_data_root(&root).unwrap();
        let old_path = old.join("Image/a.png");
        let mut record = crate::model::ClipRecord::new_text("raw", "QQ.exe");
        record.storage_revision = Some("core2:test".into());
        record.kind = crate::model::ClipKind::QQRichTextItem;
        record.qq_rich_text = Some(format!(r#"<QQRichEditFormat><EditElement type="3" filepath="{}" /></QQRichEditFormat>"#, old_path.display()));
        record.html_data = Some(crate::cf_html::build_clipboard_html(
            &format!(r#"<html><body><img src="{}"></body></html>"#, url::Url::from_file_path(&old_path).unwrap()), None));
        let raw_xml = record.qq_rich_text.clone();
        let raw_html = record.html_data.clone();
        let output = project_clip_for_output(&record, &resolver).unwrap();
        assert_eq!(record.qq_rich_text, raw_xml);
        assert_eq!(record.html_data, raw_html);
        assert_eq!(output.storage_revision, record.storage_revision);
        let parsed = crate::im_rich::document::ImRichDocument::parse(
            output.qq_rich_text.as_deref().unwrap(), crate::im_rich::document::RichOrigin::Qq,
        ).unwrap();
        let image = parsed.nodes().iter().find(|node| node.kind() == crate::im_rich::document::RichNodeKind::Image).unwrap();
        assert_eq!(resolver.resolve(image.path(), LocatorBase::Absolute).unwrap(), root.join("Image/a.png"));
        assert_eq!(resolver.read_local(image.path(), LocatorBase::Absolute).unwrap(), b"clone-image");
        assert!(!output.html_data.as_deref().unwrap().contains("never-created-original"));
        let io = project_clip_for_io(&record, &resolver).unwrap();
        assert!(io.storage_revision.is_none());
        assert_eq!(record.storage_revision.as_deref(), Some("core2:test"));
        assert!(!old.exists());
    }

    #[test]
    fn ordinary_external_file_list_is_preserved_without_reading_it() {
        let temp = fixture();
        let resolver = ResourceResolver::new(temp.path(), &[]).unwrap();
        let mut record = crate::model::ClipRecord::new_text("", "");
        record.kind = crate::model::ClipKind::FileClipItem;
        record.file_paths = vec![temp.path().join("outside-never-created/file.txt").to_string_lossy().into_owned()];
        let output = project_clip_for_output(&record, &resolver).unwrap();
        assert_eq!(record.file_paths, output.file_paths);
        assert!(!temp.path().join("outside-never-created").exists());
        assert_eq!(project_clip_for_io(&record, &resolver).unwrap_err(), "resource_missing");
    }

    #[test]
    fn io_file_lists_only_export_selected_clone_and_clear_revision_last() {
        let temp = fixture();
        let root = temp.path().join("clone");
        let old = temp.path().join("old");
        fs::create_dir_all(root.join("Files")).unwrap();
        fs::create_dir_all(old.join("Files")).unwrap();
        fs::write(root.join("Files/a.txt"), b"clone-only").unwrap();
        fs::write(old.join("Files/a.txt"), b"original-sentinel").unwrap();
        let resolver = ResourceResolver::new(&root, &[old.clone()]).unwrap();
        let mut record = crate::model::ClipRecord::new_text("", "");
        record.kind = crate::model::ClipKind::FileClipItem;
        record.storage_revision = Some("core2:file".into());
        record.file_paths = vec![old.join("Files/a.txt").to_string_lossy().into_owned()];
        assert_eq!(project_clip_for_output(&record, &resolver).unwrap().file_paths, record.file_paths);
        let output = project_clip_for_io(&record, &resolver).unwrap();
        assert_eq!(Path::new(&output.file_paths[0]), root.join("Files/a.txt"));
        assert_eq!(resolver.read_local(&output.file_paths[0], LocatorBase::Absolute).unwrap(), b"clone-only");
        assert!(output.storage_revision.is_none());
        assert_eq!(record.storage_revision.as_deref(), Some("core2:file"));
        record.file_paths = vec![temp.path().join("unmapped/a.txt").to_string_lossy().into_owned()];
        assert_eq!(project_clip_for_io(&record, &resolver).unwrap_err(), "unmapped_external_resource");
        assert!(!temp.path().join("unmapped").exists());
        record.file_paths = vec![old.join("Files").to_string_lossy().into_owned()];
        assert_eq!(project_clip_for_io(&record, &resolver).unwrap_err(), "resource_not_regular_file");
    }

    #[test]
    fn output_refuses_unmapped_and_unknown_local_markup_without_touching_source() {
        let temp = fixture();
        let resolver = ResourceResolver::new(temp.path(), &[]).unwrap();
        let outside = temp.path().parent().unwrap().join("unmapped-never-created/a.png");
        let mut record = crate::model::ClipRecord::new_text("", "");
        record.html_data = Some(format!(r#"<img src="{}">"#, outside.display()));
        let original = record.html_data.clone();
        assert!(project_clip_for_output(&record, &resolver).is_err());
        assert_eq!(record.html_data, original);
        record.html_data = Some(r#"<img srcset="file:///not-accessed.png 1x">"#.into());
        assert!(project_clip_for_output(&record, &resolver).is_err());
        record.html_data = Some(r#"<img src="https://example.invalid/preserved.png">"#.into());
        assert_eq!(project_clip_for_output(&record, &resolver).unwrap().html_data, record.html_data);
    }

    #[test]
    fn missing_and_non_directory_ancestor_are_distinct_errors() {
        let temp = fixture();
        let resolver = ResourceResolver::new(temp.path(), &[]).unwrap();
        assert_eq!(resolver.open_local("absent.png", LocatorBase::DataRoot).unwrap_err(), "resource_missing");
        fs::write(temp.path().join("file"), b"not-directory").unwrap();
        assert_eq!(resolver.open_local("file/a.png", LocatorBase::DataRoot).unwrap_err(), "resource_ancestor_not_directory");
    }

    #[cfg(windows)]
    #[test]
    fn placeholder_is_refused_before_data_access() {
        use std::os::windows::ffi::OsStrExt;
        #[link(name = "kernel32")]
        extern "system" { fn SetFileAttributesW(name: *const u16, flags: u32) -> i32; }
        let temp = fixture();
        let path = temp.path().join("offline.png");
        fs::write(&path, b"not-hydrated").unwrap();
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        assert_ne!(unsafe { SetFileAttributesW(wide.as_ptr(), 0x1000) }, 0);
        let resolver = ResourceResolver::new(temp.path(), &[]).unwrap();
        let result = resolver.open_local("offline.png", LocatorBase::DataRoot);
        assert_ne!(unsafe { SetFileAttributesW(wide.as_ptr(), 0x80) }, 0);
        assert_eq!(result.unwrap_err(), "resource_link_or_placeholder_refused");
    }
}
