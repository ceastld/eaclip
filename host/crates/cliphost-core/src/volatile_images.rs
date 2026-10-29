//! Preserve images whose producers publish a disposable file-drop path.
//! Ordinary files remain references; this module neither searches directories
//! nor deletes source files or previously published images.

use std::io::Cursor;
use std::path::{Component, Path};

use crate::model::{ClipKind, ClipRecord};

const MAX_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_IMAGE_PIXELS: u64 = 40 * 1024 * 1024;
const MAX_DECODE_BYTES: u64 = 256 * 1024 * 1024;

pub(crate) fn is_volatile_image_record(record: &ClipRecord) -> bool {
    record.kind == ClipKind::FileClipItem
        && record.file_paths.len() == 1
        && is_volatile_image_path(Path::new(&record.file_paths[0]), &std::env::temp_dir())
}

/// The returned clone preserves identity, timestamps and storage revision, so
/// callers can conditionally repair an existing history record as well as
/// prepare a new capture. A failed snapshot must leave the original reference.
pub(crate) fn snapshot_volatile_image(
    record: &ClipRecord,
    data_root: &Path,
) -> Result<Option<ClipRecord>, &'static str> {
    snapshot_with_temp_root(record, data_root, &std::env::temp_dir())
}

fn snapshot_with_temp_root(
    record: &ClipRecord,
    data_root: &Path,
    system_temp: &Path,
) -> Result<Option<ClipRecord>, &'static str> {
    if record.kind != ClipKind::FileClipItem || record.file_paths.len() != 1 {
        return Ok(None);
    }
    let source = Path::new(&record.file_paths[0]);
    if !is_volatile_image_path(source, system_temp) {
        return Ok(None);
    }
    let bytes = crate::storage_core::resources::read_local_file_bounded(source, MAX_SOURCE_BYTES)
        .map_err(|_| "volatile_image_source_unavailable")?;
    let extension = validate_image(&bytes)?;
    // Every incoming capture owns a fresh artifact. Existing capture dedupe
    // may remove that artifact after merging; it must never own the source.
    let name = format!("volatile-{}.{}", uuid::Uuid::new_v4().simple(), extension);
    let destination = crate::storage_core::resources::publish_bytes_compatible(
        &crate::clipboard::ClipImageDirs::under(data_root).temp,
        &name,
        &bytes,
    )
    .map_err(|_| "volatile_image_publish_failed")?;
    let mut snapshot = record.clone();
    snapshot.kind = ClipKind::ImageClipItem;
    snapshot.body_text = Some(String::new());
    snapshot.file_paths.clear();
    snapshot.image_path = Some(destination.to_string_lossy().into_owned());
    snapshot.content_hash = crate::model::sha256_hex(&bytes);
    // Byte identity preserves animation, color profiles and other metadata.
    // Do not run the legacy unbounded perceptual decoder on external inputs.
    snapshot.perceptual_hash.clear();
    Ok(Some(snapshot))
}

fn is_volatile_image_path(path: &Path, system_temp: &Path) -> bool {
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return false;
    }
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    if !matches!(
        extension.to_ascii_lowercase().as_str(),
        "png" | "jpg" | "jpeg" | "bmp" | "gif" | "webp" | "tif" | "tiff"
    ) {
        return false;
    }
    let components: Vec<String> = path
        .components()
        .filter_map(|part| match part {
            Component::Normal(value) => Some(value.to_string_lossy().to_ascii_lowercase()),
            _ => None,
        })
        .collect();
    let parent = &components[..components.len().saturating_sub(1)];
    let known_cache = parent.windows(2).any(|parts| parts == ["pixpin", "temp"])
        || parent
            .windows(3)
            .any(|parts| parts == ["tencent", "wetype", "clipboardtmp"])
        || parent
            .windows(4)
            .any(|parts| parts[0] == "xwechat_files" && parts[2] == "temp" && parts[3] == "rwtemp");
    known_cache || beneath_temp(path, system_temp)
}

fn beneath_temp(path: &Path, system_temp: &Path) -> bool {
    // A malformed TEMP value must not turn the entire drive into a cache.
    if !system_temp.is_absolute()
        || system_temp.parent().is_none()
        || system_temp.file_name().is_none()
        || system_temp
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return false;
    }
    let mut parts = path.components();
    for expected in system_temp.components() {
        let Some(actual) = parts.next() else {
            return false;
        };
        #[cfg(windows)]
        if !actual
            .as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(&expected.as_os_str().to_string_lossy())
        {
            return false;
        }
        #[cfg(not(windows))]
        if actual != expected {
            return false;
        }
    }
    parts.next().is_some()
}

fn validate_image(bytes: &[u8]) -> Result<&'static str, &'static str> {
    let format = image::guess_format(bytes).map_err(|_| "volatile_image_invalid")?;
    let extension = match format {
        image::ImageFormat::Png => "png",
        image::ImageFormat::Jpeg => "jpg",
        image::ImageFormat::Bmp => "bmp",
        image::ImageFormat::Gif => "gif",
        image::ImageFormat::WebP => "webp",
        image::ImageFormat::Tiff => "tiff",
        _ => return Err("volatile_image_unsupported"),
    };
    let mut header = image::ImageReader::with_format(Cursor::new(bytes), format);
    header.limits(decode_limits());
    let (width, height) = header
        .into_dimensions()
        .map_err(|_| "volatile_image_invalid")?;
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS {
        return Err("volatile_image_pixel_limit");
    }
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(decode_limits());
    reader
        .decode()
        .map_err(|_| "volatile_image_decode_failed")?;
    Ok(extension)
}

fn decode_limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    limits
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn image_bytes(format: image::ImageFormat) -> Vec<u8> {
        let image = image::RgbImage::from_pixel(3, 2, image::Rgb([180, 90, 40]));
        let mut output = Cursor::new(Vec::new());
        image.write_to(&mut output, format).unwrap();
        output.into_inner()
    }

    fn file_record(path: &Path) -> ClipRecord {
        let mut item = ClipRecord::new_text(path.to_string_lossy(), "PixPin.exe");
        item.kind = ClipKind::FileClipItem;
        item.file_paths = vec![path.to_string_lossy().into_owned()];
        item.content_hash = crate::model::hash_files(&item.file_paths);
        item
    }

    #[test]
    fn volatile_image_policy_only_matches_single_images_in_disposable_locations() {
        let root = tempfile::tempdir().unwrap();
        let temp = root.path().join("SystemTemp");
        for cache in [
            "PixPin/Temp",
            "Tencent/WeType/ClipboardTmp",
            "xwechat_files/account/temp/RWTemp",
            "SystemTemp/nested",
        ] {
            let path = root.path().join(cache).join("image.JPG");
            assert!(is_volatile_image_path(&path, &temp), "{cache}");
        }
        for relative in [
            "Pictures/a.png",
            "PixPin/Templates/a.png",
            "SystemTempSibling/a.png",
            "PixPin/Temp/movie.mp4",
        ] {
            assert!(
                !is_volatile_image_path(&root.path().join(relative), &temp),
                "{relative}"
            );
        }
        assert!(!is_volatile_image_path(
            &root.path().join("PixPin/Temp/../Pictures/a.png"),
            &temp
        ));
        let item = file_record(&root.path().join("Pictures/never-opened.png"));
        assert!(
            snapshot_with_temp_root(&item, &root.path().join("data"), &temp)
                .unwrap()
                .is_none()
        );
        let mut multiple = file_record(&root.path().join("PixPin/Temp/never-opened.png"));
        multiple.file_paths.push("another.png".into());
        assert!(
            snapshot_with_temp_root(&multiple, &root.path().join("data"), &temp)
                .unwrap()
                .is_none()
        );
        assert!(!root.path().join("data").exists());
    }

    #[test]
    fn volatile_image_snapshot_preserves_encoded_bytes_and_record_identity() {
        let root = tempfile::tempdir().unwrap();
        let temp = root.path().join("cache");
        fs::create_dir_all(&temp).unwrap();
        // The suffix is deliberately inaccurate: publication uses actual JPEG.
        let source = temp.join("shot.png");
        let bytes = image_bytes(image::ImageFormat::Jpeg);
        fs::write(&source, &bytes).unwrap();
        let mut original = file_record(&source);
        original.storage_revision = Some("original-revision".into());
        original.is_top = true;
        let saved = snapshot_with_temp_root(&original, &root.path().join("data"), &temp)
            .unwrap()
            .unwrap();
        assert_eq!(saved.id, original.id);
        assert_eq!(saved.create_time, original.create_time);
        assert_eq!(saved.storage_revision, original.storage_revision);
        assert!(saved.is_top);
        assert_eq!(saved.kind, ClipKind::ImageClipItem);
        assert!(saved.file_paths.is_empty());
        assert_eq!(saved.content_hash, crate::model::sha256_hex(&bytes));
        let destination = Path::new(saved.image_path.as_deref().unwrap());
        assert_eq!(destination.extension().unwrap(), "jpg");
        assert_eq!(
            destination.parent().unwrap(),
            root.path().join("data/Image/Temp")
        );
        fs::remove_file(&source).unwrap();
        assert_eq!(fs::read(destination).unwrap(), bytes);
        assert_eq!(
            original.file_paths,
            vec![source.to_string_lossy().into_owned()]
        );
    }

    #[test]
    fn volatile_image_snapshot_preserves_gif_animation_without_reencoding() {
        let root = tempfile::tempdir().unwrap();
        let cache = root.path().join("cache");
        fs::create_dir_all(&cache).unwrap();
        let mut bytes = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut bytes);
            encoder
                .set_repeat(image::codecs::gif::Repeat::Infinite)
                .unwrap();
            for color in [[255, 0, 0, 255], [0, 0, 255, 255]] {
                encoder
                    .encode_frame(image::Frame::from_parts(
                        image::RgbaImage::from_pixel(2, 2, image::Rgba(color)),
                        0,
                        0,
                        image::Delay::from_numer_denom_ms(125, 1),
                    ))
                    .unwrap();
            }
        }
        let source = cache.join("animation.gif");
        fs::write(&source, &bytes).unwrap();
        let saved =
            snapshot_with_temp_root(&file_record(&source), &root.path().join("data"), &cache)
                .unwrap()
                .unwrap();
        assert_eq!(fs::read(saved.image_path.unwrap()).unwrap(), bytes);
        assert_eq!(fs::read(source).unwrap(), bytes);
    }

    #[test]
    fn volatile_image_invalid_missing_and_oversized_sources_publish_nothing() {
        let root = tempfile::tempdir().unwrap();
        let cache = root.path().join("cache");
        fs::create_dir_all(&cache).unwrap();
        let source = cache.join("shot.png");
        let item = file_record(&source);
        let data = root.path().join("data");
        assert!(snapshot_with_temp_root(&item, &data, &cache).is_err());
        fs::write(&source, b"invalid image").unwrap();
        assert_eq!(
            snapshot_with_temp_root(&item, &data, &cache).unwrap_err(),
            "volatile_image_invalid"
        );
        fs::File::create(&source)
            .unwrap()
            .set_len(MAX_SOURCE_BYTES + 1)
            .unwrap();
        assert_eq!(
            snapshot_with_temp_root(&item, &data, &cache).unwrap_err(),
            "volatile_image_source_unavailable"
        );
        assert!(!data.exists());
        assert_eq!(item.file_paths, vec![source.to_string_lossy().into_owned()]);
    }

    #[test]
    fn volatile_image_pixel_budget_rejects_large_header_before_pixel_allocation() {
        let mut bmp = image_bytes(image::ImageFormat::Bmp);
        bmp[18..22].copy_from_slice(&10000u32.to_le_bytes());
        bmp[22..26].copy_from_slice(&10000u32.to_le_bytes());
        assert_eq!(
            validate_image(&bmp).unwrap_err(),
            "volatile_image_pixel_limit"
        );
    }

    #[test]
    fn volatile_image_publish_failure_keeps_original_file_and_reference() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("shot.png");
        let bytes = image_bytes(image::ImageFormat::Png);
        fs::write(&source, &bytes).unwrap();
        let data = root.path().join("data");
        fs::write(&data, b"existing file").unwrap();
        let item = file_record(&source);
        assert_eq!(
            snapshot_with_temp_root(&item, &data, root.path()).unwrap_err(),
            "volatile_image_publish_failed"
        );
        assert_eq!(fs::read(&source).unwrap(), bytes);
        assert_eq!(fs::read(&data).unwrap(), b"existing file");
        assert_eq!(item.kind, ClipKind::FileClipItem);
    }

    #[cfg(windows)]
    #[test]
    fn volatile_image_placeholder_is_refused_without_publishing() {
        use std::os::windows::ffi::OsStrExt;
        #[link(name = "kernel32")]
        extern "system" {
            fn SetFileAttributesW(name: *const u16, flags: u32) -> i32;
        }
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("shot.png");
        fs::write(&source, image_bytes(image::ImageFormat::Png)).unwrap();
        let wide: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
        assert_ne!(unsafe { SetFileAttributesW(wide.as_ptr(), 0x1000) }, 0);
        let result = snapshot_with_temp_root(
            &file_record(&source),
            &root.path().join("data"),
            root.path(),
        );
        assert_ne!(unsafe { SetFileAttributesW(wide.as_ptr(), 0x80) }, 0);
        assert_eq!(result.unwrap_err(), "volatile_image_source_unavailable");
        assert!(!root.path().join("data").exists());
    }

    #[test]
    fn volatile_image_cloud_named_source_is_refused_without_data_access() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("OneDrive/PixPin/Temp/not-opened.png");
        let result = snapshot_with_temp_root(
            &file_record(&source),
            &root.path().join("data"),
            &root.path().join("other"),
        );
        assert_eq!(result.unwrap_err(), "volatile_image_source_unavailable");
        assert!(!root.path().join("data").exists());
    }
}
