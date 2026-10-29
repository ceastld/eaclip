use chrono::{DateTime, Utc};

pub const DEFAULT_BURST_GAP_SECONDS: u32 = 45;
pub const DEFAULT_MAX_QUEUE_LENGTH: usize = 12;
pub const DEFAULT_SESSION_IDLE_SECONDS: u32 = 60;
pub const MIN_SESSION_IDLE_SECONDS: u32 = 10;
pub const MAX_SESSION_IDLE_SECONDS: u32 = 600;
pub const MIN_MAX_QUEUE_LENGTH: usize = 1;
pub const MAX_MAX_QUEUE_LENGTH: usize = 50;

#[derive(Debug, Clone)]
pub struct FifoCandidate {
    pub id: String,
    pub created_at: DateTime<Utc>,
    pub sub_index: i32,
    pub pinned: bool,
}

pub fn clamp_max_queue_length(length: usize) -> usize {
    length.clamp(MIN_MAX_QUEUE_LENGTH, MAX_MAX_QUEUE_LENGTH)
}

pub fn clamp_session_idle_seconds(seconds: u32) -> u32 {
    seconds.clamp(MIN_SESSION_IDLE_SECONDS, MAX_SESSION_IDLE_SECONDS)
}

pub fn infer_from_endpoint(
    newest_first: &[FifoCandidate],
    end_id: &str,
    burst_gap_seconds: u32,
    max_queue_length: usize,
    skip_pinned: bool,
) -> Vec<String> {
    if newest_first.is_empty() {
        return Vec::new();
    }
    let max_queue_length = clamp_max_queue_length(max_queue_length);
    let Some(end_index) = newest_first.iter().position(|c| c.id == end_id) else {
        return Vec::new();
    };
    let mut collected = Vec::new();
    collected.push(newest_first[end_index].clone());
    let mut previous = newest_first[end_index].clone();
    let gap = chrono::Duration::seconds(burst_gap_seconds as i64);
    for candidate in newest_first.iter().skip(end_index + 1) {
        if collected.len() >= max_queue_length {
            break;
        }
        if skip_pinned && candidate.pinned {
            continue;
        }
        if previous.created_at - candidate.created_at > gap {
            break;
        }
        collected.push(candidate.clone());
        previous = candidate.clone();
    }
    collected.reverse();
    collected.into_iter().map(|c| c.id).collect()
}

/// C# `FifoPasteRange.OrderSelectedAscending`.
pub fn order_selected_ascending(selected: &[FifoCandidate]) -> Vec<String> {
    let mut rows: Vec<&FifoCandidate> = selected
        .iter()
        .filter(|item| !item.id.trim().is_empty())
        .collect();
    rows.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.sub_index.cmp(&b.sub_index))
    });
    rows.into_iter().map(|item| item.id.clone()).collect()
}

/// C# `FifoPasteSession.ResolvePreviewText` (36 chars + ellipsis).
pub const HUD_PREVIEW_MAX_CHARS: usize = 36;

pub fn hud_preview_text(summary: &str, display_text: &str, title: &str) -> Option<String> {
    let raw = [summary, display_text, title]
        .into_iter()
        .map(|s| s.replace(['\r', '\n'], " ").trim().to_string())
        .find(|s| !s.is_empty())?;
    let chars: Vec<char> = raw.chars().collect();
    if chars.len() <= HUD_PREVIEW_MAX_CHARS {
        Some(raw)
    } else {
        Some(chars[..HUD_PREVIEW_MAX_CHARS].iter().collect::<String>() + "…")
    }
}

/// C# `FifoPasteSession.ShowArmedHud` message.
pub fn armed_hud_message(index: usize, total: usize, next: &str, chord: &str) -> String {
    let chord = if chord.trim().is_empty() {
        "Alt+`"
    } else {
        chord.trim()
    };
    crate::trf!("{index}/{total} · 下一项：{next} · {chord} 粘贴 · Esc 取消", "{index}/{total} · Next: {next} · {chord} to paste · Esc to cancel")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn item(id: &str, at: DateTime<Utc>, pinned: bool) -> FifoCandidate {
        item_at(id, at, 0, pinned)
    }

    fn item_at(id: &str, at: DateTime<Utc>, sub_index: i32, pinned: bool) -> FifoCandidate {
        FifoCandidate {
            id: id.into(),
            created_at: at,
            sub_index,
            pinned,
        }
    }

    #[test]
    fn infer_returns_oldest_first_through_endpoint() {
        let now = Utc.with_ymd_and_hms(2026, 8, 11, 22, 0, 0).unwrap();
        let items = [
            item("1", now, false),
            item("2", now - chrono::Duration::seconds(5), false),
            item("3", now - chrono::Duration::seconds(10), false),
            item("4", now - chrono::Duration::seconds(15), false),
        ];
        let queue = infer_from_endpoint(&items, "1", 45, 12, true);
        assert_eq!(queue, vec!["4", "3", "2", "1"]);
    }

    #[test]
    fn order_selected_ascending_is_oldest_first() {
        let now = Utc.with_ymd_and_hms(2026, 8, 11, 22, 0, 0).unwrap();
        let selected = [
            item("new", now, false),
            item("old", now - chrono::Duration::seconds(10), false),
        ];
        assert_eq!(
            order_selected_ascending(&selected),
            vec!["old".to_string(), "new".to_string()]
        );
    }

    #[test]
    fn order_selected_ascending_uses_sub_index_like_csharp() {
        let now = Utc.with_ymd_and_hms(2026, 8, 11, 22, 0, 0).unwrap();
        let selected = [
            item_at("older-b", now, 1, false),
            item_at("older-a", now, 0, false),
        ];
        assert_eq!(
            order_selected_ascending(&selected),
            vec!["older-a".to_string(), "older-b".to_string()]
        );
    }

    #[test]
    fn armed_hud_preview_and_message_match_csharp() {
        assert_eq!(hud_preview_text("fifo-a", "", ""), Some("fifo-a".into()));
        let long = "x".repeat(40);
        let preview = hud_preview_text(&long, "", "").unwrap();
        assert_eq!(preview.chars().count(), 37);
        assert!(preview.ends_with('…'));
        assert_eq!(
            armed_hud_message(1, 2, "fifo-a", ""),
            "1/2 · 下一项：fifo-a · Alt+` 粘贴 · Esc 取消"
        );
        assert_eq!(
            armed_hud_message(2, 2, "next", "Ctrl+`"),
            "2/2 · 下一项：next · Ctrl+` 粘贴 · Esc 取消"
        );
    }
}
