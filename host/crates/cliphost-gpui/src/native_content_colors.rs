//! Pure projections of `colorInlineTokens` and its use in `clipInlineLinkRender`.
//!
//! Recognition is intentionally broader than successful CSS color parsing:
//! `rgb(garbage)` still has a swatch wrapper in the frontend. Keep such tokens
//! with `color: None` and paint a transparent swatch with the usual border.
//! Labels/ranges address the original display string, never a clipboard payload.

use std::ops::Range;

pub use crate::overlay::html::HtmlColor as RgbaColor;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColorToken {
    /// UTF-8 bytes in the exact supplied display string.
    pub range: Range<usize>,
    /// Exact CSS candidate (and tooltip), without surrounding JS whitespace.
    pub css: String,
    /// RGBA bytes, including alpha / 255. `None` means a transparent swatch;
    /// recognition/label retention do not depend on CSS paint validity.
    pub color: Option<RgbaColor>,
}

// ECMAScript trim/\s includes FEFF but excludes Unicode NEXT LINE (0085).
fn js_whitespace(ch: char) -> bool {
    matches!(ch, '\u{0009}'..='\u{000d}' | ' ' | '\u{00a0}' | '\u{1680}'
        | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}'
        | '\u{205f}' | '\u{3000}' | '\u{feff}')
}

fn word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn function_prefix(text: &str, start: usize) -> Option<usize> {
    ["rgb(", "rgba(", "hsl(", "hsla("]
        .into_iter()
        .find_map(|prefix| {
            text.get(start..start + prefix.len())
                .filter(|value| value.eq_ignore_ascii_case(prefix))
                .map(|_| prefix.len())
        })
}

fn exact_hex(text: &str) -> bool {
    text.strip_prefix('#').is_some_and(|body| {
        matches!(body.len(), 3 | 6 | 8) && body.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

fn recognized_css(text: &str) -> bool {
    exact_hex(text) || (function_prefix(text, 0).is_some() && text.ends_with(')'))
}

fn token(text: &str, range: Range<usize>) -> ColorToken {
    let css = text[range.clone()].trim_matches(js_whitespace).to_owned();
    ColorToken {
        range,
        color: parse_swatch_color(&css),
        css,
    }
}

/// Equivalent to the frontend's global /gi regex, including its ASCII word
/// boundary after hex, absent leading boundary, and first-`)` functional end.
/// Neither invalid CSS nor nested function syntax changes regex advancement.
pub fn collect_color_tokens(text: &str) -> Vec<ColorToken> {
    let bytes = text.as_bytes();
    let mut result = Vec::new();
    let mut cursor = 0;
    while cursor < text.len() {
        let end = if bytes[cursor] == b'#' {
            // Preserve regex alternative order: 3, then 6, then 8 digits.
            [3, 6, 8].into_iter().find_map(|digits| {
                let end = cursor + 1 + digits;
                let body = bytes.get(cursor + 1..end)?;
                (body.iter().all(u8::is_ascii_hexdigit)
                    && !bytes.get(end).is_some_and(|byte| word(*byte)))
                .then_some(end)
            })
        } else if let Some(prefix_len) = function_prefix(text, cursor) {
            text[cursor + prefix_len..]
                .find(')')
                .map(|offset| cursor + prefix_len + offset + 1)
        } else {
            None
        };
        if let Some(end) = end {
            result.push(token(text, cursor..end));
            cursor = end;
        } else {
            cursor += text[cursor..].chars().next().unwrap().len_utf8();
        }
    }
    result
}

/// The web renderer first splits at link boundaries, then at changes in search
/// hit flags, and invokes the color scanner separately on each resulting slice.
/// Supply those boundaries in bytes (the existing UTF-16 adapter expands split
/// surrogate scalars). Invalid/non-boundary offsets are ignored safely.
pub fn collect_color_tokens_with_boundaries(text: &str, boundaries: &[usize]) -> Vec<ColorToken> {
    let mut cuts = vec![0, text.len()];
    cuts.extend(
        boundaries
            .iter()
            .copied()
            .filter(|offset| text.is_char_boundary(*offset)),
    );
    cuts.sort_unstable();
    cuts.dedup();
    let mut result = Vec::new();
    for slice in cuts.windows(2) {
        let base = slice[0];
        result.extend(
            collect_color_tokens(&text[base..slice[1]])
                .into_iter()
                .map(|mut token| {
                    token.range = base + token.range.start..base + token.range.end;
                    token
                }),
        );
    }
    result
}

/// The compact toast's whole-preview branch: JS trim, then reject any JS \s.
/// It is not the main list's classifier. `rgb(1, 2, 3)` remains inline because
/// of its spaces; `rgb(1,2,3)` and even `rgb(garbage)` qualify as whole previews.
pub fn resolve_pure_color_token(text: &str) -> Option<ColorToken> {
    let trimmed = text.trim_matches(js_whitespace);
    if trimmed.is_empty() || trimmed.chars().any(js_whitespace) || !recognized_css(trimmed) {
        return None;
    }
    let start = text.len() - text.trim_start_matches(js_whitespace).len();
    Some(token(text, start..start + trimmed.len()))
}

fn css_whitespace(ch: char) -> bool {
    matches!(ch, ' ' | '\t' | '\n' | '\r' | '\u{000c}')
}

fn trim_css(text: &str) -> &str {
    text.trim_matches(css_whitespace)
}

// Comments delimit CSS numeric tokens. An unterminated comment is not a color.
fn without_comments(text: &str) -> Option<String> {
    let mut output = String::new();
    let mut cursor = 0;
    while let Some(relative) = text[cursor..].find("/*") {
        let start = cursor + relative;
        output.push_str(&text[cursor..start]);
        let end = start + 2 + text[start + 2..].find("*/")?;
        output.push(' ');
        cursor = end + 2;
    }
    output.push_str(&text[cursor..]);
    Some(output)
}

/// CSS number syntax, rather than Rust's acceptance of NaN/Infinity.
fn number(text: &str) -> Option<f64> {
    let bytes = text.as_bytes();
    let mut cursor = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    let integer_start = cursor;
    while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
        cursor += 1;
    }
    let mut digits = cursor - integer_start;
    if bytes.get(cursor) == Some(&b'.') {
        cursor += 1;
        let fraction_start = cursor;
        while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
            cursor += 1;
        }
        // CSS tokenizes a trailing '.' separately from a number.
        if cursor == fraction_start {
            return None;
        }
        digits += cursor - fraction_start;
    }
    if digits == 0 {
        return None;
    }
    if matches!(bytes.get(cursor), Some(b'e' | b'E')) {
        cursor += 1;
        if matches!(bytes.get(cursor), Some(b'+' | b'-')) {
            cursor += 1;
        }
        let exponent_start = cursor;
        while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
            cursor += 1;
        }
        if exponent_start == cursor {
            return None;
        }
    }
    if cursor != bytes.len() {
        return None;
    }
    let number = text.parse::<f64>().ok()?;
    number.is_finite().then_some(number)
}

#[derive(Clone, Copy)]
struct Scalar {
    value: f64,
    percent: bool,
    missing: bool,
}

fn scalar(text: &str, allow_none: bool) -> Option<Scalar> {
    if allow_none && text.eq_ignore_ascii_case("none") {
        return Some(Scalar {
            value: 0.0,
            percent: false,
            missing: true,
        });
    }
    let percent = text.ends_with('%');
    let value = number(if percent {
        &text[..text.len() - 1]
    } else {
        text
    })?;
    Some(Scalar {
        value,
        percent,
        missing: false,
    })
}

fn byte(value: f64) -> u8 {
    value.clamp(0.0, 255.0).round() as u8
}

fn alpha(value: Option<&str>, modern: bool) -> Option<u8> {
    match value {
        None => Some(255),
        Some(value) => {
            let value = scalar(value, modern)?;
            Some(byte(if value.percent {
                value.value / 100.0 * 255.0
            } else {
                value.value * 255.0
            }))
        }
    }
}

fn hue(text: &str, modern: bool) -> Option<f64> {
    if modern && text.eq_ignore_ascii_case("none") {
        return Some(0.0);
    }
    let lower = text.to_ascii_lowercase();
    let (value, scale) = [
        ("grad", 0.9),
        ("turn", 360.0),
        ("deg", 1.0),
        ("rad", 180.0 / std::f64::consts::PI),
    ]
    .into_iter()
    .find_map(|(suffix, scale)| lower.strip_suffix(suffix).map(|value| (value, scale)))
    .unwrap_or((&lower, 1.0));
    let degrees = number(value)? * scale;
    degrees.is_finite().then(|| degrees.rem_euclid(360.0))
}

fn functional_color(body: &str, hsl: bool) -> Option<RgbaColor> {
    let body = without_comments(body)?;
    let modern = !body.contains(',');
    let (parts, opacity): (Vec<&str>, Option<&str>) = if modern {
        let mut halves = body.split('/');
        let values = halves.next()?;
        let opacity = halves.next().map(trim_css);
        if halves.next().is_some() {
            return None;
        }
        (
            values
                .split(css_whitespace)
                .filter(|v| !v.is_empty())
                .collect(),
            opacity,
        )
    } else {
        if body.contains('/') {
            return None;
        }
        let mut values: Vec<_> = body.split(',').map(trim_css).collect();
        if !(3..=4).contains(&values.len()) {
            return None;
        }
        let opacity = (values.len() == 4).then(|| values.pop().unwrap());
        (values, opacity)
    };
    if parts.len() != 3 {
        return None;
    }
    let alpha = alpha(opacity, modern)?;
    let (red, green, blue) = if hsl {
        let hue = hue(parts[0], modern)? / 60.0;
        let saturation = scalar(parts[1], modern)?;
        let lightness = scalar(parts[2], modern)?;
        if !modern && (!saturation.percent || !lightness.percent) {
            return None;
        }
        let saturation = (saturation.value / 100.0).clamp(0.0, 1.0);
        let lightness = (lightness.value / 100.0).clamp(0.0, 1.0);
        let chroma = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
        let intermediate = chroma * (1.0 - (hue.rem_euclid(2.0) - 1.0).abs());
        let offset = lightness - chroma / 2.0;
        let (red, green, blue) = match hue.floor() as u8 {
            0 => (chroma, intermediate, 0.0),
            1 => (intermediate, chroma, 0.0),
            2 => (0.0, chroma, intermediate),
            3 => (0.0, intermediate, chroma),
            4 => (intermediate, 0.0, chroma),
            _ => (chroma, 0.0, intermediate),
        };
        (
            byte((red + offset) * 255.0),
            byte((green + offset) * 255.0),
            byte((blue + offset) * 255.0),
        )
    } else {
        let channels: Vec<_> = parts
            .iter()
            .map(|value| scalar(value, modern))
            .collect::<Option<_>>()?;
        if !modern
            && channels
                .iter()
                .any(|value| value.percent != channels[0].percent)
        {
            return None;
        }
        let channel = |value: Scalar| {
            if value.missing {
                0
            } else {
                byte(if value.percent {
                    value.value / 100.0 * 255.0
                } else {
                    value.value
                })
            }
        };
        (
            channel(channels[0]),
            channel(channels[1]),
            channel(channels[2]),
        )
    };
    Some(RgbaColor {
        red,
        green,
        blue,
        alpha,
    })
}

/// Paint the recognizer's hex and literal RGB/HSL candidates, including legacy
/// comma forms, modern whitespace/slash forms, percentages and CSS angle units.
/// Unresolved context/relative colors retain their token with no paint rather
/// than borrowing another label or silently inventing a color.
pub fn parse_swatch_color(css: &str) -> Option<RgbaColor> {
    let css = css.trim_matches(js_whitespace);
    if exact_hex(css) {
        return crate::overlay::html::parse_html_color(css);
    }
    let prefix = function_prefix(css, 0)?;
    let body = css.get(prefix..css.len().checked_sub(1)?)?;
    if !css.ends_with(')') {
        return None;
    }
    functional_color(body, css[..prefix].to_ascii_lowercase().starts_with("hsl"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(text: &str) -> Vec<&str> {
        collect_color_tokens(text)
            .into_iter()
            .map(|token| &text[token.range])
            .collect()
    }

    fn rgba(red: u8, green: u8, blue: u8, alpha: u8) -> RgbaColor {
        RgbaColor {
            red,
            green,
            blue,
            alpha,
        }
    }

    #[test]
    fn regex_hex_alternatives_and_ascii_boundary_match_web_fixtures() {
        let text = "#abc #AB12cd #01234567 #abcd #1234567 #abc_ #abcdefg a#123 #abcé #abc中";
        assert_eq!(
            labels(text),
            ["#abc", "#AB12cd", "#01234567", "#123", "#abc", "#abc"]
        );
        assert_eq!(parse_swatch_color("#abc"), Some(rgba(170, 187, 204, 255)));
        assert_eq!(parse_swatch_color("#01234567"), Some(rgba(1, 35, 69, 103)));
        assert!(parse_swatch_color("#abcd").is_none());
        assert!(parse_swatch_color("rebeccapurple").is_none());
    }

    #[test]
    fn function_regex_is_case_insensitive_unbounded_and_stops_at_first_close() {
        let text =
            "😀中prefiXRGB(1, 2, 3) RGBA() hSl(120\n100%\t50%) rgb(var(--c),0,0) rgb(unclosed";
        assert_eq!(
            labels(text),
            [
                "RGB(1, 2, 3)",
                "RGBA()",
                "hSl(120\n100%\t50%)",
                "rgb(var(--c)"
            ]
        );
        for token in collect_color_tokens(text) {
            assert!(text.is_char_boundary(token.range.start));
            assert!(text.is_char_boundary(token.range.end));
            assert_eq!(&text[token.range], token.css);
        }
    }

    #[test]
    fn invalid_css_keeps_recognized_swatch_wrappers_and_exact_labels() {
        let text = "rgb(garbage) hsl(garbage) rgba(1,2) rgb(NaN,0,0)";
        let tokens = collect_color_tokens(text);
        assert_eq!(tokens.len(), 4);
        assert!(tokens.iter().all(|token| token.color.is_none()));
        assert_eq!(tokens[0].css, "rgb(garbage)");
        assert!(resolve_pure_color_token("rgb(garbage)").is_some());
    }

    #[test]
    fn literal_rgb_alpha_percent_clamping_and_number_syntax_paint_correctly() {
        for (css, expected) in [
            ("rgb(1, 2, 3)", rgba(1, 2, 3, 255)),
            ("RGBA(10,20,30,.5)", rgba(10, 20, 30, 128)),
            ("rgb(100% 0% 50% / 25%)", rgba(255, 0, 128, 64)),
            ("rgba(-1 300 1e2 / 120%)", rgba(0, 255, 100, 255)),
            ("rgb(1 20% 3 / .25)", rgba(1, 51, 3, 64)),
            ("rgb(none 2 none / none)", rgba(0, 2, 0, 0)),
            ("rgb(1/* comment */ 2 3)", rgba(1, 2, 3, 255)),
        ] {
            assert_eq!(parse_swatch_color(css), Some(expected), "{css}");
        }
        for css in [
            "rgb(1,2%,3)",
            "rgb(1,2,3 / .5)",
            "rgb(1 2 3 / / .5)",
            "rgb(1. 2 3)",
            "rgb(0x10 2 3)",
            "rgb(1\u{a0}2 3)",
            "rgb(1/* 2 3)",
        ] {
            assert!(parse_swatch_color(css).is_none(), "{css}");
        }
    }

    #[test]
    fn hsl_legacy_modern_angle_units_wrap_and_alpha_paint_correctly() {
        for (css, expected) in [
            ("hsl(0,100%,50%)", rgba(255, 0, 0, 255)),
            ("HSLA(120,100%,50%,.5)", rgba(0, 255, 0, 128)),
            ("hsl(-120deg 100% 50% / 25%)", rgba(0, 0, 255, 64)),
            ("hsl(.5turn 100% 50%)", rgba(0, 255, 255, 255)),
            ("hsl(200grad 100% 50%)", rgba(0, 255, 255, 255)),
            ("hsl(3.141592653589793rad 100% 50%)", rgba(0, 255, 255, 255)),
            ("hsl(none none 50% / none)", rgba(128, 128, 128, 0)),
            ("hsl(0 100 50)", rgba(255, 0, 0, 255)),
            ("hsl(0,200%,150%)", rgba(255, 255, 255, 255)),
        ] {
            assert_eq!(parse_swatch_color(css), Some(expected), "{css}");
        }
        for css in [
            "hsl(0,100,50)",
            "hsl(0% 100% 50%)",
            "hsl(from red h s l)",
            "hsla(0,100%,50%,NaN)",
            "hsl(1e308turn 100% 50%)",
        ] {
            assert!(parse_swatch_color(css).is_none(), "{css}");
        }
    }

    #[test]
    fn hit_and_link_boundaries_rescan_slices_instead_of_preserving_whole_colors() {
        let text = "https://example.test/#abcdef rgb(1,2,3)";
        let hex = text.find('#').unwrap();
        let rgb = text.find("rgb").unwrap();
        assert_eq!(collect_color_tokens(text).len(), 2);
        let split = collect_color_tokens_with_boundaries(text, &[hex + 4, rgb + 5]);
        assert_eq!(split.len(), 1);
        assert_eq!(split[0].range, hex..hex + 4);
        assert_eq!(split[0].css, "#abc");
        let unicode = "😀#abc";
        assert_eq!(
            collect_color_tokens_with_boundaries(unicode, &[1, usize::MAX]),
            collect_color_tokens(unicode)
        );
    }

    #[test]
    fn compact_whole_color_classifier_uses_exact_js_trim_and_whitespace_rules() {
        let text = "\u{feff}\u{a0}#abc \n";
        let token = resolve_pure_color_token(text).unwrap();
        assert_eq!(token.css, "#abc");
        assert_eq!(&text[token.range], "#abc");
        assert!(resolve_pure_color_token("rgb(1,2,3)").is_some());
        for text in [
            "",
            " ",
            "#abcd",
            "#abc #def",
            "rgb(1, 2,3)",
            "hsl(0\u{feff}100%\u{feff}50%)",
            "red",
        ] {
            assert!(resolve_pure_color_token(text).is_none(), "{text}");
        }
        // NEXT LINE is not JS \s, so the recognizer accepts the whole candidate;
        // CSS itself does not interpret it as a numeric whitespace separator.
        let token = resolve_pure_color_token("rgb(1\u{85}2\u{85}3)").unwrap();
        assert!(token.color.is_none());
    }
}
