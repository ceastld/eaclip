use super::*;
use cliphost_core::{dto::clip_to_dto, model::{ClipKind, ClipRecord}};
use gpui::ImageFormat;
use std::cell::Cell;

fn fixture() -> tempfile::TempDir {
    // Resolve only the compile-time repository, never a user data/config root.
    let repository = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(3).unwrap();
    let root = repository.join(".tmp/history-image-resources");
    for ancestor in root.parent().unwrap().ancestors().collect::<Vec<_>>().into_iter().rev() {
        let metadata = fs::symlink_metadata(ancestor).unwrap();
        check_metadata(&metadata).unwrap();
        assert!(metadata.is_dir());
    }
    fs::create_dir_all(&root).unwrap();
    check_metadata(&fs::symlink_metadata(&root).unwrap()).unwrap();
    tempfile::Builder::new().prefix("display-").tempdir_in(root).unwrap()
}

fn png(width: u32, height: u32, value: u8) -> Vec<u8> {
    let image = image::RgbImage::from_pixel(width, height, image::Rgb([value, 23, 45]));
    let mut bytes = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(image).write_to(&mut bytes, image::ImageFormat::Png).unwrap();
    bytes.into_inner()
}

fn image_item() -> ClipboardItemDto {
    let mut item = ClipRecord::new_text("synthetic", "fixture");
    item.kind = ClipKind::ImageClipItem;
    // Do not give DTO conversion a path: it must not perform fixture metadata I/O.
    clip_to_dto(&item, None)
}

fn animated_webp(canvas: (u32, u32), frame_size: (u32, u32), count: usize) -> Vec<u8> {
    use image::ImageEncoder;
    let u24 = |value: u32| value.to_le_bytes()[..3].to_vec();
    let chunk = |tag: &[u8; 4], data: &[u8], output: &mut Vec<u8>| {
        output.extend(tag);
        output.extend((data.len() as u32).to_le_bytes());
        output.extend(data);
        if data.len() % 2 != 0 { output.push(0); }
    };
    let mut body = b"WEBP".to_vec();
    let mut extended = vec![0x02, 0, 0, 0];
    extended.extend(u24(canvas.0 - 1));
    extended.extend(u24(canvas.1 - 1));
    chunk(b"VP8X", &extended, &mut body);
    chunk(b"ANIM", &[0; 6], &mut body);
    for index in 0..count {
        let raster = image::RgbaImage::from_pixel(frame_size.0, frame_size.1,
            image::Rgba([20 + (index * 50 % 200) as u8, 40, 220, 255]));
        let mut encoded = Vec::new();
        image::codecs::webp::WebPEncoder::new_lossless(&mut encoded)
            .write_image(raster.as_raw(), frame_size.0, frame_size.1, image::ExtendedColorType::Rgba8).unwrap();
        let mut frame = vec![0; 6];
        frame.extend(u24(frame_size.0 - 1));
        frame.extend(u24(frame_size.1 - 1));
        frame.extend(u24(40));
        frame.push(2);
        let mut offset = 12;
        while offset + 8 <= encoded.len() {
            let length = u32::from_le_bytes(encoded[offset + 4..offset + 8].try_into().unwrap()) as usize;
            let end = offset + 8 + length + length % 2;
            assert!(end <= encoded.len());
            if &encoded[offset..offset + 4] == b"VP8L" { frame.extend(&encoded[offset..end]); }
            offset = end;
        }
        assert!(frame.len() > 16);
        chunk(b"ANMF", &frame, &mut body);
    }
    let mut bytes = b"RIFF".to_vec();
    bytes.extend((body.len() as u32).to_le_bytes());
    bytes.extend(body);
    bytes
}

#[test]
fn primary_image_probe_reads_headers_not_a_large_payload() {
    let dir = fixture();
    let path = dir.path().join("header.png");
    let mut original = png(320, 160, 9);
    original.resize(4 * 1024 * 1024, 0); // Valid PNG with harmless trailing bytes.
    fs::write(&path, &original).unwrap();
    let source = probe_source(&path).unwrap();
    assert_eq!((source.width, source.height), (320, 160));
    assert_eq!(source.size_bytes, original.len() as u64);
    assert!(source.header_bytes_read <= MAX_PROBE_BYTES);
    assert!(source.header_bytes_read < original.len() / 8);
    assert_eq!(fs::read(path).unwrap(), original);
}

#[test]
fn large_static_primary_image_keeps_natural_size_and_original_but_bounds_display_pixels() {
    let dir = fixture();
    let path = dir.path().join("large.png");
    let original = png(4096, 2048, 17);
    fs::write(&path, &original).unwrap();
    let version = probe_source(&path).unwrap();
    let resources = load_path_resources(&path, &version.source_version).unwrap();
    let display = resources.images[PRIMARY_IMAGE_RESOURCE].as_ref().unwrap();
    assert_eq!((display.width, display.height), (4096.0, 2048.0));
    let pixels = image::load_from_memory(display.image.bytes()).unwrap();
    assert_eq!((pixels.width(), pixels.height()), (1024, 512));
    assert!(resources.retained_bytes() <= native_content::CONTENT_RESOURCE_BUDGET_BYTES);
    assert_ne!(display.image.bytes(), original.as_slice());
    assert_eq!(fs::read(path).unwrap(), original);
}

#[test]
fn same_path_replacement_requires_new_version_and_never_returns_old_display() {
    let dir = fixture();
    let path = dir.path().join("same.png");
    fs::write(&path, png(12, 8, 3)).unwrap();
    let first = probe_source(&path).unwrap();
    let old = load_path_resources(&path, &first.source_version).unwrap();
    let original_id = old.images[PRIMARY_IMAGE_RESOURCE].as_ref().unwrap().image.id();
    fs::write(&path, png(12, 8, 203)).unwrap();
    let next = probe_source(&path).unwrap();
    assert_ne!(first.source_version, next.source_version);
    assert_eq!(load_path_resources(&path, &first.source_version).err().as_deref(), Some("HISTORY_IMAGE_SOURCE_CHANGED"));
    let replacement = load_path_resources(&path, &next.source_version).unwrap();
    assert_ne!(original_id, replacement.images[PRIMARY_IMAGE_RESOURCE].as_ref().unwrap().image.id());
    let item = image_item();
    assert_ne!(primary_image_projection(&item, &first.source_version), primary_image_projection(&item, &next.source_version));
    let mut changed = item.clone();
    changed.title.push_str(" changed without changing create_time");
    assert_eq!(primary_image_projection(&item, &next.source_version), primary_image_projection(&changed, &next.source_version));
}

#[test]
fn cloud_and_reparse_admission_reject_before_any_image_data_reader() {
    let dir = fixture();
    let path = dir.path().join("resident.png");
    let original = png(8, 4, 1);
    fs::write(&path, &original).unwrap();
    for unsafe_flag in [0x400, 0x1000, 0x40000, 0x400000] {
        let read_called = Cell::new(false);
        let result = load_path_with(&path, "not-used", |metadata| {
            check_metadata(metadata)?;
            if metadata.is_file() { check_attributes(unsafe_flag)?; }
            Ok(())
        }, |_, _| { read_called.set(true); panic!("unsafe file reached data reader") });
        assert_eq!(result.err().as_deref(), Some("HISTORY_IMAGE_UNSAFE_PATH"));
        assert!(!read_called.get());
    }
    assert_eq!(fs::read(path).unwrap(), original);
}

#[test]
fn missing_invalid_and_oversized_sources_are_completed_unavailable_loads() {
    let dir = fixture();
    assert!(probe_source(&dir.path().join("missing.png")).is_err());
    let path = dir.path().join("bad.png");
    fs::write(&path, [1, 2, 3]).unwrap();
    assert_eq!(probe_source(&path).err().as_deref(), Some("HISTORY_IMAGE_HEADER"));
    File::create(&path).unwrap().set_len(MAX_SOURCE_BYTES + 1).unwrap();
    assert_eq!(probe_source(&path).err().as_deref(), Some("HISTORY_IMAGE_SIZE"));
    let resources = unavailable();
    assert!(resources.images.contains_key(PRIMARY_IMAGE_RESOURCE));
    assert!(resources.images[PRIMARY_IMAGE_RESOURCE].is_none());
}

#[test]
fn small_gif_animation_retains_all_frames_and_natural_dimensions() {
    use image::{AnimationDecoder, codecs::gif::GifEncoder, Delay, Frame};
    let dir = fixture();
    let path = dir.path().join("animated.gif");
    let mut original = Vec::new();
    {
        let mut encoder = GifEncoder::new(&mut original);
        for value in [10, 210] {
            encoder.encode_frame(Frame::from_parts(
                image::RgbaImage::from_pixel(24, 12, image::Rgba([value, 4, 8, 255])),
                0, 0, Delay::from_numer_denom_ms(40, 1),
            )).unwrap();
        }
    }
    fs::write(&path, &original).unwrap();
    let version = probe_source(&path).unwrap();
    let resources = load_path_resources(&path, &version.source_version).unwrap();
    let image = resources.images[PRIMARY_IMAGE_RESOURCE].as_ref().unwrap();
    assert_eq!(image.image.format(), ImageFormat::Gif);
    assert_eq!((image.width, image.height), (24.0, 12.0));
    assert_eq!(image.image.bytes(), original.as_slice());
    assert_eq!(image::codecs::gif::GifDecoder::new(Cursor::new(image.image.bytes())).unwrap()
        .into_frames().collect_frames().unwrap().len(), 2);
    assert!(resources.retained_bytes() <= native_content::CONTENT_RESOURCE_BUDGET_BYTES);
}

#[test]
fn static_webp_is_resized_display_only_instead_of_retaining_original_raster_size() {
    let dir = fixture();
    let path = dir.path().join("static.webp");
    let mut original = Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(1600, 800).write_to(&mut original, image::ImageFormat::WebP).unwrap();
    let original = original.into_inner();
    fs::write(&path, &original).unwrap();
    let version = probe_source(&path).unwrap();
    let resources = load_path_resources(&path, &version.source_version).unwrap();
    let display = resources.images[PRIMARY_IMAGE_RESOURCE].as_ref().unwrap();
    assert_eq!(display.image.format(), ImageFormat::Png);
    assert_eq!((display.width, display.height), (1600.0, 800.0));
    let pixels = image::load_from_memory(display.image.bytes()).unwrap();
    assert_eq!((pixels.width(), pixels.height()), (1024, 512));
    assert_eq!(fs::read(path).unwrap(), original);
}

#[test]
fn admitted_webp_animation_preserves_three_real_frames_and_accounts_for_all_canvases() {
    use image::AnimationDecoder;
    let dir = fixture();
    let path = dir.path().join("animated.webp");
    let original = animated_webp((24, 12), (24, 12), 3);
    let decoder = image::codecs::webp::WebPDecoder::new(Cursor::new(&original)).unwrap();
    assert!(decoder.has_animation());
    let frames = decoder.into_frames().collect_frames().unwrap();
    assert_eq!(frames.len(), 3);
    assert_ne!(frames[0].buffer(), frames[1].buffer());
    fs::write(&path, &original).unwrap();
    let version = probe_source(&path).unwrap();
    let resources = load_path_resources(&path, &version.source_version).unwrap();
    let image = resources.images[PRIMARY_IMAGE_RESOURCE].as_ref().unwrap();
    assert_eq!(image.image.format(), ImageFormat::Webp);
    assert_eq!((image.width, image.height), (24.0, 12.0));
    assert_eq!(image.image.bytes(), original.as_slice());
    assert!(resources.retained_bytes() >= 24 * 12 * 4 * 3 * 2);
    assert!(resources.retained_bytes() <= native_content::CONTENT_RESOURCE_BUDGET_BYTES);
    assert_eq!(fs::read(path).unwrap(), original);
}

#[test]
fn oversized_webp_animation_uses_unavailable_snapshot_instead_of_flattening_or_reading_original_again() {
    let dir = fixture();
    let path = dir.path().join("large-animation.webp");
    // Tiny subframes still require complete retained 2048x1024 canvas frames.
    let original = animated_webp((2048, 1024), (1, 1), 3);
    fs::write(&path, &original).unwrap();
    let version = probe_source(&path).unwrap();
    let resources = load_path_resources(&path, &version.source_version).unwrap();
    assert!(resources.images[PRIMARY_IMAGE_RESOURCE].is_none());
    assert!(resources.retained_bytes() < 1024);
    assert_eq!(fs::read(path).unwrap(), original);
}

#[cfg(windows)]
#[test]
fn active_writer_is_not_read_as_a_stable_primary_image() {
    use std::os::windows::fs::OpenOptionsExt;
    let dir = fixture();
    let path = dir.path().join("busy.png");
    let original = png(8, 4, 7);
    fs::write(&path, &original).unwrap();
    let version = probe_source(&path).unwrap();
    let writer = OpenOptions::new().write(true).share_mode(0).open(&path).unwrap();
    assert!(load_path_resources(&path, &version.source_version).is_err());
    drop(writer);
    assert_eq!(fs::read(path).unwrap(), original);
}
