//! Pure list-text links and UTF-16 search spans, matching `clipInlineLinkRender`.
//! Ranges address the supplied display text in bytes. Only opening targets are
//! normalized; web labels exclude prose punctuation and Markdown wrappers.

use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    sync::OnceLock,
};

use crate::dto::HighlightHitRangeDto;
use url::Url;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpenTarget {
    Http(String),
    ShellPath(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlineLink {
    pub range: Range<usize>,
    pub target: OpenTarget,
}

fn trim_for_open(raw: &str) -> &str {
    raw.trim_end_matches(|ch| {
        matches!(
            ch,
            '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '>'
        )
    })
}

// ECMAScript \s differs from Rust's Unicode White_Space (FEFF, but not 0085).
fn js_whitespace(ch: char) -> bool {
    matches!(ch, '\u{0009}'..='\u{000d}' | ' ' | '\u{00a0}' | '\u{1680}'
        | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}'
        | '\u{205f}' | '\u{3000}' | '\u{feff}')
}

fn line_break(ch: char) -> bool {
    matches!(ch, '\r' | '\n' | '\u{2028}' | '\u{2029}')
}

fn illegal_windows_char(ch: char) -> bool {
    line_break(ch) || ch < ' ' || matches!(ch, '<' | '>' | ':' | '"' | '|' | '?' | '*')
}

fn separator(ch: char) -> bool {
    matches!(ch, '\\' | '/')
}

fn prefix_at(text: &str, offset: usize, prefix: &str) -> bool {
    text.get(offset..offset + prefix.len())
        .is_some_and(|part| part.eq_ignore_ascii_case(prefix))
}

fn http_target(raw: &str) -> Option<OpenTarget> {
    let parsed = Url::parse(raw).ok()?;
    matches!(parsed.scheme(), "http" | "https").then(|| OpenTarget::Http(parsed.to_string()))
}

/// Prose separators are not part of a copied web address. In particular, a
/// Chinese comma must not silently become part of an IDN hostname or URL path.
fn prose_separator(ch: char) -> bool {
    matches!(
        ch,
        '，' | '。'
            | '；'
            | '：'
            | '？'
            | '！'
            | '、'
            | '（'
            | '）'
            | '【'
            | '】'
            | '《'
            | '》'
            | '〈'
            | '〉'
            | '“'
            | '”'
            | '‘'
            | '’'
    )
}

fn bare_start_boundary(ch: char) -> bool {
    js_whitespace(ch)
        || prose_separator(ch)
        || matches!(
            ch,
            '<' | '>'
                | '"'
                | '\''
                | '`'
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | ','
                | ';'
                | '!'
                | '?'
        )
}

fn bare_context_delimiter(ch: char) -> bool {
    js_whitespace(ch) || prose_separator(ch) || matches!(ch, '<' | '>' | '"' | '\'' | '`')
}

fn web_delimiter(ch: char) -> bool {
    js_whitespace(ch)
        || prose_separator(ch)
        || matches!(
            ch,
            '<' | '>' | '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}'
        )
}

fn trim_web_punctuation(raw: &str) -> &str {
    raw.trim_end_matches(['.', ',', ';', ':', '!', '?'])
}

/// Raw brackets delimit addresses in prose. The brackets around an HTTP IPv6
/// host are structural, so preserve those before validating with `Url`.
fn web_end(line: &str, start: usize, explicit: bool) -> usize {
    let scheme_end = start
        + if prefix_at(line, start, "https://") {
            8
        } else {
            7
        };
    let mut authority = explicit;
    let mut ipv6 = false;
    for (relative, ch) in line[start..].char_indices() {
        let pos = start + relative;
        if explicit && pos < scheme_end {
            continue;
        }
        if authority && matches!(ch, '/' | '?' | '#') {
            authority = false;
        }
        if ch == '[' && authority && (pos == scheme_end || line.as_bytes()[pos - 1] == b'@') {
            ipv6 = true;
            continue;
        }
        if ch == ']' && ipv6 {
            ipv6 = false;
            continue;
        }
        if web_delimiter(ch) {
            return pos;
        }
    }
    line.len()
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct DomainRules {
    tlds: HashSet<String>,
    ambiguous_file_suffixes: HashSet<String>,
}

fn domain_rules() -> &'static DomainRules {
    static RULES: OnceLock<DomainRules> = OnceLock::new();
    RULES.get_or_init(|| {
        serde_json::from_str(include_str!("../data/content-link-domains.json"))
            .expect("checked-in IANA domain rules")
    })
}

/// Bare text has much less evidence than an explicit HTTP URI. Validate DNS
/// labels and a known root suffix, reject credentials/paths, and require `www.`
/// for IDNs or suffixes commonly used as file extensions. No DNS or other I/O.
fn bare_http_target(raw: &str) -> Option<OpenTarget> {
    let www = raw
        .get(..4)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("www."));
    let mut authority_end = raw.len();
    for (offset, ch) in raw.char_indices() {
        if matches!(ch, '/' | '?' | '#') {
            authority_end = offset;
            break;
        }
        // DNS names are bounded. Reject punctuation immediately instead of
        // rescanning a large prose token such as word(word(word(...).
        if offset >= 1024
            || (ch.is_ascii() && !ch.is_ascii_alphanumeric() && !matches!(ch, '.' | '-' | ':'))
            || (!www && !ch.is_ascii())
        {
            return None;
        }
    }
    let authority = &raw[..authority_end];
    if !authority.contains('.') || authority.contains(['@', '%', '\\']) {
        return None;
    }
    if let Some((_, port)) = authority.rsplit_once(':') {
        if port.is_empty()
            || !port.bytes().all(|ch| ch.is_ascii_digit())
            || port.parse::<u16>().ok().is_none_or(|port| port == 0)
        {
            return None;
        }
    }
    if !www && !authority.is_ascii() {
        return None;
    }
    // Invalid authority checks must not repeatedly parse a potentially huge
    // trailing query/path while scanning a non-URL token with many separators.
    let parsed = Url::parse(&format!("https://{authority}")).ok()?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return None;
    }
    let url::Host::Domain(host) = parsed.host()? else {
        return None;
    };
    if host.len() > 253 {
        return None;
    }
    let labels: Vec<_> = host.split('.').collect();
    if labels.len() < if www { 3 } else { 2 }
        || labels.iter().any(|label| {
            label.is_empty()
                || label.len() > 63
                || label.starts_with('-')
                || label.ends_with('-')
                || !label
                    .bytes()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == b'-')
        })
    {
        return None;
    }
    let suffix = *labels.last()?;
    let rules = domain_rules();
    if !rules.tlds.contains(suffix) || (!www && rules.ambiguous_file_suffixes.contains(suffix)) {
        return None;
    }
    let parsed = Url::parse(&format!("https://{raw}")).ok()?;
    Some(OpenTarget::Http(parsed.to_string()))
}

fn collect_bare_urls(line: &str, line_offset: usize, out: &mut Vec<InlineLink>) {
    let mut pos = 0;
    let mut blocked_prefix = false;
    let mut token_end = None;
    let mut token_backslash = false;
    let mut segment_end = 0;
    while pos < line.len() {
        let ch = line[pos..].chars().next().unwrap();
        if bare_context_delimiter(ch) {
            blocked_prefix = false;
            token_end = None;
            segment_end = 0;
            pos += ch.len_utf8();
            continue;
        }
        let boundary = pos == 0
            || line[..pos]
                .chars()
                .next_back()
                .is_some_and(bare_start_boundary);
        if !ch.is_ascii_alphanumeric() || !boundary || blocked_prefix {
            blocked_prefix |= matches!(ch, '@' | '/' | '\\' | '%' | ':');
            pos += ch.len_utf8();
            continue;
        }
        let end = *token_end.get_or_insert_with(|| {
            let end = line[pos..]
                .find(bare_context_delimiter)
                .map_or(line.len(), |relative| pos + relative);
            token_backslash = line[pos..end].contains('\\');
            end
        });
        if token_backslash {
            pos = end;
            continue;
        }
        if segment_end <= pos {
            segment_end = web_end(&line[..end], pos, false);
        }
        let raw = trim_web_punctuation(&line[pos..segment_end]);
        if let Some(target) = bare_http_target(raw) {
            out.push(InlineLink {
                range: line_offset + pos..line_offset + pos + raw.len(),
                target,
            });
            pos = segment_end;
        } else {
            // Ordinary punctuation can introduce an address: Visit(www...).
            // The prefix guard prevents entering a scheme/mail/path token.
            pos += ch.len_utf8();
        }
    }
}

fn decode_file_path(path: &str) -> Option<String> {
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' => {
                let hi = char::from(*bytes.get(i + 1)?).to_digit(16)?;
                let lo = char::from(*bytes.get(i + 2)?).to_digit(16)?;
                out.push((hi * 16 + lo) as u8);
                i += 2;
            }
            byte => out.push(byte),
        }
        i += 1;
    }
    // JavaScript decodeURIComponent rejects malformed escapes and UTF-8.
    String::from_utf8(out).ok()
}

fn file_target(raw: &str) -> Option<OpenTarget> {
    let parsed = Url::parse(trim_for_open(raw)).ok()?;
    if parsed.scheme() != "file" {
        return None;
    }
    let pathname = decode_file_path(parsed.path())?;
    let bytes = pathname.as_bytes();
    if bytes.len() >= 4
        && bytes[0] == b'/'
        && bytes[1].is_ascii_alphabetic()
        && bytes[2] == b':'
        && bytes[3] == b'/'
    {
        return Some(OpenTarget::ShellPath(pathname[1..].replace('/', "\\")));
    }
    parsed
        .host_str()
        .filter(|host| !host.is_empty())
        .map(|host| OpenTarget::ShellPath(format!("\\\\{host}{}", pathname.replace('/', "\\"))))
}

fn collect_urls(line: &str, line_offset: usize, file: bool, out: &mut Vec<InlineLink>) {
    let mut pos = 0;
    while pos < line.len() {
        let matched = if file {
            prefix_at(line, pos, "file://")
        } else {
            prefix_at(line, pos, "http://") || prefix_at(line, pos, "https://")
        };
        if !matched {
            pos += line[pos..].chars().next().unwrap().len_utf8();
            continue;
        }
        let end = if file {
            line[pos..]
                .find(|ch| js_whitespace(ch) || matches!(ch, '<' | '>' | '"' | '\''))
                .map_or(line.len(), |relative| pos + relative)
        } else {
            web_end(line, pos, true)
        };
        let raw = if file {
            &line[pos..end]
        } else {
            trim_web_punctuation(&line[pos..end])
        };
        let target = if file {
            file_target(raw)
        } else {
            http_target(raw)
        };
        if let Some(target) = target {
            out.push(InlineLink {
                range: line_offset + pos..line_offset + pos + raw.len(),
                target,
            });
        }
        // Regex matchAll advances across the whole candidate even if parsing fails.
        pos = end;
    }
}

fn normalize_windows_match(raw: &str) -> Option<String> {
    let path = trim_for_open(raw).replace('/', "\\");
    let bytes = path.as_bytes();
    if bytes.len() < 3 {
        return None;
    }
    let drive = bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'\\';
    let unc = path.strip_prefix("\\\\").is_some_and(|tail| {
        let mut parts = tail.split('\\');
        parts.next().is_some_and(|server| !server.is_empty())
            && parts.next().is_some_and(|share| !share.is_empty())
    });
    if !drive && !unc {
        return None;
    }
    let body = &path[if drive { 3 } else { 2 }..];
    if body
        .chars()
        .any(|ch| ch < ' ' || matches!(ch, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
    {
        return None;
    }
    Some(path)
}

fn extend_drive(line: &str, start: usize) -> Option<(String, usize)> {
    let bytes = line.as_bytes();
    let letter = *bytes.get(start)?;
    if !letter.is_ascii_alphabetic()
        || bytes.get(start + 1) != Some(&b':')
        || !matches!(bytes.get(start + 2), Some(b'\\' | b'/'))
    {
        return None;
    }
    let mut path = format!("{}:\\", char::from(letter.to_ascii_uppercase()));
    let mut pos = start + 3;
    while pos < line.len() {
        let ch = line[pos..].chars().next()?;
        if illegal_windows_char(ch) {
            break;
        }
        if separator(ch) {
            path.push('\\');
            pos += 1;
            while pos < line.len() && matches!(bytes[pos], b'\\' | b'/') {
                pos += 1;
            }
        } else {
            path.push(ch);
            pos += ch.len_utf8();
        }
    }
    let path = path.trim_end_matches('\\');
    normalize_windows_match(path).map(|path| (path, pos))
}

fn extend_unc(line: &str, start: usize) -> Option<(String, usize)> {
    if !line.get(start..)?.starts_with("\\\\") {
        return None;
    }
    let mut pos = start + 2;
    if pos == line.len() || illegal_windows_char(line[pos..].chars().next()?) {
        return None;
    }
    while pos < line.len() {
        let ch = line[pos..].chars().next()?;
        if separator(ch) {
            break;
        }
        if illegal_windows_char(ch) {
            return None;
        }
        pos += ch.len_utf8();
    }
    if pos == line.len() {
        return None;
    }
    pos += 1;
    while pos < line.len() && separator(line[pos..].chars().next()?) {
        pos += 1;
    }
    let share_start = pos;
    if pos == line.len() || illegal_windows_char(line[pos..].chars().next()?) {
        return None;
    }
    while pos < line.len() {
        let ch = line[pos..].chars().next()?;
        if separator(ch) || illegal_windows_char(ch) {
            break;
        }
        pos += ch.len_utf8();
    }
    if pos == share_start {
        return None;
    }
    let mut end = pos;
    while pos < line.len() && separator(line[pos..].chars().next()?) {
        pos += 1;
        while pos < line.len() && separator(line[pos..].chars().next()?) {
            pos += 1;
        }
        if pos == line.len() {
            end = pos;
            break;
        }
        while pos < line.len() {
            let ch = line[pos..].chars().next()?;
            if separator(ch) {
                break;
            }
            if illegal_windows_char(ch) {
                return normalize_windows_match(&line[start..pos]).map(|path| (path, pos));
            }
            pos += ch.len_utf8();
        }
        end = pos;
    }
    normalize_windows_match(&line[start..end]).map(|path| (path, end))
}

fn collect_plain_line(line: &str, offset: usize, out: &mut Vec<InlineLink>) {
    collect_urls(line, offset, false, out);
    collect_urls(line, offset, true, out);
    collect_bare_urls(line, offset, out);
    for (start, ch) in line.char_indices() {
        if !ch.is_ascii_alphabetic()
            || line[..start]
                .chars()
                .next_back()
                .is_some_and(|ch| ch.is_ascii_alphanumeric())
        {
            continue;
        }
        if let Some((path, end)) = extend_drive(line, start) {
            out.push(InlineLink {
                range: offset + start..offset + end,
                target: OpenTarget::ShellPath(path),
            });
        }
    }
    let mut scan = 0;
    while let Some(relative) = line[scan..].find("\\\\") {
        let start = scan + relative;
        if let Some((path, end)) = extend_unc(line, start) {
            out.push(InlineLink {
                range: offset + start..offset + end,
                target: OpenTarget::ShellPath(path),
            });
            scan = end;
        } else {
            scan = start + 1;
        }
    }
}

#[derive(Debug)]
struct MarkdownLink {
    whole: Range<usize>,
    destination: Range<usize>,
}

fn advance_char(text: &str, pos: &mut usize) -> Option<char> {
    let ch = text.get(*pos..)?.chars().next()?;
    *pos += ch.len_utf8();
    Some(ch)
}

fn skip_space(text: &str, pos: &mut usize) {
    while text[*pos..].chars().next().is_some_and(js_whitespace) {
        advance_char(text, pos);
    }
}

/// Read structure without rendering/unescaping Markdown. Unsupported targets
/// still suppress the label: its visible URL may lead somewhere else entirely.
fn markdown_destination(
    line: &str,
    open: usize,
    furthest: &mut usize,
) -> Option<(Range<usize>, usize)> {
    let mut pos = open + 1;
    skip_space(line, &mut pos);
    let start;
    let end;
    if line[pos..].starts_with('<') {
        pos += 1;
        start = pos;
        loop {
            *furthest = (*furthest).max(pos);
            let before = pos;
            match advance_char(line, &mut pos)? {
                '>' => {
                    end = before;
                    break;
                }
                '<' => return None,
                '\\' => {
                    advance_char(line, &mut pos)?;
                }
                _ => {}
            }
        }
    } else {
        start = pos;
        let mut depth = 0;
        loop {
            *furthest = (*furthest).max(pos);
            let ch = line[pos..].chars().next()?;
            match ch {
                ')' if depth == 0 => break,
                ch if js_whitespace(ch) => break,
                '(' => depth += 1,
                ')' => depth -= 1,
                '\\' => {
                    advance_char(line, &mut pos);
                    advance_char(line, &mut pos)?;
                    continue;
                }
                _ => {}
            }
            advance_char(line, &mut pos);
        }
        if depth != 0 {
            return None;
        }
        end = pos;
    }
    let before_space = pos;
    skip_space(line, &mut pos);
    *furthest = (*furthest).max(pos);
    if line[pos..].starts_with(')') {
        return Some((start..end, pos + 1));
    }
    // A title needs separating whitespace and one of Markdown's delimiters.
    if before_space == pos {
        return None;
    }
    let close = match advance_char(line, &mut pos)? {
        '"' => '"',
        '\'' => '\'',
        '(' => ')',
        _ => return None,
    };
    loop {
        *furthest = (*furthest).max(pos);
        match advance_char(line, &mut pos)? {
            '\\' => {
                advance_char(line, &mut pos)?;
            }
            ch if ch == close => break,
            '(' if close == ')' => return None,
            _ => {}
        }
    }
    skip_space(line, &mut pos);
    *furthest = (*furthest).max(pos);
    line[pos..]
        .starts_with(')')
        .then_some((start..end, pos + 1))
}

fn markdown_links(line: &str) -> Vec<MarkdownLink> {
    // Prepair labels once; repeated unclosed '[' must not rescan the line.
    let mut stack = Vec::new();
    let mut pairs = HashMap::new();
    let mut pos = 0;
    while let Some(ch) = advance_char(line, &mut pos) {
        match ch {
            '\\' => {
                advance_char(line, &mut pos);
            }
            '[' => stack.push(pos - 1),
            ']' => {
                if let Some(start) = stack.pop() {
                    pairs.insert(start, pos - 1);
                }
            }
            _ => {}
        }
    }
    let mut links = Vec::new();
    let mut failed_until = 0;
    let mut blocked_prefix = false;
    pos = 0;
    while pos < line.len() {
        let start = pos;
        let ch = advance_char(line, &mut pos).unwrap();
        if bare_context_delimiter(ch) {
            blocked_prefix = false;
            continue;
        }
        blocked_prefix |= matches!(ch, '@' | '/' | '\\' | '%' | ':');
        if ch == '\\' {
            advance_char(line, &mut pos);
            continue;
        }
        if ch != '[' || start < failed_until || blocked_prefix {
            continue;
        }
        let Some(&close) = pairs.get(&start) else {
            continue;
        };
        if !line[close + 1..].starts_with('(') {
            continue;
        }
        let mut furthest = close + 2;
        if let Some((destination, end)) = markdown_destination(line, close + 1, &mut furthest) {
            links.push(MarkdownLink {
                whole: start..end,
                destination,
            });
            pos = end;
            blocked_prefix = false;
        } else {
            // Failed nested destinations/titles are ordinary text. Do not
            // repeatedly parse the same malformed suffix from later labels.
            failed_until = furthest;
        }
    }
    links
}

fn formatting_boundary(ch: char) -> bool {
    bare_start_boundary(ch) || matches!(ch, '.' | ':')
}

/// Only paired outer markers introduce boundaries; '*'/'_'/'~' inside a URL
/// remain untouched. This is link extraction, not a Markdown renderer.
fn collect_formatted_line(line: &str, offset: usize, out: &mut Vec<InlineLink>) {
    let mut pos = 0;
    let mut openers = HashMap::new();
    let mut markers = Vec::new();
    let mut context_end = 0;
    let mut first_blocked = None;
    let mut context_backslash = false;
    while pos + 2 <= line.len() {
        let ch = line[pos..].chars().next().unwrap();
        if bare_context_delimiter(ch) {
            pos += ch.len_utf8();
            context_end = pos;
            continue;
        }
        if pos >= context_end {
            context_end = line[pos..]
                .find(bare_context_delimiter)
                .map_or(line.len(), |end| pos + end);
            context_backslash = line[pos..context_end].contains('\\');
            first_blocked = line[pos..context_end]
                .find(['@', '/', '\\', '%', ':'])
                .map(|at| pos + at);
        }
        let marker = line[pos..].get(..2).unwrap_or("");
        if !matches!(marker, "**" | "__" | "~~") {
            pos += ch.len_utf8();
            continue;
        }
        if let Some(&open) = openers.get(marker).filter(|&&open| {
            pos > open + 2
                && !line[..pos].chars().next_back().is_some_and(js_whitespace)
                && line[pos + 2..]
                    .chars()
                    .next()
                    .is_none_or(formatting_boundary)
        }) {
            markers.push(open..open + 2);
            markers.push(pos..pos + 2);
            openers.remove(marker);
        } else if (pos == 0
            || line[..pos]
                .chars()
                .next_back()
                .is_some_and(bare_start_boundary))
            && line[pos + 2..]
                .chars()
                .next()
                .is_some_and(|ch| !js_whitespace(ch))
            && !context_backslash
            && first_blocked.is_none_or(|at| at > pos)
        {
            openers.insert(marker, pos);
        }
        pos += 2;
    }
    markers.sort_by_key(|range| range.start);
    let mut plain_start = 0;
    for marker in markers {
        collect_plain_line(&line[plain_start..marker.start], offset + plain_start, out);
        plain_start = marker.end;
    }
    collect_plain_line(&line[plain_start..], offset + plain_start, out);
}

fn collect_line(line: &str, offset: usize, out: &mut Vec<InlineLink>) {
    let mut start = 0;
    let markdown_links = markdown_links(line);
    for (index, markdown) in markdown_links.iter().enumerate() {
        collect_formatted_line(&line[start..markdown.whole.start], offset + start, out);
        let destination_start = offset + markdown.destination.start;
        let destination = &line[markdown.destination.clone()];
        let explicit =
            prefix_at(destination, 0, "http://") || prefix_at(destination, 0, "https://");
        // Markdown's label is independent of its target, but outer path/mail/
        // scheme context still matters for a target lacking an HTTP prefix.
        let prefix = line[start..markdown.whole.start]
            .rsplit(bare_context_delimiter)
            .next()
            .unwrap_or("");
        let next = markdown_links
            .get(index + 1)
            .map_or(line.len(), |next| next.whole.start);
        let suffix = line[markdown.whole.end..next]
            .split(bare_context_delimiter)
            .next()
            .unwrap_or("");
        let bare_context = !prefix.contains(['@', '/', '\\', '%', ':']) && !suffix.contains('\\');
        let mut links = Vec::new();
        collect_plain_line(destination, destination_start, &mut links);
        out.extend(links.into_iter().filter(|link| {
            link.range.start == destination_start
                && (explicit || bare_context)
                && matches!(link.target, OpenTarget::Http(_))
        }));
        start = markdown.whole.end;
    }
    collect_formatted_line(&line[start..], offset + start, out);
}

/// No I/O, existence probes or URI launch. Sorting is stable, with the earliest
/// candidate winning and the longest candidate winning only at the same start.
pub fn collect_inline_links(text: &str) -> Vec<InlineLink> {
    let mut candidates = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((offset, ch)) = chars.next() {
        if !line_break(ch) {
            continue;
        }
        if offset > start {
            collect_line(&text[start..offset], start, &mut candidates);
        }
        start = offset + ch.len_utf8();
        if ch == '\r' && chars.peek().is_some_and(|(_, next)| *next == '\n') {
            let (offset, ch) = chars.next().unwrap();
            start = offset + ch.len_utf8();
        }
    }
    if start < text.len() {
        collect_line(&text[start..], start, &mut candidates);
    }
    candidates.sort_by(|a, b| {
        a.range
            .start
            .cmp(&b.range.start)
            .then_with(|| b.range.len().cmp(&a.range.len()))
    });
    let mut links: Vec<InlineLink> = Vec::new();
    for candidate in candidates {
        if links
            .last()
            .is_none_or(|link| candidate.range.start >= link.range.end)
        {
            links.push(candidate);
        }
    }
    links
}

/// Whether the text contains an HTTP(S) link recognized by list highlighting.
/// File URLs and local paths do not match. Each complete line retains the same
/// candidate-overlap rules, and a match stops scanning subsequent lines.
pub fn has_http_link(text: &str) -> bool {
    text.split(line_break).any(|line| {
        if !line.contains('.')
            && !line.as_bytes().windows(7).any(|prefix| {
                prefix.eq_ignore_ascii_case(b"http://") || prefix.eq_ignore_ascii_case(b"https:/")
            })
        {
            return false;
        }
        collect_inline_links(line)
            .iter()
            .any(|link| matches!(link.target, OpenTarget::Http(_)))
    })
}

/// Convert the union of UTF-16 hit flags into valid UTF-8 byte ranges. A span
/// cutting a surrogate pair marks that whole scalar instead of slicing invalid
/// UTF-8. Adjacent/overlapping ranges join, just like contiguous web hit flags.
pub fn utf16_hit_ranges(text: &str, hits: &[HighlightHitRangeDto]) -> Vec<Range<usize>> {
    let mut utf16 = 0;
    let scalars: Vec<_> = text
        .char_indices()
        .map(|(byte, ch)| {
            let units = utf16..utf16 + ch.len_utf16();
            utf16 = units.end;
            (units, byte..byte + ch.len_utf8())
        })
        .collect();
    let mut ranges = Vec::new();
    for hit in hits {
        let start = (hit.start.max(0) as usize).min(utf16);
        let end = (hit.end.max(0) as usize).min(utf16).max(start);
        if start == end {
            continue;
        }
        let mut overlapping = scalars
            .iter()
            .filter(|(units, _)| units.end > start && units.start < end);
        if let Some((_, first)) = overlapping.next() {
            let end = overlapping.last().map_or(first.end, |(_, last)| last.end);
            ranges.push(first.start..end);
        }
    }
    ranges.sort_by_key(|range| range.start);
    let mut merged: Vec<Range<usize>> = Vec::new();
    for range in ranges {
        if let Some(last) = merged.last_mut().filter(|last| range.start <= last.end) {
            last.end = last.end.max(range.end);
        } else {
            merged.push(range);
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_url_shared_cases_match_highlighting_and_filtering_without_false_positives() {
        let cases: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../data/content-link-cases.json")).unwrap();
        for case in cases {
            let text = case["text"].as_str().unwrap();
            let links: Vec<_> = collect_inline_links(text)
                .into_iter()
                .filter_map(|link| {
                    if let OpenTarget::Http(target) = link.target {
                        Some(serde_json::json!({"label": &text[link.range], "target":target}))
                    } else {
                        None
                    }
                })
                .collect();
            assert_eq!(serde_json::json!(links), case["links"], "{text}");
            assert_eq!(has_http_link(text), !links.is_empty(), "{text}");
        }
        let prose = "ordinary?words".repeat(10_000);
        assert!(!has_http_link(&prose));
        // Malformed Markdown cannot make repeated labels reparse a long tail.
        let malformed = format!("{}{}", "[label](".repeat(10_000), "unterminated");
        assert!(collect_inline_links(&malformed).is_empty());
        assert!(collect_inline_links(&"**word(~~word[__word".repeat(10_000)).is_empty());
    }

    fn projected(text: &str) -> Vec<(&str, OpenTarget)> {
        collect_inline_links(text)
            .into_iter()
            .map(|link| (&text[link.range], link.target))
            .collect()
    }

    #[test]
    fn http_filter_uses_highlight_links_across_single_and_multiple_lines() {
        for text in [
            "https://example.test/path",
            "页面标题\r\nhttps://example.test/中文?q=1",
            "打开 (HTTPS://EXAMPLE.TEST/path). 查看",
            "第一行\n第二行\n第三行\nhttps://example.test/",
            "标题\u{2028}https://example.test/\u{2029}说明",
        ] {
            assert!(has_http_link(text), "expected HTTP link: {text:?}");
        }
        for text in [
            "",
            "ordinary text",
            "https://",
            "http://<invalid>",
            "www.example.test",
            "mailto:a@example.test ftp://example.test/a",
            "file:///C:/a.txt",
            "C:\\a.txt",
            "\\\\server\\share\\a.txt",
        ] {
            assert!(!has_http_link(text), "unexpected HTTP link: {text:?}");
        }
    }

    #[test]
    fn http_filter_keeps_existing_path_overlap_rules() {
        for text in [
            "C:\\a.txt https://example.test/",
            "C:\\a.txt https://example.test/\nhttps://another.test/",
            "file:///C:/https://example.test/",
            "\\\\server\\share\\https://example.test/",
            "https://example.test/C:\\a.txt",
            "标题\r\nC:\\a.txt\r\n链接 https://example.test/",
        ] {
            assert_eq!(
                has_http_link(text),
                collect_inline_links(text)
                    .iter()
                    .any(|link| matches!(link.target, OpenTarget::Http(_))),
                "filter/highlight mismatch: {text:?}",
            );
        }
        assert!(!has_http_link("C:\\a.txt https://example.test/"));
    }

    #[test]
    fn unicode_url_label_excludes_prose_punctuation_while_target_is_normalized() {
        let text = "😀 https://EXAMPLE.com:443/路?q=中文). tail";
        let links = collect_inline_links(text);
        assert_eq!(links[0].range.start, "😀 ".len());
        assert_eq!(
            projected(text),
            [(
                "https://EXAMPLE.com:443/路?q=中文",
                OpenTarget::Http("https://example.com/%E8%B7%AF?q=%E4%B8%AD%E6%96%87".into())
            )]
        );
        assert_eq!(
            projected("https://example.test/C:\\a.txt"),
            [(
                "https://example.test/C:\\a.txt",
                OpenTarget::Http("https://example.test/C:/a.txt".into())
            )]
        );
    }

    #[test]
    fn file_urls_decode_like_web_and_beat_their_inner_drive_candidate() {
        assert_eq!(
            projected("file:///C:/A+B/%E4%B8%AD.txt?x=1#frag"),
            [(
                "file:///C:/A+B/%E4%B8%AD.txt?x=1#frag",
                OpenTarget::ShellPath("C:\\A B\\中.txt".into())
            )]
        );
        assert_eq!(
            projected("file://server/share/a%20b.txt"),
            [(
                "file://server/share/a%20b.txt",
                OpenTarget::ShellPath("\\\\server\\share\\a b.txt".into())
            )]
        );
        assert_eq!(
            projected("file:///C:/A B.txt"),
            [("file:///C:/A", OpenTarget::ShellPath("C:\\A".into()))]
        );
        assert_eq!(
            projected("file://server"),
            [(
                "file://server",
                OpenTarget::ShellPath("\\\\server\\".into())
            )]
        );
        assert!(collect_inline_links("file:///tmp/a.txt").is_empty());
    }

    #[test]
    fn invalid_file_percent_decoding_still_allows_the_inner_literal_drive() {
        for text in ["file:///C:/bad%FF.txt", "file:///C:/bad%ZZ.txt"] {
            assert_eq!(collect_inline_links(text)[0].range.start, 8);
            assert_eq!(projected(text)[0].0, &text[8..]);
        }
        assert_eq!(
            projected("file:///C:/a%00b.txt"),
            [(
                "file:///C:/a%00b.txt",
                OpenTarget::ShellPath("C:\\a\0b.txt".into())
            )]
        );
    }

    #[test]
    fn drive_paths_keep_spaces_and_original_labels_without_crossing_lines() {
        let text = "c:/目录//two words/file.txt.)\nD:\\next.txt";
        assert_eq!(
            projected(text),
            [
                (
                    "c:/目录//two words/file.txt.)",
                    OpenTarget::ShellPath("C:\\目录\\two words\\file.txt".into())
                ),
                ("D:\\next.txt", OpenTarget::ShellPath("D:\\next.txt".into()))
            ]
        );
        assert_eq!(
            projected("abcC:\\bad.txt _C:\\good.txt"),
            [("C:\\good.txt", OpenTarget::ShellPath("C:\\good.txt".into()))]
        );
        assert_eq!(
            projected("C:\\a.txt https://example.test/"),
            [(
                "C:\\a.txt https",
                OpenTarget::ShellPath("C:\\a.txt https".into())
            )]
        );
        assert!(collect_inline_links("C:\\").is_empty());
        assert_eq!(
            projected("C:\\ \nC:\\"),
            [("C:\\ ", OpenTarget::ShellPath("C:\\ ".into()))]
        );
        assert_eq!(
            projected("C:/\\a//b\\\\c.txt"),
            [(
                "C:/\\a//b\\\\c.txt",
                OpenTarget::ShellPath("C:\\\\a\\b\\c.txt".into())
            )]
        );
    }

    #[test]
    fn unc_scanning_preserves_web_overlap_and_device_paths_for_core_validation() {
        assert_eq!(
            projected("\\\\server/share/a b.txt;\r\nend"),
            [(
                "\\\\server/share/a b.txt;",
                OpenTarget::ShellPath("\\\\server\\share\\a b.txt".into())
            )]
        );
        assert_eq!(
            projected("\\\\server\\\\share\\a.txt"),
            [(
                "\\\\share\\a.txt",
                OpenTarget::ShellPath("\\\\share\\a.txt".into())
            )]
        );
        assert_eq!(
            projected("\\\\.\\pipe\\name"),
            [(
                "\\\\.\\pipe\\name",
                OpenTarget::ShellPath("\\\\.\\pipe\\name".into())
            )]
        );
        assert!(collect_inline_links("\\\\server").is_empty());
    }

    #[test]
    fn utf8_link_ranges_remain_exact_across_non_bmp_and_all_line_breaks() {
        let text = "😀中C:/目录/😀.txt\r\nfile:///D:/a.txt\u{2028}E:/三.txt\u{2029}F:/四.txt";
        let links = collect_inline_links(text);
        assert_eq!(links.len(), 4);
        assert_eq!(links[0].range.start, "😀中".len());
        assert_eq!(links[1].range.start, "😀中C:/目录/😀.txt\r\n".len());
        for link in links {
            assert!(text.is_char_boundary(link.range.start));
            assert!(text.is_char_boundary(link.range.end));
            assert!(!text[link.range].chars().any(line_break));
        }
        assert!(
            collect_inline_links("mailto:a@b.test ftp://host/a www.example.test ~/a ./b")
                .is_empty()
        );
    }

    #[test]
    fn utf16_hits_clip_union_and_expand_only_split_surrogate_scalars() {
        let text = "a😀中e\u{301}";
        let hits = [
            HighlightHitRangeDto { start: 2, end: 3 },
            HighlightHitRangeDto { start: 3, end: 4 },
            HighlightHitRangeDto { start: 5, end: 6 },
            HighlightHitRangeDto { start: 4, end: 1 },
            HighlightHitRangeDto { start: 20, end: 30 },
        ];
        assert_eq!(utf16_hit_ranges(text, &hits), [1..8, 9..11]);
        assert_eq!(
            utf16_hit_ranges(text, &[HighlightHitRangeDto { start: -3, end: 99 }]),
            [0..text.len()]
        );
        assert!(utf16_hit_ranges(text, &[HighlightHitRangeDto { start: 2, end: 2 }]).is_empty());
        assert!(utf16_hit_ranges("", &hits).is_empty());
    }
}
