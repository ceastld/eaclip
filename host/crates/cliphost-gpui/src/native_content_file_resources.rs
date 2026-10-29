//! Background-only file-row/local-path resource loading. The immutable list
//! projection supplies display/cache keys; normalized paths are used only for
//! metadata and Core's existing reads. No details requests, timeout threads or
//! shell/filesystem work belong in row render.

use cliphost_core::{host::HostState, shell_path::normalize_local_shell_path};

use super::{
    files::{FileListProjection, LocalPathProjection, FILE_CHIP_PREVIEW_LIMIT},
    ContentPathInfo, ContentResources, CONTENT_RESOURCE_BUDGET_BYTES,
};

/// Capture one metadata snapshot here. Existing Core image/icon APIs perform
/// their own admission/probes afterwards; this is not a claim of one total OS
/// lookup across those independent APIs. Error status matches the Web probes.
fn path_info(raw: &str, invalid_exists: Option<bool>) -> (ContentPathInfo, Option<String>) {
    let Ok(full) = normalize_local_shell_path(raw) else {
        return (
            ContentPathInfo {
                exists: invalid_exists,
                size_bytes: None,
            },
            None,
        );
    };
    let info = match std::fs::metadata(&full) {
        Ok(metadata) => ContentPathInfo {
            exists: Some(true),
            size_bytes: metadata.is_file().then_some(metadata.len()),
        },
        Err(_) => ContentPathInfo {
            exists: Some(false),
            size_bytes: None,
        },
    };
    (info, Some(full))
}

/// Run on the parent's resource worker. At most eight files retain metadata,
/// Shell icons and optional small local-raster resources. Invalid/device paths
/// become completed failures before reaching any Shell/image API. Missing valid
/// files still follow the current Core icon behavior, with no new icon semantics.
pub(super) fn load_file_resources(
    host: &HostState,
    files: &FileListProjection,
    resources: &mut ContentResources,
) {
    for entry in files.entries.iter().take(FILE_CHIP_PREVIEW_LIMIT) {
        let (info, full) = path_info(&entry.path, Some(false));
        let exists = info.exists;
        resources.paths.insert(entry.path.clone(), info);
        resources.limit_retained_bytes(CONTENT_RESOURCE_BUDGET_BYTES);

        let icon = full.as_ref().and_then(|_| {
            host.file_icon_png(Some(&entry.path), None, None)
                .ok()
                .and_then(super::prepare_icon_bytes)
        });
        resources.icons.insert(entry.path.clone(), icon);
        resources.limit_retained_bytes(CONTENT_RESOURCE_BUDGET_BYTES);

        if entry.is_image {
            let image = full
                .as_deref()
                .filter(|_| exists != Some(false))
                .and_then(|path| host.local_image_bytes(path))
                .and_then(|(bytes, _)| super::list_image(bytes));
            resources.images.insert(entry.path.clone(), image);
            resources.limit_retained_bytes(CONTENT_RESOURCE_BUDGET_BYTES);
        }
    }
}

/// Text's local-path probe preserves `ok:false` as unknown (`exists:None`), as
/// the WebView does. Plain text paths request no Shell icon; readable local
/// raster paths alone can load an image. Failure never changes the DTO/payload.
pub(super) fn load_local_path_resources(
    host: &HostState,
    path: &LocalPathProjection,
    resources: &mut ContentResources,
) {
    let (info, full) = path_info(&path.path, None);
    let exists = info.exists;
    resources.paths.insert(path.path.clone(), info);
    resources.limit_retained_bytes(CONTENT_RESOURCE_BUDGET_BYTES);
    if path.is_image {
        let image = full
            .as_deref()
            .filter(|_| exists == Some(true))
            .and_then(|path| host.local_image_bytes(path))
            .and_then(|(bytes, _)| super::list_image(bytes));
        resources.images.insert(path.path.clone(), image);
        resources.limit_retained_bytes(CONTENT_RESOURCE_BUDGET_BYTES);
    }
}
