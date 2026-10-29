//! Web wallpaper surfaces, derived from the unchanged opaque theme tokens.

use crate::{
    preview::{DesignTokens, ThemeTokens},
    window_background::WindowBackgroundOptions,
};

#[derive(Clone, Copy)]
struct Color {
    r: f64,
    g: f64,
    b: f64,
    a: f64,
}

fn parse(value: &str) -> Color {
    let hex = value.strip_prefix('#').expect("validated theme color");
    let packed = u32::from_str_radix(hex, 16).expect("validated theme color");
    let rgb = if hex.len() == 8 { packed >> 8 } else { packed };
    Color {
        r: ((rgb >> 16) & 255) as f64,
        g: ((rgb >> 8) & 255) as f64,
        b: (rgb & 255) as f64,
        a: if hex.len() == 8 {
            (packed & 255) as f64
        } else {
            255.0
        },
    }
}

fn hex(color: Color) -> String {
    format!(
        "#{:02X}{:02X}{:02X}{:02X}",
        color.r.round() as u8,
        color.g.round() as u8,
        color.b.round() as u8,
        color.a.round() as u8
    )
}

pub(crate) fn mix(first: &str, second: &str, second_weight: f64) -> String {
    let (first, second) = (parse(first), parse(second));
    let left = first.a * (1.0 - second_weight);
    let right = second.a * second_weight;
    let alpha = left + right;
    if alpha <= 0.0 {
        return "#00000000".into();
    }
    let channel = |a, b| (a * left + b * right) / alpha;
    hex(Color {
        r: channel(first.r, second.r),
        g: channel(first.g, second.g),
        b: channel(first.b, second.b),
        a: alpha,
    })
}

fn fade(value: &str, options: &WindowBackgroundOptions, weight: f32) -> String {
    let mut color = parse(value);
    color.a = f64::from(options.glass_alpha((color.a / 255.0) as f32, weight)) * 255.0;
    hex(color)
}

fn theme(base: &ThemeTokens, options: &WindowBackgroundOptions) -> ThemeTokens {
    let mut result = base.clone();
    result.background = "#00000000".into();
    result.surface = fade(&base.surface, options, 0.72);
    result.secondary = fade(&base.secondary, options, 0.68);
    result.tertiary = fade(&base.tertiary, options, 0.60);
    result.selected = fade(&mix(&base.surface, &base.accent, 0.30), options, 0.74);
    result.hover = fade(
        &mix(&base.surface, &base.border_secondary, 0.09),
        options,
        0.76,
    );
    result.search_background = fade(&base.secondary, options, 0.68);
    result.tab_active = fade(&mix(&base.tertiary, &base.accent, 0.30), options, 0.68);
    result.title_active = result.tab_active.clone();
    result
}

pub fn tokens(base: &DesignTokens, options: &WindowBackgroundOptions) -> DesignTokens {
    let mut result = base.clone();
    result.themes.light = theme(&base.themes.light, options);
    result.themes.dark = theme(&base.themes.dark, options);
    result
}

/// One shared shell tint behind title, toolbar and rows; child backgrounds are
/// translucent layers over it. Keep the original Custom alpha.
pub fn page_tint(base: &ThemeTokens, options: &WindowBackgroundOptions) -> gpui::Hsla {
    crate::view::color(&fade(&base.surface, options, 0.78))
}

pub fn primary_strong(base: &ThemeTokens, options: &WindowBackgroundOptions) -> gpui::Hsla {
    crate::view::color(&fade(&base.surface, options, 0.88))
}

pub fn titlebar_colors(
    base: &ThemeTokens,
    options: &WindowBackgroundOptions,
) -> (gpui::Hsla, gpui::Hsla) {
    let mut tint = parse(&base.text);
    tint.a *= 0.04;
    (
        crate::view::color(&hex(tint)),
        crate::view::color(&fade(&base.border, options, 0.28)),
    )
}

pub fn tree_colors(
    base: &ThemeTokens,
    options: &WindowBackgroundOptions,
) -> (gpui::Hsla, gpui::Hsla) {
    (
        crate::view::color(&fade(
            &mix(&base.secondary, &base.accent, 0.08),
            options,
            0.78,
        )),
        crate::view::color(&fade(
            &mix(&base.secondary, &base.accent, 0.22),
            options,
            0.76,
        )),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preview::TOKENS_JSON;

    #[test]
    fn surfaces_match_web_alpha_weights_without_changing_text_or_source() {
        let base = DesignTokens::from_json(TOKENS_JSON).unwrap();
        let options = WindowBackgroundOptions::default();
        let result = tokens(&base, &options);
        assert_eq!(
            parse(&result.themes.light.surface).a,
            (255.0_f64 * 0.72 * 0.405).round()
        );
        assert_eq!(
            parse(&result.themes.light.secondary).a,
            (255.0_f64 * 0.68 * 0.405).round()
        );
        assert_eq!(result.themes.light.text, base.themes.light.text);
        assert_eq!(result.themes.light.background, "#00000000");
        assert_eq!(parse(&base.themes.light.surface).a, 255.0);
    }

    #[test]
    fn custom_transparency_is_multiplied_and_zero_alpha_is_safe() {
        let options = WindowBackgroundOptions::default();
        assert_eq!(fade("#12345680", &options, 0.72), "#12345625");
        assert_eq!(mix("#12345600", "#65432100", 0.3), "#00000000");
        assert_eq!(mix("#FF000000", "#00FF0080", 0.5), "#00FF0040");
    }
}
