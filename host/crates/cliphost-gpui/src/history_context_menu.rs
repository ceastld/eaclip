//! History-specific action availability and immutable menu-source projection.
use super::*;
use crate::menu::{HistoryMenuContext, HistoryMenuPeer};
use cliphost_core::dto::ClipboardItemDto;

fn text_like(kind: &str) -> bool {
    matches!(kind, "TextClipItem" | "HtmlClipItem" | "QQRichTextItem")
}

pub(super) fn entries(
    items: &[&ClipboardItemDto],
    count: usize,
    providers: (bool, bool),
) -> Vec<MenuEntry> {
    let single = count == 1;
    let multi = count > 1;
    let first = items.first().copied();
    let has_text = items.iter().any(|item| text_like(&item.kind));
    let locate =
        first.is_some_and(|item| matches!(item.kind.as_str(), "FileClipItem" | "ImageClipItem"));
    let all_pinned = !items.is_empty() && items.iter().all(|item| item.is_top);
    let any_pinned = items.iter().any(|item| item.is_top);
    let mut entries = vec![
        MenuAction::tile(
            "paste",
            if multi {
                tr("按选择顺序粘贴", "Paste in selection order")
            } else {
                tr("粘贴", "Paste")
            },
        ),
        MenuAction::tile("copy-only", tr("复制", "Copy")),
        MenuAction::tile("favorite", tr("收藏", "Favorites")),
        MenuAction::tile(
            "pin-toggle",
            if all_pinned {
                tr("取消置顶", "Unpin")
            } else if any_pinned {
                tr("切换置顶", "Toggle pin")
            } else {
                tr("置顶", "Pin")
            },
        ),
        MenuAction::danger_tile("delete", tr("删除", "Delete")),
        MenuEntry::Separator,
    ];
    // The native normalized action aliases plain text; expose that capability once.
    if has_text {
        entries.push(MenuAction::tile(
            "paste-plain-text",
            tr("纯文本粘贴", "Paste as plain text"),
        ));
    }
    if multi {
        entries.push(MenuAction::tile(
            "reverse-paste",
            tr("逆序粘贴", "Paste in reverse order"),
        ));
    }
    entries.push(MenuAction::tile(
        "fifo-paste",
        tr("连续粘贴…", "Sequential paste…"),
    ));
    if locate {
        entries.push(MenuEntry::Separator);
        entries.push(MenuAction::tile(
            "reveal-file",
            tr("定位文件", "Locate file"),
        ));
        entries.push(MenuAction::tile(
            "reveal-folder",
            tr("定位文件夹", "Locate folder"),
        ));
    }
    entries.push(MenuEntry::Separator);
    if single {
        entries.push(MenuAction::row("edit", tr("编辑内容", "Edit content")));
    }
    entries.push(MenuAction::row(
        "lan-share",
        tr("发送到局域网", "Send to LAN"),
    ));
    if let Some(item) = first.filter(|_| single) {
        if item.kind == "ImageClipItem" {
            entries.push(MenuAction::row(
                "copy-as-gif",
                tr("复制为 GIF", "Copy as GIF"),
            ));
        }
        let text = if item.summary.is_empty() {
            &item.display_text
        } else {
            &item.summary
        };
        if text_like(&item.kind) && text.contains('\n') {
            entries.push(MenuAction::row(
                "split-lines",
                tr("按行拆分", "Split by line"),
            ));
        }
        if item.kind == "FileClipItem" && item.file_count > 1 {
            entries.push(MenuAction::row(
                "split-files",
                tr("按文件拆分", "Split by file"),
            ));
        }
    }
    let can_pin_overlay = items.iter().any(|item| {
        text_like(&item.kind) || matches!(item.kind.as_str(), "ImageClipItem" | "FileClipItem")
    });
    if can_pin_overlay && providers.0 {
        entries.push(MenuAction::row(
            "pin-snipaste",
            tr("Snipaste贴图", "Pin with Snipaste"),
        ));
    }
    if can_pin_overlay && providers.1 {
        entries.push(MenuAction::row(
            "pin-pixpin",
            tr("PixPin贴图", "Pin with PixPin"),
        ));
    }
    entries.push(MenuEntry::Separator);
    entries.push(MenuAction::row(
        "distinct",
        tr("去除重复", "Remove duplicates"),
    ));
    entries
}

pub(super) fn context(items: &[&ClipboardItemDto], count: usize) -> HistoryMenuContext {
    if count != 1 {
        return HistoryMenuContext {
            title: tr("选择顺序", "Selection order").into(),
            kind_label: cliphost_core::trf!("已选 {} 项", "{} selected", count),
            icon: "list".into(),
            count,
        };
    }
    let first = items.first().copied();
    let (kind_label, icon) = match first.map(|item| item.kind.as_str()).unwrap_or_default() {
        "FileClipItem" => (tr("文件", "Files"), "file"),
        "ImageClipItem" => (tr("图片", "Image"), "image"),
        "HtmlClipItem" | "QQRichTextItem" => (tr("富文本", "Rich text"), "text"),
        _ => (tr("文本", "Text"), "text"),
    };
    let title = first
        .and_then(|item| {
            [&item.title, &item.summary, &item.display_text]
                .into_iter()
                .find(|value| !value.trim().is_empty())
        })
        .cloned()
        .unwrap_or_default();
    HistoryMenuContext {
        title,
        kind_label: kind_label.into(),
        icon: icon.into(),
        count,
    }
}

pub(super) fn peers(value: &Value) -> Vec<HistoryMenuPeer> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|peer| {
            let id = peer["deviceId"].as_str()?;
            if id.trim().is_empty() || peer["paired"] != true || peer["supportsManualTransfer"] == false {
                return None;
            }
            Some(HistoryMenuPeer {
                id: id.into(),
                name: peer["alias"]
                    .as_str()
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or(id)
                    .into(),
                icon: match peer["deviceKind"].as_str().unwrap_or_default() {
                    "laptop" => "laptop",
                    "phone" => "phone",
                    _ => "desktop",
                }
                .into(),
                online: peer["online"] == true,
                paired: true,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cliphost_core::{dto::clip_to_dto, model::ClipRecord};

    fn item(kind: &str, pinned: bool) -> ClipboardItemDto {
        let mut dto = clip_to_dto(&ClipRecord::new_text("sample".to_owned(), "fixture"), None);
        dto.kind = kind.into();
        dto.is_top = pinned;
        dto
    }
    fn keys(entries: &[MenuEntry]) -> Vec<&'static str> {
        entries
            .iter()
            .filter_map(|entry| {
                if let MenuEntry::Action(action) = entry {
                    Some(action.key)
                } else {
                    None
                }
            })
            .collect()
    }

    #[test]
    fn mixed_selection_reveal_uses_the_actual_first_item_and_keeps_text_paste_once() {
        let text = item("TextClipItem", false);
        let file = item("FileClipItem", false);
        let text_first = keys(&entries(&[&text, &file], 2, (false, false)));
        assert!(!text_first.contains(&"reveal-file"));
        assert!(!text_first.contains(&"edit"));
        assert!(text_first.contains(&"reverse-paste"));
        assert_eq!(
            text_first
                .iter()
                .filter(|key| **key == "paste-plain-text")
                .count(),
            1
        );
        assert!(!text_first.contains(&"paste-standard-text"));
        let file_first = keys(&entries(&[&file, &text], 2, (false, false)));
        assert!(file_first.contains(&"reveal-file"));
        assert!(file_first.contains(&"reveal-folder"));
        let files = keys(&entries(&[&file], 1, (false, false)));
        assert!(!files.contains(&"paste-plain-text"));
        assert!(files.contains(&"fifo-paste"));
    }

    #[test]
    fn mixed_pin_state_and_optional_native_tools_keep_their_real_semantics() {
        let pinned = item("ImageClipItem", true);
        let plain = item("TextClipItem", false);
        for (items, label) in [
            (vec![&pinned], tr("取消置顶", "Unpin")),
            (vec![&plain], tr("置顶", "Pin")),
            (vec![&pinned, &plain], tr("切换置顶", "Toggle pin")),
        ] {
            let result = entries(&items, items.len(), (true, false));
            assert!(result.iter().any(|entry| matches!(entry, MenuEntry::Action(action) if action.key == "pin-toggle" && action.label == label)));
            assert!(keys(&result).contains(&"pin-snipaste"));
            assert!(!keys(&result).contains(&"pin-pixpin"));
        }
    }

    #[test]
    fn device_projection_keeps_offline_paired_targets_and_rejects_unpaired_or_empty_ids() {
        let result = peers(&json!([
            {"deviceId":"a","alias":"Laptop","deviceKind":"laptop","paired":true,"online":false},
            {"deviceId":"b","paired":false,"online":true},
            {"deviceId":"phone","paired":true,"online":true,"supportsManualTransfer":false},
            {"deviceId":"","paired":true},
            {"deviceId":"c","deviceKind":"future","paired":true,"online":true}
        ]));
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].id, "a");
        assert!(!result[0].online);
        assert_eq!(result[0].icon, "laptop");
        assert_eq!(result[1].name, "c");
        assert_eq!(result[1].icon, "desktop");
    }
}
