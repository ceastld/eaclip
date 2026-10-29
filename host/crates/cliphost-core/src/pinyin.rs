//! C# `PinyinConverter.GetPinyin` / `IsValidCnChar`.

use crate::pinyin_data::{DICT, GLOBAL, MULTI};

const MIN_CHAR: u32 = 19968;
const MAX_CHAR: u32 = 40959;

pub fn is_valid_cn_char(ch: char) -> bool {
    let code = ch as u32;
    code >= MIN_CHAR && code <= MAX_CHAR
}

pub fn get_pinyin(ch: char) -> String {
    let num = ch as u32;
    if num < MIN_CHAR || num > MAX_CHAR {
        return String::new();
    }
    let index1 = (num - MIN_CHAR) as usize;
    let pinyin_dict = i32::from(DICT[index1]);
    if pinyin_dict >= 0 {
        return GLOBAL[pinyin_dict as usize].to_string();
    }
    if pinyin_dict == -1 {
        return ch.to_string();
    }
    let index2 = (-pinyin_dict - 2) as usize;
    let mut multi = MULTI[index2];
    let mut parts: Vec<&str> = Vec::new();
    let mut index3 = (multi & 4095) as usize;
    while multi > 0 {
        parts.push(GLOBAL[index3]);
        multi >>= 12;
        index3 = (multi & 4095) as usize;
    }
    parts.join(" ")
}

/// The first non-empty space-separated piece of `get_pinyin(ch)`.
pub enum Primary {
    Reading(&'static str),
    Char(char),
}

/// `get_pinyin(ch).split(' ').find(|p| !p.is_empty())` without allocating.
pub fn primary_pinyin(ch: char) -> Option<Primary> {
    let num = ch as u32;
    if num < MIN_CHAR || num > MAX_CHAR {
        return None;
    }
    let first = |reading: &'static str| reading.split(' ').find(|p| !p.is_empty());
    let pinyin_dict = i32::from(DICT[(num - MIN_CHAR) as usize]);
    if pinyin_dict >= 0 {
        return first(GLOBAL[pinyin_dict as usize]).map(Primary::Reading);
    }
    if pinyin_dict == -1 {
        return (ch != ' ').then_some(Primary::Char(ch));
    }
    let mut multi = MULTI[(-pinyin_dict - 2) as usize];
    let mut index = (multi & 4095) as usize;
    while multi > 0 {
        if let Some(reading) = first(GLOBAL[index]) {
            return Some(Primary::Reading(reading));
        }
        multi >>= 12;
        index = (multi & 4095) as usize;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_chars_match_common_pinyin() {
        assert!(get_pinyin('文').split(' ').any(|p| p == "wen"));
        assert!(get_pinyin('件').split(' ').any(|p| p == "jian"));
    }
}
