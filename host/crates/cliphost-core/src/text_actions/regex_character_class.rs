//! Translate .NET [] grammar before either regex engine can reinterpret it.
//! Literal brackets/set operators and subtraction order differ from Rust's
//! nested classes. Emit scalar escapes and an explicit base-minus-exclusion.
use std::fmt::Write;

const INVALID: &str = "正则字符类无效，已停止替换。";

fn boundary_word(c: char) -> bool {
    static WORD: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\A[\p{L}\p{Mn}\p{Nd}\p{Pc}\u{200c}\u{200d}]\z").unwrap()
    });
    WORD.is_match(c.encode_utf8(&mut [0; 4]))
}

fn category(name: &str) -> bool {
    matches!(
        name,
        "L" | "Lu"
            | "Ll"
            | "Lt"
            | "Lm"
            | "Lo"
            | "M"
            | "Mn"
            | "Mc"
            | "Me"
            | "N"
            | "Nd"
            | "Nl"
            | "No"
            | "P"
            | "Pc"
            | "Pd"
            | "Ps"
            | "Pe"
            | "Pi"
            | "Pf"
            | "Po"
            | "S"
            | "Sm"
            | "Sc"
            | "Sk"
            | "So"
            | "Z"
            | "Zs"
            | "Zl"
            | "Zp"
            | "C"
            | "Cc"
            | "Cf"
            | "Cs"
            | "Co"
            | "Cn"
    )
}

pub(super) fn translate(pattern: &str, options: i32) -> Result<String, String> {
    let mut extended = options & 32 != 0;
    let mut casei = options & 1 != 0;
    let ecma = options & 256 != 0;
    let invariant = options & 512 != 0;
    let mut output = String::with_capacity(pattern.len());
    let mut scopes = Vec::new();
    let mut i = 0;
    while i < pattern.len() {
        let tail = &pattern[i..];
        let c = tail.chars().next().unwrap();
        if tail.starts_with("(?#") || (extended && c == '#') {
            let end = if c == '#' {
                i + tail.find('\n').unwrap_or(tail.len())
            } else {
                i + tail.find(')').ok_or(INVALID)? + 1
            };
            output.push_str(&pattern[i..end]);
            i = end;
        } else if c == '[' {
            if ecma {
                ecma_class(pattern, &mut i, &mut output, casei, invariant)?;
            } else {
                class(pattern, &mut i, &mut output)?;
            }
        } else if c == '\\' {
            let escaped = pattern[i + 1..].chars().next().ok_or(INVALID)?;
            if ecma && super::regex_ecmascript::shorthand(escaped).is_some() {
                write!(
                    output,
                    "(?-i:{})",
                    super::regex_ecmascript::shorthand(escaped).unwrap()
                )
                .unwrap();
                i += 2;
            } else if ecma && matches!(escaped, 'b' | 'B') {
                output.push_str(&super::regex_ecmascript::boundary(escaped == 'B'));
                i += 2;
            } else if matches!(escaped, '0' | 'x' | 'u' | 'c' | 'a' | 'e')
                || (ecma && !matches!(escaped, '1'..='9' | 'k' | 'p' | 'P' | 'A' | 'G' | 'Z' | 'z'))
            {
                let Atom::Scalar(value, _) = atom(pattern, &mut i, ecma)? else {
                    return Err(INVALID.into());
                };
                if ecma && casei {
                    output.push_str(&super::regex_ecmascript::literal(
                        char::from_u32(value).ok_or(INVALID)?,
                        invariant,
                    ));
                } else {
                    scalar(&mut output, value);
                }
            } else if ecma
                && escaped == 'k'
                && matches!(pattern.as_bytes().get(i + 2), Some(b'<' | b'\''))
            {
                let close = if pattern.as_bytes()[i + 2] == b'<' {
                    '>'
                } else {
                    '\''
                };
                let end = i + 3 + pattern[i + 3..].find(close).ok_or(INVALID)? + 1;
                output.push_str(&pattern[i..end]);
                i = end;
            } else if ecma && matches!(escaped, 'p' | 'P') && pattern[i + 2..].starts_with('{') {
                let end = i + 3 + pattern[i + 3..].find('}').ok_or(INVALID)? + 1;
                output.push_str(&pattern[i..end]);
                i = end;
            } else {
                output.push('\\');
                output.push(escaped);
                i += 1 + escaped.len_utf8();
            }
        } else if c == '(' {
            let name_close = if tail.starts_with("(?<")
                && !matches!(pattern.as_bytes().get(i + 3), Some(b'=' | b'!'))
            {
                Some('>')
            } else if tail.starts_with("(?'") {
                Some('\'')
            } else {
                None
            };
            let condition = tail
                .starts_with("(?(")
                .then(|| pattern[i + 3..].find(')'))
                .flatten()
                .map(|n| &pattern[i + 3..i + 3 + n]);
            if ecma
                && (name_close.is_some()
                    || condition.is_some_and(|s| {
                        super::regex_mask::is_capture_name(s) || super::regex_mask::is_number(s)
                    }))
            {
                let close = name_close.unwrap_or(')');
                let end = i + 3 + pattern[i + 3..].find(close).ok_or(INVALID)? + 1;
                scopes.push((extended, casei));
                output.push_str(&pattern[i..end]);
                i = end;
            } else if let Some((end, next_extended, scoped)) =
                super::regex_mask::inline_flags(pattern, i, extended)
            {
                if scoped {
                    scopes.push((extended, casei));
                }
                let mut enable = true;
                for flag in pattern[i + 2..end - 1].chars() {
                    if flag == '-' {
                        enable = false;
                    } else if flag == 'i' {
                        casei = enable;
                    }
                }
                extended = next_extended;
                output.push_str(&pattern[i..end]);
                i = end;
            } else {
                scopes.push((extended, casei));
                output.push(c);
                i += 1;
            }
        } else {
            if c == ')' {
                if let Some(previous) = scopes.pop() {
                    (extended, casei) = previous;
                }
            }
            if ecma
                && casei
                && (c.to_lowercase().ne(std::iter::once(c))
                    || c.to_uppercase().ne(std::iter::once(c)))
            {
                output.push_str(&super::regex_ecmascript::literal(c, invariant));
            } else {
                output.push(c);
            }
            i += c.len_utf8();
        }
    }
    Ok(output)
}

enum Atom {
    Scalar(u32, bool), // An escaped hyphen cannot start a .NET range.
    Set(String),
}

fn scalar(output: &mut String, value: u32) {
    write!(output, "\\x{{{value:x}}}").unwrap();
}

fn open(pattern: &str, i: &mut usize, output: &mut String) {
    *i += 1; // '['
    output.push_str("[[");
    if pattern[*i..].starts_with('^') {
        *i += 1;
        output.push('^');
    }
}

fn class(pattern: &str, i: &mut usize, output: &mut String) -> Result<(), String> {
    open(pattern, i, output);
    let mut first = true;
    // Iterative nesting: adversarial subtraction cannot overflow the Rust stack.
    let mut parents = 0usize;
    loop {
        let tail = &pattern[*i..];
        if tail.is_empty() {
            return Err(INVALID.into());
        }
        if tail.starts_with(']') && !first {
            *i += 1;
            output.push_str("]]");
            while parents > 0 {
                if !pattern[*i..].starts_with(']') {
                    return Err(INVALID.into());
                }
                *i += 1;
                output.push(']');
                parents -= 1;
            }
            return Ok(());
        }
        if tail.starts_with("-[") && !first {
            *i += 1;
            output.push_str("]--");
            parents += 1;
            open(pattern, i, output);
            first = true;
            continue;
        }
        match atom(pattern, i, false)? {
            Atom::Set(set) => output.push_str(&set),
            Atom::Scalar(begin, range_start) => {
                let tail = &pattern[*i..];
                let range = range_start
                    && tail.starts_with('-')
                    && tail[1..].chars().next().is_some_and(|c| c != ']');
                if range {
                    *i += 1;
                    if pattern[*i..].starts_with('[') {
                        scalar(output, begin);
                        output.push_str("]--");
                        parents += 1;
                        open(pattern, i, output);
                        first = true;
                        continue;
                    }
                    let Atom::Scalar(end, _) = atom(pattern, i, false)? else {
                        return Err(INVALID.into());
                    };
                    if begin > end {
                        return Err(INVALID.into());
                    }
                    scalar(output, begin);
                    output.push('-');
                    scalar(output, end);
                } else {
                    scalar(output, begin);
                }
            }
        }
        first = false;
    }
}

// Each base class is case-expanded before its negation and subtraction. Rust's
// full folding of the final nested expression cannot preserve that ordering.
fn ecma_class(
    pattern: &str,
    i: &mut usize,
    output: &mut String,
    casei: bool,
    invariant: bool,
) -> Result<(), String> {
    struct Frame {
        base: String,
        negative: bool,
        first: bool,
    }
    fn open(pattern: &str, i: &mut usize) -> Frame {
        *i += 1;
        let negative = pattern[*i..].starts_with('^');
        if negative {
            *i += 1;
        }
        Frame {
            base: String::new(),
            negative,
            first: true,
        }
    }
    fn finish(frame: Frame, casei: bool, invariant: bool) -> Result<String, String> {
        let base = if frame.base.is_empty() {
            String::new()
        } else if casei {
            super::regex_ecmascript::fold_class(&frame.base, invariant)?
        } else {
            frame.base
        };
        if base.is_empty() {
            return if frame.negative {
                Ok(r"[\s\S]".into())
            } else {
                Err(INVALID.into())
            };
        }
        Ok(format!(
            "[{}{}]",
            if frame.negative { "^" } else { "" },
            base
        ))
    }
    let mut current = open(pattern, i);
    let mut parents = Vec::new();
    loop {
        let tail = &pattern[*i..];
        if tail.is_empty() {
            return Err(INVALID.into());
        }
        if tail.starts_with(']') && (!current.first || current.negative) {
            *i += 1;
            let mut result = finish(current, casei, invariant)?;
            while let Some(parent) = parents.pop() {
                if !pattern[*i..].starts_with(']') {
                    return Err(INVALID.into());
                }
                *i += 1;
                result = format!("[{}--{result}]", finish(parent, casei, invariant)?);
            }
            write!(output, "(?-i:{result})").unwrap();
            return Ok(());
        }
        if tail.starts_with("-[") && !current.first {
            *i += 1;
            parents.push(current);
            current = open(pattern, i);
            continue;
        }
        match atom(pattern, i, true)? {
            Atom::Set(set) => current.base.push_str(&set),
            Atom::Scalar(begin, range_start) => {
                let tail = &pattern[*i..];
                if range_start
                    && tail.starts_with('-')
                    && tail[1..].chars().next().is_some_and(|c| c != ']')
                {
                    *i += 1;
                    if pattern[*i..].starts_with('[') {
                        scalar(&mut current.base, begin);
                        current.first = false;
                        parents.push(current);
                        current = open(pattern, i);
                        continue;
                    }
                    let Atom::Scalar(end, _) = atom(pattern, i, true)? else {
                        return Err(INVALID.into());
                    };
                    if begin > end {
                        return Err(INVALID.into());
                    }
                    scalar(&mut current.base, begin);
                    current.base.push('-');
                    scalar(&mut current.base, end);
                } else {
                    scalar(&mut current.base, begin);
                }
            }
        }
        current.first = false;
    }
}

fn next(pattern: &str, i: &mut usize) -> Result<char, String> {
    let c = pattern[*i..].chars().next().ok_or(INVALID)?;
    *i += c.len_utf8();
    Ok(c)
}

fn hex(pattern: &str, i: &mut usize, count: usize) -> Result<u32, String> {
    let mut value = 0;
    for _ in 0..count {
        value = value * 16 + next(pattern, i)?.to_digit(16).ok_or(INVALID)?;
    }
    // Rust scalar engines cannot represent standalone UTF-16 surrogates.
    char::from_u32(value).ok_or(INVALID)?;
    Ok(value)
}

fn atom(pattern: &str, i: &mut usize, ecma: bool) -> Result<Atom, String> {
    let c = next(pattern, i)?;
    if c != '\\' {
        return Ok(Atom::Scalar(c as u32, true));
    }
    let escaped = next(pattern, i)?;
    if ecma {
        if let Some(set) = super::regex_ecmascript::shorthand(escaped) {
            return Ok(Atom::Set(set.into()));
        }
    }
    let value = match escaped {
        'w' => return Ok(Atom::Set(r"[\p{L}\p{Mn}\p{Nd}\p{Pc}]".into())),
        'W' => return Ok(Atom::Set(r"[^\p{L}\p{Mn}\p{Nd}\p{Pc}]".into())),
        'd' | 'D' | 's' | 'S' => return Ok(Atom::Set(format!("\\{escaped}"))),
        'p' | 'P' => {
            if next(pattern, i)? != '{' {
                return Err(INVALID.into());
            }
            let start = *i;
            let end = start + pattern[start..].find('}').ok_or(INVALID)?;
            let property = &pattern[start..end];
            if !category(property) && !property.starts_with("Is") {
                return Err(INVALID.into());
            }
            *i = end + 1;
            return Ok(Atom::Set(format!("\\{escaped}{{{property}}}")));
        }
        'x' => hex(pattern, i, 2)?,
        'u' => hex(pattern, i, 4)?,
        '0'..='7' => {
            let mut value = escaped.to_digit(8).unwrap();
            for _ in 0..2 {
                let Some(c) = pattern[*i..]
                    .chars()
                    .next()
                    .filter(|c| ('0'..='7').contains(c))
                else {
                    break;
                };
                if ecma && value >= 32 {
                    break;
                }
                *i += 1;
                value = value * 8 + c.to_digit(8).unwrap();
            }
            value & 0xff
        }
        'c' => {
            let c = next(pattern, i)?.to_ascii_uppercase() as u32;
            if !(0x40..=0x5f).contains(&c) {
                return Err(INVALID.into());
            }
            c - 0x40
        }
        'a' => 7,
        'b' => 8,
        'e' => 27,
        'f' => 12,
        'n' => 10,
        'r' => 13,
        't' => 9,
        'v' => 11,
        c if ecma || !boundary_word(c) => c as u32,
        _ => return Err(INVALID.into()),
    };
    Ok(Atom::Scalar(value, escaped != '-'))
}
