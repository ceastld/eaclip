//! Shared titlebar search sizing. Caption hit regions still use painted bounds.

use crate::preview::DesignTokens;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TitlebarLayout {
    pub search_width: f32,
    pub drag_width: f32,
}

/// Reserve two navigation buttons, three window buttons and the gaps between
/// navigation, search, drag area and window controls before sizing the search.
/// Below the supported window width, search yields its minimum before controls
/// overflow; this calculation does not change the product's minimum window size.
pub fn titlebar_layout(width: f32, tokens: &DesignTokens) -> TitlebarLayout {
    titlebar_layout_with_search(
        width,
        tokens,
        tokens.titlebar_search.preferred_search_width,
        tokens.titlebar_search.min_search_width,
    )
}

/// Confirmed history search: compact at rest and while focused. Favorites keep
/// the shared search tokens above (frontend `.next-titlebar--keyboard-preview`).
pub fn history_titlebar_layout(width: f32, tokens: &DesignTokens) -> TitlebarLayout {
    titlebar_layout_with_search(width, tokens, 128.0, 90.0)
}

fn titlebar_layout_with_search(
    width: f32,
    tokens: &DesignTokens,
    preferred_search_width: f32,
    min_search_width: f32,
) -> TitlebarLayout {
    let layout = &tokens.layout;
    let titlebar = tokens.titlebar_search;
    let width = if width.is_finite() {
        width.max(0.0)
    } else {
        0.0
    };
    let navigation_width = 2.0 * titlebar.button_size + layout.chip_gap;
    let window_buttons_width = 3.0 * titlebar.button_size + 2.0 * layout.chrome_button_gap;
    let available = (width
        - 2.0 * layout.title_bar_padding_x
        - navigation_width
        - window_buttons_width
        - 3.0 * layout.title_bar_gap)
        .max(0.0);
    let search_width = preferred_search_width
        .min((available - titlebar.min_drag_width).max(min_search_width))
        .min(available);
    TitlebarLayout {
        search_width,
        drag_width: (available - search_width).max(0.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preview::TOKENS_JSON;

    #[test]
    fn confirmed_titlebar_widths_keep_search_stable_and_reserve_native_drag_space() {
        let tokens = DesignTokens::from_json(TOKENS_JSON).unwrap();
        for (width, search_width, drag_width) in [
            (300.0, 96.0, 48.0),
            (320.0, 116.0, 48.0),
            (420.0, 180.0, 84.0),
            (560.0, 180.0, 224.0),
        ] {
            assert_eq!(
                titlebar_layout(width, &tokens),
                TitlebarLayout {
                    search_width,
                    drag_width
                },
                "titlebar layout at {width} DIP"
            );
        }
    }

    #[test]
    fn compact_history_search_shares_the_navigation_and_window_control_reservation() {
        let tokens = DesignTokens::from_json(TOKENS_JSON).unwrap();
        for (width, search_width, drag_width) in [
            (294.0, 90.0, 48.0),
            (300.0, 96.0, 48.0),
            (320.0, 116.0, 48.0),
            (420.0, 128.0, 136.0),
            (560.0, 128.0, 276.0),
        ] {
            let history = history_titlebar_layout(width, &tokens);
            assert_eq!(history, TitlebarLayout { search_width, drag_width }, "history at {width} DIP");
            let favorites = titlebar_layout(width, &tokens);
            assert_eq!(history.search_width + history.drag_width,
                favorites.search_width + favorites.drag_width,
                "history must preserve the same navigation/window-control reservation at {width} DIP");
        }
    }

    #[test]
    fn titlebar_layout_clamps_unsupported_and_invalid_window_widths() {
        let tokens = DesignTokens::from_json(TOKENS_JSON).unwrap();
        assert_eq!(
            titlebar_layout(240.0, &tokens),
            TitlebarLayout {
                search_width: 84.0,
                drag_width: 0.0
            }
        );
        assert_eq!(history_titlebar_layout(240.0, &tokens), TitlebarLayout {
            search_width: 84.0,
            drag_width: 0.0,
        });
        for width in [0.0, -1.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(
                titlebar_layout(width, &tokens),
                TitlebarLayout {
                    search_width: 0.0,
                    drag_width: 0.0
                },
                "titlebar layout at {width} DIP"
            );
            assert_eq!(history_titlebar_layout(width, &tokens), TitlebarLayout {
                search_width: 0.0,
                drag_width: 0.0,
            }, "history at {width}");
        }
    }
}
