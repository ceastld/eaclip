use super::*;
use sha2::{Digest, Sha256};

const RGB_MASKS: [u32; 3] = [0x00FF_0000, 0x0000_FF00, 0x0000_00FF];

fn set_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn v5(width: u32, height: i32, alpha_mask: u32, rgba: &[u8], repeated_masks: bool) -> Vec<u8> {
    assert_eq!(
        rgba.len(),
        width as usize * height.unsigned_abs() as usize * 4
    );
    let mut bytes = vec![0; BITMAPV5HEADER_SIZE];
    set_u32(&mut bytes, 0, BITMAPV5HEADER_SIZE as u32);
    set_u32(&mut bytes, 4, width);
    bytes[8..12].copy_from_slice(&height.to_le_bytes());
    bytes[12..14].copy_from_slice(&1u16.to_le_bytes());
    bytes[14..16].copy_from_slice(&32u16.to_le_bytes());
    set_u32(&mut bytes, 16, BI_BITFIELDS);
    set_u32(&mut bytes, 20, rgba.len() as u32);
    for (index, mask) in RGB_MASKS.into_iter().enumerate() {
        set_u32(&mut bytes, 40 + index * 4, mask);
    }
    set_u32(&mut bytes, 52, alpha_mask);
    set_u32(&mut bytes, 56, 0x7352_4742); // LCS_sRGB
    set_u32(&mut bytes, 108, 4); // LCS_GM_IMAGES, as in the WPF clipboard sample.
    if repeated_masks {
        for mask in RGB_MASKS {
            bytes.extend_from_slice(&mask.to_le_bytes());
        }
    }
    let stride = width as usize * 4;
    for row in 0..height.unsigned_abs() as usize {
        let source_row = if height > 0 {
            height as usize - row - 1
        } else {
            row
        };
        for pixel in rgba[source_row * stride..(source_row + 1) * stride].chunks_exact(4) {
            bytes.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
        }
    }
    bytes
}

fn rgba_from_v5(bytes: Vec<u8>) -> Vec<u8> {
    let png = dibv5_to_png(bytes).expect("valid test V5");
    image::load_from_memory(&png).unwrap().to_rgba8().into_raw()
}

fn seed11_rgba() -> Vec<u8> {
    let mut rgba = Vec::new();
    for y in 0..32 {
        for x in 0..64 {
            rgba.extend_from_slice(&[
                ((x * 17 + y * 3 + 11 * 11) % 256) as u8,
                ((x * 5 + y * 29 + 11 * 7) % 256) as u8,
                ((x * 13 + y * 19 + 11 * 23) % 256) as u8,
                255,
            ]);
        }
    }
    rgba
}

#[test]
fn dibv5_standard_transparency_and_row_orientation_are_preserved() {
    let rgba = [1, 2, 3, 0, 4, 5, 6, 64, 7, 8, 9, 128, 10, 11, 12, 255];
    for height in [2, -2] {
        assert_eq!(rgba_from_v5(v5(2, height, 0xFF00_0000, &rgba, false)), rgba);
    }
}

#[test]
fn dibv5_zero_alpha_mask_means_opaque_even_when_pixel_alpha_is_zero() {
    let rgba = [1, 2, 3, 0, 4, 5, 6, 64, 7, 8, 9, 128, 10, 11, 12, 0];
    let expected = [1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255];
    for height in [2, -2] {
        assert_eq!(rgba_from_v5(v5(2, height, 0, &rgba, false)), expected);
    }
}

#[test]
fn dibv5_ambiguous_wpf_uses_independent_bitmap_for_real_seed11_pixels() {
    // Reconstructs the non-sensitive 64x32 probe image and complete V5 layout
    // captured during real dual-Windows clipboard sync, without external files.
    let expected = seed11_rgba();
    assert_eq!(
        format!("{:x}", Sha256::digest(&expected)),
        "4203e849df034662341dc3a969d6300171be5889e3e9f3f74d16f0eb95b8dc06"
    );
    let bytes = v5(64, 32, 0, &expected, true);
    assert_eq!(bytes.len(), 8328);
    assert_eq!(
        format!("{:x}", Sha256::digest(&bytes)),
        "f3b62ffdd2c3f388dcab9e616442abed60afb6e0f3043e350e090658add955db"
    );
    // The V5 bytes alone also describe standard pixels plus allocation tail.
    // The native reader may prefer a separately copied same-sized CF_BITMAP.
    assert_ne!(rgba_from_v5(bytes.clone()), expected);
    for bytes in [bytes, v5(64, -32, 0, &expected, true)] {
        let bgra: Vec<_> = expected
            .chunks_exact(4)
            .flat_map(|pixel| [pixel[2], pixel[1], pixel[0], pixel[3]])
            .collect();
        let mut reads = 0;
        let bitmap = dibv5_bitmap_fallback::<()>(&bytes, || {
            reads += 1;
            Ok(Some((bgra, 64, 32)))
        })
        .unwrap()
        .unwrap();
        let png = bgra32_to_png_opaque(&bitmap.0, bitmap.1, bitmap.2).unwrap();
        assert_eq!(reads, 1);
        assert_eq!(
            image::load_from_memory(&png).unwrap().to_rgba8().into_raw(),
            expected
        );
    }
}

#[test]
fn dibv5_standard_mask_colored_pixels_are_not_mistaken_for_extended_masks() {
    let rgba = [255, 0, 0, 0, 0, 255, 0, 0, 0, 0, 255, 0];
    let bytes = v5(3, 1, 0, &rgba, false);
    assert_eq!(&bytes[124..136], &bytes[40..52]);
    assert_eq!(
        rgba_from_v5(bytes),
        [255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255]
    );
}

#[test]
fn dibv5_standard_allocation_tail_does_not_invalidate_complete_pixels() {
    let rgba = [21, 43, 65, 255, 32, 54, 76, 255];
    for alpha_mask in [0, 0xFF00_0000] {
        for tail_bytes in [0, 1, 4, 8, 12, 16, 64] {
            let mut bytes = v5(2, 1, alpha_mask, &rgba, false);
            bytes.extend(std::iter::repeat_n(0xAB, tail_bytes));
            assert_eq!(rgba_from_v5(bytes), rgba);
        }
    }
    // Standard opaque mask-colored pixels plus 12 allocation bytes are byte-
    // ambiguous with WPF's layout. A pure decoder must keep standard semantics.
    let mask_pixels = [255, 0, 0, 0, 0, 255, 0, 0, 0, 0, 255, 0];
    for alpha_mask in [0, 0xFF00_0000] {
        let mut bytes = v5(3, 1, alpha_mask, &mask_pixels, false);
        bytes.extend_from_slice(&[0xAB; 12]);
        let expected = if alpha_mask == 0 {
            vec![255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255]
        } else {
            mask_pixels.to_vec()
        };
        assert_eq!(rgba_from_v5(bytes.clone()), expected);
        let png = crate::host::HostState::decode_native_drop_dib(&bytes, true).unwrap();
        assert_eq!(
            image::load_from_memory(&png).unwrap().to_rgba8().into_raw(),
            expected
        );
    }
}

#[test]
fn dibv5_bitmap_fallback_never_reads_ordinary_or_transparent_layouts() {
    let rgba = [255, 0, 0, 0, 0, 255, 0, 0, 0, 0, 255, 0];
    let standard = v5(3, 1, 0, &rgba, false);
    let mut transparent_tail = v5(3, 1, 0xFF00_0000, &rgba, false);
    transparent_tail.extend_from_slice(&[0xAB; 12]);
    let mut ordinary_tail = v5(3, 1, 0, &[20; 12], false);
    ordinary_tail.extend_from_slice(&[0xAB; 12]);
    let mut invalid = v5(3, 1, 0, &rgba, true);
    set_u32(&mut invalid, 12, 0);
    for bytes in [standard, transparent_tail, ordinary_tail, invalid] {
        assert!(
            dibv5_bitmap_fallback::<()>(&bytes, || panic!("unrelated bitmap read"))
                .unwrap()
                .is_none()
        );
    }
}

#[test]
fn dibv5_missing_or_mismatched_bitmap_keeps_standard_pixels() {
    let rgba = [255, 0, 0, 0, 0, 255, 0, 0, 0, 0, 255, 0];
    let mut bytes = v5(3, 1, 0, &rgba, false);
    bytes.extend_from_slice(&[0xAB; 12]);
    assert_eq!(dibv5_ambiguous_opaque_size(&bytes), Some((3, 1)));
    for supplied in [None, Some((vec![8; 12], 1, 3)), Some((vec![8; 8], 3, 1))] {
        let bitmap = dibv5_bitmap_fallback::<()>(&bytes, || Ok(supplied)).unwrap();
        assert!(bitmap.is_none());
        assert_eq!(
            rgba_from_v5(bytes.clone()),
            [255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255]
        );
    }
    assert_eq!(
        dibv5_bitmap_fallback(&bytes, || Err("sequence changed")).unwrap_err(),
        "sequence changed"
    );
}

#[test]
fn dibv5_rejects_invalid_headers_and_incomplete_standard_pixels() {
    let valid = v5(64, 32, 0, &seed11_rgba(), true);
    for (offset, value) in [
        (0, 40),
        (4, 0),
        (8, i32::MIN as u32),
        (12, 0),
        (16, BI_RGB),
        (20, 8180),
        (32, u32::MAX),
        (40, 0x0000_00FF),
        (52, 1),
        (112, 136),
        (116, 12),
    ] {
        let mut invalid = valid.clone();
        set_u32(&mut invalid, offset, value);
        assert!(
            dibv5_to_png(invalid).is_none(),
            "offset {offset}, value {value}"
        );
    }
    for length in [0, 123, 124, 135, valid.len() - 13] {
        let mut invalid = valid.clone();
        invalid.truncate(length);
        assert!(dibv5_to_png(invalid).is_none(), "truncated length {length}");
    }
}

#[test]
fn dibv5_near_match_uses_standard_layout_instead_of_guessing_mask_skip() {
    let expected = seed11_rgba();
    let valid = v5(64, 32, 0, &expected, true);
    // These streams can also describe legal standard pixels plus allocation
    // tail. Do not reject them or infer a different start from mask colors alone.
    for (offset, value) in [
        (20, 0),
        (32, 1),
        (52, 0xFF00_0000),
        (56, 0),
        (124, 0x0000_FF00),
    ] {
        let mut standard = valid.clone();
        set_u32(&mut standard, offset, value);
        assert_ne!(rgba_from_v5(standard), expected);
    }
    for length in [valid.len() - 12, valid.len() - 1, valid.len() + 1] {
        let mut standard = valid.clone();
        standard.resize(length, 0);
        assert_ne!(rgba_from_v5(standard), expected);
    }
}

#[test]
fn dibv5_color_table_is_skipped_before_pixels_and_cannot_trigger_mask_variant() {
    let rgba = [21, 43, 65, 255];
    let mut bytes = v5(1, 1, 0, &rgba, false);
    set_u32(&mut bytes, 32, 3);
    let table = RGB_MASKS
        .into_iter()
        .flat_map(u32::to_le_bytes)
        .collect::<Vec<_>>();
    bytes.splice(124..124, table);
    assert_eq!(rgba_from_v5(bytes.clone()), rgba);
    set_u32(&mut bytes, 32, 4);
    assert!(dibv5_to_png(bytes).is_none());
}

#[test]
fn dib_color_tables_follow_infoheader_masks_and_have_checked_bounds() {
    for bit_count in [24u16, 32] {
        let mut bytes = vec![0; BITMAPINFOHEADER_SIZE];
        set_u32(&mut bytes, 0, BITMAPINFOHEADER_SIZE as u32);
        set_u32(&mut bytes, 4, 1);
        set_u32(&mut bytes, 8, 1);
        bytes[12..14].copy_from_slice(&1u16.to_le_bytes());
        bytes[14..16].copy_from_slice(&bit_count.to_le_bytes());
        if bit_count == 32 {
            set_u32(&mut bytes, 16, BI_BITFIELDS);
            for mask in RGB_MASKS {
                bytes.extend_from_slice(&mask.to_le_bytes());
            }
        }
        set_u32(&mut bytes, 32, 2);
        bytes.extend_from_slice(&[1, 2, 3, 0, 4, 5, 6, 0]);
        bytes.extend_from_slice(&[65, 43, 21, 255]);
        let png = dib_to_png(bytes.clone()).unwrap();
        assert_eq!(
            image::load_from_memory(&png).unwrap().to_rgba8().as_raw(),
            &[21, 43, 65, 255]
        );
        bytes.pop();
        assert!(dib_to_png(bytes.clone()).is_none());
        set_u32(&mut bytes, 32, u32::MAX);
        assert!(dib_to_png(bytes).is_none());
    }
}

#[test]
fn dibv5_profiles_are_only_allowed_after_complete_standard_pixels() {
    let rgba = [21, 43, 65, 255];
    let mut bytes = v5(1, 1, 0, &rgba, false);
    set_u32(&mut bytes, 56, 0x4D42_4544); // PROFILE_EMBEDDED
    let profile_offset = bytes.len() as u32;
    set_u32(&mut bytes, 112, profile_offset);
    set_u32(&mut bytes, 116, 4);
    bytes.extend_from_slice(&[1, 2, 3, 4]);
    assert_eq!(rgba_from_v5(bytes.clone()), rgba);
    for (offset, value) in [(112, 124), (112, u32::MAX), (116, 0), (116, u32::MAX)] {
        let mut invalid = bytes.clone();
        set_u32(&mut invalid, offset, value);
        assert!(dibv5_to_png(invalid).is_none());
    }
}
