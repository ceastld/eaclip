//! Query and field preparation local to CeaBox. The existing matchers retain
//! their public behavior; this child module reuses their matching primitives.

use std::{
    collections::HashMap,
    sync::{Arc, OnceLock},
};

use crate::model::{ClipKind, ClipRecord, FavorRecord};

use super::*;

/// One immutable string can serve several record fields without retaining rich
/// payloads. Expensive forms are populated only after a literal match misses.
struct Text {
    value: String,
    upper: OnceLock<Vec<u16>>,
    lower: OnceLock<String>,
    normalized: OnceLock<Option<Box<Text>>>,
    encoded: OnceLock<EncodedPinyin>,
    tokens: OnceLock<Vec<PinyinToken>>,
    latin: OnceLock<Vec<(usize, usize)>>,
    has_ideograph: bool,
}

impl Text {
    fn new(value: String) -> Self {
        let has_ideograph = contains_ideograph(&value);
        Self {
            value,
            upper: OnceLock::new(),
            lower: OnceLock::new(),
            normalized: OnceLock::new(),
            encoded: OnceLock::new(),
            tokens: OnceLock::new(),
            latin: OnceLock::new(),
            has_ideograph,
        }
    }

    fn upper(&self) -> &[u16] {
        self.upper
            .get_or_init(|| self.value.encode_utf16().map(to_upper_u16).collect())
    }

    fn lower(&self) -> &str {
        self.lower.get_or_init(|| self.value.to_lowercase())
    }

    fn normalized(&self) -> &Self {
        self.normalized
            .get_or_init(|| {
                let normalized = nfkc(&self.value);
                (normalized != self.value).then(|| Box::new(Self::new(normalized)))
            })
            .as_deref()
            .unwrap_or(self)
    }

    fn contains(&self, needle: &[u16]) -> bool {
        needle.is_empty()
            || (self.upper().len() >= needle.len()
                && self
                    .upper()
                    .windows(needle.len())
                    .any(|part| part == needle))
    }

    fn pinyin_encoded(&self, query: &Term) -> bool {
        if !(self.has_ideograph || query.has_ideograph) {
            return false;
        }
        let encoded = self.encoded.get_or_init(|| {
            let units: Vec<u16> = get_pinyin_match_string(&self.value)
                .encode_utf16()
                .collect();
            let mut starts = [0; 30];
            let mut count = 0;
            let mut boundary = true;
            for (index, unit) in units.iter().enumerate() {
                if count >= starts.len() {
                    break;
                }
                if boundary && *unit != u16::from(b' ') {
                    starts[count] = index as i32;
                    count += 1;
                }
                boundary = *unit == u16::from(b' ');
            }
            EncodedPinyin {
                units,
                starts,
                count,
            }
        });
        !query.lower_units.is_empty()
            && is_multi_char_match_any_part(
                &encoded.units,
                &query.lower_units,
                &encoded.starts,
                encoded.count,
            )
    }

    fn gap(&self, query: &GapQuery) -> bool {
        if query.latin.len() >= 2 && query.ascii_latin {
            let units = self.upper();
            let segments = self
                .latin
                .get_or_init(|| build_latin_segments(&encode_utf16(&self.value)));
            for start in 0..segments.len() {
                let mut offset = 0;
                for &(segment_start, segment_len) in &segments[start..] {
                    if offset >= query.latin.len() {
                        break;
                    }
                    let mut consumed = 0;
                    while consumed < segment_len
                        && offset + consumed < query.latin.len()
                        && units[segment_start + consumed] == query.latin[offset + consumed]
                    {
                        consumed += 1;
                    }
                    if consumed == 0 {
                        break;
                    }
                    offset += consumed;
                }
                if offset >= query.latin.len() {
                    return true;
                }
            }
        }
        if query.has_compact_ideograph
            && query.compact_units.len() >= 2
            && prepared_gap_subsequence(self.upper(), &query.compact_units)
        {
            return true;
        }
        if !self.has_ideograph {
            return false;
        }
        let tokens = self.tokens.get_or_init(|| build_pinyin_tokens(&self.value));
        prepared_pinyin_gap(
            tokens,
            &query.pinyin_units,
            DEFAULT_MAX_PINYIN_TOKEN_SKIPS as u8,
        )
    }

    fn pinyin_term(&self, query: &Term) -> bool {
        self.contains(&query.upper)
            || self.pinyin_encoded(query)
            || self.normalized().gap(&query.gap)
    }

    fn matches(&self, query: &Term) -> bool {
        if query.value.trim().is_empty() {
            return true;
        }
        if self.value.is_empty() {
            return false;
        }
        let text = self.normalized();
        let term = query.normalized();
        text.contains(&term.upper) || text.pinyin_encoded(term) || text.gap(&term.gap)
    }
}

struct EncodedPinyin {
    units: Vec<u16>,
    starts: [i32; 30],
    count: usize,
}

struct GapQuery {
    latin: Vec<u16>,
    ascii_latin: bool,
    compact_units: Vec<u16>,
    has_compact_ideograph: bool,
    pinyin_units: Vec<u16>,
}

impl GapQuery {
    fn new(value: &str) -> Self {
        let value = nfkc(value.trim());
        let latin = compact_latin_query(&value);
        let compact = compact_whitespace(&value);
        Self {
            ascii_latin: is_all_ascii_letters(&latin),
            latin: latin.encode_utf16().map(to_upper_u16).collect(),
            compact_units: compact.encode_utf16().map(to_upper_u16).collect(),
            has_compact_ideograph: contains_ideograph(&compact),
            pinyin_units: if latin.is_empty() {
                value.to_lowercase()
            } else {
                latin
            }
            .encode_utf16()
            .collect(),
        }
    }
}

struct Term {
    value: String,
    upper: Vec<u16>,
    lower_units: Vec<u16>,
    has_ideograph: bool,
    gap: GapQuery,
    normalized: OnceLock<Option<Box<Term>>>,
}

impl Term {
    fn new(value: String) -> Self {
        let upper = value.encode_utf16().map(to_upper_u16).collect();
        let lower_units = value.to_lowercase().encode_utf16().collect();
        let has_ideograph = contains_ideograph(&value);
        let gap = GapQuery::new(&value);
        Self {
            value,
            upper,
            lower_units,
            has_ideograph,
            gap,
            normalized: OnceLock::new(),
        }
    }

    fn normalized(&self) -> &Self {
        self.normalized
            .get_or_init(|| {
                let normalized = nfkc(self.value.trim());
                (normalized != self.value).then(|| Box::new(Self::new(normalized)))
            })
            .as_deref()
            .unwrap_or(self)
    }
}

/// Boolean equivalent of `try_gap_subsequence`, including its first-path and
/// average-gap rules. No reachable position extends past the bounded leading
/// gap plus one maximum gap per query character, so a long body stays cheap.
fn prepared_gap_subsequence(hay: &[u16], query: &[u16]) -> bool {
    if hay.is_empty() || query.is_empty() {
        return false;
    }
    let gap = DEFAULT_MAX_TEXT_GAP_BETWEEN_CHARS as usize;
    let lead = DEFAULT_MAX_TEXT_LEADING_GAP as usize;
    let end = hay.len().min(
        lead.saturating_add(1)
            .saturating_add(query.len().saturating_sub(1).saturating_mul(gap + 1)),
    );
    let hay = &hay[..end];
    let mut starts = vec![None; hay.len()];
    for (index, unit) in hay.iter().enumerate().take(lead + 1) {
        if *unit == query[0] {
            starts[index] = Some(index);
        }
    }
    for unit in &query[1..] {
        let mut next = vec![None; hay.len()];
        for (previous, start) in starts.iter().enumerate() {
            let Some(start) = *start else {
                continue;
            };
            for index in (previous + 1)..hay.len().min(previous + gap + 2) {
                if next[index].is_none() && hay[index] == *unit {
                    next[index] = Some(start);
                }
            }
        }
        starts = next;
    }
    let span = query.len() + DEFAULT_MAX_TEXT_AVERAGE_GAP as usize * query.len().saturating_sub(1);
    starts
        .into_iter()
        .enumerate()
        .any(|(end, start)| start.is_some_and(|start| end - start + 1 <= span))
}

/// The path-producing matcher remembers (token, query offset, skip streak).
/// Existence needs only the smallest skip streak for each query offset at the
/// current token: a smaller streak permits every future transition of a larger
/// one. Keep two query-sized rows instead of a full text-sized failed-state map.
fn prepared_pinyin_gap(tokens: &[PinyinToken], query: &[u16], max_skips: u8) -> bool {
    if query.is_empty() || tokens.is_empty() {
        return false;
    }
    let absent = max_skips.saturating_add(1);
    let mut active = vec![absent; query.len()];
    let mut next = vec![absent; query.len()];
    let mut any_active = false;
    for token in tokens {
        let consume_start = get_matched_prefix_length(&token.key, query, 0);
        if !any_active {
            if consume_start == 0 {
                continue;
            }
            if consume_start == query.len() {
                return true;
            }
            active[consume_start] = 0;
            any_active = true;
            continue;
        }
        next.fill(absent);
        for (offset, streak) in active.iter().copied().enumerate() {
            if streak == absent {
                continue;
            }
            let consume = get_matched_prefix_length(&token.key, query, offset);
            if consume > 0 {
                if offset + consume == query.len() {
                    return true;
                }
                next[offset + consume] = 0;
            }
            if streak < max_skips {
                next[offset] = next[offset].min(streak + 1);
            }
        }
        if consume_start > 0 {
            if consume_start == query.len() {
                return true;
            }
            next[consume_start] = 0;
        }
        any_active = next.iter().any(|streak| *streak != absent);
        std::mem::swap(&mut active, &mut next);
    }
    false
}

#[derive(Clone)]
pub(crate) struct PreparedField {
    text: Arc<Text>,
    ranking: Arc<OnceLock<Arc<Text>>>,
}

impl PreparedField {
    pub(crate) fn new(value: &str) -> Self {
        Self {
            text: Arc::new(Text::new(value.into())),
            ranking: Arc::new(OnceLock::new()),
        }
    }

    fn ranking(&self) -> &Text {
        self.ranking.get_or_init(|| {
            let canonical = canonical_rank(&self.text.value);
            if canonical == self.text.value {
                self.text.clone()
            } else {
                Arc::new(Text::new(canonical))
            }
        })
    }
}

fn canonical_rank(value: &str) -> String {
    normalize_for_search(value)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

pub(crate) struct PreparedQuery {
    term: Term,
    terms: Vec<Term>,
    lower: String,
    rank: String,
    rank_terms: Vec<Term>,
}

impl PreparedQuery {
    /// `query` is the host-normalized input, exactly as passed to the existing
    /// qualification predicates. Ranking has its established separate form.
    pub(crate) fn new(query: &str) -> Self {
        let rank = canonical_rank(query);
        Self {
            term: Term::new(query.into()),
            terms: Self::pinyin_terms(query),
            lower: query.to_lowercase(),
            rank_terms: Self::pinyin_terms(&rank),
            rank,
        }
    }

    fn pinyin_terms(query: &str) -> Vec<Term> {
        query
            .replace('\'', "")
            .split(' ')
            .filter(|term| !term.is_empty())
            .map(|term| Term::new(term.into()))
            .collect()
    }

    fn pinyin(&self, field: &PreparedField) -> bool {
        self.term.value.trim().is_empty()
            || (!field.text.value.trim().is_empty()
                && self.terms.iter().all(|term| field.text.pinyin_term(term)))
    }

    fn matches(&self, field: &PreparedField) -> bool {
        field.text.matches(&self.term)
    }

    pub(crate) fn field_score(&self, field: &PreparedField, title: bool) -> i32 {
        if self.term.value.is_empty() || field.text.value.is_empty() {
            return 0;
        }
        let text = field.ranking();
        let quality = if text.value == self.rank {
            5
        } else if text.value.starts_with(&self.rank) {
            4
        } else if text.value.contains(&self.rank) {
            3
        } else if self
            .rank
            .split(' ')
            .filter(|term| !term.is_empty())
            .all(|term| text.value.contains(term))
        {
            2
        } else if !text.value.trim().is_empty()
            && self.rank_terms.iter().all(|term| text.pinyin_term(term))
        {
            1
        } else {
            0
        };
        match (title, quality) {
            (true, 5) => 1_000,
            (true, 4) => 800,
            (true, 3) => 700,
            (true, 2) => 650,
            (true, 1) => 500,
            (false, 5) => 450,
            (false, 4) => 350,
            (false, 3) => 300,
            (false, 2) => 250,
            (false, 1) => 200,
            _ => 0,
        }
    }

    pub(crate) fn matches_clip(&self, record: &PreparedClip) -> bool {
        if self.term.value.trim().is_empty() {
            return true;
        }
        if record.image {
            return record
                .image_title
                .as_ref()
                .is_some_and(|title| self.matches(title))
                || self.matches(&record.description)
                || record
                    .image_name
                    .as_ref()
                    .is_some_and(|name| self.pinyin(name));
        }
        let primary = match &record.filter {
            Filter::Pinyin(fields) => fields.iter().any(|field| self.pinyin(field)),
            Filter::Command(field) => field.text.lower().contains(&self.lower),
            Filter::None => false,
        };
        primary
            || self.matches(&record.title)
            || (!Arc::ptr_eq(&record.summary.text, &record.title.text)
                && self.matches(&record.summary))
            || (!Arc::ptr_eq(&record.description.text, &record.title.text)
                && !Arc::ptr_eq(&record.description.text, &record.summary.text)
                && self.matches(&record.description))
    }

    pub(crate) fn matches_favor(&self, record: &PreparedFavor) -> bool {
        if self.term.value.trim().is_empty() {
            return true;
        }
        let (primary, name_tested) = match &record.filter {
            Filter::Pinyin(fields) => (
                fields.iter().any(|field| self.pinyin(field)),
                fields
                    .iter()
                    .any(|field| Arc::ptr_eq(&field.text, &record.name.text)),
            ),
            Filter::Command(field) => (field.text.lower().contains(&self.lower), false),
            Filter::None => (false, false),
        };
        primary
            || (!name_tested && self.pinyin(&record.name))
            || record.name.text.lower().contains(&self.lower)
    }

    pub(crate) fn score_clip(&self, record: &PreparedClip) -> i32 {
        if self.term.value.is_empty() {
            return 0;
        }
        let title = self.field_score(&record.title, true);
        // The secondary field cannot beat any title hit. Most text records use
        // their body as title, so avoid joining or normalizing that body again.
        if title >= 450 {
            return title;
        }
        let secondary = record.secondary.get_or_init(|| {
            PreparedField::new(&format!(
                "{}\n{}",
                record.summary.text.value, record.description.text.value
            ))
        });
        title.max(self.field_score(secondary, false))
    }

    pub(crate) fn score_favor(&self, record: &PreparedFavor) -> i32 {
        if self.term.value.is_empty() {
            return 0;
        }
        let title = self.field_score(&record.name, true);
        if title >= 450 {
            title
        } else {
            title.max(self.field_score(&record.body, false))
        }
    }
}

enum Filter {
    Pinyin(Vec<PreparedField>),
    Command(PreparedField),
    None,
}

#[derive(Default)]
struct Fields(HashMap<String, PreparedField>);

impl Fields {
    fn field(&mut self, value: &str) -> PreparedField {
        self.0
            .entry(value.into())
            .or_insert_with(|| PreparedField::new(value))
            .clone()
    }

    fn pinyin<'a>(&mut self, values: impl IntoIterator<Item = &'a str>) -> Filter {
        let mut fields: Vec<PreparedField> = Vec::new();
        for value in values {
            let field = self.field(value);
            if !fields
                .iter()
                .any(|existing| Arc::ptr_eq(&existing.text, &field.text))
            {
                fields.push(field);
            }
        }
        Filter::Pinyin(fields)
    }
}

pub(crate) struct PreparedClip {
    filter: Filter,
    title: PreparedField,
    summary: PreparedField,
    description: PreparedField,
    secondary: OnceLock<PreparedField>,
    image: bool,
    image_title: Option<PreparedField>,
    image_name: Option<PreparedField>,
}

impl PreparedClip {
    pub(crate) fn new(item: &ClipRecord) -> Self {
        let mut fields = Fields::default();
        let summary = item.csharp_summary();
        let body = item.body_text.as_deref().unwrap_or("");
        let filter = match item.kind {
            ClipKind::FileClipItem => fields.pinyin(item.file_paths.iter().map(String::as_str)),
            ClipKind::TextClipItem | ClipKind::HtmlClipItem | ClipKind::QQRichTextItem => {
                fields.pinyin([body])
            }
            ClipKind::LinkClipItem => fields.pinyin([item.title.as_deref().unwrap_or(""), body]),
            ClipKind::CmdClipItem => Filter::Command(fields.field(body)),
            ClipKind::StepClipItem => fields.pinyin([body, &summary]),
            ClipKind::ActionClipItem => {
                let action =
                    crate::quicker_action::ActionReference::parse(body).unwrap_or_default();
                fields.pinyin([
                    item.title.as_deref().unwrap_or(""),
                    item.description.as_str(),
                    action.title.as_deref().unwrap_or(""),
                    action.description.as_deref().unwrap_or(""),
                ])
            }
            ClipKind::ImageClipItem | ClipKind::FavoriteClipItem => Filter::None,
        };
        let image_title = item
            .title
            .as_deref()
            .map(str::trim)
            .filter(|title| !title.is_empty() && !title.eq_ignore_ascii_case("(No title)"))
            .map(|title| fields.field(title));
        let image_name = image_file_name(item.image_path.as_deref().unwrap_or(""))
            .map(|name| fields.field(name));
        Self {
            filter,
            title: fields.field(&item.visual_title()),
            summary: fields.field(&summary),
            description: fields.field(&item.description),
            secondary: OnceLock::new(),
            image: item.kind == ClipKind::ImageClipItem,
            image_title,
            image_name,
        }
    }
}

pub(crate) struct PreparedFavor {
    filter: Filter,
    name: PreparedField,
    body: PreparedField,
}

impl PreparedFavor {
    pub(crate) fn new(item: &FavorRecord) -> Self {
        let mut fields = Fields::default();
        let body = item.body_text.as_deref().unwrap_or("");
        let filter = match item.kind {
            ClipKind::FileClipItem => fields.pinyin(item.file_paths.iter().map(String::as_str)),
            ClipKind::TextClipItem | ClipKind::HtmlClipItem | ClipKind::QQRichTextItem => {
                fields.pinyin([body])
            }
            ClipKind::LinkClipItem => fields.pinyin([item.name.as_str(), body]),
            ClipKind::CmdClipItem => Filter::Command(fields.field(body)),
            ClipKind::StepClipItem => fields.pinyin([item.name.as_str(), body]),
            ClipKind::ActionClipItem => {
                let action =
                    crate::quicker_action::ActionReference::parse(body).unwrap_or_default();
                fields.pinyin([
                    action.title.as_deref().unwrap_or(""),
                    action.description.as_deref().unwrap_or(""),
                ])
            }
            ClipKind::ImageClipItem | ClipKind::FavoriteClipItem => Filter::None,
        };
        Self {
            filter,
            name: fields.field(&item.name),
            body: fields.field(body),
        }
    }
}

fn image_file_name(path: &str) -> Option<&str> {
    let trimmed = path.trim().trim_end_matches(['\\', '/']);
    std::path::Path::new(trimmed)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXTS: &[&str] = &[
        "",
        " ",
        "项目计划",
        "计划  项目",
        "项目附件 会议",
        "中文 English 文本",
        "ＡＢＣ",
        "abc ABC",
        "fooBar some file",
        "foo-bar_some-file",
        "hello\tworld\r\n你好",
        "简体中文和繁體中文",
        "😀项目🦀",
        "C:\\资料\\项目说明.md",
        "https://example.invalid/项目",
        "İSTANBUL ıstanbul",
        "ı x",
        "straße STRASSE",
        "cafe\u{301} Café",
        "ce'e's",
        "\u{3000}项目\u{3000}计划",
        "项目abcd计划",
        "项abcdef计划",
        "a b c d e f g h i j k l m n o p q r s t u v w x y z 一 二 三 四 五 项目",
        "ã¨\u{0081}",
    ];

    const QUERIES: &[&str] = &[
        "",
        " ",
        "项目",
        "计划 项目",
        "xm",
        "xiangmu",
        "xmjh",
        "x m",
        "x'm",
        "x'",
        "'",
        "''",
        "abc",
        "ＡＢＣ",
        "fbsf",
        "foo file",
        "foobar",
        "HW",
        "hello\tworld",
        "资料",
        "项目计划",
        "项计划",
        "中文",
        "zw",
        "İ",
        "ı",
        "ß",
        "café",
        "cafe\u{301}",
        "😀",
        "missing",
        "你好",
        "nh",
        "\u{3000}项目\u{3000}",
        "项目\t计划",
        " 项目 ",
        "一二三四五项目",
        "ix",
    ];

    #[test]
    fn prepared_text_predicates_equal_existing_unicode_pinyin_and_gap_matchers() {
        for text in TEXTS {
            let field = PreparedField::new(text);
            for query in QUERIES {
                let compiled = PreparedQuery::new(query);
                assert_eq!(
                    compiled.pinyin(&field),
                    pinyin_search(query, &[text]),
                    "pinyin {text:?} {query:?}"
                );
                assert_eq!(
                    compiled.matches(&field),
                    matches(text, query),
                    "matches {text:?} {query:?}"
                );
                for title in [true, false] {
                    assert_eq!(
                        compiled.field_score(&field, title),
                        crate::ceabox::field_score(text, query, title),
                        "score {text:?} {query:?} {title}"
                    );
                }
            }
        }
    }

    #[test]
    fn prepared_bounded_gap_preserves_the_existing_first_path() {
        // Exhaustive short alphabets exercise alternate paths and the average
        // gap bound; long prefixes exercise the 64 UTF-16 leading-gap boundary.
        for bits in 0usize..1024 {
            let text: String = (0..10)
                .map(|index| {
                    if bits & (1 << index) == 0 {
                        '项'
                    } else {
                        '目'
                    }
                })
                .collect();
            for query in ["项目", "目项", "项项目", "项目项", "项项项", "目目项目"]
            {
                assert_eq!(
                    prepared_gap_subsequence(&encode_utf16(&text), &encode_utf16(query)),
                    try_gap_subsequence(&text, query).is_some(),
                    "{text} {query}"
                );
            }
        }
        for lead in [0, 63, 64, 65, 2000] {
            for gap in [0, 2, 3, 4, 5, 1000] {
                let text = format!("{}项{}目", "a".repeat(lead), "b".repeat(gap));
                assert_eq!(
                    prepared_gap_subsequence(&encode_utf16(&text), &encode_utf16("项目")),
                    try_gap_subsequence(&text, "项目").is_some()
                );
            }
        }
    }

    #[test]
    fn prepared_pinyin_boolean_dp_equals_original_path_matcher() {
        let alphabet = ['项', '目', '计', 'a', ' ', '😀'];
        let queries = [
            "x",
            "m",
            "xm",
            "xmx",
            "xma",
            "xmj",
            "xiamu",
            "xiangmu",
            "a",
            "aa",
            "ax",
            "axm",
            "jxm",
            "notmatched",
            "😀",
        ];
        for mut index in 0..1296 {
            let text: String = (0..4)
                .map(|_| {
                    let character = alphabet[index % alphabet.len()];
                    index /= alphabet.len();
                    character
                })
                .collect();
            let tokens = build_pinyin_tokens(&text);
            for query in queries {
                let units = encode_utf16(query);
                for max_skips in [0, 1, DEFAULT_MAX_PINYIN_TOKEN_SKIPS] {
                    assert_eq!(
                        prepared_pinyin_gap(&tokens, &units, max_skips as u8),
                        try_pinyin_token_fuzzy(&tokens, query, max_skips).is_some(),
                        "{text:?} {query} {max_skips}"
                    );
                }
            }
        }
    }

    #[test]
    fn field_preparation_is_lazy_and_reuses_equal_record_strings() {
        let record = ClipRecord::new_text("项目计划", "fixture");
        let prepared = PreparedClip::new(&record);
        assert!(Arc::ptr_eq(&prepared.title.text, &prepared.summary.text));
        assert!(prepared.title.text.encoded.get().is_none());
        assert!(prepared.title.text.tokens.get().is_none());
        assert!(PreparedQuery::new("项目").matches_clip(&prepared));
        assert!(prepared.title.text.encoded.get().is_none());
        assert!(prepared.title.text.tokens.get().is_none());
    }

    // Frozen qualification rules from host.rs. Comparing these independently
    // keeps the CeaBox cache from broadening image-path or action-body search.
    fn original_clip(item: &ClipRecord, query: &str) -> bool {
        if query.trim().is_empty() {
            return true;
        }
        if item.kind == ClipKind::ImageClipItem {
            let title = item.title.as_deref().unwrap_or("").trim();
            return (!title.is_empty()
                && !title.eq_ignore_ascii_case("(No title)")
                && matches(title, query))
                || matches(&item.description, query)
                || image_file_name(item.image_path.as_deref().unwrap_or(""))
                    .is_some_and(|name| pinyin_search(query, &[name]));
        }
        let body = item.body_text.as_deref().unwrap_or("");
        let primary = match item.kind {
            ClipKind::FileClipItem => pinyin_search(
                query,
                &item
                    .file_paths
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
            ),
            ClipKind::TextClipItem | ClipKind::HtmlClipItem | ClipKind::QQRichTextItem => {
                pinyin_search(query, &[body])
            }
            ClipKind::LinkClipItem => {
                pinyin_search(query, &[item.title.as_deref().unwrap_or(""), body])
            }
            ClipKind::CmdClipItem => body.to_lowercase().contains(&query.to_lowercase()),
            ClipKind::StepClipItem => pinyin_search(query, &[body, &item.csharp_summary()]),
            ClipKind::ActionClipItem => {
                let action =
                    crate::quicker_action::ActionReference::parse(body).unwrap_or_default();
                pinyin_search(
                    query,
                    &[
                        item.title.as_deref().unwrap_or(""),
                        &item.description,
                        action.title.as_deref().unwrap_or(""),
                        action.description.as_deref().unwrap_or(""),
                    ],
                )
            }
            ClipKind::ImageClipItem | ClipKind::FavoriteClipItem => false,
        };
        primary
            || matches(&item.visual_title(), query)
            || matches(&item.csharp_summary(), query)
            || matches(&item.description, query)
    }

    fn original_favor(item: &FavorRecord, query: &str) -> bool {
        if query.is_empty() {
            return true;
        }
        let body = item.body_text.as_deref().unwrap_or("");
        let primary = match item.kind {
            ClipKind::FileClipItem => pinyin_search(
                query,
                &item
                    .file_paths
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
            ),
            ClipKind::TextClipItem | ClipKind::HtmlClipItem | ClipKind::QQRichTextItem => {
                pinyin_search(query, &[body])
            }
            ClipKind::LinkClipItem | ClipKind::StepClipItem => {
                pinyin_search(query, &[&item.name, body])
            }
            ClipKind::CmdClipItem => body.to_lowercase().contains(&query.to_lowercase()),
            ClipKind::ActionClipItem => {
                let action =
                    crate::quicker_action::ActionReference::parse(body).unwrap_or_default();
                pinyin_search(
                    query,
                    &[
                        action.title.as_deref().unwrap_or(""),
                        action.description.as_deref().unwrap_or(""),
                    ],
                )
            }
            ClipKind::ImageClipItem => query.is_empty(),
            ClipKind::FavoriteClipItem => false,
        };
        primary
            || pinyin_search(query, &[&item.name])
            || item.name.to_lowercase().contains(&query.to_lowercase())
    }

    #[test]
    fn prepared_record_qualification_and_scores_equal_all_existing_kinds() {
        let kinds = [
            ClipKind::TextClipItem,
            ClipKind::HtmlClipItem,
            ClipKind::QQRichTextItem,
            ClipKind::ImageClipItem,
            ClipKind::FileClipItem,
            ClipKind::LinkClipItem,
            ClipKind::CmdClipItem,
            ClipKind::ActionClipItem,
            ClipKind::StepClipItem,
            ClipKind::FavoriteClipItem,
        ];
        let mut queries: Vec<_> = QUERIES
            .iter()
            .map(|query| normalize_for_search(query))
            .collect();
        queries.extend(
            [
                "参数",
                "cansu",
                "Description",
                "位置",
                "payload",
                "folder-only",
                "报告",
                "bg",
                "截图",
                "jt",
                "(No title)",
            ]
            .into_iter()
            .map(str::to_owned),
        );
        for kind in kinds {
            for title in [None, Some("项目计划"), Some("(No title)"), Some(" ")] {
                let mut clip = ClipRecord::new_text("{\"Id\":\"fixture-action\",\"Title\":\"报告\",\"Description\":\"描述说明\",\"payload\":\"位置\"}", "fixture");
                clip.kind = kind;
                clip.title = title.map(str::to_owned);
                clip.description = "Description 中文 😀".into();
                clip.execute_param = "参数\nsecond".into();
                clip.file_paths = vec![
                    "C:\\folder-only\\报告.md".into(),
                    "C:\\项目\\计划.txt".into(),
                ];
                clip.image_path = Some("C:\\folder-only\\截图.png".into());
                let favor = FavorRecord::from_clip(&clip, None);
                let prepared_clip = PreparedClip::new(&clip);
                let prepared_favor = PreparedFavor::new(&favor);
                for query in &queries {
                    let compiled = PreparedQuery::new(query);
                    assert_eq!(
                        compiled.matches_clip(&prepared_clip),
                        original_clip(&clip, query),
                        "clip {kind:?} {title:?} {query:?}"
                    );
                    assert_eq!(
                        compiled.matches_favor(&prepared_favor),
                        original_favor(&favor, query),
                        "favor {kind:?} {title:?} {query:?}"
                    );
                    let original_clip_score =
                        crate::ceabox::field_score(&clip.visual_title(), query, true).max(
                            crate::ceabox::field_score(
                                &format!("{}\n{}", clip.csharp_summary(), clip.description),
                                query,
                                false,
                            ),
                        );
                    assert_eq!(
                        compiled.score_clip(&prepared_clip),
                        original_clip_score,
                        "clip score {kind:?} {title:?} {query:?}"
                    );
                    let original_favor_score = crate::ceabox::field_score(&favor.name, query, true)
                        .max(crate::ceabox::field_score(
                            favor.body_text.as_deref().unwrap_or(""),
                            query,
                            false,
                        ));
                    assert_eq!(
                        compiled.score_favor(&prepared_favor),
                        original_favor_score,
                        "favor score {kind:?} {title:?} {query:?}"
                    );
                }
            }
        }
    }
}
