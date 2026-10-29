//! Exact clipboard-image identity, shared by capture and device sync.
//! Preserve original bytes. Known 8-bit PNG encodings compare exact decoded
//! samples, without making descriptive metadata a new clipboard-image identity.

use sha2::{Digest, Sha256};

pub(crate) fn image_key(bytes: &[u8]) -> Result<String, String> {
    let metadata = crate::clipboard_sync::validate_image(bytes)?;
    if metadata.mime_type != "image/png" || !pixel_identity_png(bytes) {
        return Ok(format!(
            "image-bytes-v1:{}",
            crate::model::sha256_hex(bytes)
        ));
    }
    let mut reader =
        image::ImageReader::with_format(std::io::Cursor::new(bytes), image::ImageFormat::Png);
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(crate::clipboard_sync::MAX_IMAGE_PIXELS * 8);
    reader.limits(limits);
    let image = reader
        .decode()
        .map_err(|error| error.to_string())?
        .to_rgba8();
    let mut hash = Sha256::new();
    // Keep image-pixels-v1 compatible with already-created origin markers.
    hash.update(image.width().to_le_bytes());
    hash.update(image.height().to_le_bytes());
    hash.update(image.as_raw());
    Ok(format!("image-pixels-v1:{:x}", hash.finalize()))
}

const SRGB_CHROMATICITIES: [u32; 8] = [31270, 32900, 64000, 33000, 30000, 60000, 15000, 6000];

fn standard_chromaticities(data: &[u8]) -> bool {
    data.len() == 32
        && data
            .chunks_exact(4)
            .zip(SRGB_CHROMATICITIES)
            .all(|(value, expected)| u32::from_be_bytes(value.try_into().unwrap()) == expected)
}

/// Windows commonly adds standard sRGB declarations and descriptive metadata
/// when republishing a PNG. These do not change its decoded clipboard samples.
/// Physical sizing (pHYs) is excluded from pixel identity; stored bytes retain it.
/// tRNS is different: the decoder must include it in RGBA, never discard it.
/// ICC/EXIF/APNG, nonstandard color metadata, 16-bit and unknown chunks retain
/// byte identity because decoding to RGBA8 does not preserve their semantics.
pub(crate) fn pixel_identity_png(bytes: &[u8]) -> bool {
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return false;
    }
    let mut offset = 8usize;
    let mut color_type = None;
    let mut palette_entries = None;
    let mut transparency = false;
    let mut image_data = false;
    let mut data_ended = false;
    let mut color_chunks = [false; 3];
    let mut physical_size = false;
    let mut timestamp = false;
    while let Some(header) = bytes.get(offset..offset.saturating_add(8)) {
        let length = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
        let Some(end) = offset.checked_add(12).and_then(|v| v.checked_add(length)) else {
            return false;
        };
        if end > bytes.len() {
            return false;
        }
        let kind = &header[4..8];
        let data = &bytes[offset + 8..end - 4];
        if image_data && kind != b"IDAT" {
            data_ended = true;
        }
        match kind {
            b"IHDR"
                if offset == 8
                    && length == 13
                    && data[8] == 8
                    && matches!(data[9], 0 | 2 | 3 | 4 | 6) =>
            {
                color_type = Some(data[9]);
            }
            b"PLTE"
                if color_type.is_some()
                    && !image_data
                    && !transparency
                    && palette_entries.is_none()
                    && length > 0
                    && length <= 768
                    && length % 3 == 0 =>
            {
                palette_entries = Some(length / 3);
            }
            b"tRNS" if !image_data && !transparency => {
                let valid = match color_type {
                    Some(0) => length == 2 && data[0] == 0,
                    Some(2) => length == 6 && data[0] == 0 && data[2] == 0 && data[4] == 0,
                    Some(3) => palette_entries.is_some_and(|count| length > 0 && length <= count),
                    _ => false,
                };
                if !valid {
                    return false;
                }
                transparency = true;
            }
            b"sRGB" | b"gAMA" | b"cHRM"
                if color_type.is_some() && !image_data && palette_entries.is_none() =>
            {
                let (index, valid) = match kind {
                    b"sRGB" => (0, length == 1 && data[0] <= 3),
                    b"gAMA" => (1, data == 45455u32.to_be_bytes()),
                    _ => (2, standard_chromaticities(data)),
                };
                if !valid || color_chunks[index] {
                    return false;
                }
                color_chunks[index] = true;
            }
            b"pHYs"
                if color_type.is_some()
                    && !image_data
                    && !physical_size
                    && length == 9
                    && data[8] <= 1 =>
            {
                physical_size = true;
            }
            b"tIME"
                if color_type.is_some()
                    && !timestamp
                    && length == 7
                    && (1..=12).contains(&data[2])
                    && (1..=31).contains(&data[3])
                    && data[4] <= 23
                    && data[5] <= 59
                    && data[6] <= 60 =>
            {
                timestamp = true;
            }
            b"tEXt" | b"iTXt" | b"zTXt" if color_type.is_some() => {}
            b"IDAT"
                if color_type.is_some()
                    && !data_ended
                    && (color_type != Some(3) || palette_entries.is_some()) =>
            {
                image_data = true;
            }
            b"IEND" if image_data && length == 0 => return end == bytes.len(),
            _ => return false,
        }
        offset = end;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(image: image::DynamicImage) -> Vec<u8> {
        let mut bytes = std::io::Cursor::new(Vec::new());
        image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
        bytes.into_inner()
    }
    fn rgb() -> image::RgbImage {
        image::ImageBuffer::from_fn(3, 2, |x, y| {
            image::Rgb([
                (x * 71 + 30) as u8,
                (y * 113 + 40) as u8,
                (x * 23 + y * 19) as u8,
            ])
        })
    }
    fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut bytes = (data.len() as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(data);
        let mut crc = u32::MAX;
        for byte in &bytes[4..] {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb88320 & 0u32.wrapping_sub(crc & 1));
            }
        }
        bytes.extend_from_slice(&(!crc).to_be_bytes());
        bytes
    }
    fn tagged(bytes: &[u8], chunks: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
        let mut output = bytes[..33].to_vec();
        for (kind, data) in chunks {
            output.extend(chunk(kind, data));
        }
        output.extend_from_slice(&bytes[33..]);
        output
    }

    #[test]
    fn standard_windows_color_and_descriptive_metadata_share_exact_pixels() {
        let bytes = encode(image::DynamicImage::ImageRgb8(rgb()));
        let chromaticities: Vec<u8> = SRGB_CHROMATICITIES
            .into_iter()
            .flat_map(u32::to_be_bytes)
            .collect();
        let mut physical = 3780u32.to_be_bytes().to_vec();
        physical.extend_from_slice(&3780u32.to_be_bytes());
        physical.push(1);
        let gamma = 45455u32.to_be_bytes();
        let timestamp = [0x07, 0xea, 10, 21, 12, 30, 0];
        let mut compressed_comment = b"note\0\0".to_vec();
        compressed_comment.extend_from_slice(&[0x78, 0x9c, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01]);
        let metadata = tagged(
            &bytes,
            &[
                (b"sRGB", &[0]),
                (b"gAMA", &gamma),
                (b"cHRM", &chromaticities),
                (b"pHYs", &physical),
                (b"tEXt", b"note\0same pixels"),
                (b"iTXt", b"note\0\0\0\0\0same pixels"),
                (b"zTXt", &compressed_comment),
                (b"tIME", &timestamp),
            ],
        );
        assert_ne!(bytes, metadata);
        assert_eq!(image_key(&bytes).unwrap(), image_key(&metadata).unwrap());
        for intent in 0..=3 {
            assert_eq!(
                image_key(&bytes).unwrap(),
                image_key(&tagged(&bytes, &[(b"sRGB", &[intent])])).unwrap()
            );
        }
    }

    #[test]
    fn opaque_rgb_and_rgba_match_but_actual_alpha_and_pixel_changes_do_not() {
        let original = image::DynamicImage::ImageRgb8(rgb());
        let bytes = encode(original.clone());
        let mut rgba = original.to_rgba8();
        assert_eq!(
            image_key(&bytes).unwrap(),
            image_key(&encode(image::DynamicImage::ImageRgba8(rgba.clone()))).unwrap()
        );
        rgba.get_pixel_mut(0, 0)[3] = 254;
        assert_ne!(
            image_key(&bytes).unwrap(),
            image_key(&encode(image::DynamicImage::ImageRgba8(rgba))).unwrap()
        );
        let mut changed = rgb();
        changed.get_pixel_mut(1, 1)[0] += 1;
        assert_ne!(
            image_key(&bytes).unwrap(),
            image_key(&encode(image::DynamicImage::ImageRgb8(changed))).unwrap()
        );
        let solid = |color| {
            encode(image::DynamicImage::ImageRgb8(
                image::ImageBuffer::from_pixel(3, 2, image::Rgb(color)),
            ))
        };
        assert_ne!(
            image_key(&solid([0, 0, 0])).unwrap(),
            image_key(&solid([255, 0, 0])).unwrap()
        );
    }

    #[test]
    fn transparency_chunk_is_decoded_rather_than_ignored() {
        let original = image::DynamicImage::ImageRgb8(rgb());
        let bytes = encode(original.clone());
        let pixel = rgb()[(0, 0)];
        let transparent_color = [0, pixel[0], 0, pixel[1], 0, pixel[2]];
        let transparent = tagged(&bytes, &[(b"tRNS", &transparent_color)]);
        let mut rgba = original.to_rgba8();
        rgba.get_pixel_mut(0, 0)[3] = 0;
        assert_ne!(image_key(&bytes).unwrap(), image_key(&transparent).unwrap());
        assert_eq!(
            image_key(&transparent).unwrap(),
            image_key(&encode(image::DynamicImage::ImageRgba8(rgba))).unwrap()
        );
    }

    #[test]
    fn nonstandard_or_unknown_display_metadata_does_not_enter_pixel_domain() {
        let bytes = encode(image::DynamicImage::ImageRgb8(rgb()));
        for (kind, data) in [
            (b"gAMA", 100000u32.to_be_bytes().to_vec()),
            (b"cHRM", vec![0; 32]),
            (b"sRGB", vec![4]),
            (b"iCCP", vec![]),
            (b"eXIf", vec![]),
            (b"acTL", vec![]),
            (b"sBIT", vec![8, 8, 8]),
            (b"cICP", vec![1, 13, 0, 1]),
            (b"prIV", vec![]),
        ] {
            let input = tagged(&bytes, &[(kind, &data)]);
            assert!(!pixel_identity_png(&input), "unsupported {kind:?}");
        }
        let nonstandard_gamma = tagged(&bytes, &[(b"gAMA", &100000u32.to_be_bytes())]);
        assert!(image_key(&nonstandard_gamma)
            .unwrap()
            .starts_with("image-bytes-v1:"));
        assert_ne!(
            image_key(&bytes).unwrap(),
            image_key(&nonstandard_gamma).unwrap()
        );
    }

    #[test]
    fn malformed_metadata_duplicates_and_trailing_bytes_are_not_normalized() {
        let bytes = encode(image::DynamicImage::ImageRgb8(rgb()));
        for chunks in [
            vec![(b"sRGB", &[][..])],
            vec![(b"pHYs", &[][..])],
            vec![(b"sRGB", &[0][..]), (b"sRGB", &[0][..])],
        ] {
            assert!(!pixel_identity_png(&tagged(&bytes, &chunks)));
        }
        let mut trailing = bytes;
        trailing.push(0);
        assert!(!pixel_identity_png(&trailing));
    }
}
