//! C# `Cea.Utils.Text.Layout.TryTextSplit` — unwrap PDF/paper line breaks.

use regex::Regex;
use std::sync::OnceLock;

pub fn try_text_split(text: &str) -> String {
    match text_split(text, "\n") {
        Ok(next) => next,
        Err(()) => text.to_string(),
    }
}

fn text_split(text: &str, splitter: &str) -> Result<String, ()> {
    let mut text = text.trim().to_string();
    text = empty_line_re().replace_all(&text, "").into_owned();
    text = text
        .replace('ﬁ', "fi")
        .replace('ﬃ', "ffi")
        .replace('ﬂ', "fl")
        .replace('ﬀ', "fl");
    text = eq_abbr_re().replace_all(&text, "Eq.").into_owned();
    text = strip_cjk_spaces(&text);
    text = multi_space_re().replace_all(&text, " ").into_owned();
    text = text.replace(['\u{0002}', '\u{0003}'], "-");

    let mut lines: Vec<String> = text.split('\n').map(|line| line.trim().to_string()).collect();
    if lines.is_empty() {
        return Ok(text);
    }
    if lines.iter().any(|line| line.is_empty()) {
        return Err(());
    }
    if lines.len() == 1 {
        return Ok(text);
    }

    let n = lines.len();
    let mut is_end = vec![false; n];
    let mut is_item = vec![false; n];
    let mut is_end_abbr = vec![false; n];
    let mut is_end_full = vec![false; n];
    let mut is_first_capital = vec![false; n];
    let mut is_title = vec![false; n];

    for (index, line) in lines.iter_mut().enumerate() {
        *line = line.trim().to_string();
        if line.is_empty() {
            return Err(());
        }
        is_end[index] = end_punct_re().is_match(line);
        is_item[index] = item_re().is_match(line);
        is_end_abbr[index] = end_abbr_re().is_match(line);
        is_first_capital[index] = first_capital_re().is_match(line);
        is_title[index] = title_re().is_match(line);
        let last = line.chars().last().ok_or(())?;
        let first = line.chars().next().ok_or(())?;
        is_end_full[index] = is_fullwidth(last);
        let _ = first;
    }

    let mut delim = vec![String::new(); n];
    for index in 0..n - 1 {
        delim[index] = if is_end[index] {
            if is_end_abbr[index] {
                if is_item[index + 1] {
                    splitter.to_string()
                } else {
                    " ".into()
                }
            } else {
                splitter.to_string()
            }
        } else if is_item[index + 1] {
            splitter.to_string()
        } else if is_end_full[index] {
            String::new()
        } else {
            " ".into()
        };
    }

    for index in 0..n {
        if !is_title[index] {
            continue;
        }
        if index == 0 {
            delim[index] = splitter.to_string();
        } else if index + 1 == n {
            delim[index - 1] = splitter.to_string();
        } else {
            delim[index - 1] = splitter.to_string();
            delim[index] = splitter.to_string();
        }
    }

    let items: Vec<usize> = (0..n).filter(|&i| is_item[i]).collect();
    let caps: Vec<usize> = (0..n).filter(|&i| is_first_capital[i]).collect();
    for (i, &item_pos) in items.iter().enumerate() {
        for &cap_pos in &caps {
            if item_pos >= cap_pos {
                continue;
            }
            let next_item = items.get(i + 1).copied();
            if next_item.is_some_and(|next| next <= cap_pos) {
                break;
            }
            let slice = &delim[item_pos..cap_pos];
            if slice.iter().all(|sep| sep == " ") {
                delim[cap_pos - 1] = splitter.to_string();
            }
            break;
        }
    }

    let mut refs: Vec<(i32, usize)> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if let Some(num) = ref_sequence(line) {
            refs.push((num, i));
        }
    }
    if refs.len() > 1 {
        for pair in refs.windows(2) {
            let (a, pa) = pair[0];
            let (b, pb) = pair[1];
            if b - a == 1 && pb.saturating_sub(pa) <= 8 {
                for j in pa..pb.saturating_sub(1) {
                    delim[j] = " ".into();
                }
            }
        }
    }

    let mut out = String::new();
    for (index, line) in lines.iter().enumerate() {
        out.push_str(line);
        out.push_str(&delim[index]);
    }
    Ok(out)
}

fn is_fullwidth(ch: char) -> bool {
    (ch as u32) > 0x7F
}

fn strip_cjk_spaces(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == ' '
            && index > 0
            && index + 1 < chars.len()
            && is_fullwidth(chars[index - 1])
            && is_fullwidth(chars[index + 1])
        {
            index += 1;
            continue;
        }
        out.push(chars[index]);
        index += 1;
    }
    out
}

fn ref_sequence(line: &str) -> Option<i32> {
    let rest = line.strip_prefix('[')?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if rest.get(digits.len()..)?.starts_with(']') {
        digits.parse().ok()
    } else {
        None
    }
}

fn empty_line_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m)^\s+$[\r\n]*").expect("empty line regex"))
}

fn eq_abbr_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[Ee]qs?\.\s").expect("eq regex"))
}

fn multi_space_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r" {2,}").expect("spaces regex"))
}

fn end_punct_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"[\\.:;!。！？?：\s]$"#).expect("end punct regex"))
}

fn item_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^•|^–\s|^-\s|^Chapter\s[1-9][0-9]{0,1}|^([1-9]*\.)+\d{1,2}[\.\s]|^·|^\[\d*\]\s[A-Z]|^\d\)\s|^\d）|^\d*\.\s|^\([Ii][Xx]\)|^\([Ii][Vv]\)|^\([Vv]\)|^\([Vv]?[Ii]{1,3}\)")
            .expect("item regex")
    })
}

fn end_abbr_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r" fig\.$| et al\.$| Fig\.$| Eq\.$| eq\.$| p\.$| pp\.$| Ph\.D\.$|cf\.$|Cf\.$|,\s\d{4};$|\.\s\(\d{4}\);$|[Ee]\.[Gg]\.$")
            .expect("abbr regex")
    })
}

fn first_capital_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Z]").expect("capital regex"))
}

fn title_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^[Aa][\s+]*[Bb][\s+]*[Ss][\s+]*[Tt][\s+]*[Rr][\s+]*[Aa][\s+]*[Cc][\s+]*[Tt]$|^[Aa]cknowledge?ments$|^[Rr]eferences$|^参[\s+]*考[\s+]*文[\s+]*献$|^致[\s+]*谢$|^附[\s+]*录$|^摘[\s+]*要$|^目[\s+]*录$|^[Dd]eclaration [Oo]f [Cc]ompeting [Ii]nterest$|^[Ii]ntroduction$")
            .expect("title regex")
    })
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unwraps_english_wrapped_sentence() {
        let out = try_text_split("This is a wrapped\nEnglish sentence without\nend punctuation");
        assert_eq!(out, "This is a wrapped English sentence without end punctuation");
    }

    #[test]
    fn joins_cjk_without_space() {
        let out = try_text_split("这是一段被\n硬拆的中文");
        assert_eq!(out, "这是一段被硬拆的中文");
    }

    #[test]
    fn keeps_bullet_and_sentence_breaks() {
        let out = try_text_split("First sentence.\n- item one\n- item two");
        assert!(out.contains("\n- item one\n- item two"));
    }
}
