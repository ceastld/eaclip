//! 便捷粘贴的独立用法说明，不随普通字段说明收进悬停帮助。

use cliphost_core::i18n::tr;
use gpui::{div, prelude::*, px, Div, FontWeight};
use gpui_rsx::rsx;

use super::{settings_widgets as widgets, SettingsPalette};
use crate::{
    preview::{SettingsTokens, ThemeTokens},
    view::color,
};

pub(super) fn shortcuts(
    tokens: &SettingsTokens,
    theme: &ThemeTokens,
    palette: SettingsPalette,
) -> Div {
    let entries = [
        (
            tr("打开与粘贴", "Open and paste"),
            tr(
                "默认按住 Alt 并按 ` 打开预览，松开 Alt 粘贴选中项。自定义快捷键时，松开其修饰键执行；没有修饰键则松开快捷键执行。已选择快捷操作时，松键执行该操作。",
                "By default, hold Alt and press ` to preview; release Alt to paste. With a custom shortcut, release its modifier keys, or the shortcut itself if it has no modifiers. If an action is selected, release to run it.",
            ),
        ),
        (
            tr("滚轮 / ↑ / ↓ / ` / Q", "Wheel / ↑ / ↓ / ` / Q"),
            tr(
                "在浮窗内向上滚动选上一项，向下选下一项；也可按方向键。默认按住 Alt，按 ` 选下一项，按 Q 选上一项。自定义快捷键时，连按主键仍选下一项。到头后循环切换。",
                "Scroll over the overlay: up for the previous item and down for the next, or use arrow keys. By default, hold Alt: ` selects next and Q selects previous. Repeating a custom shortcut's main key also selects next. Selection wraps at the ends.",
            ),
        ),
        (
            "1 / 2",
            tr(
                "选择当前条目的主操作 / 次操作，松开保持键后执行。可用操作随条目类型变化，例如模拟输入、打开链接或本地路径；以浮窗显示为准。",
                "Select the item's primary / secondary action, then release the held trigger to run it. Actions depend on the item, such as typing text or opening a link or local path. Check the overlay for available actions.",
            ),
        ),
        (
            "D",
            tr("立即删除当前条目。", "Delete the selected item immediately."),
        ),
        (
            "E",
            tr(
                "选择“粘贴并回车”，松开保持键后先粘贴，再发送一次回车。",
                "Select Paste and Enter. Release the held trigger to paste, then send Enter.",
            ),
        ),
        (
            "F",
            tr(
                "以当前项为终点准备连续粘贴队列；本次松键默认只关闭浮窗，之后每次便捷粘贴按先复制先粘贴的顺序贴队首一条。队列范围受爆发间隔和队列上限控制，空闲超时后自动取消。",
                "Prepare a sequential paste queue ending at the selected item. Releasing now closes the overlay without pasting by default. Each later quick paste pastes one item, oldest first. Burst interval and queue limit control its range; the idle timeout cancels it.",
            ),
        ),
        (
            tr("图片贴图", "Pin an image"),
            tr(
                "图片条目可点击浮窗中的“贴图”，需要先运行 Snipaste 或 PixPin。",
                "For an image, click Pin in the overlay. Snipaste or PixPin must be running.",
            ),
        ),
        (
            "Esc",
            tr(
                "取消本次便捷粘贴，松开触发键也不会粘贴。",
                "Cancel this quick paste. Releasing the trigger afterward will not paste.",
            ),
        ),
        (
            tr("鼠标侧键 + 滚轮", "Side button + wheel"),
            tr(
                "在上方选择 X1 或 X2 后，按住该侧键并滚动以打开和切换条目；松开侧键粘贴，右键或 Esc 取消。未滚动时保留侧键原有行为。",
                "Choose X1 or X2 above, then hold it and scroll to open and select. Release the side button to paste; right-click or Esc to cancel. Without scrolling, the side button keeps its original behavior.",
            ),
        ),
    ];
    let rows = entries.into_iter().map(|(key, description)| {
        widgets::row(true, false, tokens)
            .child(rsx! {
                <div min_w_0 text_size={px(13.0)} font_weight={FontWeight::MEDIUM}
                    text_color={color(&theme.text)}>
                    {key}
                </div>
            })
            .child(widgets::description(palette).child(description))
            .into_any_element()
    });
    widgets::section(tr("快捷操作", "Quick actions"), tokens, theme)
        .child(widgets::card(rows, tokens, palette))
}
