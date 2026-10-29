//! .NET ECMAScript classes and decimal-reference disambiguation.
//! Keep ordinary expressions in the linear engine; references use its bounded
//! capture evaluator. Nothing here adds observable capture groups.
use std::collections::BTreeMap;
use std::fmt::Write;

pub(super) const WORD: &str = r"[A-Za-z0-9_\x{130}]";

pub(super) fn shorthand(c: char) -> Option<&'static str> {
    Some(match c {
        'w' => WORD,
        'W' => r"[^A-Za-z0-9_\x{130}]",
        'd' => "[0-9]",
        'D' => "[^0-9]",
        's' => r"[\x{9}-\x{d}\x{20}]",
        'S' => r"[^\x{9}-\x{d}\x{20}]",
        _ => return None,
    })
}

pub(super) fn boundary(negative: bool) -> String {
    // .NET's ECMA boundary uses this fixed set even under IgnoreCase.
    if negative {
        format!("(?-i:(?:(?<={WORD})(?={WORD})|(?<!{WORD})(?!{WORD})))")
    } else {
        format!("(?-i:(?:(?<={WORD})(?!{WORD})|(?<!{WORD})(?={WORD})))")
    }
}

pub(super) struct Declarations {
    positions: BTreeMap<usize, usize>,
    top: usize,
}

impl Declarations {
    pub(super) fn new(declarations: &[(Option<String>, usize)]) -> Result<Self, String> {
        let mut names: Vec<Option<Box<str>>> = vec![None];
        let mut positions = vec![0];
        for (name, position) in declarations {
            if name
                .as_ref()
                .is_some_and(|name| names.iter().any(|n| n.as_deref() == Some(name)))
            {
                continue;
            }
            names.push(name.as_deref().map(Into::into));
            positions.push(*position);
        }
        let (numbers, aliases) = super::regex_captures::assign(&names)?;
        // An explicit number can alias an unnamed capture declared later. The
        // parser remembers the first declaration, not the final capture value.
        for (slot, alias) in aliases.into_iter().enumerate().skip(1) {
            positions[alias] = positions[alias].min(positions[slot]);
        }
        let positions: BTreeMap<_, _> = numbers
            .into_iter()
            .map(|(n, s)| (n, positions[s]))
            .collect();
        let top = (positions.last_key_value().unwrap().0 + 1).min(i32::MAX as usize);
        Ok(Self { positions, top })
    }

    pub(super) fn reference(
        &self,
        digits: &str,
        position: usize,
        ecma: bool,
    ) -> Result<Option<(String, usize)>, String> {
        if !ecma {
            let number = decimal(digits)?;
            return Ok(self
                .positions
                .contains_key(&number)
                .then(|| (digits.to_string(), digits.len())));
        }
        let mut number = (digits.as_bytes()[0] - b'0') as usize;
        let mut target = None;
        let mut consumed = 0;
        // Unlike replacement tokens, pattern parsing consumes intermediate
        // numbers up to the largest declared slot, including sparse gaps.
        while number <= self.top && consumed < digits.len() {
            if self.positions.get(&number).is_some_and(|&p| p < position) {
                target = Some(number.to_string());
            }
            consumed += 1;
            if let Some(digit) = digits.as_bytes().get(consumed) {
                number = number
                    .checked_mul(10)
                    .and_then(|n| n.checked_add((digit - b'0') as usize))
                    .ok_or("反向引用编号超过范围，已停止替换。")?;
            }
        }
        Ok(target.map(|target| (target, consumed)))
    }
}

pub(super) fn decimal(digits: &str) -> Result<usize, String> {
    digits
        .parse::<i32>()
        .map(|n| n as usize)
        .map_err(|_| "捕获组编号超过范围，已停止替换。".into())
}

pub(super) fn octal(digits: &str, ecma: bool) -> (u32, usize) {
    let mut value = 0;
    let mut consumed = 0;
    for digit in digits
        .bytes()
        .take(3)
        .take_while(|b| (b'0'..=b'7').contains(b))
    {
        value = value * 8 + (digit - b'0') as u32;
        consumed += 1;
        if ecma && value >= 32 {
            break;
        }
    }
    (value & 255, consumed)
}

pub(super) fn fold_class(base: &str, invariant: bool) -> Result<String, String> {
    use regex_syntax::hir::HirKind;
    // .NET uses Unicode simple lowercase equivalences, rather than Unicode
    // full case folding (which would incorrectly turn long-s into ASCII s).
    // Cache the small families once, then test only them against each class.
    let hir = regex_syntax::Parser::new()
        .parse(&format!("[{base}]"))
        .map_err(|e| e.to_string())?;
    let HirKind::Class(regex_syntax::hir::Class::Unicode(class)) = hir.kind() else {
        return Err("正则字符类无法转换，已停止替换。".into());
    };
    let contains = |c: char| {
        class
            .ranges()
            .binary_search_by(|r| {
                if c < r.start() {
                    std::cmp::Ordering::Greater
                } else if c > r.end() {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .is_ok()
    };
    let mut output = base.to_string();
    for family in FAMILIES.values() {
        if family.iter().any(|&c| contains(c)) {
            for c in family {
                write!(output, "\\x{{{:x}}}", *c as u32).unwrap();
            }
        }
    }
    // Non-Turkish cultures include dotted-I in the I/i family; invariant
    // culture keeps it separate. The source regex already retains literals.
    if !invariant && (contains('I') || contains('i') || contains('İ')) {
        output.push_str(r"\x{49}\x{69}\x{130}");
    }
    Ok(output)
}

fn simple_lower(c: char) -> char {
    let mut lower = c.to_lowercase();
    let first = lower.next().unwrap();
    if lower.next().is_none() {
        first
    } else {
        c
    }
}

static FAMILIES: std::sync::LazyLock<BTreeMap<char, Vec<char>>> = std::sync::LazyLock::new(|| {
    let mut groups = BTreeMap::<char, Vec<char>>::new();
    for value in 0..=u16::MAX as u32 {
        let Some(c) = char::from_u32(value) else {
            continue;
        };
        let key = simple_lower(c);
        if key != c {
            groups.entry(key).or_insert_with(|| vec![key]).push(c);
        }
    }
    groups
});

pub(super) fn literal(c: char, invariant: bool) -> String {
    let mut output = String::from("(?-i:[");
    if !invariant && matches!(c, 'I' | 'i' | 'İ') {
        output.push_str(r"\x{49}\x{69}\x{130}");
    } else if let Some(family) = FAMILIES.get(&simple_lower(c)) {
        for c in family {
            write!(output, "\\x{{{:x}}}", *c as u32).unwrap();
        }
    } else {
        write!(output, "\\x{{{:x}}}", c as u32).unwrap();
    }
    output.push_str("])");
    output
}

pub(super) fn equal_case(left: &str, right: &str, invariant: bool) -> bool {
    let key = |c| {
        if !invariant && matches!(c, 'I' | 'i' | 'İ') {
            'i'
        } else {
            simple_lower(c)
        }
    };
    left.chars().map(key).eq(right.chars().map(key))
}

#[cfg(test)]
mod tests {
    use super::super::regex_pattern::{compile, Engine};

    #[test]
    fn ecmascript_regular_patterns_keep_the_linear_engine() {
        for pattern in [
            r"\w+",
            r"[\w-[A-Z]]+",
            r"(?<name>ascii)+",
            r"\x41\cA",
            r"(?i:I[\W])",
        ] {
            assert!(
                matches!(compile(pattern, 257).unwrap().regex, Engine::Linear(_)),
                "{pattern}"
            );
        }
    }

    #[test]
    fn ecmascript_references_use_the_bounded_capture_engine() {
        assert!(matches!(
            compile(r"(a)?\1", 256).unwrap().regex,
            Engine::Directional(_)
        ));
    }
}
