use std::{ops::Range, sync::LazyLock};

use gpui::{App, TextPaintMask};
use regex::Regex;
use serde_json::Value;

pub(crate) fn configure(config: &Value, cx: &mut App) {
    let enabled = config["maskSensitiveText"].as_bool().unwrap_or(false);
    if enabled != cx.try_global::<TextPaintMask>().is_some() {
        if enabled {
            cx.set_global(TextPaintMask(sensitive_ranges));
        } else {
            cx.remove_global::<TextPaintMask>();
        }
        cx.refresh_windows();
    }
}

/// The Win32 tooltip does not pass through GPUI's text painter. Remove the
/// original glyphs before handing the display string to that renderer.
#[cfg(windows)]
pub(crate) fn mask_native_tooltip(mut text: String) -> String {
    let mut offset = 0;
    let ranges = text
        .split_inclusive('\n')
        .flat_map(|line| {
            let start = offset;
            offset += line.len();
            sensitive_ranges(line)
                .into_iter()
                .map(move |range| (start + range.start)..(start + range.end))
        })
        .collect::<Vec<_>>();
    for range in ranges.into_iter().rev() {
        let mask = "█".repeat(text[range.clone()].chars().count().min(12));
        text.replace_range(range, &mask);
    }
    text
}

// Keep labels and surrounding prose legible, including when a credential is
// too short or too repetitive to meet an entropy threshold.
static LABELED: LazyLock<Regex> = LazyLock::new(|| {
    let label = r"(?:\b(?:[a-z0-9]+[_-])*(?:password|passwd|pwd|(?:api|private|secret)[_\x20-]?key|(?:access[_\x20-]?|refresh[_\x20-]?|auth[_\x20-]?)?token|(?:client[_\x20-]?)?secret)\b|密码|口令|密钥|令牌|验证码|提取码)";
    // Query parameters have an '&' delimiter. In other assignments it can
    // be part of the password. Preserve coverage while typing an open quote.
    Regex::new(&format!(
        r#"(?ix)
        [?&]{label}=([^&\s]+)
        |{label}["']?\s*(?:[:=：]|是|为|\bis\b)\s*
         (?:"((?:\\.|[^"\\])+)|'((?:\\.|[^'\\])+)|([^\s"'，。；]+))
    "#
    ))
    .unwrap()
});

static TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i:\b(?:Bearer|Basic)\s+)([A-Za-z0-9._~+/=-]+)|\b((?:gh[pousr]_|github_pat_|sk-|glpat-|npm_|hf_)[A-Za-z0-9_-]{8,}|eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+)|(?i:\b[a-z][a-z0-9+.-]*://)[^\s/@:]+:([^\s/@]+)@").unwrap()
});

static CANDIDATE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[!-~]{6,}").unwrap());

fn sensitive_ranges(line: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    for captures in LABELED.captures_iter(line) {
        if let Some(value) = captures.iter().skip(1).flatten().next() {
            ranges.push(value.range());
        }
    }
    for captures in TOKEN.captures_iter(line) {
        if let Some(value) = captures.iter().skip(1).flatten().next() {
            ranges.push(value.range());
        }
    }
    ranges.sort_unstable_by_key(|range| range.start);
    // Only score unlabelled candidates. In a URL or JSON document, a matched
    // credential must not cause the surrounding label or URL to disappear.
    let explicit_count = ranges.len();
    let mut next_explicit = 0;
    for candidate in CANDIDATE.find_iter(line) {
        while next_explicit < explicit_count && ranges[next_explicit].end <= candidate.start() {
            next_explicit += 1;
        }
        if next_explicit < explicit_count && ranges[next_explicit].start < candidate.end() {
            continue;
        }
        if looks_random(candidate.as_str(), line.trim() == candidate.as_str()) {
            ranges.push(candidate.range());
        }
    }
    ranges.sort_unstable_by_key(|range| range.start);
    ranges.dedup_by(|next, previous| {
        if next.start <= previous.end {
            previous.end = previous.end.max(next.end);
            true
        } else {
            false
        }
    });
    ranges
}

/// Empirical Shannon entropy in bits per ASCII character is a visual heuristic.
/// Character classes and length distinguish short generated strings from words.
fn looks_random(token: &str, standalone: bool) -> bool {
    if token.contains("://") || token.contains(":\\") || shortcut(token) {
        return false;
    }
    let mut counts = [0usize; 128];
    let mut lower = 0;
    let mut upper = 0;
    let mut digits = 0;
    let mut symbols = 0;
    for byte in token.bytes() {
        counts[byte as usize] += 1;
        match byte {
            b'a'..=b'z' => lower += 1,
            b'A'..=b'Z' => upper += 1,
            b'0'..=b'9' => digits += 1,
            _ => symbols += 1,
        }
    }
    let n = token.len();
    let mixed_case = lower > 0 && upper > 0;
    let digit_transitions = token
        .as_bytes()
        .windows(2)
        .filter(|pair| pair[0].is_ascii_digit() != pair[1].is_ascii_digit())
        .count();
    let case_transitions = token
        .as_bytes()
        .windows(2)
        .filter(|pair| {
            pair.iter().all(u8::is_ascii_alphabetic)
                && pair[0].is_ascii_uppercase() != pair[1].is_ascii_uppercase()
        })
        .count();
    let threshold = if uuid_shape(token) {
        if !standalone {
            return false;
        }
        3.0
    } else if n >= 6 && digits > 0 && lower + upper > 0 && (symbols > 0 || digit_transitions >= 3) {
        if n < 8 {
            2.4
        } else {
            2.75
        }
    } else if n >= 6
        && mixed_case
        && upper >= 2
        && lower >= 2
        && (n >= 8 && digits > 0 || case_transitions >= 4)
    {
        if n < 8 {
            2.4
        } else {
            2.75
        }
    } else if n >= 6 && standalone && digits >= 2 && lower + upper >= 2 {
        if n < 8 {
            2.4
        } else {
            2.8
        }
    } else if n >= 16
        && lower + upper > 0
        && digits > 0
        && token.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        3.0
    } else if n >= 16
        && (mixed_case && upper >= n / 4 && lower >= n / 4
            || digits >= n / 4 && lower + upper >= n / 4)
    {
        3.5
    } else if n >= 16 && lower + upper >= n / 2 {
        if standalone {
            3.3
        } else {
            3.7
        }
    } else {
        return false;
    };
    let entropy: f64 = counts
        .into_iter()
        .filter(|&count| count != 0)
        .map(|count| {
            let p = count as f64 / n as f64;
            -p * p.log2()
        })
        .sum();
    entropy >= threshold
}

fn shortcut(token: &str) -> bool {
    let Some((modifiers, key)) = token.rsplit_once('+') else {
        return false;
    };
    !key.is_empty()
        && key.bytes().all(|b| b.is_ascii_alphanumeric())
        && modifiers.split('+').all(|part| {
            ["Ctrl", "Alt", "Shift", "Win"]
                .iter()
                .any(|name| part.eq_ignore_ascii_case(name))
        })
}

fn uuid_shape(token: &str) -> bool {
    token.len() == 36
        && token.bytes().enumerate().all(|(i, byte)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn native_tooltip_masks_credential_values_and_preserves_surrounding_text() {
        let text = "登录说明\r\n密码是 abc\nMore details at https://example.com/help";
        assert_eq!(
            mask_native_tooltip(text.into()),
            "登录说明\r\n密码是 ███\nMore details at https://example.com/help",
        );
    }

    fn matches(line: &str) -> Vec<&str> {
        sensitive_ranges(line)
            .into_iter()
            .map(|range| &line[range])
            .collect()
    }

    #[test]
    fn masks_standalone_and_embedded_generated_passwords() {
        assert_eq!(matches("测试用：aZ7!qR2#；其他内容"), ["aZ7!qR2#"]);
        assert_eq!(matches("Q9dK7bZ2"), ["Q9dK7bZ2"]);
        assert_eq!(matches("one Q9vL2kX7aB3m end"), ["Q9vL2kX7aB3m"]);
        assert_eq!(matches("aBzRnTqMkLpHsWcY"), ["aBzRnTqMkLpHsWcY"]);
        assert_eq!(
            matches("1ac07bd53482fe60a67cb903"),
            ["1ac07bd53482fe60a67cb903"]
        );
    }

    #[test]
    fn masks_short_and_fewer_class_random_strings() {
        for secret in [
            "Q7m2x9",
            "abc123",
            "aZqRkM",
            "v7r2m9k4",
            "QmRxZaVt",
            "marmot4927",
            "mrxqvznktsdbhfjl",
            "372960481532706a",
            "550e8400-e29b-41d4-a716-446655440000",
        ] {
            assert_eq!(matches(secret), [secret]);
        }
        assert_eq!(matches("短密码 v7r2m9k4 结束"), ["v7r2m9k4"]);
        assert_eq!(matches("记录 abcdeABCDEFGHabc 结束"), ["abcdeABCDEFGHabc"]);
        assert!(matches("Meeting notes for November2026").is_empty());
    }

    #[test]
    fn masks_natural_language_environment_and_url_credentials() {
        for (line, expected) in [
            ("密码是 123456", "123456"),
            ("提取码：ab12", "ab12"),
            ("验证码为 391704", "391704"),
            ("Password is tiny", "tiny"),
            ("API Key: tiny", "tiny"),
            ("密码是 abc，可以登录", "abc"),
            (r#"OPENAI_API_KEY="fake-key""#, "fake-key"),
            (r#"{"clientSecret":"short"}"#, "short"),
            ("https://tester:short@example.test/path", "short"),
            ("Authorization: Basic c3ludGhldGlj", "c3ludGhldGlj"),
        ] {
            assert_eq!(matches(line), [expected]);
        }
    }

    #[test]
    fn masks_only_credential_values_in_context() {
        assert_eq!(
            matches(r#"{"password":"short pass","token":"abc"}"#),
            ["short pass", "abc"]
        );
        assert_eq!(
            matches("密码：重复字 Bearer fake-token"),
            ["重复字", "fake-token"]
        );
        assert_eq!(
            matches("https://example.test/?access_token=1234&sort=date"),
            ["1234"]
        );
        assert_eq!(
            matches("https://example.test/?token=abc&refresh_token=xyz"),
            ["abc", "xyz"]
        );
        assert_eq!(
            matches("see ghp_synthetic_token now"),
            ["ghp_synthetic_token"]
        );
        assert_eq!(
            matches("token=ghp_synthetic_token"),
            ["ghp_synthetic_token"]
        );
        assert_eq!(matches("password=v7&x!G;9,m4"), ["v7&x!G;9,m4"]);
    }

    #[test]
    fn leaves_ordinary_display_text_intact() {
        for line in [
            "工作笔记 2026-11-17",
            "Meeting notes for November2026",
            "1234567890123456",
            "Aa1!Aa1!Aa1!Aa1!",
            "ClipHost settings",
            "Ctrl+Shift+F12",
            "https://example.test/releases/2026-11-17",
            r"C:\Docs\Meeting2026.txt",
            "记录 ID 550e8400-e29b-41d4-a716-446655440000",
            "ClipHost",
            "Password settings",
        ] {
            assert!(matches(line).is_empty(), "{line}");
        }
    }
}
