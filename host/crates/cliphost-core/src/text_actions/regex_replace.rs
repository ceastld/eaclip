//! .NET replacement strings for the supported Rust regex pattern subset.
//! Rust's native replacement syntax differs for numeric and undefined groups,
//! and has no equivalents for $&, $`, $', $+, or $_.

use super::regex_pattern::Compiled;
use regex_automata::Span;

enum Part<'a> {
    Literal(&'a str),
    Group(usize),
    Before,
    After,
    Input,
}

pub(super) fn replace(
    compiled: &Compiled,
    input: &str,
    replacement: &str,
    reverse: bool,
    ecmascript: bool,
) -> Result<String, String> {
    // Use the same sparse .NET numbering as pattern references and conditions.
    let names: Vec<_> = compiled.names.iter().map(|name| name.as_deref()).collect();
    let numbered = &compiled.numbers;
    let resolve_group = |name: &str| {
        if !name.is_empty() && name.bytes().all(|c| c.is_ascii_digit()) {
            name.parse::<usize>()
                .ok()
                .and_then(|n| numbered.get(&n).copied())
        } else {
            names.iter().position(|candidate| *candidate == Some(name))
        }
    };

    let mut parts = Vec::new();
    let mut cursor = 0;
    while let Some(relative) = replacement[cursor..].find('$') {
        let dollar = cursor + relative;
        parts.push(Part::Literal(&replacement[cursor..dollar]));
        let tail = &replacement[dollar + 1..];
        let token = match tail.as_bytes().first().copied() {
            Some(b'$') => Some((Part::Literal("$"), 1)),
            Some(b'&') => Some((Part::Group(0), 1)),
            Some(b'`') => Some((Part::Before, 1)),
            Some(b'\'') => Some((Part::After, 1)),
            Some(b'_') => Some((Part::Input, 1)),
            Some(b'+') => Some((Part::Group(*numbered.last_key_value().unwrap().1), 1)),
            Some(b'{') => {
                if let Some(end) = tail.find('}') {
                    let name = &tail[1..end];
                    if !name.is_empty() && name.bytes().all(|c| c.is_ascii_digit()) {
                        super::regex_ecmascript::decimal(name)?;
                    }
                    resolve_group(name).map(|slot| (Part::Group(slot), end + 1))
                } else {
                    None
                }
            }
            Some(b'0'..=b'9') => {
                let end = tail.bytes().take_while(u8::is_ascii_digit).count();
                super::regex_ecmascript::decimal(&tail[..end])?;
                if ecmascript {
                    let mut number = 0;
                    let mut found = None;
                    for (i, digit) in tail.bytes().take(end).enumerate() {
                        number = number * 10 + (digit - b'0') as usize;
                        if let Some(&slot) = numbered.get(&number) {
                            found = Some((Part::Group(slot), i + 1));
                        }
                    }
                    found
                } else {
                    resolve_group(&tail[..end]).map(|slot| (Part::Group(slot), end))
                }
            }
            _ => None,
        };
        if let Some((part, consumed)) = token {
            parts.push(part);
            cursor = dollar + 1 + consumed;
        } else {
            // A nonexistent reference is literal in .NET, not an empty group.
            parts.push(Part::Literal("$"));
            cursor = dollar + 1;
        }
    }
    parts.push(Part::Literal(&replacement[cursor..]));
    let reversed;
    let haystack = if reverse {
        reversed = input.chars().rev().collect::<String>();
        reversed.as_str()
    } else {
        input
    };
    let original_range = |span: Span| {
        if reverse {
            input.len() - span.end..input.len() - span.start
        } else {
            span.start..span.end
        }
    };
    let mut captures = compiled.searcher(haystack);
    let mut search_start = 0;
    let mut copied = if reverse { input.len() } else { 0 };
    // Build a single output buffer; a zero-width match at every character must
    // not allocate a separate String per match. Reverse the buffer once for RTL.
    let append = |output: &mut String, value: &str| {
        if reverse {
            output.extend(value.chars().rev());
        } else {
            output.push_str(value);
        }
    };
    let mut output = String::with_capacity(input.len());
    loop {
        captures.search(haystack, search_start)?;
        let Some(found) = captures.group(0) else {
            break;
        };
        let whole = original_range(found);
        append(
            &mut output,
            if reverse {
                &input[whole.end..copied]
            } else {
                &input[copied..whole.start]
            },
        );
        for offset in 0..parts.len() {
            let part = &parts[if reverse {
                parts.len() - 1 - offset
            } else {
                offset
            }];
            append(
                &mut output,
                match part {
                    Part::Literal(text) => text,
                    Part::Group(slot) => captures
                        .group(*slot)
                        .map_or("", |group| &input[original_range(group)]),
                    Part::Before => &input[..whole.start],
                    Part::After => &input[whole.end..],
                    Part::Input => input,
                },
            );
        }
        copied = if reverse { whole.start } else { whole.end };
        search_start = found.end;
        // .NET allows an empty match immediately after a nonempty match;
        // regex's built-in iterators suppress it. Advance only after empties.
        if found.start == found.end {
            let Some(next) = haystack[search_start..].chars().next() else {
                break;
            };
            search_start += next.len_utf8();
        }
    }
    append(
        &mut output,
        if reverse {
            &input[..copied]
        } else {
            &input[copied..]
        },
    );
    if reverse {
        Ok(output.chars().rev().collect())
    } else {
        Ok(output)
    }
}
