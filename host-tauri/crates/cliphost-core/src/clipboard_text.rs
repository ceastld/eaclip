//! Text carried by registered clipboard formats, including WPF/WinForms strings.
//! Decode only an NRBF root string; never instantiate managed objects or classes.

pub(crate) const MANAGED_PREFIX: &[u8; 16] = &[
    0x96, 0xa7, 0x9e, 0xfd, 0x13, 0x3b, 0x70, 0x43, 0xa6, 0x79, 0x56, 0x10, 0x6b, 0xb2, 0x88, 0xfb,
];
const MAX_BYTES: usize = 32 * 1024 * 1024;

pub(crate) fn decode(bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() || bytes.len() > MAX_BYTES {
        return None;
    }
    if let Some(payload) = bytes.strip_prefix(MANAGED_PREFIX) {
        // A recognized binary envelope must never fall through to text decoding.
        return managed_string(payload).filter(|text| !text.is_empty());
    }
    let utf16 = bytes.starts_with(&[0xff, 0xfe]) || bytes.get(1) == Some(&0);
    let text = if utf16 {
        let bytes = bytes.strip_prefix(&[0xff, 0xfe]).unwrap_or(bytes);
        if bytes.len() % 2 != 0 {
            return None;
        }
        let units: Vec<_> = bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .take_while(|&unit| unit != 0)
            .collect();
        String::from_utf16(&units).ok()?
    } else {
        let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
        let end = bytes
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(bytes.len());
        std::str::from_utf8(&bytes[..end]).ok()?.to_string()
    };
    (!text.is_empty()).then_some(text)
}

fn managed_string(bytes: &[u8]) -> Option<String> {
    // SerializedStreamHeader: root ID, header ID (-1), major 1, minor 0.
    if bytes.len() < 24
        || bytes[0] != 0
        || bytes[5..17] != [255, 255, 255, 255, 1, 0, 0, 0, 0, 0, 0, 0]
    {
        return None;
    }
    let root = i32::from_le_bytes(bytes[1..5].try_into().ok()?);
    // BinaryObjectString (6) must be the root; class/reference/array graphs are unsupported.
    if root <= 0 || bytes[17] != 6 || bytes[18..22] != bytes[1..5] {
        return None;
    }
    let mut cursor = 22;
    let mut length = 0usize;
    for shift in (0..=28).step_by(7) {
        let byte = *bytes.get(cursor)?;
        cursor += 1;
        if shift == 28 && byte > 7 {
            return None;
        }
        length |= ((byte & 0x7f) as usize) << shift;
        if byte & 0x80 == 0 {
            let end = cursor.checked_add(length)?;
            let value = bytes.get(cursor..end)?;
            // MessageEnd (11), then only allocator padding is allowed.
            if bytes.get(end) != Some(&11) || !bytes.get(end + 1..)?.iter().all(|&b| b == 0) {
                return None;
            }
            return std::str::from_utf8(value).ok().map(str::to_string);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quicker_wire_rejects_truncation_invalid_lengths_objects_and_trailing_records() {
        let wire = include_bytes!("../tests/fixtures/quicker-action-v2-wpf.bin");
        for end in 0..wire.len() {
            assert!(decode(&wire[..end]).is_none(), "truncated at {end}");
        }
        for (offset, value) in [(16, 1), (17, 0), (33, 5), (34, 2), (wire.len() - 1, 0)] {
            let mut bad = wire.to_vec();
            bad[offset] = value;
            assert!(decode(&bad).is_none(), "invalid at {offset}");
        }
        let mut overflow = wire[..38].to_vec();
        overflow.extend_from_slice(&[255, 255, 255, 255, 127]);
        assert!(decode(&overflow).is_none());
        let mut extra = wire.to_vec();
        extra.extend_from_slice(&[6, 1]);
        assert!(decode(&extra).is_none());
        let mut padded = wire.to_vec();
        padded.extend_from_slice(&[0; 16]);
        assert_eq!(decode(&padded), decode(wire));
    }

    #[test]
    fn quicker_wire_accepts_raw_unicode_and_rejects_invalid_text() {
        let text = "{\"Title\":\"中文🦀\"}";
        assert_eq!(decode(text.as_bytes()).as_deref(), Some(text));
        let mut utf16: Vec<u8> = text
            .encode_utf16()
            .chain([0])
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(decode(&utf16).as_deref(), Some(text));
        utf16.splice(..0, [255, 254]);
        assert_eq!(decode(&utf16).as_deref(), Some(text));
        assert!(decode(&[0xff, 0x31]).is_none());
        assert!(decode(&[b'{', 0, 1]).is_none());
    }
}
