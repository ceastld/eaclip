//! Bounded text/code preview for file hover cards (2026-10-24).
//! Reads only a prefix of the file, rejects binary content, and highlights
//! with a small per-family lexer. No whole-file reads, no parser dependency.
use std::{io::Read, ops::Range, path::Path};

/// Prefix read per preview; large logs or sources never load completely.
pub const READ_BYTES: usize = 64 * 1024;
pub const MAX_LINES: usize = 160;
pub const MAX_LINE_CHARS: usize = 320;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Language {
    Plain,
    CLike,
    Rust,
    Python,
    Shell,
    Batch,
    PowerShell,
    Json,
    Ini,
    Yaml,
    Markdown,
    Markup,
    Sql,
    Css,
    Lua,
}

impl Language {
    pub fn label(self) -> &'static str {
        match self {
            Self::Plain => "Text",
            Self::CLike => "Code",
            Self::Rust => "Rust",
            Self::Python => "Python",
            Self::Shell => "Shell",
            Self::Batch => "Batch",
            Self::PowerShell => "PowerShell",
            Self::Json => "JSON",
            Self::Ini => "Config",
            Self::Yaml => "YAML",
            Self::Markdown => "Markdown",
            Self::Markup => "XML/HTML",
            Self::Sql => "SQL",
            Self::Css => "CSS",
            Self::Lua => "Lua",
        }
    }
}

pub fn language_for_extension(extension: &str) -> Option<Language> {
    use Language::*;
    Some(match extension.to_ascii_lowercase().as_str() {
        "txt" | "text" | "log" | "csv" | "tsv" | "nfo" | "srt" | "lrc" => Plain,
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "cs" | "java" | "js" | "mjs" | "cjs"
        | "jsx" | "ts" | "tsx" | "go" | "kt" | "kts" | "swift" | "dart" | "scala" | "groovy"
        | "gradle" | "php" | "m" | "mm" | "vue" | "svelte" | "proto" | "glsl" | "hlsl" | "wgsl"
        | "zig" => CLike,
        "rs" => Rust,
        "py" | "pyw" | "pyi" => Python,
        "sh" | "bash" | "zsh" | "fish" => Shell,
        "bat" | "cmd" => Batch,
        "ps1" | "psm1" | "psd1" => PowerShell,
        "json" | "jsonc" | "json5" | "jsonl" => Json,
        "ini" | "cfg" | "conf" | "toml" | "properties" | "env" | "editorconfig" | "gitignore"
        | "gitattributes" | "reg" => Ini,
        "yml" | "yaml" => Yaml,
        "md" | "markdown" | "mdx" => Markdown,
        "xml" | "html" | "htm" | "xhtml" | "xaml" | "svg" | "csproj" | "vbproj" | "props"
        | "targets" | "resx" | "config" | "plist" | "nuspec" => Markup,
        "sql" => Sql,
        "css" | "scss" | "sass" | "less" => Css,
        "lua" => Lua,
        _ => return None,
    })
}

/// Extension first, then well-known names, then a light content sniff.
pub fn detect_language(path: &Path, text: &str) -> Language {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match name.as_str() {
        "makefile" | "dockerfile" => return Language::Shell,
        ".gitignore" | ".gitattributes" | ".editorconfig" | ".env" => return Language::Ini,
        _ => {}
    }
    if let Some(language) = name.rsplit_once('.').and_then(|(_, ext)| language_for_extension(ext)) {
        if language != Language::Plain {
            return language;
        }
    }
    let head = text.trim_start_matches('\u{feff}').trim_start();
    if let Some(shebang) = head.strip_prefix("#!") {
        let line = shebang.lines().next().unwrap_or_default();
        return if line.contains("python") {
            Language::Python
        } else if line.contains("pwsh") || line.contains("powershell") {
            Language::PowerShell
        } else {
            Language::Shell
        };
    }
    if head.starts_with("<?xml") || head.starts_with("<!DOCTYPE") || head.starts_with("<html") {
        return Language::Markup;
    }
    if (head.starts_with('{') || head.starts_with('[')) && serde_json::from_str::<serde_json::Value>(head).is_ok() {
        return Language::Json;
    }
    Language::Plain
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextPreview {
    pub lines: Vec<String>,
    pub language: Language,
    /// More content exists past the prefix or line limits.
    pub truncated: bool,
    pub size: u64,
    pub encoding: &'static str,
}

/// Read at most `READ_BYTES`. Errors are user-facing short reasons.
pub fn read_preview(path: &Path) -> Result<TextPreview, String> {
    // Metadata queries do not hydrate cloud placeholders. Never open a
    // recall-on-access file or follow a file link for an automatic preview.
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        let attributes = std::fs::symlink_metadata(path)
            .map_err(|error| error.to_string())?.file_attributes();
        if attributes & (0x1000 | 0x40000 | 0x400000 | 0x400) != 0 {
            return Err(cliphost_core::i18n::tr("文件尚未保存在本机", "File is not available locally").into());
        }
    }
    let metadata = std::fs::metadata(path).map_err(|error| error.to_string())?;
    if !metadata.is_file() {
        return Err("Not a file".into());
    }
    let mut bytes = Vec::with_capacity(READ_BYTES.min(metadata.len() as usize + 1));
    std::fs::File::open(path)
        .map_err(|error| error.to_string())?
        .take(READ_BYTES as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    let mut preview = decode_preview(&bytes, metadata.len() > bytes.len() as u64)?;
    preview.language = detect_language(path, &preview.lines.join("\n"));
    preview.size = metadata.len();
    Ok(preview)
}

/// Decode a prefix; `more` means the file continues past `bytes`.
pub fn decode_preview(bytes: &[u8], more: bool) -> Result<TextPreview, String> {
    let (text, encoding, mut truncated) = if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        (utf16(rest, u16::from_le_bytes), "UTF-16 LE", more)
    } else if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        (utf16(rest, u16::from_be_bytes), "UTF-16 BE", more)
    } else {
        let body = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
        if body.iter().take(8192).any(|byte| *byte == 0) {
            return Err("Binary file".into());
        }
        match std::str::from_utf8(body) {
            Ok(text) => (text.to_owned(), "UTF-8", more),
            // A multi-byte character may be cut at the prefix boundary.
            Err(error) if more && body.len() - error.valid_up_to() < 4 && error.error_len().is_none() => {
                (std::str::from_utf8(&body[..error.valid_up_to()]).unwrap().to_owned(), "UTF-8", true)
            }
            Err(_) => (system_codepage(body), "ANSI", more),
        }
    };
    if text.chars().take(4096).filter(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t' | '\u{c}')).count() > 32 {
        return Err("Binary file".into());
    }
    let mut lines = Vec::new();
    let mut source = text.split('\n').peekable();
    while let Some(line) = source.next() {
        // The last partial line of a cut prefix is dropped, not shown broken.
        if more && source.peek().is_none() && !lines.is_empty() {
            truncated = true;
            break;
        }
        if lines.len() == MAX_LINES {
            truncated = true;
            break;
        }
        let line = line.strip_suffix('\r').unwrap_or(line).replace('\t', "    ");
        lines.push(if line.chars().count() > MAX_LINE_CHARS {
            truncated = true;
            line.chars().take(MAX_LINE_CHARS).chain(['…']).collect()
        } else {
            line
        });
    }
    while lines.len() > 1 && lines.last().is_some_and(|line| line.is_empty()) && !truncated {
        lines.pop();
    }
    Ok(TextPreview { lines, language: Language::Plain, truncated, size: 0, encoding })
}

fn utf16(bytes: &[u8], read: fn([u8; 2]) -> u16) -> String {
    let units = bytes.chunks_exact(2).map(|pair| read([pair[0], pair[1]])).collect::<Vec<_>>();
    String::from_utf16_lossy(&units)
}

#[cfg(windows)]
fn system_codepage(bytes: &[u8]) -> String {
    use crate::windows_api::Win32::Globalization::{MultiByteToWideChar, CP_ACP, MULTI_BYTE_TO_WIDE_CHAR_FLAGS};
    let length = unsafe { MultiByteToWideChar(CP_ACP, MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0), bytes, None) };
    if length <= 0 {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let mut wide = vec![0u16; length as usize];
    let written = unsafe { MultiByteToWideChar(CP_ACP, MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0), bytes, Some(&mut wide)) };
    String::from_utf16_lossy(&wide[..written.max(0) as usize])
}
#[cfg(not(windows))]
fn system_codepage(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Token {
    Keyword,
    String,
    Comment,
    Number,
    /// Tags, headings, section headers, keys and types.
    Name,
    /// Attributes, variables, preprocessor and decorators.
    Meta,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Carry {
    #[default]
    None,
    Block,
    Triple(char),
    Fence,
}

struct Spec {
    line: &'static [&'static str],
    block: Option<(&'static str, &'static str)>,
    quotes: &'static [char],
    triple: bool,
    keywords: &'static [&'static str],
    ignore_case: bool,
    variable: Option<char>,
    meta_line: Option<char>,
}

const C_KEYWORDS: &[&str] = &[
    "abstract", "async", "await", "break", "case", "catch", "class", "const", "continue", "default",
    "delete", "do", "else", "enum", "export", "extends", "false", "final", "finally", "for", "fn",
    "func", "function", "go", "if", "implements", "import", "in", "instanceof", "interface", "internal",
    "is", "let", "namespace", "new", "null", "nil", "override", "package", "private", "protected",
    "public", "readonly", "return", "sealed", "static", "struct", "super", "switch", "this", "throw",
    "true", "try", "type", "typeof", "using", "var", "virtual", "void", "while", "yield", "val",
    "int", "long", "bool", "boolean", "char", "float", "double", "string", "auto", "unsigned",
    "signed", "short", "byte", "object", "dynamic", "operator", "template", "typename", "where",
    "from", "as", "of", "get", "set", "defer", "chan", "map", "range", "select", "undefined",
];
const RUST_KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true", "type", "unsafe",
    "use", "where", "while", "Some", "None", "Ok", "Err", "bool", "u8", "u16", "u32", "u64", "usize",
    "i8", "i16", "i32", "i64", "isize", "f32", "f64", "str", "char", "String", "Vec", "Option", "Result",
];
const PY_KEYWORDS: &[&str] = &[
    "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif",
    "else", "except", "False", "finally", "for", "from", "global", "if", "import", "in", "is",
    "lambda", "None", "nonlocal", "not", "or", "pass", "raise", "return", "True", "try", "while",
    "with", "yield", "self", "print", "match", "case",
];
const SHELL_KEYWORDS: &[&str] = &[
    "if", "then", "else", "elif", "fi", "for", "in", "do", "done", "while", "until", "case", "esac",
    "function", "return", "export", "local", "echo", "exit", "set", "unset", "source", "readonly",
    "FROM", "RUN", "COPY", "ADD", "CMD", "ENTRYPOINT", "ENV", "ARG", "WORKDIR", "EXPOSE", "USER",
];
const BATCH_KEYWORDS: &[&str] = &[
    "echo", "set", "if", "else", "goto", "call", "exit", "for", "in", "do", "not", "exist",
    "defined", "errorlevel", "setlocal", "endlocal", "pushd", "popd", "start", "shift", "off",
];
const PS_KEYWORDS: &[&str] = &[
    "begin", "break", "catch", "class", "continue", "data", "do", "dynamicparam", "else", "elseif",
    "end", "exit", "filter", "finally", "for", "foreach", "function", "if", "in", "param", "process",
    "return", "switch", "throw", "trap", "try", "until", "using", "while", "true", "false", "null",
];
const SQL_KEYWORDS: &[&str] = &[
    "select", "from", "where", "and", "or", "not", "insert", "into", "values", "update", "set",
    "delete", "create", "table", "drop", "alter", "index", "join", "left", "right", "inner", "outer",
    "on", "group", "by", "order", "having", "limit", "offset", "as", "distinct", "null", "is", "in",
    "like", "between", "union", "all", "case", "when", "then", "else", "end", "primary", "key",
    "foreign", "references", "default", "exists", "view", "with", "begin", "commit", "rollback",
];
const LUA_KEYWORDS: &[&str] = &[
    "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "goto", "if", "in",
    "local", "nil", "not", "or", "repeat", "return", "then", "true", "until", "while",
];
const JSON_KEYWORDS: &[&str] = &["true", "false", "null"];

fn spec(language: Language) -> Option<Spec> {
    let base = Spec {
        line: &[],
        block: None,
        quotes: &['"', '\''],
        triple: false,
        keywords: &[],
        ignore_case: false,
        variable: None,
        meta_line: None,
    };
    Some(match language {
        Language::CLike => Spec { line: &["//"], block: Some(("/*", "*/")), quotes: &['"', '\'', '`'],
            keywords: C_KEYWORDS, meta_line: Some('#'), ..base },
        Language::Rust => Spec { line: &["//"], block: Some(("/*", "*/")), quotes: &['"'],
            keywords: RUST_KEYWORDS, meta_line: Some('#'), ..base },
        Language::Python => Spec { line: &["#"], triple: true, keywords: PY_KEYWORDS, meta_line: Some('@'), ..base },
        Language::Shell => Spec { line: &["#"], keywords: SHELL_KEYWORDS, variable: Some('$'), ..base },
        Language::Batch => Spec { line: &["::", "rem ", "REM "], quotes: &['"'], keywords: BATCH_KEYWORDS,
            ignore_case: true, variable: Some('%'), ..base },
        Language::PowerShell => Spec { line: &["#"], block: Some(("<#", "#>")), keywords: PS_KEYWORDS,
            ignore_case: true, variable: Some('$'), ..base },
        Language::Json => Spec { line: &["//"], block: Some(("/*", "*/")), quotes: &['"'], keywords: JSON_KEYWORDS, ..base },
        Language::Sql => Spec { line: &["--"], block: Some(("/*", "*/")), keywords: SQL_KEYWORDS, ignore_case: true, ..base },
        Language::Css => Spec { block: Some(("/*", "*/")), meta_line: Some('@'), ..base },
        Language::Lua => Spec { line: &["--"], keywords: LUA_KEYWORDS, ..base },
        _ => return None,
    })
}

/// Byte ranges per line. Block comments, triple strings and Markdown fences
/// carry across lines; everything else restarts per line.
pub fn highlight(language: Language, lines: &[String]) -> Vec<Vec<(Range<usize>, Token)>> {
    let mut carry = Carry::None;
    lines
        .iter()
        .map(|line| match language {
            Language::Plain => Vec::new(),
            Language::Markdown => markdown_line(line, &mut carry),
            Language::Markup => markup_line(line, &mut carry),
            Language::Ini | Language::Yaml => config_line(line, language),
            _ => spec(language).map(|spec| code_line(line, &spec, &mut carry)).unwrap_or_default(),
        })
        .collect()
}

fn code_line(line: &str, spec: &Spec, carry: &mut Carry) -> Vec<(Range<usize>, Token)> {
    let bytes = line.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0;
    match *carry {
        Carry::Block => {
            let close = spec.block.map_or("*/", |(_, close)| close);
            match line.find(close) {
                Some(end) => {
                    spans.push((0..end + close.len(), Token::Comment));
                    i = end + close.len();
                    *carry = Carry::None;
                }
                None => return vec![(0..line.len(), Token::Comment)],
            }
        }
        Carry::Triple(quote) => {
            let close = [quote as u8; 3];
            match line.as_bytes().windows(3).position(|window| window == close) {
                Some(end) => {
                    spans.push((0..end + 3, Token::String));
                    i = end + 3;
                    *carry = Carry::None;
                }
                None => return vec![(0..line.len(), Token::String)],
            }
        }
        _ => {}
    }
    if i == 0 {
        if let Some(meta) = spec.meta_line {
            let trimmed = line.trim_start();
            if trimmed.starts_with(meta) {
                return vec![(line.len() - trimmed.len()..line.len(), Token::Meta)];
            }
        }
    }
    while i < bytes.len() {
        let rest = &line[i..];
        if let Some(marker) = spec.line.iter().find(|marker| {
            if spec.ignore_case { rest.to_ascii_lowercase().starts_with(&marker.to_ascii_lowercase()) } else { rest.starts_with(**marker) }
        }) {
            // `rem` only counts at a statement start.
            if !marker.trim().eq_ignore_ascii_case("rem") || line[..i].trim().is_empty() {
                spans.push((i..line.len(), Token::Comment));
                break;
            }
        }
        if let Some((open, close)) = spec.block {
            if rest.starts_with(open) {
                match rest[open.len()..].find(close) {
                    Some(end) => {
                        let stop = i + open.len() + end + close.len();
                        spans.push((i..stop, Token::Comment));
                        i = stop;
                        continue;
                    }
                    None => {
                        spans.push((i..line.len(), Token::Comment));
                        *carry = Carry::Block;
                        break;
                    }
                }
            }
        }
        let c = rest.chars().next().unwrap();
        if spec.quotes.contains(&c) {
            if spec.triple && rest.starts_with(&c.to_string().repeat(3)) {
                match rest[3..].find(&c.to_string().repeat(3)) {
                    Some(end) => {
                        let stop = i + 6 + end;
                        spans.push((i..stop, Token::String));
                        i = stop;
                        continue;
                    }
                    None => {
                        spans.push((i..line.len(), Token::String));
                        *carry = Carry::Triple(c);
                        break;
                    }
                }
            }
            let stop = string_end(line, i, c);
            spans.push((i..stop, Token::String));
            i = stop;
            continue;
        }
        if Some(c) == spec.variable {
            let stop = i + c.len_utf8() + word_len(&line[i + c.len_utf8()..]);
            if stop > i + c.len_utf8() {
                spans.push((i..stop, Token::Meta));
                i = stop;
                continue;
            }
        }
        if c.is_ascii_digit() && !previous_is_word(line, i) {
            let stop = i + rest
                .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '.' || ch == '_'))
                .unwrap_or(rest.len());
            spans.push((i..stop, Token::Number));
            i = stop;
            continue;
        }
        if c.is_alphabetic() || c == '_' {
            let len = word_len(rest);
            let word = &rest[..len];
            let keyword = if spec.ignore_case {
                spec.keywords.iter().any(|keyword| keyword.eq_ignore_ascii_case(word))
            } else {
                spec.keywords.contains(&word)
            };
            if keyword {
                spans.push((i..i + len, Token::Keyword));
            } else if !spec.keywords.is_empty() && word.chars().next().is_some_and(char::is_uppercase)
                && word.chars().any(char::is_lowercase)
            {
                spans.push((i..i + len, Token::Name));
            }
            i += len.max(1);
            continue;
        }
        i += c.len_utf8();
    }
    spans
}

fn string_end(line: &str, start: usize, quote: char) -> usize {
    let mut escaped = false;
    for (offset, c) in line[start + quote.len_utf8()..].char_indices() {
        if escaped {
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == quote {
            return start + quote.len_utf8() + offset + c.len_utf8();
        }
    }
    line.len()
}
fn word_len(text: &str) -> usize {
    text.find(|c: char| !(c.is_alphanumeric() || c == '_')).unwrap_or(text.len())
}
fn previous_is_word(line: &str, index: usize) -> bool {
    line[..index].chars().next_back().is_some_and(|c| c.is_alphanumeric() || c == '_')
}

fn config_line(line: &str, language: Language) -> Vec<(Range<usize>, Token)> {
    let trimmed = line.trim_start();
    let indent = line.len() - trimmed.len();
    if trimmed.starts_with('#') || (language == Language::Ini && trimmed.starts_with(';')) {
        return vec![(indent..line.len(), Token::Comment)];
    }
    if language == Language::Ini && trimmed.starts_with('[') {
        return vec![(indent..line.len(), Token::Name)];
    }
    let separator = if language == Language::Yaml { ':' } else { '=' };
    let mut spans = Vec::new();
    let body = trimmed.strip_prefix("- ").map(|body| (indent + 2, body)).unwrap_or((indent, trimmed));
    if let Some(at) = body.1.find(separator).filter(|at| *at > 0) {
        spans.push((body.0..body.0 + at, Token::Name));
        let value = body.0 + at + 1;
        let rest = line[value..].trim_start();
        let start = line.len() - rest.len();
        if rest.starts_with('"') || rest.starts_with('\'') {
            spans.push((start..string_end(line, start, rest.chars().next().unwrap()), Token::String));
        } else if rest.parse::<f64>().is_ok() || matches!(rest, "true" | "false" | "null" | "yes" | "no") {
            spans.push((start..line.len(), Token::Number));
        }
        if let Some(hash) = line[value..].find(" #") {
            spans.push((value + hash + 1..line.len(), Token::Comment));
        }
    }
    spans
}

fn markdown_line(line: &str, carry: &mut Carry) -> Vec<(Range<usize>, Token)> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
        *carry = if *carry == Carry::Fence { Carry::None } else { Carry::Fence };
        return vec![(0..line.len(), Token::Meta)];
    }
    if *carry == Carry::Fence {
        return vec![(0..line.len(), Token::String)];
    }
    if trimmed.starts_with('#') {
        return vec![(0..line.len(), Token::Name)];
    }
    if trimmed.starts_with('>') {
        return vec![(0..line.len(), Token::Comment)];
    }
    let mut spans = Vec::new();
    let mut search = 0;
    while let Some(start) = line[search..].find('`').map(|at| search + at) {
        let Some(end) = line[start + 1..].find('`').map(|at| start + 1 + at) else { break };
        spans.push((start..end + 1, Token::String));
        search = end + 1;
    }
    if let Some(marker) = ["- ", "* ", "+ "].iter().find(|marker| trimmed.starts_with(**marker)) {
        let start = line.len() - trimmed.len();
        spans.insert(0, (start..start + marker.len() - 1, Token::Keyword));
    }
    spans
}

fn markup_line(line: &str, carry: &mut Carry) -> Vec<(Range<usize>, Token)> {
    let mut spans = Vec::new();
    let mut i = 0;
    if *carry == Carry::Block {
        match line.find("-->") {
            Some(end) => {
                spans.push((0..end + 3, Token::Comment));
                i = end + 3;
                *carry = Carry::None;
            }
            None => return vec![(0..line.len(), Token::Comment)],
        }
    }
    while let Some(open) = line[i..].find('<').map(|at| i + at) {
        if line[open..].starts_with("<!--") {
            match line[open + 4..].find("-->") {
                Some(end) => {
                    let stop = open + 4 + end + 3;
                    spans.push((open..stop, Token::Comment));
                    i = stop;
                    continue;
                }
                None => {
                    spans.push((open..line.len(), Token::Comment));
                    *carry = Carry::Block;
                    return spans;
                }
            }
        }
        let name_start = open + 1 + line[open + 1..].find(|c: char| c != '/' && c != '?' && c != '!').unwrap_or(0);
        let name_len = line[name_start..]
            .find(|c: char| !(c.is_alphanumeric() || matches!(c, '-' | '_' | ':' | '.')))
            .unwrap_or(line.len() - name_start);
        if name_len == 0 {
            i = open + 1;
            continue;
        }
        spans.push((name_start..name_start + name_len, Token::Name));
        let close = line[name_start..].find('>').map(|at| name_start + at).unwrap_or(line.len());
        let mut j = name_start + name_len;
        while j < close {
            let c = line[j..].chars().next().unwrap();
            if c == '"' || c == '\'' {
                let stop = string_end(line, j, c).min(close.max(j + 1));
                spans.push((j..stop, Token::String));
                j = stop;
            } else if c.is_alphabetic() {
                let len = line[j..]
                    .find(|c: char| !(c.is_alphanumeric() || matches!(c, '-' | '_' | ':' | '.')))
                    .unwrap_or(close - j);
                spans.push((j..j + len, Token::Meta));
                j += len.max(1);
            } else {
                j += c.len_utf8();
            }
        }
        i = (close + 1).min(line.len());
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    fn tokens(language: Language, text: &str) -> Vec<Vec<(String, Token)>> {
        let lines = text.lines().map(str::to_owned).collect::<Vec<_>>();
        highlight(language, &lines)
            .into_iter()
            .zip(&lines)
            .map(|(spans, line)| spans.into_iter().map(|(range, token)| (line[range].to_owned(), token)).collect())
            .collect()
    }

    #[test]
    fn decode_rejects_binary_and_keeps_utf8_cut_at_the_prefix_boundary() {
        assert!(decode_preview(b"MZ\0\0\x90\0", false).is_err());
        let text = "第一行\n第二行很长".as_bytes();
        let cut = &text[..text.len() - 1];
        let preview = decode_preview(cut, true).unwrap();
        assert_eq!(preview.lines, vec!["第一行"]);
        assert!(preview.truncated);
        let preview = decode_preview("a\r\nb\tc\n".as_bytes(), false).unwrap();
        assert_eq!(preview.lines, vec!["a", "b    c"]);
        assert!(!preview.truncated);
        let utf16 = [0xFF, 0xFE, b'h', 0, b'i', 0];
        assert_eq!(decode_preview(&utf16, false).unwrap().lines, vec!["hi"]);
    }

    #[test]
    fn decode_limits_lines_and_line_length() {
        let many = "x\n".repeat(MAX_LINES + 10);
        let preview = decode_preview(many.as_bytes(), false).unwrap();
        assert_eq!(preview.lines.len(), MAX_LINES);
        assert!(preview.truncated);
        let long = "y".repeat(MAX_LINE_CHARS + 5);
        let preview = decode_preview(long.as_bytes(), false).unwrap();
        assert_eq!(preview.lines[0].chars().count(), MAX_LINE_CHARS + 1);
    }

    #[test]
    fn read_preview_never_reads_past_the_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.log");
        let line = "0123456789abcdef\n";
        std::fs::write(&path, line.repeat(READ_BYTES / line.len() * 3)).unwrap();
        let preview = read_preview(&path).unwrap();
        assert!(preview.truncated);
        assert!(preview.size > READ_BYTES as u64 * 2);
        assert_eq!(preview.lines.len(), MAX_LINES);
        assert_eq!(preview.language, Language::Plain);
    }

    #[test]
    fn language_comes_from_extension_name_or_content() {
        assert_eq!(detect_language(Path::new("a.RS"), ""), Language::Rust);
        assert_eq!(detect_language(Path::new("Dockerfile"), ""), Language::Shell);
        assert_eq!(detect_language(Path::new("notes"), "#!/usr/bin/env python3\n"), Language::Python);
        assert_eq!(detect_language(Path::new("data.txt"), "{\"a\": 1}"), Language::Json);
        assert_eq!(detect_language(Path::new("readme"), "hello"), Language::Plain);
        assert!(language_for_extension("exe").is_none());
    }

    #[test]
    fn code_lexer_marks_keywords_strings_comments_and_block_carry() {
        let lines = tokens(Language::Rust, "let s = \"a // b\"; // tail\n/* open\nstill */ fn x() {}");
        assert_eq!(lines[0], vec![("let".into(), Token::Keyword), ("\"a // b\"".into(), Token::String), ("// tail".into(), Token::Comment)]);
        assert_eq!(lines[1], vec![("/* open".into(), Token::Comment)]);
        assert_eq!(lines[2][0], ("still */".into(), Token::Comment));
        assert_eq!(lines[2][1], ("fn".into(), Token::Keyword));
        let python = tokens(Language::Python, "def f():\n    \"\"\"doc\n    end\"\"\" # c");
        assert_eq!(python[0][0], ("def".into(), Token::Keyword));
        assert_eq!(python[1][0].1, Token::String);
        assert_eq!(python[2], vec![("    end\"\"\"".into(), Token::String), ("# c".into(), Token::Comment)]);
    }

    #[test]
    fn config_markdown_and_markup_lines() {
        assert_eq!(tokens(Language::Ini, "[core]\nname = \"x\"")[1],
            vec![("name ".into(), Token::Name), ("\"x\"".into(), Token::String)]);
        assert_eq!(tokens(Language::Markdown, "# Title\n```rs\nlet a\n```")[2], vec![("let a".into(), Token::String)]);
        let markup = tokens(Language::Markup, "<a href=\"x\"><!-- c --></a>");
        assert_eq!(markup[0][0], ("a".into(), Token::Name));
        assert_eq!(markup[0][1], ("href".into(), Token::Meta));
        assert_eq!(markup[0][2], ("\"x\"".into(), Token::String));
        assert_eq!(markup[0][3], ("<!-- c -->".into(), Token::Comment));
    }
}
