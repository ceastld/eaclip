//! A failure dialog needs no GPUI window, dispatcher or clipboard access.
use std::{
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

static SHOWN: AtomicBool = AtomicBool::new(false);

fn claim_dialog(shown: &AtomicBool, allowed: bool) -> bool {
    allowed
        && shown
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
}

/// Shared with the native fault filter: no allocation, logging or locks.
pub(crate) fn claim_failure_dialog() -> bool {
    claim_dialog(&SHOWN, true)
}

fn message(reason: &str, stack: Option<&str>, location: Option<&Path>, saved: bool) -> String {
    let (reason, truncated) = excerpt(reason, 900, 6);
    let mut text = format!("错误：\r\n{reason}\r\n");
    if truncated {
        text.push_str("（窗口显示错误摘要。）\r\n");
    }
    text.push_str("\r\n");
    if saved {
        if let Some(path) = location {
            text.push_str(&format!("诊断日志：{}\r\n", path.display()));
        }
    } else {
        text.push_str("诊断日志未能写入。\r\n");
        if let Some(path) = location {
            text.push_str(&format!("诊断位置：{}\r\n", path.display()));
        }
    }
    text.push_str("按 Ctrl+C 可复制本窗口内容，用于反馈错误。\r\n");
    if let Some(stack) = stack {
        text.push_str("\r\nRust 调用栈：\r\n");
        let (visible, truncated) = excerpt(stack, 2_000, 12);
        text.push_str(&visible);
        if truncated {
            text.push_str(if saved {
                "\r\n（窗口仅显示部分调用栈；完整内容见诊断日志。）"
            } else {
                "\r\n（窗口仅显示部分调用栈。）"
            });
        }
    }
    // Native MessageBox strings must not be cut short by a panic payload's NUL.
    text.replace('\0', "�")
}

fn excerpt(value: &str, max_chars: usize, max_lines: usize) -> (String, bool) {
    let lines: Vec<_> = value.lines().collect();
    let visible = lines
        .iter()
        .take(max_lines)
        .copied()
        .collect::<Vec<_>>()
        .join("\r\n");
    let text: String = visible.chars().take(max_chars).collect();
    let truncated = lines.len() > max_lines || text.len() < visible.len();
    (text, truncated)
}

pub(crate) fn show_run_failure(allowed: bool, reason: &str, location: Option<&Path>, saved: bool) {
    if allowed && claim_failure_dialog() {
        native_message_box("ClipHost 启动失败", &message(reason, None, location, saved));
    }
}

pub(crate) fn show_panic(
    allowed: bool,
    reason: &str,
    stack: Option<&str>,
    location: Option<&Path>,
    saved: bool,
) {
    if allowed && claim_failure_dialog() {
        native_message_box(
            "ClipHost 内部错误",
            &message(reason, stack, location, saved),
        );
    }
}

fn native_message_box(title: &str, text: &str) {
    #[cfg(windows)]
    {
        use std::{ffi::c_void, ptr};
        #[link(name = "user32")]
        extern "system" {
            fn MessageBoxW(
                window: *mut c_void,
                text: *const u16,
                title: *const u16,
                flags: u32,
            ) -> i32;
        }
        let title: Vec<u16> = title.encode_utf16().chain(Some(0)).collect();
        let text: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
        // MB_OK | MB_ICONERROR | MB_SETFOREGROUND. Standard MessageBox already
        // supports Ctrl+C; do not copy anything to the clipboard ourselves.
        unsafe {
            MessageBoxW(
                ptr::null_mut(),
                text.as_ptr(),
                title.as_ptr(),
                0x0000_0010 | 0x0001_0000,
            );
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (title, text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_error_reports_reason_path_and_copy_hint_without_a_handler_stack() {
        let text = message(
            "database incompatible",
            None,
            Some(Path::new("diagnostics/startup.jsonl")),
            true,
        );
        assert!(text.contains("database incompatible") && text.contains("startup.jsonl"));
        assert!(text.contains("Ctrl+C"));
        assert!(!text.contains("Rust 调用栈"));
    }

    #[test]
    fn panic_reports_available_stack_even_when_logging_failed() {
        let text = message("panic\0fixture", Some("frame 1\nframe 2"), None, false);
        assert!(text.contains("诊断日志未能写入") && text.contains("frame 1"));
        assert!(text.contains("Rust 调用栈") && !text.contains('\0'));
    }

    #[test]
    fn a_suppressed_test_failure_does_not_consume_the_only_dialog_slot() {
        let shown = AtomicBool::new(false);
        assert!(!claim_dialog(&shown, false));
        assert!(claim_dialog(&shown, true));
        assert!(!claim_dialog(&shown, true));
    }

    #[test]
    fn concurrent_failures_claim_at_most_one_dialog() {
        let shown = std::sync::Arc::new(AtomicBool::new(false));
        let workers: Vec<_> = (0..16)
            .map(|_| {
                let shown = shown.clone();
                std::thread::spawn(move || claim_dialog(&shown, true))
            })
            .collect();
        assert_eq!(
            workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .filter(|won| *won)
                .count(),
            1
        );
    }

    #[test]
    fn oversized_reason_and_stack_remain_bounded_without_claiming_a_missing_log() {
        let value = "stack frame\n".repeat(100);
        let text = message(&value, Some(&value), None, false);
        assert!(text.contains("错误摘要") && text.contains("窗口仅显示部分调用栈"));
        assert!(!text.contains("完整内容见诊断日志"));
        assert!(text.lines().count() < 30);
    }
}
