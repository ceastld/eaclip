//! Conservative asset preparation for an offline v1 -> v2 migration.
//!
//! This module never changes source files or rich-text payloads. Only a missing
//! path is a `Missing` reference; unreadable files, links and cloud placeholders
//! are errors. A parser limitation is an explicit `Unresolved` reference.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AssetStatus {
    Ready,
    Missing,
    Unresolved,
}

impl AssetStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Missing => "missing",
            Self::Unresolved => "unresolved",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetMetadata {
    pub id: String,
    pub byte_length: i64,
    pub media_type: String,
    /// Relative to the candidate data root; never a source absolute path.
    pub relative_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetReference {
    pub scope: String,
    pub item_id: String,
    pub slot: String,
    pub ordinal: u32,
    pub original_locator: String,
    pub status: AssetStatus,
    pub asset: Option<AssetMetadata>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchivedFile {
    pub source_relative_path: String,
    pub archive_relative_path: String,
    pub sha256: String,
    pub byte_length: i64,
    pub protected: bool,
    /// Hash of the archived bytes (ciphertext when protected), not the source.
    pub actual_archive_sha256: String,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ArchiveSummary {
    pub files: Vec<ArchivedFile>,
    pub total_bytes: u64,
}

pub struct AssetImporter {
    source_root: PathBuf,
    source_aliases: Vec<PathBuf>,
    destination_root: PathBuf,
    assets: BTreeMap<String, AssetMetadata>,
    source_files: BTreeMap<PathBuf, (String, u64)>,
    missing_paths: BTreeSet<PathBuf>,
    archived_trees: BTreeMap<PathBuf, Vec<PathBuf>>,
    incomplete: bool,
}

impl AssetImporter {
    pub fn new(source_root: &Path, destination_root: &Path) -> Result<Self, String> {
        let source = absolute_without_parents(source_root)?;
        inspect_path(&source)?.ok_or("The source data root does not exist.")?;
        if !fs::metadata(&source).map_err(io_error)?.is_dir() {
            return Err("The source data root is not a directory.".into());
        }
        let source_root = fs::canonicalize(&source).map_err(io_error)?;
        let destination = absolute_without_parents(destination_root)?;
        if same_path(&source_root, &destination) || is_under(&source_root, &destination) {
            return Err("The candidate root must not equal or contain the source root.".into());
        }
        // A staging candidate beneath source_root is permitted, but an archive
        // root may never contain it (checked by archive_legacy_tree).
        ensure_directory(&destination)?;
        let destination_root = fs::canonicalize(&destination).map_err(io_error)?;
        if same_path(&source_root, &destination_root) || is_under(&source_root, &destination_root) {
            return Err("The candidate root overlaps the source root.".into());
        }
        Ok(Self {
            source_root,
            source_aliases: Vec::new(),
            destination_root,
            assets: BTreeMap::new(),
            source_files: BTreeMap::new(),
            missing_paths: BTreeSet::new(),
            archived_trees: BTreeMap::new(),
            incomplete: false,
        })
    }

    /// Explicit historical roots from an offline copy's provenance manifest.
    /// These paths are validated lexically only: no alias or ancestor is ever
    /// inspected or opened. Only asset locators are resolved through aliases;
    /// stored payloads and ordinary file-list content remain unchanged.
    pub fn set_source_aliases(&mut self, aliases: &[PathBuf]) -> Result<(), String> {
        let mut validated = Vec::with_capacity(aliases.len());
        for alias in aliases {
            validate_source_alias(alias)?;
            let alias: PathBuf = alias.components().collect();
            if validated.iter().any(|previous: &PathBuf| {
                same_path(&alias, previous)
                    || relative_beneath(&alias, previous).is_some()
                    || relative_beneath(previous, &alias).is_some()
            }) {
                return Err("Historical source aliases must not duplicate or overlap.".into());
            }
            validated.push(alias);
        }
        self.source_aliases = validated;
        Ok(())
    }

    pub fn assets(&self) -> impl Iterator<Item = &AssetMetadata> {
        self.assets.values()
    }

    /// Incomplete means the migration must retain its old-file archive and must
    /// not claim a self-contained backup or enable deletion of old resources.
    pub fn has_incomplete_references(&self) -> bool {
        self.incomplete
    }

    /// Run immediately before publishing a candidate. This covers resources
    /// found by the parser and every file retained in the legacy archive.
    pub fn revalidate_sources(&self) -> Result<(), String> {
        for (root, expected) in &self.archived_trees {
            if &enumerate_tree(root)? != expected {
                return Err("An archived source tree changed after preparation.".into());
            }
        }
        for (path, expected) in &self.source_files {
            let mut file = open_checked(path, &self.source_root)?;
            if &digest_file(&mut file)? != expected {
                return Err(format!(
                    "Source resource changed after preparation: {}",
                    path.display()
                ));
            }
        }
        for path in &self.missing_paths {
            if inspect_path(path)?.is_some() {
                return Err(format!(
                    "A previously missing source resource appeared: {}",
                    path.display()
                ));
            }
        }
        Ok(())
    }

    pub fn import_reference(
        &mut self,
        scope: &str,
        id: &str,
        slot: &str,
        ordinal: u32,
        locator: &str,
    ) -> Result<AssetReference, String> {
        let mut reference = AssetReference {
            scope: scope.into(),
            item_id: id.into(),
            slot: slot.into(),
            ordinal,
            original_locator: locator.into(),
            status: AssetStatus::Ready,
            asset: None,
            reason: None,
        };
        let path = match self.resolve_reference(locator) {
            Ok(path) => path,
            Err(reason) => {
                self.incomplete = true;
                reference.status = AssetStatus::Unresolved;
                reference.reason = Some(reason);
                if reference.original_locator.is_empty() {
                    reference.original_locator = format!("unparsed:{slot}");
                }
                return Ok(reference);
            }
        };
        let source_relative: PathBuf = path
            .components()
            .skip(self.source_root.components().count())
            .collect();
        if super::protected_archive::is_sensitive_path(&source_relative) {
            self.incomplete = true;
            reference.status = AssetStatus::Unresolved;
            reference.reason = Some("A sensitive source path cannot be imported as a public image asset; protected archival is required.".into());
            return Ok(reference);
        }
        let Some(metadata) = inspect_path(&path)? else {
            self.missing_paths.insert(path);
            self.incomplete = true;
            reference.status = AssetStatus::Missing;
            reference.reason = Some("The source resource does not exist.".into());
            return Ok(reference);
        };
        if !metadata.is_file() {
            return Err(format!(
                "An asset reference is not a regular file: {}",
                path.display()
            ));
        }
        let mut source = open_checked(&path, &self.source_root)?;
        let media_type = detect_media_type(&read_prefix(&mut source)?);
        if media_type == "application/octet-stream" {
            let (digest, size) = digest_file(&mut source)?;
            self.remember_source(path, &digest, size)?;
            self.incomplete = true;
            reference.status = AssetStatus::Unresolved;
            reference.reason = Some("The source is not a recognized image; it must remain in a protected archive, not public assets.".into());
            return Ok(reference);
        }
        let staged_dir = self.destination_root.join("staging").join("asset-import");
        ensure_directory(&staged_dir)?;
        let (staged, digest, size, prefix) = stage_copy(&mut source, &staged_dir)?;
        if detect_media_type(&prefix) != media_type {
            return Err("Source image format changed during preparation.".into());
        }
        self.remember_source(path, &digest, size)?;
        let relative_path = format!("assets/{}/{}", &digest[..2], digest);
        let destination = self.destination_root.join(&relative_path);
        publish(staged, &destination, &self.destination_root, &digest, size)?;
        let asset = AssetMetadata {
            id: digest,
            byte_length: i64::try_from(size).map_err(|_| "Asset exceeds SQLite integer size.")?,
            media_type: media_type.into(),
            relative_path,
        };
        if let Some(previous) = self.assets.get(&asset.id) {
            if previous != &asset {
                return Err("Conflicting metadata for one asset hash.".into());
            }
        }
        self.assets.insert(asset.id.clone(), asset.clone());
        reference.asset = Some(asset);
        Ok(reference)
    }

    /// Extract only embedded HTML/IM references. Ordinary images and icons are
    /// imported separately by the converter. The supplied payload is unchanged.
    pub fn collect_payload_references(
        &mut self,
        scope: &str,
        id: &str,
        _kind: &str,
        payload: &serde_json::Value,
        source_process: &str,
    ) -> Result<Vec<AssetReference>, String> {
        let mut record = crate::model::ClipRecord::new_text("", source_process);
        record.html_data = payload
            .get("html")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        record.html_source_url = payload
            .get("sourceUrl")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        record.qq_rich_text = payload
            .get("xml")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        self.collect_record_references(scope, id, &record)
    }

    pub fn collect_record_references(
        &mut self,
        scope: &str,
        id: &str,
        record: &crate::model::ClipRecord,
    ) -> Result<Vec<AssetReference>, String> {
        let mut references = Vec::new();
        for slot in ["html", "im"] {
            let mut scratch = record.clone();
            if slot == "html" {
                scratch.qq_rich_text = None;
            } else {
                scratch.html_data = None;
            }
            let raw = if slot == "html" {
                scratch.html_data.as_deref()
            } else {
                scratch.qq_rich_text.as_deref()
            };
            if raw.is_none_or(str::is_empty) {
                continue;
            }
            let mut locators = Vec::new();
            // The legacy import parser treats a verbatim drive prefix as an
            // unknown URI scheme. Normalize only recognized IM image nodes in
            // the disposable document, while retaining each original locator.
            let mut originals = if slot == "im" {
                normalize_im_parser_paths(&mut scratch)?
            } else {
                BTreeMap::new()
            };
            // The existing rewrite parser expects a fully qualified destination
            // when serializing HTML attributes. Its output is discarded here;
            // a relative original locator must not terminate enumeration early.
            let placeholder = self
                .destination_root
                .join("assets/parser-placeholder")
                .to_string_lossy()
                .into_owned();
            let result =
                crate::csharp_import_assets::rewrite_embedded_images(&mut scratch, &mut |path| {
                    let original = originals
                        .get_mut(path)
                        .and_then(VecDeque::pop_front)
                        .unwrap_or_else(|| path.to_owned());
                    locators.push(original);
                    // Rewrite only a disposable clone; never feed it back to storage.
                    Ok(placeholder.clone())
                });
            for (ordinal, locator) in locators.iter().enumerate() {
                references.push(self.import_reference(
                    scope,
                    id,
                    slot,
                    u32::try_from(ordinal).map_err(|_| "Too many embedded references.")?,
                    locator,
                )?);
            }
            if let Err(reason) = result {
                self.incomplete = true;
                references.push(AssetReference {
                    scope: scope.into(),
                    item_id: id.into(),
                    slot: slot.into(),
                    ordinal: u32::try_from(locators.len())
                        .map_err(|_| "Too many embedded references.")?,
                    original_locator: format!("unparsed:{slot}"),
                    status: AssetStatus::Unresolved,
                    asset: None,
                    reason: Some(reason),
                });
            }
        }
        Ok(references)
    }

    /// Preserve an explicitly selected source-owned tree when raw markup may
    /// contain unenumerated references. No external user directory is followed.
    /// Re-running verifies existing archive bytes rather than overwriting them.
    pub fn archive_legacy_tree(&mut self, relative: &Path) -> Result<ArchiveSummary, String> {
        validate_relative(relative)?;
        let source = self.source_root.join(relative);
        if is_under(&self.destination_root, &source) || same_path(&source, &self.destination_root) {
            return Err("An archive source must not contain the candidate root.".into());
        }
        let Some(metadata) = inspect_path(&source)? else {
            self.missing_paths.insert(source);
            return Ok(ArchiveSummary::default());
        };
        if !metadata.is_dir() {
            return Err("The archive source must be a directory.".into());
        }
        let source_tree = source.clone();
        let expected_tree = enumerate_tree(&source_tree)?;
        let mut pending = vec![source];
        let mut summary = ArchiveSummary::default();
        while let Some(directory) = pending.pop() {
            let mut entries = fs::read_dir(&directory)
                .map_err(io_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(io_error)?;
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                let path = entry.path();
                let metadata = inspect_path(&path)?
                    .ok_or("An archive source disappeared during enumeration.")?;
                if metadata.is_dir() {
                    pending.push(path);
                    continue;
                }
                if !metadata.is_file() {
                    return Err("An archive entry is not a regular file.".into());
                }
                let relative = path
                    .strip_prefix(&self.source_root)
                    .map_err(|_| "Archive boundary violation.")?
                    .to_owned();
                let mut archive_relative =
                    Path::new("migration").join("legacy-assets").join(&relative);
                let stage_dir = self.destination_root.join("staging").join("asset-import");
                ensure_directory(&stage_dir)?;
                let mut input = open_checked(&path, &self.source_root)?;
                let protected = super::protected_archive::is_sensitive_path(&relative)
                    || detect_media_type(&read_prefix(&mut input)?) == "application/octet-stream";
                let (digest, size, archive_digest) = if protected {
                    let mut name = archive_relative.into_os_string();
                    name.push(".dpapi");
                    archive_relative = PathBuf::from(name);
                    protect_archive_file(
                        &mut input,
                        &self.destination_root.join(&archive_relative),
                        &self.destination_root,
                        &stage_dir,
                    )?
                } else {
                    let (staged, digest, size, _) = stage_copy(&mut input, &stage_dir)?;
                    publish(
                        staged,
                        &self.destination_root.join(&archive_relative),
                        &self.destination_root,
                        &digest,
                        size,
                    )?;
                    (digest.clone(), size, digest)
                };
                self.remember_source(path, &digest, size)?;
                summary.total_bytes = summary
                    .total_bytes
                    .checked_add(size)
                    .ok_or("Archive size overflow.")?;
                summary.files.push(ArchivedFile {
                    source_relative_path: slash(&relative),
                    archive_relative_path: slash(&archive_relative),
                    sha256: digest,
                    byte_length: i64::try_from(size).map_err(|_| "Archive file too large.")?,
                    protected,
                    actual_archive_sha256: archive_digest,
                });
            }
        }
        if enumerate_tree(&source_tree)? != expected_tree {
            return Err("An archive source tree changed during preparation.".into());
        }
        self.archived_trees.insert(source_tree, expected_tree);
        summary
            .files
            .sort_by(|a, b| a.source_relative_path.cmp(&b.source_relative_path));
        Ok(summary)
    }

    fn remember_source(&mut self, path: PathBuf, digest: &str, size: u64) -> Result<(), String> {
        let expected = (digest.to_owned(), size);
        if self.missing_paths.contains(&path)
            || self
                .source_files
                .get(&path)
                .is_some_and(|previous| previous != &expected)
        {
            return Err(
                "Source asset changed between repeated references; migration must restart.".into(),
            );
        }
        self.source_files.insert(path, expected);
        Ok(())
    }

    fn resolve_reference(&self, locator: &str) -> Result<PathBuf, String> {
        let raw = locator.strip_prefix("icon:").unwrap_or(locator);
        if raw.trim().is_empty() {
            return Err("The resource locator is empty.".into());
        }
        let original = if raw.to_ascii_lowercase().starts_with("file:") {
            // URL parsing removes dot segments. Reject traversal first so a
            // historical alias cannot authorize a normalized escape/re-entry.
            if raw[5..]
                .split(['?', '#'])
                .next()
                .unwrap_or("")
                .split(['/', '\\'])
                .any(|part| part.to_ascii_lowercase().replace("%2e", ".") == "..")
            {
                return Err("Parent traversal is not accepted in asset file URLs.".into());
            }
            url::Url::parse(raw)
                .ok()
                .and_then(|url| url.to_file_path().ok())
                .ok_or("The file URL is not an absolute local path.")?
        } else {
            if raw.contains("://") || raw.starts_with("data:") {
                return Err("An external resource is not a source-owned asset.".into());
            }
            PathBuf::from(raw)
        };
        reject_unsafe_components(&original)?;
        let path = if original.is_absolute() {
            // Prefer a locator already within this clone, even when an alias
            // happens to contain the clone's own path. Never inspect the old
            // root: strip components lexically and check only the joined clone.
            if relative_beneath(&original, &self.source_root).is_some() {
                original
            } else if let Some(relative) = self
                .source_aliases
                .iter()
                .find_map(|alias| relative_beneath(&original, alias))
            {
                validate_relative(&relative)?;
                self.source_root.join(relative)
            } else {
                original
            }
        } else {
            let first = original
                .components()
                .next()
                .map(|p| p.as_os_str().to_string_lossy().to_ascii_lowercase());
            if matches!(first.as_deref(), Some("image" | "blobs" | "lanshare")) {
                self.source_root.join(original)
            } else {
                self.source_root.join("Image").join(original)
            }
        };
        if !is_under(&path, &self.source_root) {
            return Err(
                "The resource is outside the selected source data root; it was not read.".into(),
            );
        }
        Ok(path)
    }
}

fn validate_source_alias(alias: &Path) -> Result<(), String> {
    if !alias.is_absolute() {
        return Err("A historical source alias must be an absolute directory path.".into());
    }
    reject_unsafe_components(alias)?;
    let text = alias
        .to_str()
        .ok_or("A historical source alias must be valid Unicode.")?;
    if text
        .split(['/', '\\'])
        .any(|part| part == "." || part == "..")
    {
        return Err("Historical source aliases must not contain dot components.".into());
    }
    #[cfg(windows)]
    for component in alias.components() {
        match component {
            Component::Prefix(prefix)
                if !matches!(
                    prefix.kind(),
                    std::path::Prefix::Disk(_) | std::path::Prefix::UNC(_, _)
                ) =>
            {
                return Err("Historical source aliases must not use a device namespace.".into());
            }
            Component::Normal(name) => {
                let name = name
                    .to_str()
                    .ok_or("Invalid Unicode source alias component.")?;
                let stem = name
                    .split('.')
                    .next()
                    .unwrap_or("")
                    .trim_end_matches(' ')
                    .to_ascii_lowercase();
                let reserved = matches!(
                    stem.as_str(),
                    "con" | "prn" | "aux" | "nul" | "conin$" | "conout$" | "clock$"
                ) || ["com", "lpt"].iter().any(|prefix| {
                    stem.strip_prefix(*prefix).is_some_and(|suffix| {
                        matches!(
                            suffix,
                            "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                        )
                    })
                });
                if reserved || name.chars().any(|c| c < ' ' || "<>:\"|?*".contains(c)) {
                    return Err(
                        "Historical source aliases contain an ambiguous Windows name.".into(),
                    );
                }
            }
            _ => {}
        }
    }
    if !alias
        .components()
        .any(|part| matches!(part, Component::Normal(_)))
    {
        return Err("A historical source alias must not be a drive or share root.".into());
    }
    Ok(())
}

/// Component-based containment avoids matching CeastldExtra to Ceastld. The
/// returned suffix retains its spelling; case folding is only for comparison.
fn relative_beneath(path: &Path, root: &Path) -> Option<PathBuf> {
    let mut components = path.components();
    for expected in root.components() {
        let actual = components.next()?;
        #[cfg(windows)]
        let equal = comparison(Path::new(actual.as_os_str()))
            == comparison(Path::new(expected.as_os_str()));
        #[cfg(not(windows))]
        let equal = actual == expected;
        if !equal {
            return None;
        }
    }
    let suffix: PathBuf = components.collect();
    (!suffix.as_os_str().is_empty()).then_some(suffix)
}

fn slash(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}
fn io_error(error: std::io::Error) -> String {
    format!("Asset I/O failed: {error}")
}

fn normalize_im_parser_paths(
    record: &mut crate::model::ClipRecord,
) -> Result<BTreeMap<String, VecDeque<String>>, String> {
    use crate::im_rich::document::{ImRichDocument, RichNodeKind, RichOrigin};
    let mut originals: BTreeMap<String, VecDeque<String>> = BTreeMap::new();
    let Some(raw) = record.qq_rich_text.as_deref() else {
        return Ok(originals);
    };
    let origin = match crate::im_rich::resolve_origin(&record.source_process_name, raw) {
        crate::im_rich::ImRichOrigin::Qq => RichOrigin::Qq,
        crate::im_rich::ImRichOrigin::WeChat => RichOrigin::Wechat,
        crate::im_rich::ImRichOrigin::Wework => RichOrigin::Wework,
        crate::im_rich::ImRichOrigin::Unknown => RichOrigin::Unknown,
    };
    let Ok(mut document) = ImRichDocument::parse(raw, origin) else {
        // Let the strict import parser report the unhandled original syntax.
        return Ok(originals);
    };
    let images: Vec<_> = document
        .nodes()
        .iter()
        .filter(|node| node.kind() == RichNodeKind::Image && !node.path().is_empty())
        .map(|node| (node.id(), node.path().to_owned()))
        .collect();
    for (id, original) in images {
        let normalized = if let Some(rest) = original.strip_prefix(r"\\?\UNC\") {
            format!(r"\\{rest}")
        } else {
            original
                .strip_prefix(r"\\?\")
                .unwrap_or(&original)
                .to_owned()
        };
        originals
            .entry(normalized.clone())
            .or_default()
            .push_back(original.clone());
        if normalized != original {
            document
                .set_path(id, &normalized)
                .map_err(|error| format!("Cannot inspect verbatim IM image path: {error}"))?;
        }
    }
    record.qq_rich_text = Some(document.serialize());
    Ok(originals)
}

fn validate_relative(path: &Path) -> Result<(), String> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|p| !matches!(p, Component::Normal(_)))
    {
        return Err("Expected a nonempty relative path without traversal.".into());
    }
    reject_unsafe_components(path)
}

fn reject_unsafe_components(path: &Path) -> Result<(), String> {
    if path.components().any(|p| matches!(p, Component::ParentDir)) {
        return Err("Parent traversal is not accepted in asset paths.".into());
    }
    #[cfg(windows)]
    {
        let text = path.to_string_lossy().replace('/', "\\");
        let lower = text.to_ascii_lowercase();
        if lower.starts_with(r"\\.\") || lower.starts_with(r"\\?\globalroot\") {
            return Err("Device namespace asset paths are not accepted.".into());
        }
        for component in path.components() {
            if let Component::Normal(name) = component {
                let name = name.to_string_lossy();
                if name.contains(':') || name.ends_with('.') || name.ends_with(' ') {
                    return Err(
                        "Alternate streams and ambiguous Windows names are not accepted.".into(),
                    );
                }
            }
        }
    }
    Ok(())
}

fn absolute_without_parents(path: &Path) -> Result<PathBuf, String> {
    reject_unsafe_components(path)?;
    std::path::absolute(path).map_err(io_error)
}

#[cfg(windows)]
fn comparison(path: &Path) -> String {
    let text = path.to_string_lossy().replace('/', "\\");
    let text = if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else {
        text.strip_prefix(r"\\?\").unwrap_or(&text).to_owned()
    };
    text.trim_end_matches('\\').to_lowercase()
}

fn same_path(a: &Path, b: &Path) -> bool {
    #[cfg(windows)]
    {
        comparison(a) == comparison(b)
    }
    #[cfg(not(windows))]
    {
        a == b
    }
}

fn is_under(path: &Path, root: &Path) -> bool {
    #[cfg(windows)]
    {
        comparison(path).starts_with(&(comparison(root) + "\\"))
    }
    #[cfg(not(windows))]
    {
        path != root && path.starts_with(root)
    }
}

fn safe_metadata(metadata: &Metadata) -> Result<(), String> {
    if metadata.file_type().is_symlink() {
        return Err("Asset paths must not contain symbolic links.".into());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        reject_windows_attributes(metadata.file_attributes())?;
    }
    Ok(())
}

#[cfg(windows)]
fn reject_windows_attributes(attributes: u32) -> Result<(), String> {
    const BLOCKED: u32 = 0x400 | 0x1000 | 0x40000 | 0x400000;
    if attributes & BLOCKED != 0 {
        return Err("Asset path is a reparse point or cloud placeholder (Offline/Recall); content was not read.".into());
    }
    Ok(())
}

/// Inspect each ancestor before descending, so links/placeholders are rejected
/// before content access. A missing component is the only non-error absence.
fn inspect_path(path: &Path) -> Result<Option<Metadata>, String> {
    let mut current = PathBuf::new();
    let mut last = None;
    let mut components = path.components().peekable();
    while let Some(component) = components.next() {
        current.push(component.as_os_str());
        if matches!(component, Component::Prefix(_)) {
            continue;
        }
        reject_cloud_directory_name(component.as_os_str())?;
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                safe_metadata(&metadata)?;
                // On Windows, ERROR_PATH_NOT_FOUND for file/child is reported
                // as NotFound. Reject the non-directory ancestor first rather
                // than silently classifying the resource as absent.
                if components.peek().is_some() && !metadata.is_dir() {
                    return Err("An asset path ancestor is not a directory.".into());
                }
                last = Some(metadata);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io_error(error)),
        }
    }
    Ok(last)
}

fn ensure_directory(path: &Path) -> Result<(), String> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        if matches!(component, Component::Prefix(_)) {
            continue;
        }
        reject_cloud_directory_name(component.as_os_str())?;
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&current).map_err(io_error)?;
                fs::symlink_metadata(&current).map_err(io_error)?
            }
            Err(error) => return Err(io_error(error)),
        };
        safe_metadata(&metadata)?;
        if !metadata.is_dir() {
            return Err("An asset destination parent is not a directory.".into());
        }
    }
    Ok(())
}

fn reject_cloud_directory_name(name: &std::ffi::OsStr) -> Result<(), String> {
    let name = name.to_string_lossy().to_ascii_lowercase();
    if name.starts_with("onedrive") || name == "dropbox" || name.starts_with("icloud") {
        return Err(
            "Cloud-synchronized directories are not traversed by the offline asset importer."
                .into(),
        );
    }
    Ok(())
}

fn enumerate_tree(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut directories = vec![root.to_owned()];
    let mut files = Vec::new();
    while let Some(directory) = directories.pop() {
        let metadata = inspect_path(&directory)?.ok_or("An archive directory disappeared.")?;
        if !metadata.is_dir() {
            return Err("An archive directory changed its type.".into());
        }
        for entry in fs::read_dir(&directory).map_err(io_error)? {
            let path = entry.map_err(io_error)?.path();
            let metadata = inspect_path(&path)?.ok_or("An archive entry disappeared.")?;
            if metadata.is_dir() {
                directories.push(path);
            } else if metadata.is_file() {
                files.push(path);
            } else {
                return Err("An archive entry is not a regular file.".into());
            }
        }
    }
    files.sort();
    Ok(files)
}

fn open_checked(path: &Path, root: &Path) -> Result<File, String> {
    inspect_path(path)?.ok_or("An asset disappeared before it could be opened.")?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Do not follow a substituted final link; keep source bytes stable by
        // denying write/delete sharing while this read handle is held.
        options.custom_flags(0x0020_0000).share_mode(1);
    }
    let file = options.open(path).map_err(io_error)?;
    let metadata = file.metadata().map_err(io_error)?;
    safe_metadata(&metadata)?;
    if !metadata.is_file() {
        return Err("An asset is not a regular file.".into());
    }
    let opened_path = opened_path(&file, path)?;
    if !is_under(&opened_path, root) {
        return Err("Opened asset escaped its selected data root.".into());
    }
    inspect_path(path)?.ok_or("An asset path disappeared after opening.")?;
    Ok(file)
}

#[cfg(windows)]
fn opened_path(file: &File, _path: &Path) -> Result<PathBuf, String> {
    use std::os::windows::io::AsRawHandle;
    #[link(name = "kernel32")]
    extern "system" {
        fn GetFinalPathNameByHandleW(
            handle: *mut std::ffi::c_void,
            path: *mut u16,
            size: u32,
            flags: u32,
        ) -> u32;
    }
    let mut buffer = vec![0u16; 512];
    loop {
        let size = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle(),
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                0,
            )
        };
        if size == 0 {
            return Err(io_error(std::io::Error::last_os_error()));
        }
        if size as usize >= buffer.len() {
            buffer.resize(size as usize + 1, 0);
            continue;
        }
        return Ok(PathBuf::from(
            String::from_utf16(&buffer[..size as usize])
                .map_err(|_| "Invalid Unicode file path.")?,
        ));
    }
}

#[cfg(not(windows))]
fn opened_path(_file: &File, path: &Path) -> Result<PathBuf, String> {
    fs::canonicalize(path).map_err(io_error)
}

fn digest_file(file: &mut File) -> Result<(String, u64), String> {
    file.seek(SeekFrom::Start(0)).map_err(io_error)?;
    let mut hash = Sha256::new();
    let mut size = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer).map_err(io_error)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
        size = size
            .checked_add(count as u64)
            .ok_or("Asset size overflow.")?;
    }
    Ok((format!("{:x}", hash.finalize()), size))
}

fn read_prefix(file: &mut File) -> Result<Vec<u8>, String> {
    file.seek(SeekFrom::Start(0)).map_err(io_error)?;
    let mut prefix = Vec::new();
    (&mut *file)
        .take(512)
        .read_to_end(&mut prefix)
        .map_err(io_error)?;
    file.seek(SeekFrom::Start(0)).map_err(io_error)?;
    Ok(prefix)
}

const MAX_PROTECTED_BYTES: u64 = 512 * 1024 * 1024;

fn read_bounded(file: &mut File, maximum: u64) -> Result<Vec<u8>, String> {
    if file.metadata().map_err(io_error)?.len() > maximum {
        return Err(
            "Protected archive exceeds its 512 MiB limit; no plaintext fallback is allowed.".into(),
        );
    }
    file.seek(SeekFrom::Start(0)).map_err(io_error)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(
            usize::try_from(file.metadata().map_err(io_error)?.len())
                .map_err(|_| "Protected archive length does not fit memory size.")?,
        )
        .map_err(|_| {
            "Cannot reserve protected archive memory; no plaintext fallback is allowed."
        })?;
    (&mut *file)
        .take(maximum + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() as u64 > maximum {
        return Err(
            "Protected archive grew beyond its limit; no plaintext fallback is allowed.".into(),
        );
    }
    Ok(bytes)
}

fn protect_archive_file(
    input: &mut File,
    destination: &Path,
    root: &Path,
    stage_dir: &Path,
) -> Result<(String, u64, String), String> {
    let before = input.metadata().map_err(io_error)?;
    let plaintext = read_bounded(input, MAX_PROTECTED_BYTES)?;
    let digest = format!("{:x}", Sha256::digest(&plaintext));
    let size = plaintext.len() as u64;
    let after = input.metadata().map_err(io_error)?;
    if before.len() != size
        || after.len() != size
        || before.modified().ok() != after.modified().ok()
        || digest_file(input)? != (digest.clone(), size)
    {
        return Err("Source protected resource changed during preparation.".into());
    }
    if inspect_path(destination)?.is_none() {
        let ciphertext = super::protected_archive::protect(&plaintext)?;
        if super::protected_archive::unprotect(&ciphertext)? != plaintext {
            return Err("Protected archive round-trip validation failed.".into());
        }
        let encrypted_digest = format!("{:x}", Sha256::digest(&ciphertext));
        let mut staged = tempfile::NamedTempFile::new_in(stage_dir).map_err(io_error)?;
        staged.write_all(&ciphertext).map_err(io_error)?;
        staged.flush().map_err(io_error)?;
        staged.as_file().sync_all().map_err(io_error)?;
        publish(
            staged,
            destination,
            root,
            &encrypted_digest,
            ciphertext.len() as u64,
        )?;
    }
    // DPAPI output is randomized. Reuse an existing archive only after decrypting
    // and comparing its plaintext; never replace it with newly randomized bytes.
    let mut archived = open_checked(destination, root)?;
    let ciphertext = read_bounded(&mut archived, MAX_PROTECTED_BYTES + 1024 * 1024 + 4096)?;
    if super::protected_archive::unprotect(&ciphertext)? != plaintext {
        return Err(
            "Existing protected archive does not contain the source bytes; it was not overwritten."
                .into(),
        );
    }
    Ok((digest, size, format!("{:x}", Sha256::digest(&ciphertext))))
}

fn stage_copy(
    source: &mut File,
    directory: &Path,
) -> Result<(tempfile::NamedTempFile, String, u64, Vec<u8>), String> {
    let before = source.metadata().map_err(io_error)?;
    let mut staged = tempfile::NamedTempFile::new_in(directory).map_err(io_error)?;
    let mut hash = Sha256::new();
    let mut size = 0u64;
    let mut prefix = Vec::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = source.read(&mut buffer).map_err(io_error)?;
        if count == 0 {
            break;
        }
        if prefix.len() < 512 {
            prefix.extend_from_slice(&buffer[..count.min(512 - prefix.len())]);
        }
        hash.update(&buffer[..count]);
        staged.write_all(&buffer[..count]).map_err(io_error)?;
        size = size
            .checked_add(count as u64)
            .ok_or("Asset size overflow.")?;
    }
    staged.flush().map_err(io_error)?;
    staged.as_file().sync_all().map_err(io_error)?;
    let digest = format!("{:x}", hash.finalize());
    let after = source.metadata().map_err(io_error)?;
    if before.len() != size
        || after.len() != size
        || before.modified().ok() != after.modified().ok()
        || digest_file(source)? != (digest.clone(), size)
        || digest_file(staged.as_file_mut())? != (digest.clone(), size)
    {
        return Err(
            "Source asset changed while it was being prepared; candidate was not published.".into(),
        );
    }
    Ok((staged, digest, size, prefix))
}

fn publish(
    staged: tempfile::NamedTempFile,
    destination: &Path,
    root: &Path,
    digest: &str,
    size: u64,
) -> Result<(), String> {
    if !is_under(destination, root) {
        return Err("Asset publication escaped the candidate root.".into());
    }
    ensure_directory(
        destination
            .parent()
            .ok_or("Asset destination has no parent.")?,
    )?;
    let verify = || -> Result<(), String> {
        let mut existing = open_checked(destination, root)?;
        if digest_file(&mut existing)? != (digest.to_owned(), size) {
            return Err("Existing destination has different bytes; it was not overwritten.".into());
        }
        Ok(())
    };
    if inspect_path(destination)?.is_some() {
        return verify();
    }
    match staged.persist_noclobber(destination) {
        Ok(file) => {
            drop(file);
            verify()
        }
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => verify(),
        Err(error) => Err(io_error(error.error)),
    }
}

fn detect_media_type(prefix: &[u8]) -> &'static str {
    match image::guess_format(prefix).ok() {
        Some(image::ImageFormat::Png) => "image/png",
        Some(image::ImageFormat::Jpeg) => "image/jpeg",
        Some(image::ImageFormat::Gif) => "image/gif",
        Some(image::ImageFormat::WebP) => "image/webp",
        Some(image::ImageFormat::Bmp) => "image/bmp",
        Some(image::ImageFormat::Tiff) => "image/tiff",
        Some(image::ImageFormat::Ico) => "image/x-icon",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture(payload: &[u8]) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(payload);
        bytes
    }

    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let root =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.tmp/storage-v2-assets-tests");
        fs::create_dir_all(&root).unwrap();
        let root = fs::canonicalize(root).unwrap();
        let temp = tempfile::Builder::new()
            .prefix("assets-")
            .tempdir_in(root)
            .unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("candidate");
        fs::create_dir_all(source.join("Image/Temp")).unwrap();
        (temp, source, destination)
    }

    // canonicalize() gives the fixture a verbatim Windows prefix. Provenance
    // aliases deliberately accept ordinary disk/UNC roots only.
    fn ordinary_alias(path: &Path) -> PathBuf {
        #[cfg(windows)]
        {
            let text = path.to_str().unwrap();
            if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
                return PathBuf::from(format!(r"\\{rest}"));
            }
            PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(text))
        }
        #[cfg(not(windows))]
        {
            path.to_owned()
        }
    }

    #[test]
    fn historical_alias_reads_only_the_clone_even_when_the_old_root_does_not_exist() {
        let (temp, source, destination) = fixture();
        let alias = ordinary_alias(&temp.path().join("absent-old-machine/Ceastld"));
        let bytes = picture(b"offline clone only");
        fs::write(source.join("Image/Temp/a.png"), &bytes).unwrap();
        let locator = alias
            .join("Image/Temp/a.png")
            .to_string_lossy()
            .into_owned();
        let mut importer = AssetImporter::new(&source, &destination).unwrap();
        importer.set_source_aliases(&[alias.clone()]).unwrap();
        let reference = importer
            .import_reference("history", "h", "image", 0, &locator)
            .unwrap();
        assert_eq!(reference.status, AssetStatus::Ready);
        assert_eq!(reference.original_locator, locator);
        let asset = reference.asset.unwrap();
        assert_eq!(asset.id, format!("{:x}", Sha256::digest(&bytes)));
        assert_eq!(
            fs::read(destination.join(asset.relative_path)).unwrap(),
            bytes
        );
        assert!(!alias.exists());
        importer.revalidate_sources().unwrap();
    }

    #[test]
    fn historical_alias_file_urls_keep_unicode_encoding_and_windows_case_semantics() {
        let (temp, source, destination) = fixture();
        let alias = ordinary_alias(&temp.path().join("old-资料/Ceastld"));
        let filename = "图 # % &.png";
        let bytes = picture(b"encoded unicode alias");
        fs::write(source.join("Image/Temp").join(filename), &bytes).unwrap();
        #[cfg(windows)]
        let locator_root = PathBuf::from(alias.to_str().unwrap().to_uppercase());
        #[cfg(not(windows))]
        let locator_root = alias.clone();
        let locator_path = locator_root.join("Image/Temp").join(filename);
        let locator = url::Url::from_file_path(&locator_path).unwrap().to_string();
        assert!(locator.contains("%23") && locator.contains("%25"));
        let mut importer = AssetImporter::new(&source, &destination).unwrap();
        importer.set_source_aliases(&[alias.clone()]).unwrap();
        let reference = importer
            .import_reference("favorite", "f", "content_icon", 0, &locator)
            .unwrap();
        assert_eq!(reference.status, AssetStatus::Ready);
        assert_eq!(reference.original_locator, locator);
        assert_eq!(
            fs::read(destination.join(reference.asset.unwrap().relative_path)).unwrap(),
            bytes
        );
        assert!(!alias.exists());
    }

    #[test]
    fn historical_alias_authorization_rejects_overlap_prefix_tricks_and_traversal() {
        let (temp, source, destination) = fixture();
        let alias = ordinary_alias(&temp.path().join("old/Ceastld"));
        let bytes = picture(b"authorized clone");
        fs::write(source.join("Image/Temp/a.png"), &bytes).unwrap();
        let mut importer = AssetImporter::new(&source, &destination).unwrap();
        importer.set_source_aliases(&[alias.clone()]).unwrap();
        for aliases in [
            vec![alias.clone(), alias.clone()],
            vec![alias.clone(), alias.join("Image")],
            vec![alias.join("Image"), alias.clone()],
        ] {
            assert!(importer.set_source_aliases(&aliases).is_err());
        }
        let mut invalid = vec![
            PathBuf::from("relative/Ceastld"),
            alias.join(".."),
            alias.join("."),
            alias.ancestors().last().unwrap().to_owned(),
        ];
        #[cfg(windows)]
        {
            assert!(importer
                .set_source_aliases(&[
                    alias.clone(),
                    PathBuf::from(alias.to_str().unwrap().to_uppercase())
                ])
                .is_err());
            invalid.extend([
                PathBuf::from(r"\\?\C:\offline\Ceastld"),
                PathBuf::from(r"\\.\C:\offline\Ceastld"),
                PathBuf::from(r"\\server\share"),
                PathBuf::from(r"C:\offline\folder:stream"),
                PathBuf::from(r"C:\offline\trailing."),
                PathBuf::from("C:\\offline\\trailing "),
                PathBuf::from(r"C:\offline\NUL.txt"),
            ]);
        }
        for path in invalid {
            assert!(importer.set_source_aliases(&[path]).is_err());
        }
        let alias_url = url::Url::from_directory_path(&alias).unwrap().to_string();
        let unauthorized = temp.path().join("outside/Image/Temp/a.png");
        fs::create_dir_all(unauthorized.parent().unwrap()).unwrap();
        fs::write(&unauthorized, picture(b"must not be read")).unwrap();
        let locators = [
            alias
                .with_file_name("CeastldExtra")
                .join("Image/Temp/a.png")
                .to_string_lossy()
                .into_owned(),
            alias
                .join("../Ceastld/Image/Temp/a.png")
                .to_string_lossy()
                .into_owned(),
            format!("{alias_url}../Ceastld/Image/Temp/a.png"),
            format!("{alias_url}%2e%2e/Ceastld/Image/Temp/a.png?ignored=true"),
            unauthorized.to_string_lossy().into_owned(),
        ];
        for locator in locators {
            let reference = importer
                .import_reference("history", "h", "image", 0, &locator)
                .unwrap();
            assert_eq!(reference.status, AssetStatus::Unresolved);
            assert_eq!(reference.original_locator, locator);
        }
        assert_eq!(importer.assets().count(), 0);
        assert!(!destination.join("assets").exists());
        // Failed replacement must retain the last valid authorization.
        assert_eq!(
            importer
                .import_reference(
                    "history",
                    "h",
                    "image",
                    0,
                    &alias.join("Image/Temp/a.png").to_string_lossy()
                )
                .unwrap()
                .status,
            AssetStatus::Ready
        );
        let payload =
            serde_json::json!({"paths":[alias.join("documents/private.txt").to_string_lossy()]});
        let original_payload = payload.clone();
        assert!(importer
            .collect_payload_references("history", "files", "files", &payload, "")
            .unwrap()
            .is_empty());
        assert_eq!(payload, original_payload);
    }

    #[test]
    fn a_current_source_locator_takes_precedence_over_an_enclosing_alias() {
        let (temp, source, destination) = fixture();
        let bytes = picture(b"already in current clone");
        let path = source.join("Image/Temp/current.png");
        fs::write(&path, &bytes).unwrap();
        let mut importer = AssetImporter::new(&source, &destination).unwrap();
        importer
            .set_source_aliases(&[ordinary_alias(temp.path())])
            .unwrap();
        let locator = path.to_string_lossy();
        let reference = importer
            .import_reference("history", "h", "image", 0, &locator)
            .unwrap();
        assert_eq!(reference.status, AssetStatus::Ready);
        assert_eq!(reference.original_locator, locator);
        assert_eq!(
            fs::read(destination.join(reference.asset.unwrap().relative_path)).unwrap(),
            bytes
        );
        assert!(!source.join("source").exists());
    }

    #[test]
    fn historical_alias_cannot_publish_sensitive_paths_as_public_assets() {
        let (temp, source, destination) = fixture();
        let alias = ordinary_alias(&temp.path().join("absent-old/Ceastld"));
        let path = source.join("Configs/LanShare/key.der");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Image-looking bytes still require protection because of their path.
        fs::write(&path, picture(b"synthetic secret")).unwrap();
        #[cfg(windows)]
        let _denied_read = {
            use std::os::windows::fs::OpenOptionsExt;
            OpenOptions::new()
                .write(true)
                .share_mode(0)
                .open(&path)
                .unwrap()
        };
        let locator = alias
            .join("Configs/LanShare/key.der")
            .to_string_lossy()
            .into_owned();
        let mut importer = AssetImporter::new(&source, &destination).unwrap();
        importer.set_source_aliases(&[alias.clone()]).unwrap();
        let reference = importer
            .import_reference("favorite", "f", "override_icon", 0, &locator)
            .unwrap();
        assert_eq!(reference.status, AssetStatus::Unresolved);
        assert_eq!(reference.original_locator, locator);
        assert!(reference.asset.is_none());
        assert_eq!(importer.assets().count(), 0);
        assert!(importer.source_files.is_empty());
        assert!(!destination.join("assets").exists());
        assert!(!destination.join("staging").exists());
        assert!(!alias.exists());
    }

    #[test]
    fn same_name_different_bytes_and_same_content_have_safe_identities() {
        let (_temp, source, destination) = fixture();
        fs::create_dir_all(source.join("Image/other")).unwrap();
        fs::write(source.join("Image/Temp/a.png"), picture(b"one")).unwrap();
        fs::write(source.join("Image/other/a.png"), picture(b"two")).unwrap();
        fs::write(source.join("Image/other/b.png"), picture(b"one")).unwrap();
        let mut importer = AssetImporter::new(&source, &destination).unwrap();
        let a = importer
            .import_reference("history", "1", "image", 0, "Temp/a.png")
            .unwrap();
        let b = importer
            .import_reference("favorite", "1", "image", 0, "other/a.png")
            .unwrap();
        let c = importer
            .import_reference("favorite", "2", "image", 0, "other/b.png")
            .unwrap();
        assert_ne!(a.asset, b.asset);
        assert_eq!(a.asset, c.asset);
        assert_eq!(importer.assets().count(), 2);
        assert_eq!(
            fs::read(source.join("Image/Temp/a.png")).unwrap(),
            picture(b"one")
        );
        assert_eq!(
            fs::read(destination.join(&a.asset.unwrap().relative_path)).unwrap(),
            picture(b"one")
        );
    }

    #[test]
    fn absent_is_missing_but_directory_and_unreadable_layout_are_errors() {
        let (_temp, source, destination) = fixture();
        let mut importer = AssetImporter::new(&source, &destination).unwrap();
        let missing = importer
            .import_reference("history", "x", "image", 0, "Temp/absent.png")
            .unwrap();
        assert_eq!(missing.status, AssetStatus::Missing);
        assert!(missing.asset.is_none());
        assert!(importer
            .import_reference("history", "x", "image", 0, "Temp")
            .is_err());
        fs::write(source.join("Image/not-a-directory"), b"file").unwrap();
        assert!(importer
            .import_reference("history", "x", "image", 0, "not-a-directory/x")
            .is_err());
    }

    #[test]
    fn traversal_external_and_ambiguous_paths_are_not_read() {
        let (_temp, source, destination) = fixture();
        let mut importer = AssetImporter::new(&source, &destination).unwrap();
        for locator in [
            "../outside.png",
            "https://example.invalid/a.png",
            "file:relative",
            "",
        ] {
            let reference = importer
                .import_reference("history", "x", "image", 0, locator)
                .unwrap();
            assert_eq!(reference.status, AssetStatus::Unresolved, "{locator}");
            assert!(!reference.original_locator.is_empty());
        }
        let outside = importer
            .import_reference(
                "history",
                "x",
                "image",
                0,
                &destination.join("outside.png").to_string_lossy(),
            )
            .unwrap();
        // Destination may be a sibling here; no metadata/read is attempted.
        assert_eq!(outside.status, AssetStatus::Unresolved);
    }

    #[test]
    fn file_urls_preserve_encoded_filename_characters_and_cloud_trees_are_refused() {
        let (_temp, source, destination) = fixture();
        let path = source.join("Image/Temp/图 # % &.png");
        fs::write(&path, picture(b"image")).unwrap();
        let uri = url::Url::from_file_path(&path).unwrap().to_string();
        let mut importer = AssetImporter::new(&source, &destination).unwrap();
        assert_eq!(
            importer
                .import_reference("history", "x", "image", 0, &uri)
                .unwrap()
                .status,
            AssetStatus::Ready
        );
        fs::create_dir(source.join("Image/OneDrive-fixture")).unwrap();
        fs::write(source.join("Image/OneDrive-fixture/a.png"), b"never read").unwrap();
        assert!(importer
            .import_reference("history", "x", "image", 0, "OneDrive-fixture/a.png")
            .is_err());
        assert!(importer.archive_legacy_tree(Path::new("Image")).is_err());
    }

    #[test]
    fn existing_hash_path_is_verified_and_never_overwritten() {
        let (_temp, source, destination) = fixture();
        fs::write(source.join("Image/Temp/a.png"), picture(b"one")).unwrap();
        let mut importer = AssetImporter::new(&source, &destination).unwrap();
        let reference = importer
            .import_reference("history", "x", "image", 0, "Temp/a.png")
            .unwrap();
        let path = destination.join(&reference.asset.unwrap().relative_path);
        fs::write(&path, b"corruption").unwrap();
        assert!(importer
            .import_reference("history", "x", "image", 0, "Temp/a.png")
            .is_err());
        assert_eq!(fs::read(path).unwrap(), b"corruption");
    }

    #[test]
    fn final_source_validation_detects_content_additions_and_missing_references() {
        let (_temp, source, destination) = fixture();
        fs::write(source.join("Image/Temp/a.png"), picture(b"original")).unwrap();
        let mut importer = AssetImporter::new(&source, &destination).unwrap();
        importer
            .import_reference("history", "x", "image", 0, "Temp/a.png")
            .unwrap();
        importer.revalidate_sources().unwrap();
        fs::write(source.join("Image/Temp/a.png"), picture(b"modified")).unwrap();
        assert!(importer.revalidate_sources().is_err());

        let (_temp, source, destination) = fixture();
        let mut importer = AssetImporter::new(&source, &destination).unwrap();
        importer.archive_legacy_tree(Path::new("Image")).unwrap();
        importer.revalidate_sources().unwrap();
        fs::write(source.join("Image/new.png"), b"new").unwrap();
        assert!(importer.revalidate_sources().is_err());

        let (_temp, source, destination) = fixture();
        let mut importer = AssetImporter::new(&source, &destination).unwrap();
        importer
            .import_reference("history", "x", "image", 0, "Temp/missing.png")
            .unwrap();
        importer.revalidate_sources().unwrap();
        fs::write(source.join("Image/Temp/missing.png"), b"appeared").unwrap();
        assert!(importer.revalidate_sources().is_err());
    }

    #[test]
    fn rich_text_preserves_raw_and_records_every_known_occurrence() {
        let (_temp, source, destination) = fixture();
        fs::write(source.join("Image/Temp/a.png"), picture(b"image")).unwrap();
        let image = source.join("Image/Temp/a.png");
        let uri = url::Url::from_file_path(&image).unwrap().to_string();
        let payload = serde_json::json!({
            "html": format!("<html>\r\n<head><style>p{{color:red}}</style></head><body><img src='{uri}'><img src='{uri}'></body></html>"),
            "xml": format!("<QQRichEditFormat><EditElement type=\"3\" filepath=\"{}\" extra=\"keep\"/></QQRichEditFormat>", image.display()),
            "sourceUrl": null
        });
        let original = payload.clone();
        let mut importer = AssetImporter::new(&source, &destination).unwrap();
        let refs = importer
            .collect_payload_references("history", "x", "im_rich", &payload, "qq")
            .unwrap();
        assert_eq!(payload, original);
        assert_eq!(refs.iter().filter(|r| r.slot == "html").count(), 2);
        assert_eq!(refs.iter().filter(|r| r.slot == "im").count(), 1);
        assert!(
            refs.iter().all(|r| r.status == AssetStatus::Ready),
            "{refs:#?}"
        );
        assert_eq!(importer.assets().count(), 1);
        let relative = serde_json::json!({"html":"<img src='Temp/a.png'><img src='Temp/a.png'>"});
        let refs = importer
            .collect_payload_references("history", "relative", "html", &relative, "")
            .unwrap();
        assert_eq!(refs.len(), 2);
        assert!(
            refs.iter().all(|r| r.status == AssetStatus::Ready),
            "{refs:#?}"
        );
    }

    #[test]
    fn unknown_local_markup_is_not_reported_complete_and_archive_is_verified() {
        let (_temp, source, destination) = fixture();
        fs::write(source.join("Image/Temp/a.png"), picture(b"retain")).unwrap();
        let payload = serde_json::json!({"html":"<style>p{background:url(Temp/a.png)}</style>"});
        let mut importer = AssetImporter::new(&source, &destination).unwrap();
        let refs = importer
            .collect_payload_references("history", "x", "html", &payload, "")
            .unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].status, AssetStatus::Unresolved);
        assert_eq!(refs[0].original_locator, "unparsed:html");
        assert!(importer.has_incomplete_references());
        let archive = importer.archive_legacy_tree(Path::new("Image")).unwrap();
        assert_eq!(archive.files.len(), 1);
        assert_eq!(
            importer
                .archive_legacy_tree(Path::new("Image"))
                .unwrap()
                .files,
            archive.files
        );
        let archived = destination.join(&archive.files[0].archive_relative_path);
        assert_eq!(fs::read(&archived).unwrap(), picture(b"retain"));
        fs::write(&archived, b"changed").unwrap();
        assert!(importer.archive_legacy_tree(Path::new("Image")).is_err());
        assert_eq!(
            fs::read(source.join("Image/Temp/a.png")).unwrap(),
            picture(b"retain")
        );
    }

    #[test]
    fn archive_rejects_traversal_and_candidate_recursion() {
        let (_temp, source, destination) = fixture();
        let mut importer = AssetImporter::new(&source, &destination).unwrap();
        assert!(importer
            .archive_legacy_tree(Path::new("../outside"))
            .is_err());
        assert!(AssetImporter::new(&source, &source).is_err());
        let inside = source.join("Image/candidate");
        let mut importer = AssetImporter::new(&source, &inside).unwrap();
        assert!(importer.archive_legacy_tree(Path::new("Image")).is_err());
    }

    #[test]
    fn credential_references_are_not_read_or_published_as_image_assets() {
        let (_temp, source, destination) = fixture();
        let directory = source.join("Configs/LanShare");
        fs::create_dir_all(&directory).unwrap();
        let key = directory.join("key.der");
        fs::write(&key, b"synthetic-private-key").unwrap();
        #[cfg(windows)]
        let _denied_read = {
            use std::os::windows::fs::OpenOptionsExt;
            OpenOptions::new()
                .write(true)
                .share_mode(0)
                .open(&key)
                .unwrap()
        };
        let mut importer = AssetImporter::new(&source, &destination).unwrap();
        let reference = importer
            .import_reference("history", "key", "image", 0, &key.to_string_lossy())
            .unwrap();
        assert_eq!(reference.status, AssetStatus::Unresolved);
        assert!(reference.asset.is_none());
        assert_eq!(importer.assets().count(), 0);
        assert!(!destination.join("assets").exists());
        assert!(!destination.join("staging").exists());
    }

    #[cfg(windows)]
    #[test]
    fn resource_json_and_unknown_image_bytes_have_only_verified_protected_archives() {
        let (_temp, source, destination) = fixture();
        fs::create_dir(source.join("Resource")).unwrap();
        let json = br#"{"token":"synthetic-opaque-resource"}"#;
        let json_path = source.join("Resource/unknown.json");
        fs::write(&json_path, json).unwrap();
        let unknown = source.join("Image/Temp/pretend.png");
        fs::write(&unknown, b"not an image: synthetic private material").unwrap();
        let mut importer = AssetImporter::new(&source, &destination).unwrap();
        for path in [&json_path, &unknown] {
            let reference = importer
                .import_reference("history", "x", "image", 0, &path.to_string_lossy())
                .unwrap();
            assert_eq!(reference.status, AssetStatus::Unresolved);
            assert!(reference.asset.is_none());
        }
        assert!(!destination.join("assets").exists());
        assert!(!destination.join("staging").exists());
        let archive = importer.archive_legacy_tree(Path::new("Resource")).unwrap();
        let entry = &archive.files[0];
        assert!(entry.protected);
        assert!(entry.archive_relative_path.ends_with(".dpapi"));
        assert_eq!(entry.sha256, format!("{:x}", Sha256::digest(json)));
        let path = destination.join(&entry.archive_relative_path);
        let encrypted = fs::read(&path).unwrap();
        assert_ne!(encrypted, json);
        assert_eq!(
            entry.actual_archive_sha256,
            format!("{:x}", Sha256::digest(&encrypted))
        );
        assert_eq!(
            super::super::protected_archive::unprotect(&encrypted).unwrap(),
            json
        );
        assert_eq!(
            importer
                .archive_legacy_tree(Path::new("Resource"))
                .unwrap()
                .files,
            archive.files
        );
        let images = importer.archive_legacy_tree(Path::new("Image")).unwrap();
        assert!(images.files.iter().all(|file| file.protected));
        fs::write(&path, b"corrupt protected archive").unwrap();
        assert!(importer.archive_legacy_tree(Path::new("Resource")).is_err());
        assert_eq!(fs::read(&json_path).unwrap(), json);
    }

    #[test]
    fn protected_archive_size_guard_rejects_before_reading_or_writing() {
        let (_temp, source, destination) = fixture();
        let path = source.join("Image/oversize.data");
        fs::write(&path, b"12345").unwrap();
        let mut input = File::open(path).unwrap();
        assert!(read_bounded(&mut input, 4).is_err());
        assert!(!destination.exists());
    }

    #[cfg(windows)]
    #[test]
    fn cloud_recall_offline_and_all_reparse_attributes_are_rejected() {
        for attributes in [0x400, 0x1000, 0x40000, 0x400000, 0x400020] {
            assert!(reject_windows_attributes(attributes).is_err());
        }
        assert!(reject_windows_attributes(0x20).is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn sharing_denial_is_an_error_instead_of_a_missing_reference() {
        use std::os::windows::fs::OpenOptionsExt;
        let (_temp, source, destination) = fixture();
        let path = source.join("Image/Temp/a.png");
        fs::write(&path, picture(b"locked")).unwrap();
        let guard = OpenOptions::new()
            .write(true)
            .share_mode(0)
            .open(&path)
            .unwrap();
        let mut importer = AssetImporter::new(&source, &destination).unwrap();
        assert!(importer
            .import_reference("history", "x", "image", 0, "Temp/a.png")
            .is_err());
        drop(guard);
        assert_eq!(
            importer
                .import_reference("history", "x", "image", 0, "Temp/a.png")
                .unwrap()
                .status,
            AssetStatus::Ready
        );
    }

    #[cfg(windows)]
    #[test]
    fn junction_ancestors_and_archive_junctions_are_rejected() {
        let (_temp, source, destination) = fixture();
        let target = source.join("Image/Temp");
        fs::write(target.join("a.png"), picture(b"image")).unwrap();
        let junction = source.join("Image/junction");
        let result = std::process::Command::new("cmd.exe")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&target)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let mut importer = AssetImporter::new(&source, &destination).unwrap();
        assert!(importer
            .import_reference("history", "x", "image", 0, "junction/a.png")
            .is_err());
        assert!(importer.archive_legacy_tree(Path::new("Image")).is_err());
        fs::remove_dir(&junction).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlink_file_and_ancestor_are_rejected_before_content_access() {
        use std::os::unix::fs::symlink;
        let (_temp, source, destination) = fixture();
        fs::write(source.join("Image/Temp/a.png"), picture(b"image")).unwrap();
        symlink(
            source.join("Image/Temp/a.png"),
            source.join("Image/link.png"),
        )
        .unwrap();
        symlink(source.join("Image/Temp"), source.join("Image/linked-dir")).unwrap();
        let mut importer = AssetImporter::new(&source, &destination).unwrap();
        assert!(importer
            .import_reference("history", "x", "image", 0, "link.png")
            .is_err());
        assert!(importer
            .import_reference("history", "x", "image", 0, "linked-dir/a.png")
            .is_err());
        assert!(importer.archive_legacy_tree(Path::new("Image")).is_err());
    }
}
