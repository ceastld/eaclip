//! Shared settings surfaces and spacing. Callers attach controls and events.

use gpui::{div, prelude::*, px, relative, AnyElement, Div, FontWeight, SharedString};
use gpui_rsx::rsx;

use super::settings_visual::SettingsPalette;
use crate::{
    preview::{SettingsTokens, ThemeTokens},
    view::color,
};

const CARD_INSET: f32 = 4.0;

/// SettingsThemeModePicker.tsx, with the compact SettingsControls.css metrics.
pub(super) fn theme_thumbnail(variant: &str, theme: &ThemeTokens) -> Div {
    use gpui::{linear_color_stop, linear_gradient, Background};
    use crate::panel::css_mix;

    let light = color("#f3f4f6");
    let dark = color("#1f2937");
    let background: Background = match variant {
        "dark" => dark.into(),
        "auto" => linear_gradient(
            135.0,
            linear_color_stop(light, 0.5),
            linear_color_stop(dark, 0.5),
        ),
        "custom" => linear_gradient(
            135.0,
            linear_color_stop(color("#dbeafe"), 0.0),
            linear_color_stop(color("#fce7f3"), 1.0),
        ),
        _ => light.into(),
    };
    let foreground = color(&theme.text);
    let bar = css_mix(color("#9ca3af"), foreground, 0.12);
    let line = css_mix(color("#d1d5db"), foreground, 0.10);
    rsx! {
        <div w_full min_w_0 min_h={px(44.0)} flex_none
            p={px(6.0)} flex flex_col gap={px(4.0)}
            rounded={px(6.0)} bg={background}>
            <div h={px(5.0)} flex_none rounded={px(4.0)} bg={bar} />
            <div flex flex_col gap={px(4.0)}>
                <div h={px(4.0)} flex_none rounded={px(3.0)} bg={line} />
                <div w={relative(0.7)} h={px(4.0)} flex_none rounded={px(3.0)} bg={line} />
            </div>
        </div>
    }
}

/// One bounded content column serves every category and search.
pub(super) fn page_column() -> Div {
    rsx! {
        <div w_full max_w={px(760.0)} min_w_0 flex_none flex flex_col gap={px(20.0)} />
    }
}

pub(super) fn navigation_row(tokens: &SettingsTokens) -> Div {
    rsx! {
        <div relative min_h={px(tokens.sidebar_item_height)} px={px(10.0)} py={px(7.0)} flex
            items_center gap={px(9.0)} flex_none rounded={px(8.0)} text_size={px(14.0)}
            cursor_pointer />
    }
}

pub(super) fn navigation_marker(theme: &ThemeTokens) -> Div {
    rsx! {
        <div absolute left={px(0.0)} w={px(3.0)} h={px(16.0)} rounded_full
            bg={color(&theme.accent)} />
    }
}

/// Short enumerations share one track rather than separate outlined buttons.
pub(super) fn segmented(palette: SettingsPalette) -> Div {
    rsx! {
        <div flex flex_wrap min_w_0 items_center gap={px(2.0)} p={px(3.0)} rounded={px(8.0)}
            border_1 border_color={palette.control_border} bg={palette.page} />
    }
}

/// A group heading followed by a card or another settings surface.
pub(super) fn section(
    title: impl Into<SharedString>,
    tokens: &SettingsTokens,
    theme: &ThemeTokens,
) -> Div {
    let title = title.into();
    rsx! { <div w_full min_w_0 flex_none flex flex_col gap={px(8.0)} /> }
        .when(!title.is_empty(), |section| {
            section.child(
                rsx! {
                    <div text_size={px(tokens.group_title_font_size)}
                        font_weight={FontWeight::SEMIBOLD} text_color={color(&theme.text)}>
                        {title}
                    </div>
                },
            )
        })
}

/// Keep separators outside hoverable rows, including translucent backgrounds.
pub(super) fn card(
    rows: impl IntoIterator<Item = AnyElement>,
    tokens: &SettingsTokens,
    palette: SettingsPalette,
) -> Div {
    let mut card = rsx! {
        <div w_full min_w_0 flex_none flex flex_col p={px(CARD_INSET)}
            rounded={px(tokens.card_radius)} border_1
            border_color={palette.card_border} bg={palette.card} />
    };
    for (index, row) in rows.into_iter().enumerate() {
        if index > 0 {
            card = card.child(
                rsx! {
                    <div h={px(1.0)} flex_none
                        mx={px((tokens.row_padding_x - CARD_INSET).max(0.0))}
                        bg={palette.row_border} />
                },
            );
        }
        card = card.child(row);
    }
    card
}

/// The compact popover keeps its original row height and vertical padding.
pub(super) fn row(stacked: bool, compact: bool, tokens: &SettingsTokens) -> Div {
    let padding_y = if compact { tokens.row_padding_y } else { 8.0 };
    let min_height = if compact {
        tokens.control_height + 2.0 * tokens.row_padding_y
    } else {
        48.0
    };
    rsx! {
        <div w_full min_w_0 flex_none flex min_h={px(min_height)} py={px(padding_y)} />
    }
        .gap(px(if stacked { 8.0 } else { tokens.row_gap }))
        .when(!compact, |row| {
            row.px(px((tokens.row_padding_x - CARD_INSET).max(0.0)))
                .rounded(px((tokens.card_radius - CARD_INSET).max(0.0)))
        })
        .when(stacked, |row| row.flex_col())
        .when(!stacked, |row| row.items_center())
}

pub(super) fn label_column(stacked: bool) -> Div {
    rsx! {
        <div min_w_0 flex flex_col gap={px(2.0)} />
    }
        .when(stacked, |column| column.w_full().flex_none())
        .when(!stacked, |column| column.flex_1())
}

/// The caller supplies label content, help icons and tooltip handling.
pub(super) fn label(compact: bool, theme: &ThemeTokens) -> Div {
    rsx! {
        <div w_full min_w_0 flex_none flex items_center gap={px(6.0)} />
    }
        .text_size(px(if compact { 13.0 } else { 14.0 }))
        .line_height(relative(1.45))
        .text_color(color(&theme.text))
}

pub(super) fn description(palette: SettingsPalette) -> Div {
    rsx! {
        <div min_w_0 flex_none text_size={px(12.0)} line_height={px(18.0)}
            text_color={palette.description} />
    }
}

pub(super) fn note(text: impl Into<SharedString>, palette: SettingsPalette) -> Div {
    description(palette).px(px(CARD_INSET)).child(text.into())
}
