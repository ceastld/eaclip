//! Settings-specific layout and CSS-equivalent theme colors. These helpers use
//! the shared tokens and do not depend on the main clipboard view.

use gpui::{Hsla, Rgba};

use crate::preview::{SettingsTokens, ThemeTokens};
use crate::{window_background::WindowBackgroundOptions, window_glass};

#[derive(Clone, Copy, Debug)]
pub(super) struct SettingsGeometry {
    pub(super) sidebar_width: f32,
    pub(super) content_padding: f32,
    pub(super) content_width: f32,
    pub(super) row_inner_width: f32,
    pub(super) stack_controls: bool,
    pub(super) inline_control_width: f32,
}

impl SettingsGeometry {
    pub(super) fn new(viewport_width: f32, settings: &SettingsTokens) -> Self {
        // SettingsLayout.css uses max-width: 700px for the narrower sidebar
        // and padding. The row container, not the viewport, controls stacking.
        let narrow = viewport_width <= 700.0;
        // Keep the longest current label and its search count on one line.
        // The former 122 DIP breakpoint wrapped 资源管理器 and clipped English.
        let sidebar_min = match cliphost_core::i18n::displayed_language() {
            cliphost_core::i18n::LanguageMode::English => 217.0,
            _ => 160.0,
        };
        let sidebar_width = settings.sidebar_width.max(sidebar_min);
        let content_padding = if narrow {
            14.0
        } else {
            settings.content_padding_x
        };
        let content_width = (viewport_width - sidebar_width - 2.0 * content_padding).max(0.0);
        // Each group card has a 1px border on both sides. At a 560px viewport,
        // the compact layout leaves exactly 380px for the row's inner content.
        let row_inner_width = (content_width - 2.0 * settings.row_padding_x - 2.0).max(0.0);
        Self {
            sidebar_width,
            content_padding,
            content_width,
            row_inner_width,
            stack_controls: row_inner_width <= 380.0,
            inline_control_width: settings.control_width.min(row_inner_width * 0.46),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct SettingsPalette {
    pub(crate) page: Hsla,
    pub(crate) titlebar: Hsla,
    pub(crate) sidebar: Hsla,
    pub(crate) sidebar_border: Hsla,
    pub(crate) card: Hsla,
    pub(crate) card_border: Hsla,
    pub(crate) row_border: Hsla,
    pub(crate) control: Hsla,
    pub(crate) control_border: Hsla,
    pub(crate) control_hover: Hsla,
    pub(crate) control_hover_border: Hsla,
    pub(crate) focus_border: Hsla,
    pub(crate) selected: Hsla,
    pub(crate) selected_border: Hsla,
    pub(crate) description: Hsla,
    /// `.settings-flow-list .settings-setting-row:hover` (40% `surface-secondary`).
    pub(crate) row_hover: Hsla,
}

impl SettingsPalette {
    pub(crate) fn new(theme: &ThemeTokens) -> Self {
        let page = hex_color(&theme.background);
        let surface = hex_color(&theme.surface);
        let border = hex_color(&theme.border);
        let accent = hex_color(&theme.accent);
        let control_border = mix_color(border, hex_color(&theme.border_secondary), 0.8);
        Self {
            page: to_hsla(page),
            titlebar: to_hsla(surface),
            sidebar: to_hsla(mix_color(surface, page, 0.65)),
            sidebar_border: to_hsla(mix_color(border, page, 0.75)),
            card: to_hsla(surface),
            card_border: to_hsla(mix_color(border, page, 0.75)),
            row_border: to_hsla(mix_color(border, surface, 0.55)),
            control: to_hsla(surface),
            control_border: to_hsla(control_border),
            control_hover: to_hsla(hex_color(&theme.secondary)),
            control_hover_border: to_hsla(mix_color(accent, control_border, 0.3)),
            focus_border: to_hsla(accent),
            selected: to_hsla(mix_color(accent, surface, 0.1)),
            selected_border: to_hsla(mix_color(accent, control_border, 0.6)),
            description: to_hsla(hex_color(&theme.text_muted)),
            row_hover: to_hsla(fade_color(hex_color(&theme.secondary), 0.4)),
        }
    }

    /// Use only while a current prepared wallpaper is displayed. Loading,
    /// failed, removed or stale images use `new(base)` for the ordinary palette.
    pub(crate) fn with_background(
        base: &ThemeTokens,
        glass: &ThemeTokens,
        options: &WindowBackgroundOptions,
    ) -> Self {
        let mut result = Self::new(base);
        let edges = background_edges(
            hex_color(&base.border),
            hex_color(&base.border_secondary),
            hex_color(&base.accent),
            options.overlay_strength(),
        );
        let (titlebar, sidebar_border) = window_glass::titlebar_colors(base, options);
        result.page = to_hsla(fade_color(hex_color(&base.background), 0.0));
        // These text-colored tints are independent of the image blend slider.
        result.sidebar = to_hsla(fade_color(hex_color(&base.text), 0.035));
        result.titlebar = titlebar;
        result.sidebar_border = sidebar_border;
        result.card = window_glass::primary_strong(base, options);
        result.card_border = to_hsla(edges.card);
        result.row_border = to_hsla(edges.row);
        result.control = to_hsla(hex_color(&glass.secondary));
        result.control_border = to_hsla(edges.control);
        result.control_hover = to_hsla(hex_color(&glass.tertiary));
        result.control_hover_border = to_hsla(edges.hover);
        result.focus_border = to_hsla(edges.focus);
        result.selected = to_hsla(hex_color(&glass.tab_active));
        result.selected_border = to_hsla(edges.selected);
        // `--host-glass-row-hover`: 8% `text-primary` over transparent.
        result.row_hover = to_hsla(fade_color(hex_color(&base.text), 0.08));
        result
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct PaletteColor {
    // Keep sRGB channels on the byte scale to preserve the former opaque
    // mixes' rounding, including intermediate colors used by another mix.
    r: f32,
    g: f32,
    b: f32,
    a: f32,
}

#[derive(Clone, Copy, Debug)]
struct BackgroundEdges {
    card: PaletteColor,
    row: PaletteColor,
    control: PaletteColor,
    hover: PaletteColor,
    focus: PaletteColor,
    selected: PaletteColor,
}

fn fade_color(mut color: PaletteColor, weight: f32) -> PaletteColor {
    color.a *= weight;
    color
}

fn background_edges(
    border: PaletteColor,
    border_secondary: PaletteColor,
    accent: PaletteColor,
    overlay_strength: f32,
) -> BackgroundEdges {
    // SettingsControls.css fades card/control borders by fixed weights. Only
    // the row separator also tracks the wallpaper blend strength.
    let control = fade_color(border_secondary, 0.65);
    BackgroundEdges {
        card: fade_color(border, 0.75),
        row: fade_color(border, 0.32 * overlay_strength),
        control,
        hover: border_secondary,
        focus: accent,
        selected: mix_color(accent, control, 0.6),
    }
}

fn hex_color(hex: &str) -> PaletteColor {
    // Runtime ThemeTokens use CSS #RRGGBB / #RRGGBBAA. Persisted settings
    // continue to use WPF ARGB and are converted before reaching these tokens.
    let hex = hex.trim().strip_prefix('#').expect("validated theme color");
    assert!(matches!(hex.len(), 6 | 8), "validated theme color");
    let color = u32::from_str_radix(hex, 16).expect("validated theme color");
    let rgb = if hex.len() == 8 { color >> 8 } else { color };
    PaletteColor {
        r: f32::from((rgb >> 16) as u8),
        g: f32::from((rgb >> 8) as u8),
        b: f32::from(rgb as u8),
        a: if hex.len() == 8 {
            f32::from(color as u8) / 255.0
        } else {
            1.0
        },
    }
}

/// CSS color-mix(in srgb, first weight%, second) with premultiplied alpha.
/// Preserve the existing byte rounding when both inputs are fully opaque.
fn mix_color(first: PaletteColor, second: PaletteColor, first_weight: f32) -> PaletteColor {
    let second_weight = 1.0 - first_weight;
    if first.a == 1.0 && second.a == 1.0 {
        return PaletteColor {
            r: (first.r * first_weight + second.r * second_weight).round(),
            g: (first.g * first_weight + second.g * second_weight).round(),
            b: (first.b * first_weight + second.b * second_weight).round(),
            a: 1.0,
        };
    }
    let first_alpha = first.a * first_weight;
    let second_alpha = second.a * second_weight;
    let alpha = first_alpha + second_alpha;
    if alpha == 0.0 {
        return PaletteColor {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 0.0,
        };
    }
    PaletteColor {
        r: (first.r * first_alpha + second.r * second_alpha) / alpha,
        g: (first.g * first_alpha + second.g * second_alpha) / alpha,
        b: (first.b * first_alpha + second.b * second_alpha) / alpha,
        a: alpha,
    }
}

fn to_hsla(color: PaletteColor) -> Hsla {
    Rgba {
        r: color.r / 255.0,
        g: color.g / 255.0,
        b: color.b / 255.0,
        a: color.a,
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_accepts_css_alpha_at_the_end_and_opaque_six_digit_colors() {
        let opaque = hex_color("#123456");
        let translucent = hex_color("#1234567F");
        assert_eq!(
            [opaque.r, opaque.g, opaque.b, opaque.a],
            [18.0, 52.0, 86.0, 1.0]
        );
        assert_eq!(
            [translucent.r, translucent.g, translucent.b],
            [18.0, 52.0, 86.0]
        );
        assert_eq!(translucent.a, 127.0 / 255.0);
        assert_eq!(hex_color("#12345600").a, 0.0);
    }

    #[test]
    fn transparent_mixes_use_premultiplied_srgb_and_keep_continuous_alpha() {
        let transparent_red = hex_color("#FF000000");
        let opaque_blue = hex_color("#0000FF");
        let mixed = mix_color(transparent_red, opaque_blue, 0.5);
        assert_eq!([mixed.r, mixed.g, mixed.b, mixed.a], [0.0, 0.0, 255.0, 0.5]);
        let invisible = mix_color(transparent_red, hex_color("#0000FF00"), 0.65);
        assert_eq!(
            [invisible.r, invisible.g, invisible.b, invisible.a],
            [0.0; 4]
        );
        let half_mix = mix_color(hex_color("#FF000080"), hex_color("#0000FF80"), 0.5);
        assert!((half_mix.r - 127.5).abs() < 0.001);
        assert!((half_mix.b - 127.5).abs() < 0.001);
        assert_eq!(half_mix.a, 128.0 / 255.0);
    }

    #[test]
    fn opaque_and_nested_palette_mixes_preserve_existing_rounded_bytes() {
        let border = hex_color("#D7DEEB");
        let control_border = mix_color(border, hex_color("#BCC7DC"), 0.8);
        assert_eq!(
            [control_border.r, control_border.g, control_border.b],
            [210.0, 217.0, 232.0]
        );
        let selected_border = mix_color(hex_color("#3E7CF0"), control_border, 0.6);
        assert_eq!(
            [
                selected_border.r,
                selected_border.g,
                selected_border.b,
                selected_border.a
            ],
            [121.0, 161.0, 237.0, 1.0]
        );
        let card_border = mix_color(border, hex_color("#F5F7FB"), 0.75);
        assert_eq!(
            [card_border.r, card_border.g, card_border.b],
            [223.0, 228.0, 239.0]
        );
    }

    #[test]
    fn wallpaper_blend_fades_row_separator_without_fading_card_or_control_edges() {
        let border = hex_color("#123456");
        let secondary = hex_color("#ABCDEF");
        let accent = hex_color("#0011FF");
        let strong = background_edges(border, secondary, accent, 1.0);
        let weak = background_edges(border, secondary, accent, 0.15);
        assert_eq!(strong.card, weak.card);
        assert_eq!(strong.card.a, 0.75);
        assert_eq!(strong.control, weak.control);
        assert_eq!(strong.control.a, 0.65);
        assert_eq!(strong.hover, secondary);
        assert_eq!(weak.hover, secondary);
        assert_eq!(strong.focus, accent);
        assert_eq!(weak.focus, accent);
        assert_eq!(strong.row.a, 0.32);
        assert!((weak.row.a - 0.048).abs() < 0.000001);
        assert_eq!([weak.row.r, weak.row.g, weak.row.b], [18.0, 52.0, 86.0]);
    }

    #[test]
    fn wallpaper_edges_preserve_custom_alpha_and_premultiply_selected_border() {
        let border = hex_color("#12345680");
        let secondary = hex_color("#FF000040");
        let accent = hex_color("#0000FF80");
        let edges = background_edges(border, secondary, accent, 0.405);
        assert!((edges.card.a - 128.0 / 255.0 * 0.75).abs() < 0.000001);
        assert!((edges.row.a - 128.0 / 255.0 * 0.32 * 0.405).abs() < 0.000001);
        assert!((edges.control.a - 64.0 / 255.0 * 0.65).abs() < 0.000001);
        assert_eq!(edges.hover, secondary);
        assert_eq!(edges.focus, accent);
        let blue_alpha = 128.0 / 255.0 * 0.6;
        let red_alpha = 64.0 / 255.0 * 0.65 * 0.4;
        let alpha = blue_alpha + red_alpha;
        assert!((edges.selected.a - alpha).abs() < 0.000001);
        assert!((edges.selected.r - 255.0 * red_alpha / alpha).abs() < 0.0001);
        assert_eq!(edges.selected.g, 0.0);
        assert!((edges.selected.b - 255.0 * blue_alpha / alpha).abs() < 0.0001);
        let transparent = background_edges(
            hex_color("#12345600"),
            hex_color("#FF000000"),
            hex_color("#0000FF00"),
            0.405,
        );
        assert_eq!(transparent.selected, hex_color("#00000000"));
    }

    #[test]
    fn row_hover_is_forty_percent_secondary_and_glass_is_eight_percent_text() {
        let tokens = crate::preview::DesignTokens::from_json(crate::preview::TOKENS_JSON).unwrap();
        for dark in [false, true] {
            let theme = tokens.theme(dark);
            let ordinary = SettingsPalette::new(theme);
            let expected = hex_color(&theme.secondary);
            assert!((ordinary.row_hover.a - 0.4 * expected.a).abs() < 1e-5);
            let glass = SettingsPalette::with_background(theme, theme, &WindowBackgroundOptions::default());
            assert!((glass.row_hover.a - 0.08 * hex_color(&theme.text).a).abs() < 1e-5);
            assert_ne!(glass.row_hover, ordinary.row_hover);
        }
    }
}
