//! Existing PanelStyle row surfaces. Inputs are resolved runtime CSS colors;
//! the original palette and its glass projection must remain separate.

use gpui::{linear_color_stop, linear_gradient, Background, Hsla, Rgba};

use crate::{preview::ThemeTokens, view::color};

/// Resolve from the base color on each refresh, so wallpaper alpha is applied
/// exactly once and custom transparency survives the neutral selection style.
pub(super) fn glass_neutral(base: &ThemeTokens, options: &crate::window_background::WindowBackgroundOptions) -> String {
    let mut neutral: Rgba = color(&base.neutral_active).into();
    neutral.a = options.glass_alpha(neutral.a, 0.74);
    format!("#{:02X}{:02X}{:02X}{:02X}",
        (neutral.r * 255.0).round() as u8,
        (neutral.g * 255.0).round() as u8,
        (neutral.b * 255.0).round() as u8,
        (neutral.a * 255.0).round() as u8)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Style {
    Big,
    List,
    Chips,
    Icons,
}

#[derive(Clone, Copy)]
pub(super) struct RowColors {
    pub normal: Hsla,
    pub folder: Hsla,
    pub hover: Hsla,
    pub selected: Background,
}

pub(super) fn row_colors(
    base: &ThemeTokens,
    rendered: &ThemeTokens,
    style: Style,
    has_background: bool,
) -> RowColors {
    if style == Style::List {
        // Detached lists share the accepted favorites row treatment. Wallpaper
        // surfaces still use the already prepared glass projection once.
        let normal = if has_background { color(&rendered.surface) } else { Rgba::default().into() };
        return RowColors {
            normal,
            folder: normal,
            hover: color(&rendered.hover),
            selected: color(&rendered.neutral_active).into(),
        };
    }
    if has_background {
        // app.css:297-311 overrides every PanelStyle with these glass colors.
        return RowColors {
            normal: color(&rendered.surface),
            folder: css_mix(color(&rendered.surface), color(&rendered.tertiary), 0.12),
            hover: color(&rendered.hover),
            selected: color(&rendered.selected).into(),
        };
    }
    let primary = color(&base.surface);
    let secondary = color(&base.secondary);
    let tertiary = color(&base.tertiary);
    RowColors {
        normal: match style {
            Style::Big | Style::Icons => primary,
            Style::List => Rgba::default().into(),
            Style::Chips => css_mix(primary, secondary, 0.08),
        },
        folder: css_mix(
            primary,
            tertiary,
            if style == Style::List { 0.28 } else { 0.12 },
        ),
        hover: if style == Style::List {
            css_mix(primary, tertiary, 0.12)
        } else {
            secondary
        },
        // GPUI 0.2.2 supports two stops. Keep the CSS angle and endpoint colors;
        // app.css:11160's intermediate 55%/18%-accent stop cannot be expressed.
        selected: linear_gradient(
            165.0,
            linear_color_stop(css_mix(primary, color(&base.accent), 0.30), 0.0),
            linear_color_stop(css_mix(primary, color(&base.accent), 0.10), 1.0),
        ),
    }
}

/// CSS color-mix(in srgb) interpolates premultiplied alpha. GPUI's Rgba::blend
/// composites the second color over the first, so it is not this operation.
pub(crate) fn css_mix(first: Hsla, second: Hsla, second_weight: f32) -> Hsla {
    let first: Rgba = first.into();
    let second: Rgba = second.into();
    let left = first.a * (1.0 - second_weight);
    let right = second.a * second_weight;
    let alpha = left + right;
    if alpha <= 0.0 {
        return Rgba::default().into();
    }
    Rgba {
        r: (first.r * left + second.r * right) / alpha,
        g: (first.g * left + second.g * right) / alpha,
        b: (first.b * left + second.b * right) / alpha,
        a: alpha,
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preview::{DesignTokens, TOKENS_JSON};

    fn base() -> ThemeTokens {
        DesignTokens::from_json(TOKENS_JSON).unwrap().themes.light
    }

    fn assert_rgba(actual: Hsla, expected: Rgba) {
        let actual: Rgba = actual.into();
        for (actual, expected) in [
            (actual.r, expected.r),
            (actual.g, expected.g),
            (actual.b, expected.b),
            (actual.a, expected.a),
        ] {
            assert!((actual - expected).abs() < 0.00001);
        }
    }

    #[test]
    fn default_styles_keep_their_distinct_row_surfaces() {
        let mut base = base();
        base.surface = "#FF0000".into();
        base.secondary = "#00FF00".into();
        base.tertiary = "#0000FF".into();
        for style in [Style::Big, Style::List, Style::Chips, Style::Icons] {
            let row = row_colors(&base, &base, style, false);
            if style == Style::List {
                assert_eq!(row.normal, Rgba::default().into());
                assert_eq!(row.folder, row.normal);
                assert_eq!(row.hover, color(&base.hover));
                assert_eq!(row.selected, Background::from(color(&base.neutral_active)));
                continue;
            }
            let (normal, folder_blue, hover) = match style {
                Style::List => (
                    Rgba::default(),
                    0.28,
                    Rgba {
                        r: 0.88,
                        g: 0.0,
                        b: 0.12,
                        a: 1.0,
                    },
                ),
                Style::Chips => (
                    Rgba {
                        r: 0.92,
                        g: 0.08,
                        b: 0.0,
                        a: 1.0,
                    },
                    0.12,
                    Rgba {
                        r: 0.0,
                        g: 1.0,
                        b: 0.0,
                        a: 1.0,
                    },
                ),
                _ => (
                    Rgba {
                        r: 1.0,
                        g: 0.0,
                        b: 0.0,
                        a: 1.0,
                    },
                    0.12,
                    Rgba {
                        r: 0.0,
                        g: 1.0,
                        b: 0.0,
                        a: 1.0,
                    },
                ),
            };
            assert_rgba(row.normal, normal);
            assert_rgba(
                row.folder,
                Rgba {
                    r: 1.0 - folder_blue,
                    g: 0.0,
                    b: folder_blue,
                    a: 1.0,
                },
            );
            assert_rgba(row.hover, hover);
        }
    }

    #[test]
    fn prepared_background_overrides_every_style_without_refading() {
        let base = base();
        let mut rendered = base.clone();
        rendered.surface = "#FF000080".into();
        rendered.tertiary = "#0000FF40".into();
        rendered.hover = "#12345678".into();
        rendered.selected = "#ABCDEF80".into();
        for style in [Style::Big, Style::List, Style::Chips, Style::Icons] {
            let row = row_colors(&base, &rendered, style, true);
            assert_eq!(row.normal, color(&rendered.surface));
            assert_eq!(row.hover, color(&rendered.hover));
            assert_eq!(row.selected, Background::from(color(if style == Style::List {
                &rendered.neutral_active
            } else { &rendered.selected })));
            if style == Style::List {
                assert_eq!(row.folder, row.normal);
                continue;
            }
            let alpha = 128.0 / 255.0 * 0.88 + 64.0 / 255.0 * 0.12;
            assert_rgba(
                row.folder,
                Rgba {
                    r: 128.0 / 255.0 * 0.88 / alpha,
                    g: 0.0,
                    b: 64.0 / 255.0 * 0.12 / alpha,
                    a: alpha,
                },
            );
        }
    }

    #[test]
    fn transparent_mix_components_do_not_tint_visible_rgb() {
        assert_rgba(
            css_mix(color("#FF000000"), color("#0000FF80"), 0.12),
            Rgba {
                r: 0.0,
                g: 0.0,
                b: 1.0,
                a: 128.0 / 255.0 * 0.12,
            },
        );
        assert_rgba(
            css_mix(color("#FF000000"), color("#0000FF00"), 0.12),
            Rgba::default(),
        );
    }

    #[test]
    fn neutral_selection_keeps_custom_alpha_when_wallpaper_is_prepared() {
        let mut base = base();
        base.neutral_active = "#33445580".into();
        let options = crate::window_background::WindowBackgroundOptions {
            blend: 100,
            ..Default::default()
        };
        let prepared = glass_neutral(&base, &options);
        assert_eq!(prepared, "#3344550E");
        // Repeated refreshes always start with base, never the faded result.
        assert_eq!(glass_neutral(&base, &options), prepared);
    }

    #[test]
    fn selected_gradient_preserves_css_angle_and_two_supported_endpoints() {
        let mut base = base();
        base.surface = "#FF0000".into();
        base.accent = "#0000FF".into();
        let selected = row_colors(&base, &base, Style::Big, false).selected;
        let value = serde_json::to_value(selected).unwrap();
        assert_eq!(value["tag"], "LinearGradient");
        assert_eq!(value["gradient_angle_or_pattern_height"], 165.0);
        let stops = value["colors"].as_array().unwrap();
        assert_eq!(stops.len(), 2);
        assert_eq!(stops[0]["percentage"], 0.0);
        assert_eq!(stops[1]["percentage"], 1.0);
        for (stop, red, blue) in [(&stops[0], 0.70, 0.30), (&stops[1], 0.90, 0.10)] {
            let rgba: Rgba = serde_json::from_value(stop["color"].clone()).unwrap();
            // GPUI's serialized color rounds to 8-bit channels.
            assert!((rgba.r - red).abs() < 1.0 / 255.0);
            assert!((rgba.b - blue).abs() < 1.0 / 255.0);
            assert_eq!(rgba.g, 0.0);
            assert_eq!(rgba.a, 1.0);
        }
    }
}
