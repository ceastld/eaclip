//! Device sync shares exact image identity with ordinary clipboard capture.

pub(super) fn image_key(bytes: &[u8]) -> Result<String, String> {
    crate::image_identity::image_key(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png() -> Vec<u8> {
        let image = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            2,
            2,
            image::Rgb([64, 128, 192]),
        ));
        let mut bytes = std::io::Cursor::new(Vec::new());
        image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
        bytes.into_inner()
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

    #[test]
    fn device_sync_image_plain_png_deduplicates_changed_lossless_encoding() {
        let bytes = png();
        assert!(image_key(&bytes).unwrap().starts_with("image-pixels-v1:"));
        let mut offset = 8;
        let split = loop {
            let length = u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
            if &bytes[offset + 4..offset + 8] == b"IDAT" {
                assert!(length > 1);
                let data = &bytes[offset + 8..offset + 8 + length];
                let mut split = bytes[..offset].to_vec();
                split.extend(chunk(b"IDAT", &data[..length / 2]));
                split.extend(chunk(b"IDAT", &data[length / 2..]));
                split.extend_from_slice(&bytes[offset + length + 12..]);
                break split;
            }
            offset += length + 12;
        };
        assert_ne!(bytes, split);
        let key = image_key(&bytes).unwrap();
        assert!(key.starts_with("image-pixels-v1:"));
        assert_eq!(key, image_key(&split).unwrap());
    }

    #[test]
    fn device_sync_image_standard_windows_metadata_shares_pixel_identity() {
        let bytes = png();
        let plain = image_key(&bytes).unwrap();
        let mut tagged = bytes[..33].to_vec();
        tagged.extend(chunk(b"gAMA", &45455u32.to_be_bytes()));
        tagged.extend(chunk(b"sRGB", &[0]));
        tagged.extend(chunk(b"pHYs", &[0, 0, 14, 196, 0, 0, 14, 196, 1]));
        tagged.extend_from_slice(&bytes[33..]);
        assert_eq!(plain, image_key(&tagged).unwrap());
        let mut nonstandard = bytes[..33].to_vec();
        nonstandard.extend(chunk(b"gAMA", &100000u32.to_be_bytes()));
        nonstandard.extend_from_slice(&bytes[33..]);
        let key = image_key(&nonstandard).unwrap();
        assert!(key.starts_with("image-bytes-v1:"));
        assert_ne!(plain, key);
    }

    #[test]
    fn device_sync_image_sixteen_bit_and_jpeg_keep_original_bytes() {
        let image = image::DynamicImage::ImageRgb16(image::ImageBuffer::from_pixel(
            2,
            2,
            image::Rgb([1u16, 32768, 65534]),
        ));
        let mut bytes = std::io::Cursor::new(Vec::new());
        image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
        assert!(image_key(bytes.get_ref())
            .unwrap()
            .starts_with("image-bytes-v1:"));
        let image = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            2,
            2,
            image::Rgb([1, 2, 3]),
        ));
        let mut bytes = std::io::Cursor::new(Vec::new());
        image
            .write_to(&mut bytes, image::ImageFormat::Jpeg)
            .unwrap();
        assert!(image_key(bytes.get_ref())
            .unwrap()
            .starts_with("image-bytes-v1:"));
    }
}
