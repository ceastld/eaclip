//! C# `ClipFuzzySearch` + `PinyinHelper.Search` for clip list filter/rank/highlight.

use unicode_normalization::UnicodeNormalization;

use crate::pinyin::{get_pinyin, is_valid_cn_char};

#[path = "ceabox_matcher.rs"]
pub(crate) mod ceabox_matcher;

pub const DEFAULT_MAX_PINYIN_TOKEN_SKIPS: i32 = 4;
pub const DEFAULT_MAX_TEXT_GAP_BETWEEN_CHARS: i32 = 3;
pub const DEFAULT_MAX_TEXT_LEADING_GAP: i32 = 64;
pub const DEFAULT_MAX_TEXT_AVERAGE_GAP: i32 = 3;

pub fn normalize_for_search(query: &str) -> String {
    if query.trim().is_empty() {
        return String::new();
    }
    let trimmed = query.trim();
    let fixed = try_fix_utf8_mojibake(trimmed);
    nfkc(&fixed)
}

pub fn matches(text: &str, query: &str) -> bool {
    if query.trim().is_empty() {
        return true;
    }
    if text.is_empty() {
        return false;
    }
    let t = nfkc(text);
    let q = nfkc(query.trim());
    if q.is_empty() {
        return true;
    }
    if index_of_ignore_case(&t, &q).is_some() {
        return true;
    }
    if (contains_ideograph(&t) || contains_ideograph(&q)) && is_pinyin_match_text(&t, &q) {
        return true;
    }
    // `t` is already NFKC (idempotent); only an untrimmed query needs the
    // public entry's trim-then-normalize step again.
    let trimmed = q.trim();
    if trimmed.len() == q.len() {
        gap_match_normalized(&t, &q)
    } else {
        gap_match_normalized(&t, &nfkc(trimmed))
    }
}

pub fn get_match_score(text: &str, query: &str) -> i32 {
    if query.trim().is_empty() || text.is_empty() {
        return 0;
    }
    let t = nfkc(text);
    let q = nfkc(query.trim());
    if q.is_empty() {
        return 0;
    }
    if starts_with_ignore_case(&t, &q) {
        return 100;
    }
    if let Some(direct) = index_of_ignore_case(&t, &q) {
        let t_len = utf16_len(&t).max(1);
        let late_penalty = ((direct * 30) / t_len).min(30);
        return 80 - late_penalty as i32;
    }
    let compact_latin = compact_latin_query(&q);
    if compact_latin.len() >= 2
        && is_all_ascii_letters(&compact_latin)
        && try_latin_segment_prefix(&t, &compact_latin).is_some()
    {
        return 55;
    }
    if (contains_ideograph(&t) || contains_ideograph(&q)) && is_pinyin_match_text(&t, &q) {
        return 50;
    }
    if !contains_ideograph(&t) && !contains_ideograph(&q) {
        return 0;
    }
    let tokens = build_pinyin_tokens(&t);
    let pq = if compact_latin.is_empty() {
        q.to_lowercase()
    } else {
        compact_latin.clone()
    };
    if contains_ideograph(&t) && try_strict_pinyin_token_range(&tokens, &pq).is_some() {
        return 50;
    }
    let compact_text = compact_whitespace(&q);
    if contains_ideograph(&compact_text)
        && utf16_len(&compact_text) >= 2
        && try_gap_subsequence(&t, &compact_text).is_some()
    {
        return 25;
    }
    if contains_ideograph(&t)
        && try_pinyin_token_fuzzy(&tokens, &pq, DEFAULT_MAX_PINYIN_TOKEN_SKIPS).is_some()
    {
        return 20;
    }
    0
}

pub fn gap_match_latin_and_pinyin_tokens(text: &str, query: &str) -> bool {
    gap_match_normalized(&nfkc(text), &nfkc(query.trim()))
}

fn gap_match_normalized(t: &str, q: &str) -> bool {
    let compact_latin = compact_latin_query(&q);
    if compact_latin.len() >= 2
        && is_all_ascii_letters(&compact_latin)
        && try_latin_segment_prefix(&t, &compact_latin).is_some()
    {
        return true;
    }
    let compact_text = compact_whitespace(&q);
    if contains_ideograph(&compact_text)
        && utf16_len(&compact_text) >= 2
        && try_gap_subsequence(&t, &compact_text).is_some()
    {
        return true;
    }
    if !contains_ideograph(&t) {
        return false;
    }
    let tokens = build_pinyin_tokens(&t);
    let pq = if compact_latin.is_empty() {
        q.to_lowercase()
    } else {
        compact_latin
    };
    try_strict_pinyin_token_range(&tokens, &pq).is_some()
        || try_pinyin_token_fuzzy(&tokens, &pq, DEFAULT_MAX_PINYIN_TOKEN_SKIPS).is_some()
}

/// UTF-16 ranges into `text`; `end` is exclusive.
pub fn try_get_highlight_ranges(text: &str, query: &str) -> Option<Vec<(i32, i32)>> {
    if text.trim().is_empty() || query.trim().is_empty() {
        return None;
    }
    let q_raw = query.trim();
    if q_raw.is_empty() {
        return None;
    }
    if let Some(direct) = index_of_ignore_case(text, q_raw) {
        return Some(vec![(direct as i32, (direct + utf16_len(q_raw)) as i32)]);
    }
    let t = nfkc(text);
    let q = nfkc(q_raw);
    if q.is_empty() {
        return None;
    }
    if let Some(direct) = index_of_ignore_case(&t, &q) {
        if let Some(mapped) = map_normalized_range_to_original(text, &t, direct, direct + utf16_len(&q))
        {
            return Some(vec![mapped]);
        }
        if t == text {
            return Some(vec![(direct as i32, (direct + utf16_len(&q)) as i32)]);
        }
        return None;
    }
    let form_kc_unchanged = t == text;
    let compact_latin = compact_latin_query(&q);
    if compact_latin.len() >= 2 && is_all_ascii_letters(&compact_latin) {
        if let Some(hits) = try_latin_segment_prefix(&t, &compact_latin) {
            let ranges = merge_ranges(hits.into_iter().map(|j| (j as i32, j as i32 + 1)).collect());
            return finalize_ranges(text, &t, form_kc_unchanged, ranges);
        }
    }
    let compact_text = compact_whitespace(&q);
    if contains_ideograph(&compact_text) && utf16_len(&compact_text) >= 2 {
        if let Some(hits) = try_gap_subsequence(&t, &compact_text) {
            let ranges = merge_ranges(hits.into_iter().map(|j| (j as i32, j as i32 + 1)).collect());
            return finalize_ranges(text, &t, form_kc_unchanged, ranges);
        }
    }
    if !contains_ideograph(&t) {
        return None;
    }
    let pq = if compact_latin.is_empty() {
        q.to_lowercase()
    } else {
        compact_latin
    };
    let tokens = build_pinyin_tokens(&t);
    if let Some((start, length)) = try_strict_pinyin_token_range(&tokens, &pq) {
        return finalize_ranges(
            text,
            &t,
            form_kc_unchanged,
            vec![(start as i32, (start + length) as i32)],
        );
    }
    if let Some(token_hits) = try_pinyin_token_fuzzy(&tokens, &pq, DEFAULT_MAX_PINYIN_TOKEN_SKIPS) {
        let mut ranges = Vec::new();
        for ti in token_hits {
            if ti >= 0 && (ti as usize) < tokens.len() {
                let idx = tokens[ti as usize].index;
                ranges.push((idx as i32, idx as i32 + 1));
            }
        }
        return finalize_ranges(text, &t, form_kc_unchanged, merge_ranges(ranges));
    }
    None
}

/// C# `PinyinHelper.Search`.
pub fn pinyin_search(search: &str, texts: &[&str]) -> bool {
    if search.trim().is_empty() {
        return true;
    }
    let keys: Vec<String> = search
        .replace('\'', "")
        .split(' ')
        .filter(|k| !k.is_empty())
        .map(str::to_string)
        .collect();
    texts.iter().filter(|t| !t.trim().is_empty()).any(|text| {
        keys.iter()
            .all(|k| is_pinyin_match_or_contains(text, k))
    })
}

pub fn contains_ideograph(text: &str) -> bool {
    text.chars().any(|ch| {
        let code = ch as u32;
        (0x4E00..=0x9FFF).contains(&code)
            || (0x3400..=0x4DBF).contains(&code)
            || (0xF900..=0xFAFF).contains(&code)
    })
}

fn is_pinyin_match_or_contains(text: &str, search: &str) -> bool {
    if index_of_ignore_case(text, search).is_some() {
        return true;
    }
    if (contains_ideograph(text) || contains_ideograph(search))
        && is_pinyin_match_encoded(&get_pinyin_match_string(text), search)
    {
        return true;
    }
    gap_match_latin_and_pinyin_tokens(text, search)
}

fn is_pinyin_match_text(text: &str, search: &str) -> bool {
    is_pinyin_match_encoded(&get_pinyin_match_string(text), search)
}

fn get_pinyin_match_string(chinese: &str) -> String {
    let mut out = String::with_capacity(chinese.len() * 4);
    let mut flag1 = false;
    let mut flag2 = false;
    for ch in chinese.chars() {
        if is_valid_cn_char(ch) {
            if !out.is_empty() && !flag2 {
                out.push(' ');
            }
            let pinyin = get_pinyin(ch);
            if !pinyin.is_empty() {
                out.push_str(&pinyin.replace(' ', "|"));
                out.push('|');
            }
            out.push(ch);
            flag1 = true;
            out.push(' ');
        } else {
            if flag1 {
                out.push(' ');
                flag2 = true;
            }
            if ch == ' ' {
                if !flag2 {
                    out.push(' ');
                    flag2 = true;
                }
            } else {
                for lower in ch.to_lowercase() {
                    out.push(lower);
                }
                flag2 = false;
            }
            flag1 = false;
        }
    }
    out.trim_end_matches(' ').to_string()
}

fn is_pinyin_match_encoded(pinyin: &str, search: &str) -> bool {
    if search.is_empty() {
        return false;
    }
    let search = search.to_lowercase();
    let units: Vec<u16> = pinyin.encode_utf16().collect();
    let mut word_start = [0i32; 30];
    let mut word_count = 0usize;
    let mut flag = true;
    for (index, unit) in units.iter().enumerate() {
        if word_count >= 30 {
            break;
        }
        if flag && *unit != u16::from(b' ') {
            word_start[word_count] = index as i32;
            word_count += 1;
        }
        flag = *unit == u16::from(b' ');
    }
    is_multi_char_match_any_part(&units, &search.encode_utf16().collect::<Vec<_>>(), &word_start, word_count)
}

fn get_match_char_count(pinyin: &[u16], search: &[u16], pinyin_start: i32, search_start: i32) -> i32 {
    let mut num1 = 0i32;
    let mut index1 = pinyin_start as usize;
    let mut index2 = search_start as usize;
    let mut num2 = 0i32;
    let mut flag = true;
    while index1 < pinyin.len() && pinyin[index1] != u16::from(b' ') && index2 < search.len() {
        if pinyin[index1] == u16::from(b'|') {
            if num2 > num1 {
                num1 = num2;
                num2 = 0;
            }
            flag = true;
            index2 = search_start as usize;
        } else if flag {
            if pinyin[index1] == search[index2] {
                index2 += 1;
                num2 += 1;
            } else {
                flag = false;
            }
        }
        index1 += 1;
    }
    if num2 > num1 {
        num1 = num2;
    }
    num1
}

fn is_multi_char_match_any_part(
    pinyin: &[u16],
    search: &[u16],
    word_start: &[i32; 30],
    word_count: usize,
) -> bool {
    let mut search_start = 0i32;
    let mut index1 = 0usize;
    let mut num1 = 0i32;
    let mut num2 = -1i32;
    while index1 < word_count && (search_start as usize) < search.len() {
        let match_count = get_match_char_count(pinyin, search, word_start[index1], search_start);
        if match_count > 0 {
            num1 = match_count;
            search_start += match_count;
            if num2 < 0 {
                num2 = index1 as i32;
            }
            index1 += 1;
        } else if search_start == 0 {
            index1 += 1;
        } else {
            let mut found = false;
            for index2 in 1..num1.saturating_sub(1) {
                let match_count2 =
                    get_match_char_count(pinyin, search, word_start[index1], search_start - index2);
                if match_count2 > 0 {
                    num1 = match_count2;
                    search_start = search_start - index2 + match_count2;
                    found = true;
                    break;
                }
            }
            if found {
                index1 += 1;
            } else {
                index1 = (num2 + 1) as usize;
                num2 = -1;
                num1 = 0;
                search_start = 0;
            }
        }
    }
    search_start as usize == search.len()
}

fn nfkc(text: &str) -> String {
    text.nfkc().collect()
}

fn try_fix_utf8_mojibake(text: &str) -> String {
    if contains_ideograph(text) {
        return text.to_string();
    }
    let mut bytes = Vec::with_capacity(text.len());
    for ch in text.chars() {
        let v = ch as u32;
        if v > 255 {
            return text.to_string();
        }
        bytes.push(v as u8);
    }
    match String::from_utf8(bytes) {
        Ok(decoded) if contains_ideograph(&decoded) => decoded,
        _ => text.to_string(),
    }
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

fn encode_utf16(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

fn to_upper_u16(unit: u16) -> u16 {
    if unit < 128 {
        if (b'a'..=b'z').contains(&(unit as u8)) {
            unit - 32
        } else {
            unit
        }
    } else {
        char::from_u32(unit as u32)
            .and_then(|ch| ch.to_uppercase().next())
            .map(|ch| ch as u32 as u16)
            .unwrap_or(unit)
    }
}

fn units_eq_ignore_case(a: &[u16], b: &[u16]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| to_upper_u16(*x) == to_upper_u16(*y))
}

fn index_of_ignore_case(hay: &str, needle: &str) -> Option<usize> {
    let h = encode_utf16(hay);
    let n = encode_utf16(needle);
    if n.is_empty() {
        return Some(0);
    }
    if h.len() < n.len() {
        return None;
    }
    for i in 0..=h.len() - n.len() {
        if units_eq_ignore_case(&h[i..i + n.len()], &n) {
            return Some(i);
        }
    }
    None
}

fn starts_with_ignore_case(hay: &str, needle: &str) -> bool {
    let h = encode_utf16(hay);
    let n = encode_utf16(needle);
    h.len() >= n.len() && units_eq_ignore_case(&h[..n.len()], &n)
}

fn compact_latin_query(query: &str) -> String {
    query
        .chars()
        .filter(|ch| *ch != ' ' && *ch != '\'')
        .flat_map(char::to_lowercase)
        .collect()
}

fn compact_whitespace(query: &str) -> String {
    query.chars().filter(|ch| !ch.is_whitespace()).collect()
}

fn is_all_ascii_letters(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|ch| ('a'..='z').contains(&ch))
}

fn is_latin_alnum_u16(unit: u16) -> bool {
    matches!(unit, 0x30..=0x39 | 0x41..=0x5A | 0x61..=0x7A)
}

fn try_latin_segment_prefix(haystack: &str, query_lower: &str) -> Option<Vec<i32>> {
    let hay = encode_utf16(haystack);
    let query: Vec<u16> = query_lower.encode_utf16().collect();
    let segments = build_latin_segments(&hay);
    if segments.is_empty() || query.is_empty() {
        return None;
    }
    for start_seg in 0..segments.len() {
        let mut qi = 0usize;
        let mut path = Vec::with_capacity(query.len());
        for si in start_seg..segments.len() {
            if qi >= query.len() {
                break;
            }
            let (seg_start, seg_len) = segments[si];
            let mut consumed = 0usize;
            while consumed < seg_len && qi + consumed < query.len() {
                let mut h = hay[seg_start + consumed];
                if (b'A'..=b'Z').contains(&(h as u8)) && h < 128 {
                    h += 32;
                }
                if h != query[qi + consumed] {
                    break;
                }
                consumed += 1;
            }
            if consumed == 0 {
                break;
            }
            for k in 0..consumed {
                path.push((seg_start + k) as i32);
            }
            qi += consumed;
        }
        if qi >= query.len() {
            return Some(path);
        }
    }
    None
}

fn build_latin_segments(hay: &[u16]) -> Vec<(usize, usize)> {
    let mut segments = Vec::new();
    let mut i = 0;
    while i < hay.len() {
        if !is_latin_alnum_u16(hay[i]) {
            i += 1;
            continue;
        }
        let start = i;
        i += 1;
        while i < hay.len() && is_latin_alnum_u16(hay[i]) {
            i += 1;
        }
        segments.push((start, i - start));
    }
    segments
}

fn char_match_ignore_case(hay: u16, needle: u16) -> bool {
    to_upper_u16(hay) == to_upper_u16(needle)
}

fn try_gap_subsequence(haystack: &str, query: &str) -> Option<Vec<i32>> {
    let hay = encode_utf16(haystack);
    let q = encode_utf16(query);
    let m = q.len();
    let n = hay.len();
    if m == 0 || n == 0 {
        return None;
    }
    let max_gap = DEFAULT_MAX_TEXT_GAP_BETWEEN_CHARS as usize;
    let max_lead = DEFAULT_MAX_TEXT_LEADING_GAP as usize;
    let mut reachable = vec![false; n];
    let mut parent = vec![i32::MIN; m * n];
    for j in 0..n {
        if char_match_ignore_case(hay[j], q[0]) && j <= max_lead {
            reachable[j] = true;
            parent[j] = -1;
        }
    }
    for qi in 1..m {
        let mut next = vec![false; n];
        for j_prev in 0..n {
            if !reachable[j_prev] {
                continue;
            }
            let hi = (n - 1).min(j_prev + max_gap + 1);
            for j in (j_prev + 1)..=hi {
                if !char_match_ignore_case(hay[j], q[qi]) || next[j] {
                    continue;
                }
                next[j] = true;
                parent[qi * n + j] = j_prev as i32;
            }
        }
        reachable = next;
    }
    let max_span = m + DEFAULT_MAX_TEXT_AVERAGE_GAP as usize * m.saturating_sub(1);
    let mut end_j = None;
    for j in 0..n {
        if !reachable[j] {
            continue;
        }
        let mut j_cur = j as i32;
        let mut first = j as i32;
        let mut ok = true;
        for qi in (1..m).rev() {
            j_cur = parent[qi * n + j_cur as usize];
            if j_cur < 0 {
                ok = false;
                break;
            }
            first = j_cur;
        }
        if !ok {
            continue;
        }
        let span = j as i32 - first + 1;
        if span as usize > max_span {
            continue;
        }
        end_j = Some(j);
        break;
    }
    let end_j = end_j?;
    let mut rev = Vec::with_capacity(m);
    let mut j_walk = end_j as i32;
    for qi in (0..m).rev() {
        rev.push(j_walk);
        if qi > 0 {
            j_walk = parent[qi * n + j_walk as usize];
            if j_walk < 0 {
                return None;
            }
        }
    }
    rev.reverse();
    Some(rev)
}

struct PinyinToken {
    index: usize,
    // The matching cursors and returned ranges follow C# UTF-16 indices. Keep
    // the units once per token instead of allocating them on every DFS visit.
    key: TokenKey,
}

/// A token's UTF-16 key. Pinyin syllables and lowercased characters fit inline,
/// so building tokens for a long text does not allocate per character.
enum TokenKey {
    Inline(u8, [u16; 8]),
    Heap(Vec<u16>),
}

impl TokenKey {
    fn from_chars(chars: impl Iterator<Item = char>) -> Self {
        let (mut len, mut units) = (0usize, [0u16; 8]);
        let mut heap: Option<Vec<u16>> = None;
        let mut buffer = [0u16; 2];
        for ch in chars {
            for unit in ch.encode_utf16(&mut buffer).iter() {
                match &mut heap {
                    Some(heap) => heap.push(*unit),
                    None if len < units.len() => {
                        units[len] = *unit;
                        len += 1;
                    }
                    None => {
                        let mut spilled = units.to_vec();
                        spilled.push(*unit);
                        heap = Some(spilled);
                    }
                }
            }
        }
        match heap {
            Some(heap) => Self::Heap(heap),
            None => Self::Inline(len as u8, units),
        }
    }
}

impl std::ops::Deref for TokenKey {
    type Target = [u16];
    fn deref(&self) -> &[u16] {
        match self {
            Self::Inline(len, units) => &units[..*len as usize],
            Self::Heap(units) => units,
        }
    }
}

fn build_pinyin_tokens(text: &str) -> Vec<PinyinToken> {
    let units = encode_utf16(text);
    let mut tokens = Vec::with_capacity(units.len());
    let mut i = 0usize;
    while i < units.len() {
        let ch = utf16_char_at(&units, i);
        let unit_len = if (0xD800..=0xDBFF).contains(&units[i]) && i + 1 < units.len() {
            2
        } else {
            1
        };
        if ch.map(char::is_whitespace).unwrap_or(false) {
            i += unit_len;
            continue;
        }
        // First non-empty reading of `get_pinyin`, lowercased.
        if let Some(primary) = ch.filter(|c| is_valid_cn_char(*c)).and_then(crate::pinyin::primary_pinyin) {
            let key = match primary {
                crate::pinyin::Primary::Reading(reading) => TokenKey::from_chars(reading.chars().flat_map(char::to_lowercase)),
                crate::pinyin::Primary::Char(c) => TokenKey::from_chars(c.to_lowercase()),
            };
            tokens.push(PinyinToken { index: i, key });
            i += unit_len;
            continue;
        }
        let key = match ch {
            Some(c) => TokenKey::from_chars(c.to_lowercase()),
            None => TokenKey::from_chars(String::from_utf16_lossy(&units[i..i + unit_len]).chars()),
        };
        tokens.push(PinyinToken { index: i, key });
        i += unit_len;
    }
    tokens
}

fn utf16_char_at(units: &[u16], i: usize) -> Option<char> {
    char::decode_utf16(units[i..].iter().copied()).next()?.ok()
}

fn get_matched_prefix_length(key: &[u16], query: &[u16], query_start: usize) -> usize {
    if query_start >= query.len() {
        return 0;
    }
    key.iter().zip(&query[query_start..]).take_while(|(a, b)| a == b).count()
}

fn try_strict_pinyin_token_range(tokens: &[PinyinToken], query_lower: &str) -> Option<(usize, usize)> {
    if tokens.is_empty() {
        return None;
    }
    let query = encode_utf16(query_lower);
    let q_len = query.len();
    for s in 0..tokens.len() {
        let mut q_index = 0usize;
        let mut end = s as i32 - 1;
        for index in s..tokens.len() {
            if q_index >= q_len {
                break;
            }
            let consume = get_matched_prefix_length(&tokens[index].key, &query, q_index);
            if consume == 0 {
                break;
            }
            q_index += consume;
            end = index as i32;
        }
        if q_index == q_len && end >= s as i32 {
            let start_pos = tokens[s].index;
            let end_pos = tokens[end as usize].index;
            return Some((start_pos, end_pos - start_pos + 1));
        }
    }
    None
}

fn try_pinyin_token_fuzzy(
    tokens: &[PinyinToken],
    query_lower: &str,
    max_skips: i32,
) -> Option<Vec<i32>> {
    /// Whether a state can complete depends only on (token, query offset, skip
    /// streak), not on the path that reached it. Remembering failed states keeps
    /// the same first path while making long non-matching texts polynomial.
    struct Search<'a> {
        tokens: &'a [PinyinToken],
        query: &'a [u16],
        max_skips: i32,
        failed: Vec<bool>,
    }
    impl Search<'_> {
        fn state(&self, ti: usize, qi: usize, skip_streak: i32) -> usize {
            (ti * (self.query.len() + 1) + qi) * (self.max_skips as usize + 1) + skip_streak as usize
        }
        fn dfs(&mut self, ti: usize, qi: usize, skip_streak: i32, path: &mut Vec<i32>) -> bool {
            if qi >= self.query.len() {
                return true;
            }
            if ti >= self.tokens.len() {
                return false;
            }
            let state = self.state(ti, qi, skip_streak);
            if self.failed[state] {
                return false;
            }
            let consume = get_matched_prefix_length(&self.tokens[ti].key, self.query, qi);
            if consume > 0 {
                path.push(ti as i32);
                if self.dfs(ti + 1, qi + consume, 0, path) {
                    return true;
                }
                path.pop();
            }
            if skip_streak < self.max_skips && self.dfs(ti + 1, qi, skip_streak + 1, path) {
                return true;
            }
            self.failed[state] = true;
            false
        }
    }

    let query = encode_utf16(query_lower);
    let max_skips = max_skips.max(0);
    let mut search = Search {
        tokens,
        query: &query,
        max_skips,
        failed: vec![false; tokens.len() * (query.len() + 1) * (max_skips as usize + 1)],
    };
    for start in 0..tokens.len() {
        let c0 = get_matched_prefix_length(&tokens[start].key, &query, 0);
        if c0 == 0 {
            continue;
        }
        let mut path = vec![start as i32];
        if search.dfs(start + 1, c0, 0, &mut path) {
            return Some(path);
        }
    }
    None
}

fn merge_ranges(mut input: Vec<(i32, i32)>) -> Vec<(i32, i32)> {
    if input.len() <= 1 {
        return input;
    }
    input.sort_by_key(|r| r.0);
    let mut merged = Vec::new();
    let (mut cs, mut ce) = input[0];
    for &(s, e) in input.iter().skip(1) {
        if s <= ce {
            ce = ce.max(e);
        } else {
            merged.push((cs, ce));
            cs = s;
            ce = e;
        }
    }
    merged.push((cs, ce));
    merged
}

fn finalize_ranges(
    original: &str,
    normalized: &str,
    form_kc_unchanged: bool,
    ranges: Vec<(i32, i32)>,
) -> Option<Vec<(i32, i32)>> {
    if form_kc_unchanged || ranges.is_empty() {
        return if ranges.is_empty() { None } else { Some(ranges) };
    }
    let mut mapped = Vec::new();
    for (s, e) in ranges {
        if let Some(pair) = map_normalized_range_to_original(original, normalized, s as usize, e as usize) {
            mapped.push(pair);
        }
    }
    let merged = merge_ranges(mapped);
    if merged.is_empty() {
        None
    } else {
        Some(merged)
    }
}

fn map_normalized_range_to_original(
    original: &str,
    normalized: &str,
    norm_start: usize,
    norm_end: usize,
) -> Option<(i32, i32)> {
    let orig_u = encode_utf16(original);
    let norm_u = encode_utf16(normalized);
    if norm_start >= norm_end || norm_end > norm_u.len() {
        return None;
    }
    if orig_u == norm_u {
        return Some((norm_start as i32, norm_end as i32));
    }
    let slice = String::from_utf16_lossy(&norm_u[norm_start..norm_end]);
    if let Some(direct) = index_of_ignore_case(original, &slice) {
        return Some((direct as i32, (direct + utf16_len(&slice)) as i32));
    }
    let mut norm_pos = 0usize;
    let mut orig_pos = 0usize;
    let mut mapped_start: Option<usize> = None;
    while orig_pos < orig_u.len() && norm_pos < norm_u.len() {
        let orig_unit_len =
            if (0xD800..=0xDBFF).contains(&orig_u[orig_pos]) && orig_pos + 1 < orig_u.len() {
                2
            } else {
                1
            };
        let orig_unit = String::from_utf16_lossy(&orig_u[orig_pos..orig_pos + orig_unit_len]);
        let expanded: String = orig_unit.nfkc().collect();
        let expanded_u = encode_utf16(&expanded);
        if expanded_u.is_empty()
            || norm_pos + expanded_u.len() > norm_u.len()
            || orig_u_slice_ne(&norm_u[norm_pos..norm_pos + expanded_u.len()], &expanded_u)
        {
            return None;
        }
        let next_norm = norm_pos + expanded_u.len();
        if mapped_start.is_none() && next_norm > norm_start {
            mapped_start = Some(orig_pos);
        }
        if let Some(start) = mapped_start {
            if next_norm >= norm_end {
                let end = orig_pos + orig_unit_len;
                return if end > start {
                    Some((start as i32, end as i32))
                } else {
                    None
                };
            }
        }
        norm_pos = next_norm;
        orig_pos += orig_unit_len;
    }
    None
}

fn orig_u_slice_ne(a: &[u16], b: &[u16]) -> bool {
    a != b
}

#[cfg(test)]
mod tests {
    use super::*;

    const LONG_SETTINGS: &str = "Settings page documentation: Configure the application options, reset defaults, open panel preferences, and manage clipboard sync settings for your workspace environment. Additional notes about how to customize theme colors and keyboard shortcuts.";

    #[test]
    fn matches_rejects_sparse_latin_acronyms() {
        for q in ["cfg", "pnl", "wsp", "thm", "kb"] {
            assert!(!matches(LONG_SETTINGS, q), "{q}");
        }
    }

    #[test]
    fn matches_keeps_substring_and_segment_prefix() {
        assert!(matches(LONG_SETTINGS, "clipboard"));
        assert!(matches(LONG_SETTINGS, "settings"));
        assert!(matches(LONG_SETTINGS, "sync"));
        assert!(matches("quicker-action", "quac"));
        assert!(matches("Quicker Action", "qa"));
        assert!(matches("options panel", "opt"));
    }

    #[test]
    fn matches_keeps_tight_cjk_gap() {
        assert!(matches("调用这个方法来处理数据", "调用方法"));
        assert!(matches("调用这个方法来处理数据", "调方"));
        assert!(!matches("调用这个方法来处理数据", "调数"));
    }

    #[test]
    fn get_match_score_prefers_prefix() {
        let prefix = get_match_score("clipboard sync notes", "clip");
        let late = get_match_score(
            "long preamble about unrelated topics before clipboard appears",
            "clip",
        );
        assert!(prefix > late);
        assert!(prefix >= 100);
    }

    #[test]
    fn matches_chinese_pinyin_initials_and_full() {
        assert!(matches("文件", "wj"));
        assert!(matches("文件", "wenjian"));
        assert!(matches("打开文件管理器", "wj"));
        assert!(matches("打开文件管理器", "wenjian"));
        assert!(pinyin_search("wj", &["文件"]));
        assert!(pinyin_search("wenjian", &["请打开这个文件再保存"]));
        assert!(matches("剪贴板设置说明", "jtbsz"));
        assert!(matches("剪贴板设置说明", "jiantieban"));
        assert!(matches("文档和软件", "wj"));
        assert!(matches("我把那个文件发给你了", "wj"));
    }

    #[test]
    fn pinyin_ranges_keep_utf16_offsets_and_strict_match_precedence() {
        assert_eq!(try_get_highlight_ranges("🙂文件", "wj"), Some(vec![(2, 4)]));
        // A later contiguous match still precedes an earlier fuzzy match.
        assert_eq!(try_get_highlight_ranges("文🙂件文件", "wj"), Some(vec![(4, 6)]));
        assert_eq!(get_match_score("文🙂件文件", "wj"), 50);
        // When only fuzzy paths exist, retain the first path and its gaps.
        assert_eq!(
            try_get_highlight_ranges("文abc件文abc件", "wj"),
            Some(vec![(0, 1), (4, 5)])
        );
        assert_eq!(get_match_score("文abc件文abc件", "wj"), 20);
    }

    /// The pre-memoization search, kept to prove identical first paths.
    fn fuzzy_reference(tokens: &[PinyinToken], query_lower: &str, max_skips: i32) -> Option<Vec<i32>> {
        fn dfs(tokens: &[PinyinToken], query: &[u16], ti: usize, qi: usize, skip: i32, max: i32,
            path: &mut Vec<i32>, found: &mut Option<Vec<i32>>) -> bool {
            if qi >= query.len() { *found = Some(path.clone()); return true; }
            if ti >= tokens.len() { return false; }
            let consume = get_matched_prefix_length(&tokens[ti].key, query, qi);
            if consume > 0 {
                path.push(ti as i32);
                if dfs(tokens, query, ti + 1, qi + consume, 0, max, path, found) { return true; }
                path.pop();
            }
            skip < max && dfs(tokens, query, ti + 1, qi, skip + 1, max, path, found)
        }
        let query = encode_utf16(query_lower);
        for start in 0..tokens.len() {
            let c0 = get_matched_prefix_length(&tokens[start].key, &query, 0);
            if c0 == 0 { continue; }
            let (mut path, mut found) = (vec![start as i32], None);
            if dfs(tokens, &query, start + 1, c0, 0, max_skips, &mut path, &mut found) { return found; }
        }
        None
    }

    #[test]
    fn memoized_pinyin_fuzzy_returns_the_reference_first_path() {
        let alphabet: Vec<char> = "剪贴板测试文件中国长短重行abcdeinghlz ".chars().collect();
        let queries = ["jtb", "wenjian", "zhong", "chang", "zw", "ab", "jianqieban", "hl", "zzzz", "cs", "x"];
        let mut state = 0x9e37_79b9_u64;
        let mut next = move |n: usize| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % n as u64) as usize
        };
        for _ in 0..400 {
            let len = 1 + next(60);
            let text: String = (0..len).map(|_| alphabet[next(alphabet.len())]).collect();
            let tokens = build_pinyin_tokens(&text);
            for query in queries {
                for max_skips in [0, 1, 4] {
                    assert_eq!(try_pinyin_token_fuzzy(&tokens, query, max_skips),
                        fuzzy_reference(&tokens, query, max_skips), "{text:?} {query} {max_skips}");
                }
            }
        }
    }

    #[test]
    #[ignore]
    fn profile_long_text_steps() {
        let line = "剪贴板 perf 测试 clipboard history line with some words; ";
        let text = format!("长文本 77 {}", line.repeat(100));
        let q = "needle";
        let time = |label: &str, f: &dyn Fn()| {
            let t = std::time::Instant::now();
            for _ in 0..20 { f(); }
            eprintln!("STEP {label}: {:?}", t.elapsed() / 20);
        };
        let t = nfkc(&text);
        time("matches", &|| { std::hint::black_box(matches(&text, q)); });
        time("pinyin_search", &|| { std::hint::black_box(pinyin_search(q, &[&text])); });
        time("get_match_score", &|| { std::hint::black_box(get_match_score(&text, q)); });
        time("nfkc", &|| { std::hint::black_box(nfkc(&text)); });
        time("index_of", &|| { std::hint::black_box(index_of_ignore_case(&t, q)); });
        time("pinyin_match_string", &|| { std::hint::black_box(get_pinyin_match_string(&t)); });
        let pms = get_pinyin_match_string(&t);
        time("pinyin_match_encoded", &|| { std::hint::black_box(is_pinyin_match_encoded(&pms, q)); });
        time("latin_segment", &|| { std::hint::black_box(try_latin_segment_prefix(&t, q)); });
        time("tokens", &|| { std::hint::black_box(build_pinyin_tokens(&t)); });
        let tokens = build_pinyin_tokens(&t);
        time("strict", &|| { std::hint::black_box(try_strict_pinyin_token_range(&tokens, q)); });
        time("fuzzy", &|| { std::hint::black_box(try_pinyin_token_fuzzy(&tokens, q, DEFAULT_MAX_PINYIN_TOKEN_SKIPS)); });
        time("gap_match_all", &|| { std::hint::black_box(gap_match_latin_and_pinyin_tokens(&text, q)); });
        eprintln!("STEP len chars={} tokens={}", text.chars().count(), tokens.len());
    }

    /// The allocating tokenizer this file used before `TokenKey`.
    fn tokens_reference(text: &str) -> Vec<(usize, Vec<u16>)> {
        let units = encode_utf16(text);
        let mut tokens = Vec::new();
        let mut i = 0usize;
        while i < units.len() {
            let ch = utf16_char_at(&units, i);
            let unit_len = if (0xD800..=0xDBFF).contains(&units[i]) && i + 1 < units.len() { 2 } else { 1 };
            if ch.map(char::is_whitespace).unwrap_or(false) { i += unit_len; continue; }
            if let Some(ch) = ch.filter(|c| is_valid_cn_char(*c)) {
                let pinyin = get_pinyin(ch);
                if let Some(primary) = pinyin.split(' ').find(|p| !p.is_empty()).map(str::to_lowercase) {
                    tokens.push((i, encode_utf16(&primary)));
                    i += unit_len;
                    continue;
                }
            }
            let key = ch.map(|c| c.to_lowercase().to_string()).unwrap_or_else(|| String::from_utf16_lossy(&units[i..i + unit_len]));
            tokens.push((i, encode_utf16(&key)));
            i += unit_len;
        }
        tokens
    }

    #[test]
    fn inline_token_keys_match_the_allocating_tokenizer() {
        // Every BMP ideograph (single, multi-reading and unmapped) plus mixed
        // scripts, special lowercase expansions, surrogate pairs and whitespace.
        let mut texts: Vec<String> = (0x4E00u32..=0x9FFF).collect::<Vec<_>>().chunks(512)
            .map(|chunk| chunk.iter().filter_map(|c| char::from_u32(*c)).collect()).collect();
        texts.push("İ ΣΑΣ ẞ Ǆ Ꭰ abc ABC 😀 ﬃ ① \u{3000}中\t文".into());
        for text in &texts {
            let actual: Vec<(usize, Vec<u16>)> = build_pinyin_tokens(text).into_iter().map(|t| (t.index, t.key.to_vec())).collect();
            assert_eq!(actual, tokens_reference(text));
        }
    }
}
