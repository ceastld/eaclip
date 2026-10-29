use regex_automata::meta::Regex;
use regex_automata::{util::captures::Captures, Input, Span};
use regex_syntax::hir::{Hir, HirKind};

pub(super) enum Engine {
    Linear(Regex),
    Advanced {
        regex: fancy_regex::Regex,
        slots: Vec<usize>,
    },
    Directional(super::regex_directional::Program),
}

pub(super) struct Compiled {
    pub regex: Engine,
    pub names: Vec<Option<Box<str>>>,
    pub numbers: super::regex_captures::Numbers,
}

pub(super) fn compile(pattern: &str, options: i32) -> Result<Compiled, String> {
    let translated = super::regex_character_class::translate(pattern, options)?;
    let pattern = translated.as_str();
    let hir = regex_syntax::ParserBuilder::new()
        .case_insensitive(options & 1 != 0)
        .multi_line(options & 2 != 0)
        .dot_matches_new_line(options & 16 != 0)
        .ignore_whitespace(options & 32 != 0)
        .build()
        .parse(pattern);
    let hir = match hir {
        Ok(hir) => hir,
        Err(_) => return super::regex_advanced::compile(pattern, options),
    };
    let mut names = vec![None];
    let hir = rewrite(hir, options & 4 != 0, options & 64 != 0, &mut names)?;
    let (numbers, aliases) = super::regex_captures::assign(&names)?;
    if aliases
        .iter()
        .enumerate()
        .any(|(slot, &alias)| slot != alias)
    {
        return super::regex_advanced::compile(pattern, options);
    }
    let regex = Regex::builder()
        .build_from_hir(&hir)
        .map_err(|error| error.to_string())?;
    Ok(Compiled {
        regex: Engine::Linear(regex),
        names,
        numbers,
    })
}

pub(super) enum Searcher<'a> {
    Linear(&'a Regex, Captures),
    Advanced(&'a fancy_regex::Regex, &'a [usize], Vec<Option<Span>>),
    Directional(
        &'a super::regex_directional::Program,
        Vec<Option<Span>>,
        super::regex_capture_stack::Captures,
    ),
}

impl Compiled {
    pub(super) fn searcher(&self, text: &str) -> Searcher<'_> {
        match &self.regex {
            Engine::Linear(regex) => Searcher::Linear(regex, regex.create_captures()),
            Engine::Advanced { regex, slots } => {
                Searcher::Advanced(regex, slots, vec![None; self.names.len()])
            }
            Engine::Directional(program) => Searcher::Directional(
                program,
                vec![None; self.names.len()],
                program.captures(text),
            ),
        }
    }
}

impl Searcher<'_> {
    pub(super) fn search(&mut self, text: &str, start: usize) -> Result<(), String> {
        match self {
            Self::Directional(program, groups, captures) => {
                groups.fill(None);
                if let Some(found) = program.search(text, start, captures)? {
                    *groups = found;
                }
            }
            Self::Linear(regex, captures) => {
                regex.search_captures(&Input::new(text).span(start..text.len()), captures)
            }
            Self::Advanced(regex, slots, groups) => {
                groups.fill(None);
                if let Some(captures) = regex
                    .captures_from_pos(text, start)
                    .map_err(|error| format!("正则匹配失败，已停止替换：{error}"))?
                {
                    for (engine, &original) in slots.iter().enumerate() {
                        groups[original] = captures.get(engine).map(|m| Span {
                            start: m.start(),
                            end: m.end(),
                        });
                    }
                }
            }
        }
        Ok(())
    }

    pub(super) fn group(&self, slot: usize) -> Option<Span> {
        match self {
            Self::Linear(_, captures) => captures.get_group(slot),
            Self::Advanced(_, _, groups) => groups.get(slot).copied().flatten(),
            Self::Directional(_, groups, _) => groups.get(slot).copied().flatten(),
        }
    }
}

// Transform the expression, not just the result order: right-to-left matching
// reverses concatenations and assertions, while alternation priority and
// greediness remain unchanged. Capture slots keep their original order.
fn rewrite(
    hir: Hir,
    explicit: bool,
    reverse: bool,
    names: &mut Vec<Option<Box<str>>>,
) -> Result<Hir, String> {
    Ok(match hir.into_kind() {
        HirKind::Empty => Hir::empty(),
        HirKind::Literal(literal) if reverse => {
            let text = std::str::from_utf8(&literal.0)
                .map_err(|_| "从右向左匹配不支持拆分 UTF-8 字节的模式。")?;
            Hir::literal(text.chars().rev().collect::<String>().into_bytes())
        }
        HirKind::Literal(literal) => Hir::literal(literal.0),
        HirKind::Class(class) => Hir::class(class),
        HirKind::Look(look) => Hir::look(if reverse { look.reversed() } else { look }),
        HirKind::Repetition(mut repetition) => {
            repetition.sub = Box::new(rewrite(*repetition.sub, explicit, reverse, names)?);
            Hir::repetition(repetition)
        }
        HirKind::Capture(capture) if explicit && capture.name.is_none() => {
            rewrite(*capture.sub, explicit, reverse, names)?
        }
        HirKind::Capture(mut capture) => {
            capture.index = names.len() as u32;
            // Record names before reversing traversal. The engine's name table
            // assumes construction in increasing slot order and can omit names
            // when the reversed tree visits a higher-numbered capture first.
            names.push(capture.name.take());
            capture.sub = Box::new(rewrite(*capture.sub, explicit, reverse, names)?);
            Hir::capture(capture)
        }
        HirKind::Concat(children) => {
            let mut children = children
                .into_iter()
                .map(|child| rewrite(child, explicit, reverse, names))
                .collect::<Result<Vec<_>, _>>()?;
            if reverse {
                children.reverse();
            }
            Hir::concat(children)
        }
        HirKind::Alternation(children) => Hir::alternation(
            children
                .into_iter()
                .map(|child| rewrite(child, explicit, reverse, names))
                .collect::<Result<Vec<_>, _>>()?,
        ),
    })
}
