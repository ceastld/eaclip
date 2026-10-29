//! Adapter for the existing GlobalConfig Custom palette. The base derivation
//! follows `frontend/src/core/clipThemePalette.ts`; interactive colors follow
//! the current non-background-image rules in `app.css`. No new configuration
//! or theme source is introduced. Persisted inputs use WPF `#AARRGGBB`, while
//! runtime token strings use CSS `#RRGGBBAA`, also understood by HTML/SVG.

use crate::preview::{DesignTokens, ThemeTokens};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq)]
struct Rgba {
    r: f64,
    g: f64,
    b: f64,
    a: f64,
}

impl Rgba {
    const fn rgb(r: f64, g: f64, b: f64) -> Self {
        Self { r, g, b, a: 255.0 }
    }

    fn css_hex(self) -> String {
        let byte = |value: f64| js_round(value.clamp(0.0, 255.0)) as u8;
        format!(
            "#{:02X}{:02X}{:02X}{:02X}",
            byte(self.r),
            byte(self.g),
            byte(self.b),
            byte(self.a),
        )
    }
}

const WHITE: Rgba = Rgba::rgb(255.0, 255.0, 255.0);
const BLACK: Rgba = Rgba::rgb(0.0, 0.0, 0.0);
const TRANSPARENT: Rgba = Rgba { a: 0.0, ..BLACK };

fn js_round(value: f64) -> f64 {
    let floor = value.floor();
    if value - floor < 0.5 {
        floor
    } else {
        floor + 1.0
    }
}

fn js_whitespace(value: u32) -> bool {
    matches!(value, 0x09..=0x0D | 0x20 | 0xA0 | 0x1680 | 0x2000..=0x200A
        | 0x2028 | 0x2029 | 0x202F | 0x205F | 0x3000 | 0xFEFF)
}

/// Custom palette selection follows the frontend's JS whitespace semantics.
pub(crate) fn is_custom_mode(mode: &str) -> bool {
    mode.trim_matches(|c: char| js_whitespace(c as u32))
        .eq_ignore_ascii_case("custom")
}

/// Match JS parseInt(pair, 16), including a valid leading digit followed by
/// a non-hex character. This preserves the existing best-effort fallback.
fn parse_hex_pair(pair: &[u16]) -> Option<f64> {
    let mut index = 0;
    while pair
        .get(index)
        .is_some_and(|c| js_whitespace(u32::from(*c)))
    {
        index += 1;
    }
    let sign = match pair.get(index) {
        Some(0x2D) => {
            index += 1;
            -1.0
        }
        Some(0x2B) => {
            index += 1;
            1.0
        }
        _ => 1.0,
    };
    if pair.get(index) == Some(&0x30) && matches!(pair.get(index + 1), Some(0x58 | 0x78)) {
        index += 2;
    }
    let mut value = 0;
    let mut digits = 0;
    while let Some(digit) = pair
        .get(index)
        .and_then(|c| char::from_u32(u32::from(*c)))
        .and_then(|c| c.to_digit(16))
    {
        value = value * 16 + digit;
        digits += 1;
        index += 1;
    }
    (digits > 0).then_some(f64::from(value) * sign)
}

fn parse_persisted_color(input: Option<&str>, fallback: Rgba) -> Rgba {
    let Some(input) = input else { return fallback };
    let input = input.trim_matches(|c: char| js_whitespace(c as u32));
    let Some(hex) = input.strip_prefix('#') else {
        return fallback;
    };
    // JS slice/length uses UTF-16 code units, not UTF-8 bytes.
    let hex: Vec<_> = hex.encode_utf16().collect();
    let (offset, alpha) = match hex.len() {
        6 => (0, Some(255.0)),
        8 => (2, parse_hex_pair(&hex[..2])),
        _ => return fallback,
    };
    match (
        parse_hex_pair(&hex[offset..offset + 2]),
        parse_hex_pair(&hex[offset + 2..offset + 4]),
        parse_hex_pair(&hex[offset + 4..offset + 6]),
        alpha,
    ) {
        (Some(r), Some(g), Some(b), Some(a)) => Rgba { r, g, b, a },
        _ => fallback,
    }
}

/// Frontend mixRgba rounds each RGBA byte independently. Do not premultiply
/// or composite against the window before the base palette is derived.
fn mix_rgba(from: Rgba, to: Rgba, amount: f64) -> Rgba {
    let amount = amount.clamp(0.0, 1.0);
    let mix = |a: f64, b: f64| js_round(a + (b - a) * amount);
    Rgba {
        r: mix(from.r, to.r),
        g: mix(from.g, to.g),
        b: mix(from.b, to.b),
        a: mix(from.a, to.a),
    }
}

fn luminance(color: Rgba) -> f64 {
    let linear = |channel: f64| {
        let channel = channel / 255.0;
        if channel <= 0.03928 {
            channel / 12.92
        } else {
            ((channel + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(color.r) + 0.7152 * linear(color.g) + 0.0722 * linear(color.b)
}

/// Native surfaces with only light/dark palettes use the same Custom
/// background parsing and brightness boundary as the GPUI palette.
pub(crate) fn persisted_background_is_dark(input: Option<&str>) -> bool {
    luminance(parse_persisted_color(input, WHITE)) < 0.45
}

fn rgb_to_hsv(color: Rgba) -> (f64, f64, f64) {
    let (r, g, b) = (color.r / 255.0, color.g / 255.0, color.b / 255.0);
    let max = r.max(g).max(b);
    let delta = max - r.min(g).min(b);
    if max < 1e-9 {
        return (0.0, 0.0, max);
    }
    let saturation = delta / max;
    if delta < 1e-9 {
        return (0.0, saturation, max);
    }
    let mut hue = if (max - r).abs() < 1e-9 {
        60.0 * ((g - b) / delta)
    } else if (max - g).abs() < 1e-9 {
        60.0 * ((b - r) / delta) + 120.0
    } else {
        60.0 * ((r - g) / delta) + 240.0
    };
    if hue < 0.0 {
        hue += 360.0;
    }
    if hue >= 360.0 {
        hue -= 360.0;
    }
    (hue, saturation, max)
}

fn hsv_to_rgb(hue: f64, saturation: f64, value: f64) -> Rgba {
    let hue = ((hue % 360.0) + 360.0) % 360.0;
    let chroma = value.clamp(0.0, 1.0) * saturation.clamp(0.0, 1.0);
    let x = chroma * (1.0 - ((hue / 60.0) % 2.0 - 1.0).abs());
    let m = value.clamp(0.0, 1.0) - chroma;
    let (r, g, b) = match (hue / 60.0).floor() as u32 % 6 {
        0 => (chroma, x, 0.0),
        1 => (x, chroma, 0.0),
        2 => (0.0, chroma, x),
        3 => (0.0, x, chroma),
        4 => (x, 0.0, chroma),
        _ => (chroma, 0.0, x),
    };
    let byte = |channel: f64| js_round((channel + m).clamp(0.0, 1.0) * 255.0);
    Rgba::rgb(byte(r), byte(g), byte(b))
}

fn derive_accent(background: Rgba, foreground: Rgba) -> Rgba {
    let (hue, saturation, _) = rgb_to_hsv(foreground);
    let lum = luminance(background);
    let (hue, mut saturation) = if saturation < 0.14 {
        (217.0, 0.58)
    } else {
        (hue, (saturation + 0.12).clamp(0.38, 0.72))
    };
    let mut value = if lum < 0.22 {
        saturation = saturation.min(0.62);
        0.78
    } else if lum < 0.45 {
        0.72
    } else {
        0.48
    };
    let mut accent = hsv_to_rgb(hue, saturation, value);
    if (luminance(accent) - lum).abs() < 0.08 {
        value = if lum < 0.45 {
            (value + 0.18).min(1.0)
        } else {
            (value - 0.15).max(0.0)
        };
        accent = hsv_to_rgb(hue, saturation, value);
    }
    accent
}

#[derive(Clone, Copy)]
struct Palette {
    background: Rgba,
    text: Rgba,
    text_secondary: Rgba,
    text_muted: Rgba,
    surface: Rgba,
    secondary: Rgba,
    tertiary: Rgba,
    border: Rgba,
    border_secondary: Rgba,
    accent: Rgba,
    pin: Rgba,
    success: Rgba,
    error: Rgba,
    scrollbar_track: Rgba,
    scrollbar_thumb: Rgba,
    scrollbar_thumb_hover: Rgba,
}

fn derive_palette(background: Rgba, foreground: Rgba) -> Palette {
    let lum = luminance(background);
    let secondary = mix_rgba(background, foreground, 0.09);
    let tertiary = mix_rgba(background, foreground, 0.18);
    let accent = derive_accent(background, foreground);
    let dark = lum < 0.45;
    let scrollbar_track = mix_rgba(
        background,
        if dark { WHITE } else { BLACK },
        if dark { 0.06 } else { 0.04 },
    );
    let scrollbar_thumb = mix_rgba(
        scrollbar_track,
        if dark { WHITE } else { BLACK },
        if dark { 0.35 } else { 0.22 },
    );
    Palette {
        background: mix_rgba(
            background,
            foreground,
            if lum > 0.5 { 0.025 } else { 0.045 },
        ),
        text: foreground,
        text_secondary: mix_rgba(foreground, background, 0.3),
        text_muted: mix_rgba(foreground, background, 0.48),
        surface: background,
        secondary,
        tertiary,
        border: mix_rgba(secondary, foreground, 0.12),
        border_secondary: mix_rgba(tertiary, foreground, 0.2),
        accent,
        pin: mix_rgba(accent, Rgba::rgb(255.0, 210.0, 76.0), 0.38),
        success: if dark {
            Rgba::rgb(125.0, 211.0, 168.0)
        } else {
            Rgba::rgb(4.0, 120.0, 87.0)
        },
        error: if dark {
            Rgba::rgb(255.0, 95.0, 95.0)
        } else {
            Rgba::rgb(217.0, 48.0, 37.0)
        },
        scrollbar_track,
        scrollbar_thumb,
        scrollbar_thumb_hover: mix_rgba(
            scrollbar_thumb,
            if dark { WHITE } else { BLACK },
            if dark { 0.18 } else { 0.15 },
        ),
    }
}

/// CSS color-mix(in srgb) premultiplies alpha, unlike frontend mixRgba.
/// In particular, mixing a color with transparent keeps its RGB hue.
fn css_mix(from: Rgba, to: Rgba, amount: f64) -> Rgba {
    let amount = amount.clamp(0.0, 1.0);
    let from_weight = (1.0 - amount) * from.a.clamp(0.0, 255.0);
    let to_weight = amount * to.a.clamp(0.0, 255.0);
    let alpha = from_weight + to_weight;
    if alpha <= 0.0 {
        return TRANSPARENT;
    }
    let mix = |a: f64, b: f64| js_round((a * from_weight + b * to_weight) / alpha);
    Rgba {
        r: mix(from.r, to.r),
        g: mix(from.g, to.g),
        b: mix(from.b, to.b),
        a: js_round(alpha),
    }
}

fn theme_tokens(palette: Palette) -> ThemeTokens {
    // Derived for parity with all 16 Web variables; these three currently
    // have no separate field in the existing ThemeTokens contract.
    let _ = (
        palette.success,
        palette.scrollbar_track,
        palette.scrollbar_thumb_hover,
    );
    ThemeTokens {
        background: palette.background.css_hex(),
        surface: palette.surface.css_hex(),
        secondary: palette.secondary.css_hex(),
        tertiary: palette.tertiary.css_hex(),
        text: palette.text.css_hex(),
        text_secondary: palette.text_secondary.css_hex(),
        text_muted: palette.text_muted.css_hex(),
        border: palette.border.css_hex(),
        border_secondary: palette.border_secondary.css_hex(),
        accent: palette.accent.css_hex(),
        pin: palette.pin.css_hex(),
        // app.css:7274/7295 standard rounded clipboard row states.
        selected: css_mix(palette.surface, palette.accent, 0.12).css_hex(),
        hover: css_mix(palette.surface, palette.text, 0.07).css_hex(),
        // app.css:6523 overrides the older category-tab background.
        tab_active: css_mix(palette.accent, TRANSPARENT, 0.88).css_hex(),
        // app.css:5650 toolbar search-cluster, not the generic FilterBox.
        search_background: css_mix(palette.secondary, TRANSPARENT, 0.3).css_hex(),
        // app.css:1057 title-bar active navigation.
        title_active: css_mix(palette.tertiary, TRANSPARENT, 0.84).css_hex(),
        scrollbar_thumb: palette.scrollbar_thumb.css_hex(),
        error: palette.error.css_hex(),
        // ui-design tokens: neutral selection replaces accent tints in the main
        // window; matches the default palettes (#3a3a3a dark / #e4e6eb light).
        neutral_active: css_mix(palette.surface, palette.text, 0.11).css_hex(),
        warning: palette.pin.css_hex(),
    }
}

/// Resolve from an immutable base on every authoritative config change.
/// Both existing branches receive Custom colors; the window's light/dark
/// decision remains separate. Non-Custom restores the original base exactly.
pub fn resolved_tokens(base: &DesignTokens, config: &Value) -> DesignTokens {
    let mut resolved = base.clone();
    if !is_custom_mode(config["uiThemeMode"].as_str().unwrap_or_default()) {
        return resolved;
    }
    let background = parse_persisted_color(config["windowBackground"].as_str(), WHITE);
    let foreground = parse_persisted_color(config["fontForeground"].as_str(), BLACK);
    let theme = theme_tokens(derive_palette(background, foreground));
    resolved.themes.light = theme.clone();
    resolved.themes.dark = theme;
    resolved
}

/// The immutable base only depends on these three persisted palette inputs.
/// Compare borrowed strings while idle; clone/derive tokens only after a change.
#[derive(Default)]
pub(crate) struct ResolvedThemeCache {
    inputs: Option<[Option<String>; 3]>,
}

impl ResolvedThemeCache {
    pub(crate) fn resolve(&mut self, base: &DesignTokens, config: &Value) -> Option<DesignTokens> {
        let inputs = ["uiThemeMode", "windowBackground", "fontForeground"]
            .map(|key| config[key].as_str());
        if self.inputs.as_ref().is_some_and(|previous| {
            previous.iter().zip(inputs).all(|(old, new)| old.as_deref() == new)
        }) {
            return None;
        }
        self.inputs = Some(inputs.map(|value| value.map(str::to_owned)));
        Some(resolved_tokens(base, config))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cached_palette_rebuilds_only_for_palette_inputs_and_restores_builtin() {
        let base = DesignTokens::from_json(crate::preview::TOKENS_JSON).unwrap();
        let mut cache = ResolvedThemeCache::default();
        let mut config = json!({"uiThemeMode":"custom", "windowBackground":"#FF102030", "fontForeground":"#FFEEEEEE"});
        let first = cache.resolve(&base, &config).unwrap();
        assert!(cache.resolve(&base, &config).is_none());
        config["clipListAppearanceSlot"] = json!("simple");
        assert!(cache.resolve(&base, &config).is_none());
        config["windowBackground"] = json!("#FF203040");
        let second = cache.resolve(&base, &config).unwrap();
        assert_ne!(first.themes.light.surface, second.themes.light.surface);
        config["fontForeground"] = json!("#FFDDDDDD");
        let third = cache.resolve(&base, &config).unwrap();
        assert_ne!(second.themes.light.text, third.themes.light.text);
        config["uiThemeMode"] = json!("auto");
        let restored = cache.resolve(&base, &config).unwrap();
        assert_eq!(restored.themes.light, base.themes.light);
        assert_eq!(restored.themes.dark, base.themes.dark);
        assert!(cache.resolve(&base, &config).is_none());
    }

    #[test]
    fn persisted_colors_match_frontend_wpf_parse_and_fallback() {
        for (input, expected) in [
            ("#80112233", "#11223380"),
            ("#112233", "#112233FF"),
            ("#FZ112233", "#1122330F"),
            ("#GG112233", "#FFFFFFFF"),
            ("\u{FEFF}\u{A0}#80112233\u{A0}\u{FEFF}", "#11223380"),
            ("\u{85}#80112233\u{85}", "#FFFFFFFF"),
            ("rgb(17 34 51)", "#FFFFFFFF"),
        ] {
            assert_eq!(
                parse_persisted_color(Some(input), WHITE).css_hex(),
                expected,
                "{input:?}"
            );
        }
    }

    #[test]
    fn all_sixteen_palette_values_match_actual_frontend_goldens() {
        // Captured by executing clipThemePalette.ts, not another copy of its
        // algorithm. Order follows the returned CSS variable object.
        let fixtures = [
            (
                "#FFFFFFFF",
                "#FF000000",
                [
                    "#FFF9F9F9",
                    "#FF000000",
                    "#FF4D4D4D",
                    "#FF7A7A7A",
                    "#FFFFFFFF",
                    "#FFE8E8E8",
                    "#FFD1D1D1",
                    "#FFCCCCCC",
                    "#FFA7A7A7",
                    "#FF334F7A",
                    "#FF818169",
                    "#FF047857",
                    "#FFD93025",
                    "#FFF5F5F5",
                    "#FFBFBFBF",
                    "#FFA2A2A2",
                ],
            ),
            (
                "#FF142538",
                "#FF4AC8A8",
                [
                    "#FF162C3D",
                    "#FF4AC8A8",
                    "#FF3A9786",
                    "#FF307A72",
                    "#FF142538",
                    "#FF193442",
                    "#FF1E424C",
                    "#FF1F464E",
                    "#FF275D5E",
                    "#FF4CC7A8",
                    "#FF90CB85",
                    "#FF7DD3A8",
                    "#FFFF5F5F",
                    "#FF223244",
                    "#FF6F7A85",
                    "#FF89929B",
                ],
            ),
            (
                "#80204560",
                "#C0DB7030",
                [
                    "#8328475E",
                    "#C0DB7030",
                    "#ADA3633E",
                    "#A1815B47",
                    "#80204560",
                    "#8631495C",
                    "#8C424D57",
                    "#8D454E57",
                    "#9661544F",
                    "#FFC77A4C",
                    "#FFDC9B4C",
                    "#FF7DD3A8",
                    "#FFFF5F5F",
                    "#882D506A",
                    "#B2778D9E",
                    "#C08FA2AF",
                ],
            ),
        ];
        for (background, foreground, expected) in fixtures {
            let p = derive_palette(
                parse_persisted_color(Some(background), WHITE),
                parse_persisted_color(Some(foreground), BLACK),
            );
            let actual = [
                p.background,
                p.text,
                p.text_secondary,
                p.text_muted,
                p.surface,
                p.secondary,
                p.tertiary,
                p.border,
                p.border_secondary,
                p.accent,
                p.pin,
                p.success,
                p.error,
                p.scrollbar_track,
                p.scrollbar_thumb,
                p.scrollbar_thumb_hover,
            ];
            for (index, (actual, wpf_expected)) in actual.into_iter().zip(expected).enumerate() {
                let css_expected = format!("#{}{}", &wpf_expected[3..], &wpf_expected[1..3]);
                assert_eq!(
                    actual.css_hex(),
                    css_expected,
                    "{background}/{foreground} role {index}"
                );
            }
        }
    }

    #[test]
    fn resolved_palette_preserves_alpha_geometry_and_builtin_restore() {
        let base = DesignTokens::from_json(crate::preview::TOKENS_JSON).unwrap();
        let original = serde_json::to_value(&base).unwrap();
        let config = json!({"uiThemeMode":" CUSTOM ", "windowBackground":"#80204560", "fontForeground":"#C0DB7030"});
        let config_before = config.clone();
        let resolved = resolved_tokens(&base, &config);
        assert_eq!(resolved.themes.light.surface, "#20456080");
        assert_eq!(resolved.themes.light.text, "#DB7030C0");
        assert_eq!(resolved.themes.light.tab_active, "#C77A4C1F");
        assert_eq!(resolved.themes.light.search_background, "#31495C5E");
        assert_eq!(resolved.themes.light.title_active, "#424D5716");
        assert_eq!(
            serde_json::to_value(&resolved.themes.light).unwrap(),
            serde_json::to_value(&resolved.themes.dark).unwrap()
        );
        let mut output = serde_json::to_value(&resolved).unwrap();
        output["themes"] = original["themes"].clone();
        assert_eq!(output, original);
        assert_eq!(serde_json::to_value(&base).unwrap(), original);
        assert_eq!(config, config_before);
        for mode in ["auto", "light", "dark", "invalid", ""] {
            let mut config = config.clone();
            config["uiThemeMode"] = json!(mode);
            assert_eq!(
                serde_json::to_value(resolved_tokens(&base, &config)).unwrap(),
                original
            );
        }
    }
}
