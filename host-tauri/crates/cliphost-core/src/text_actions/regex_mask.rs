//! Preserve .NET reference targets and conditional syntax before foreign parsing.
use std::collections::HashMap;

pub(super) enum Reference {
    Text(String),
    CaptureTest(String),
}

pub(super) struct Masked {
    pub pattern: String,
    pub references: Vec<Reference>,
    pub aliases: HashMap<String, String>,
    declarations: Vec<(Option<String>, usize)>,
}

enum Group {
    Ordinary,
    Condition,
    Branches(usize),
}

pub(super) fn mask(pattern: &str, prefix: &str, options: i32) -> Result<Masked, String> {
    let first = mask_pass(pattern, prefix, options, None)?;
    let declarations = super::regex_ecmascript::Declarations::new(&first.declarations)?;
    mask_pass(pattern, prefix, options, Some(&declarations))
}

fn mask_pass(
    pattern: &str,
    prefix: &str,
    options: i32,
    declared: Option<&super::regex_ecmascript::Declarations>,
) -> Result<Masked, String> {
    let mut extended = options & 32 != 0;
    let ecma = options & 256 != 0;
    let mut declarations = Vec::new();
    let mut output = String::new();
    let mut references = Vec::new();
    let mut aliases = HashMap::new();
    let mut groups = Vec::new();
    let mut class_depth = 0usize;
    let mut i = 0;
    while i < pattern.len() {
        let tail = &pattern[i..];
        let c = tail.chars().next().unwrap();
        if class_depth == 0 && (tail.starts_with("(?#") || (extended && c == '#')) {
            let end = if c == '#' {
                i + tail.find('\n').unwrap_or(tail.len())
            } else {
                i + tail.find(')').ok_or("正则注释缺少结束符。")? + 1
            };
            output.push_str(&pattern[i..end]);
            i = end;
            continue;
        }
        if class_depth == 0 && tail.starts_with("(?(") {
            groups.push((Group::Branches(0), extended));
            let start = i + 3;
            let target = pattern[start..]
                .find(')')
                .map(|end| &pattern[start..start + end]);
            if let Some(target) = target.filter(|name| is_capture_name(name) || is_number(name)) {
                // Synthetic captures retain the original target and case flag;
                // they never become observable captures in the compiled program.
                output.push_str(&format!("(?((?<{prefix}{}>a))(?=)", references.len()));
                references.push(Reference::CaptureTest(target.to_string()));
                i = start + target.len() + 1;
            } else {
                // fancy-regex requires an extra pair of parentheses around an
                // explicit assertion. Bare .NET predicates keep the surrounding
                // direction; only explicit lookahead/lookbehind set their own.
                output.push_str(if pattern[start..].starts_with('?') {
                    "(?(("
                } else {
                    "(?((?:"
                });
                groups.push((Group::Condition, extended));
                i = start;
            }
            continue;
        }
        let name_close = if tail.starts_with("(?<")
            && !matches!(pattern.as_bytes().get(i + 3), Some(b'=' | b'!'))
        {
            Some('>')
        } else if tail.starts_with("(?'") {
            Some('\'')
        } else {
            None
        };
        if let Some(close) = name_close.filter(|_| class_depth == 0) {
            let start = i + 3;
            if let Some(end) = pattern[start..].find(close) {
                let name = &pattern[start..start + end];
                if is_capture_name(name) || is_number(name) || balancing_name(name).is_some() {
                    let destination =
                        balancing_name(name).map_or(name, |(destination, _)| destination);
                    if !destination.is_empty() {
                        declarations.push((Some(destination.to_string()), i));
                    }
                    let alias = format!("{prefix}Group{}", aliases.len());
                    output.push_str(&format!("(?<{alias}>"));
                    aliases.insert(alias, name.to_string());
                    groups.push((Group::Ordinary, extended));
                    i = start + end + 1;
                    continue;
                }
            }
        }
        if c == '\\' {
            let start = i;
            i += 1;
            let Some(next) = pattern[i..].chars().next() else {
                output.push('\\');
                break;
            };
            let target = if class_depth == 0 && ('1'..='9').contains(&next) {
                let count = pattern[i..].bytes().take_while(u8::is_ascii_digit).count();
                let digits = &pattern[i..i + count];
                if let Some(declared) = declared {
                    if let Some((target, consumed)) = declared.reference(digits, start, ecma)? {
                        i += consumed;
                        Some(target)
                    } else {
                        if !ecma && digits.len() == 1 {
                            return Err(format!("反向引用的分组不存在：{digits}"));
                        }
                        let (value, consumed) = super::regex_ecmascript::octal(digits, ecma);
                        if consumed != 0 {
                            output.push_str(&format!("\\x{{{value:x}}}"));
                            i += consumed;
                        } else if ecma {
                            output.push_str(&format!("\\x{{{:x}}}", next as u32));
                            i += next.len_utf8();
                        } else {
                            return Err("正则八进制转义无效，已停止替换。".into());
                        }
                        continue;
                    }
                } else {
                    i += count;
                    Some(digits.to_string())
                }
            } else if class_depth == 0
                && next == 'k'
                && matches!(pattern.as_bytes().get(i + 1), Some(b'<' | b'\''))
            {
                let close = if pattern.as_bytes()[i + 1] == b'<' {
                    '>'
                } else {
                    '\''
                };
                let body = i + 2;
                let end = body + pattern[body..].find(close).ok_or("反向引用缺少结束符。")?;
                let value = pattern[body..end].to_string();
                i = end + 1;
                Some(value)
            } else {
                None
            };
            if let Some(target) = target {
                output.push_str(&format!("(?<{prefix}{}>a)", references.len()));
                references.push(Reference::Text(target));
            } else {
                i += next.len_utf8();
                output.push_str(&pattern[start..i]);
            }
            continue;
        }
        if class_depth == 0 {
            if c == '(' {
                if let Some((end, next_extended, scoped)) = inline_flags(pattern, i, extended) {
                    if scoped {
                        groups.push((Group::Ordinary, extended));
                    }
                    extended = next_extended;
                    output.push_str(&pattern[i..end]);
                    i = end;
                    continue;
                }
                if !tail.starts_with("(?") && options & 4 == 0 {
                    declarations.push((None, i));
                }
                groups.push((Group::Ordinary, extended));
            } else if c == ')' {
                if let Some((group, previous_extended)) = groups.pop() {
                    extended = previous_extended;
                    if matches!(group, Group::Condition) {
                        // Keep even an empty yes branch in the parser's AST;
                        // otherwise it can collapse the whole conditional.
                        output.push_str("))(?=)");
                        i += 1;
                        continue;
                    }
                }
            } else if c == '|' {
                if let Some((Group::Branches(count), _)) = groups.last_mut() {
                    *count += 1;
                    if *count > 1 {
                        return Err("条件组只能包含两个分支，已停止替换。".into());
                    }
                }
            }
        }
        if c == '[' {
            class_depth += 1;
        }
        if c == ']' {
            class_depth = class_depth.saturating_sub(1);
        }
        output.push(c);
        i += c.len_utf8();
    }
    Ok(Masked {
        pattern: output,
        references,
        aliases,
        declarations,
    })
}

pub(super) fn inline_flags(
    pattern: &str,
    i: usize,
    mut extended: bool,
) -> Option<(usize, bool, bool)> {
    if !pattern[i..].starts_with("(?") {
        return None;
    }
    let mut enable = true;
    let mut seen = false;
    for (offset, byte) in pattern.as_bytes()[i + 2..].iter().copied().enumerate() {
        match byte {
            b'i' | b'm' | b'n' | b's' | b'x' => {
                seen = true;
                if byte == b'x' {
                    extended = enable;
                }
            }
            b'-' => enable = false,
            b':' | b')' if seen => return Some((i + 3 + offset, extended, byte == b':')),
            _ => return None,
        }
    }
    None
}

pub(super) fn is_number(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit())
}

pub(super) fn balancing_name(name: &str) -> Option<(&str, &str)> {
    let (destination, source) = name.split_once('-')?;
    ((destination.is_empty() || is_capture_name(destination) || is_number(destination))
        && (is_capture_name(source) || is_number(source)))
    .then_some((destination, source))
}

pub(super) fn is_capture_name(name: &str) -> bool {
    static NAME: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\A[\p{L}\p{Mn}\p{Pc}][\p{L}\p{Mn}\p{Nd}\p{Pc}]*\z").unwrap()
    });
    NAME.is_match(name)
}
