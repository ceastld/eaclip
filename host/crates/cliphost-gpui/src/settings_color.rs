//! WPF settings colors mirror frontend/core/wpfColor.ts. Eight digits are
//! AARRGGBB, never CSS RRGGBBAA; a six-digit color has an opaque alpha byte.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ArgbColor {
    pub(super) a: u8,
    pub(super) r: u8,
    pub(super) g: u8,
    pub(super) b: u8,
}

impl ArgbColor {
    pub(super) fn parse(input: &str) -> Option<Self> {
        let hex = input.trim().strip_prefix('#')?;
        if !matches!(hex.len(), 6 | 8) || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        let value = u32::from_str_radix(hex, 16).ok()?;
        Some(Self {
            a: if hex.len() == 8 {
                (value >> 24) as u8
            } else {
                255
            },
            r: (value >> 16) as u8,
            g: (value >> 8) as u8,
            b: value as u8,
        })
    }

    pub(super) fn serialize(self) -> String {
        format!("#{:02X}{:02X}{:02X}{:02X}", self.a, self.r, self.g, self.b)
    }

    /// Packed 0xRRGGBB for the UI; alpha remains available separately as `a`.
    /// This is not the reversed byte order used by Win32 COLORREF.
    pub(super) fn rgb(self) -> u32 {
        (u32::from(self.r) << 16) | (u32::from(self.g) << 8) | u32::from(self.b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn six_digit_colors_normalize_to_opaque_argb_without_changing_rgb() {
        let color = ArgbColor::parse(" \t#1a2B3c\n").unwrap();
        assert_eq!(
            color,
            ArgbColor {
                a: 255,
                r: 0x1a,
                g: 0x2b,
                b: 0x3c
            }
        );
        assert_eq!(color.serialize(), "#FF1A2B3C");
        assert_eq!(color.rgb(), 0x1a2b3c);
        assert_eq!(ArgbColor::parse(&color.serialize()), Some(color));
    }

    #[test]
    fn eight_digit_wpf_colors_keep_alpha_at_the_front_including_transparent() {
        for alpha in [0, 0x7f, 255] {
            let color = ArgbColor {
                a: alpha,
                r: 0x12,
                g: 0x34,
                b: 0x56,
            };
            assert_eq!(
                ArgbColor::parse(&color.serialize().to_lowercase()),
                Some(color)
            );
            assert_eq!(color.rgb(), 0x123456);
        }
        assert_eq!(
            ArgbColor::parse("#11223344"),
            Some(ArgbColor {
                a: 0x11,
                r: 0x22,
                g: 0x33,
                b: 0x44
            })
        );
    }

    #[test]
    fn malformed_colors_are_rejected_without_guessing_a_default() {
        for input in [
            "",
            " ",
            "123456",
            "0x123456",
            "#abc",
            "#abcd",
            "#12345",
            "#1234567",
            "#123456789",
            "#12345G",
            "##123456",
            "#12 456",
            "#１２３４５６",
            "#123456 extra",
        ] {
            assert_eq!(ArgbColor::parse(input), None, "{input:?}");
        }
    }

    #[test]
    fn serialization_keeps_leading_zeroes_and_channel_boundaries() {
        let color = ArgbColor {
            a: 0,
            r: 1,
            g: 0x80,
            b: 255,
        };
        assert_eq!(color.serialize(), "#000180FF");
        assert_eq!(color.rgb(), 0x0180ff);
        assert_eq!(ArgbColor::parse(&color.serialize()), Some(color));
    }
}
