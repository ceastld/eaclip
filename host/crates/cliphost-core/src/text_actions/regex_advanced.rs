//! Adapt .NET capture numbering/direction before compiling advanced expressions.
use fancy_regex::{Assertion, Expr, LookAround, RegexBuilder};
use std::collections::HashMap;

use super::regex_captures::Numbers;
use super::regex_mask::{balancing_name, is_capture_name, is_number, Reference};
use super::regex_pattern::{Compiled, Engine};

const BACKTRACK_LIMIT: usize = 100_000;

pub(super) enum Node {
    Atom(Expr),
    Concat(Vec<Node>),
    Alt(Vec<Node>),
    Group(usize, Box<Node>),
    Balance(Option<usize>, String, Box<Node>),
    Look(Box<Node>, LookAround),
    Repeat(Box<Node>, usize, usize, bool),
    Atomic(Box<Node>),
    Reference(String, bool),
    CaptureTest(String, bool),
    Conditional(Box<Node>, Box<Node>, Box<Node>),
}

pub(super) fn compile(pattern: &str, options: i32) -> Result<Compiled, String> {
    let mut prefix = "ClipHostRef".to_string();
    while pattern.contains(&prefix) {
        prefix.push('X');
    }
    let masked = super::regex_mask::mask(pattern, &prefix, options)?;
    let mut flags = String::new();
    for (bit, flag) in [(1, 'i'), (2, 'm'), (16, 's'), (32, 'x')] {
        if options & bit != 0 {
            flags.push(flag);
        }
    }
    let source = if flags.is_empty() {
        masked.pattern
    } else {
        format!("(?{flags}:{})", masked.pattern)
    };
    let tree = Expr::parse_tree(&source).map_err(|error| error.to_string())?;
    let by_index: HashMap<usize, String> = tree
        .named_groups
        .into_iter()
        .map(|(name, id)| (id, masked.aliases.get(&name).cloned().unwrap_or(name)))
        .collect();
    let mut names = vec![None];
    let mut node = from_expr(
        tree.expr,
        &mut 0,
        &by_index,
        &prefix,
        &masked.references,
        options & 4 != 0,
        &mut names,
    )?;
    let (numbered, aliases) = super::regex_captures::assign(&names)?;
    remap_groups(&mut node, &aliases);
    if options & 1024 != 0 {
        let regex = super::regex_linear::compile(node, &names, &numbered)?;
        return Ok(Compiled {
            regex: Engine::Linear(regex),
            names,
            numbers: numbered,
        });
    }
    let reverse = options & 64 != 0;
    // .NET evaluates lookbehind captures right-to-left (lookahead captures
    // left-to-right), independently of the surrounding expression's direction.
    // fancy-regex extracts lookbehind captures with a forward regex. Use the
    // bounded directional evaluator when assertion captures are observable.
    // Duplicate named groups share one logical slot, updated by each successful
    // capture. Its backtracking state restores earlier values when a branch fails.
    let mut slots = vec![0];
    capture_order(&node, reverse, &mut slots);
    if slots.len() > numbered.len()
        || needs_directional_evaluator(&node)
        || (options & 256 != 0 && !masked.references.is_empty())
    {
        let program = super::regex_directional::Program::compile(
            node,
            reverse,
            &names,
            &numbered,
            options & 256 != 0,
            options & 512 != 0,
        )?;
        return Ok(Compiled {
            regex: Engine::Directional(program),
            names,
            numbers: numbered,
        });
    }
    let mut engine_slot = vec![0; names.len()];
    for (engine, &original) in slots.iter().enumerate() {
        engine_slot[original] = engine;
    }
    let mut translated = String::new();
    render(
        &node,
        reverse,
        &names,
        &numbered,
        &engine_slot,
        &mut translated,
    )?;
    let regex = RegexBuilder::new(&translated)
        .backtrack_limit(BACKTRACK_LIMIT)
        .build()
        .map_err(|error| format!("正则表达式无法执行：{error}"))?;
    Ok(Compiled {
        regex: Engine::Advanced { regex, slots },
        names,
        numbers: numbered,
    })
}

fn from_expr(
    expr: Expr,
    index: &mut usize,
    by_index: &HashMap<usize, String>,
    prefix: &str,
    references: &[Reference],
    explicit: bool,
    names: &mut Vec<Option<Box<str>>>,
) -> Result<Node, String> {
    let mut child = |expr| from_expr(expr, index, by_index, prefix, references, explicit, names);
    Ok(match expr {
        Expr::Group(inner) => {
            *index += 1;
            let name = by_index.get(index);
            if let Some(reference) = name
                .and_then(|name| name.strip_prefix(prefix))
                .and_then(|v| v.parse::<usize>().ok())
            {
                let casei = matches!(*inner, Expr::Literal { casei: true, .. });
                match &references[reference] {
                    Reference::Text(target) => Node::Reference(target.clone(), casei),
                    Reference::CaptureTest(target) => Node::CaptureTest(target.clone(), casei),
                }
            } else {
                let balancing = name.and_then(|name| balancing_name(name));
                let name = balancing
                    .map(|(destination, _)| destination)
                    .or(name.map(String::as_str));
                if name.is_some_and(|name| {
                    !name.is_empty() && !is_capture_name(name) && !is_number(name)
                }) {
                    return Err("捕获组名称无效，已停止替换。".into());
                }
                let slot = if (explicit && name.is_none()) || name == Some("") {
                    None
                } else if let Some(existing) = name.and_then(|name| {
                    names
                        .iter()
                        .position(|stored| stored.as_deref() == Some(name))
                }) {
                    Some(existing)
                } else {
                    let slot = names.len();
                    names.push(name.map(Into::into));
                    Some(slot)
                };
                let inner = from_expr(
                    (*inner).clone(),
                    index,
                    by_index,
                    prefix,
                    references,
                    explicit,
                    names,
                )?;
                if let Some((_, source)) = balancing {
                    Node::Balance(slot, source.to_string(), Box::new(inner))
                } else {
                    match slot {
                        Some(slot) => Node::Group(slot, Box::new(inner)),
                        None => inner,
                    }
                }
            }
        }
        Expr::Concat(children) => Node::Concat(
            children
                .into_iter()
                .map(&mut child)
                .collect::<Result<_, _>>()?,
        ),
        Expr::Alt(children) => Node::Alt(
            children
                .into_iter()
                .map(&mut child)
                .collect::<Result<_, _>>()?,
        ),
        Expr::LookAround(inner, direction) => Node::Look(Box::new(child(*inner)?), direction),
        Expr::Repeat {
            child: inner,
            lo,
            hi,
            greedy,
        } => Node::Repeat(Box::new(child(*inner)?), lo, hi, greedy),
        Expr::AtomicGroup(inner) => Node::Atomic(Box::new(child(*inner)?)),
        Expr::Conditional {
            condition,
            true_branch,
            false_branch,
        } => Node::Conditional(
            Box::new(child(*condition)?),
            Box::new(child(*true_branch)?),
            Box::new(child(*false_branch)?),
        ),
        atom @ (Expr::Empty
        | Expr::Any { .. }
        | Expr::Literal { .. }
        | Expr::Delegate { .. }
        | Expr::Assertion(_)) => Node::Atom(atom),
        _ => return Err("当前 Rust 正则引擎暂不支持此表达式结构，已停止替换。".into()),
    })
}

fn remap_groups(node: &mut Node, aliases: &[usize]) {
    match node {
        Node::Balance(slot, _, child) => {
            *slot = slot.map(|slot| aliases[slot]);
            remap_groups(child, aliases);
        }
        Node::Group(slot, child) => {
            *slot = aliases[*slot];
            remap_groups(child, aliases);
        }
        Node::Concat(children) | Node::Alt(children) => {
            for child in children {
                remap_groups(child, aliases);
            }
        }
        Node::Look(child, _) | Node::Repeat(child, ..) | Node::Atomic(child) => {
            remap_groups(child, aliases)
        }
        Node::Conditional(condition, yes, no) => {
            for child in [condition, yes, no] {
                remap_groups(child, aliases);
            }
        }
        _ => {}
    }
}

fn capture_order(node: &Node, reverse: bool, slots: &mut Vec<usize>) {
    match node {
        Node::Balance(slot, _, child) => {
            slots.extend(slot);
            capture_order(child, reverse, slots);
        }
        Node::Group(slot, child) => {
            slots.push(*slot);
            capture_order(child, reverse, slots);
        }
        Node::Concat(children) if reverse => {
            for child in children.iter().rev() {
                capture_order(child, reverse, slots);
            }
        }
        Node::Concat(children) | Node::Alt(children) => {
            for child in children {
                capture_order(child, reverse, slots);
            }
        }
        Node::Look(child, _) | Node::Repeat(child, ..) | Node::Atomic(child) => {
            capture_order(child, reverse, slots)
        }
        Node::Conditional(condition, yes, no) => {
            for child in [condition, yes, no] {
                capture_order(child, reverse, slots);
            }
        }
        _ => {}
    }
}

pub(super) fn render(
    node: &Node,
    reverse: bool,
    names: &[Option<Box<str>>],
    numbered: &Numbers,
    slots: &[usize],
    out: &mut String,
) -> Result<(), String> {
    let render_child =
        |child, out: &mut String| render(child, reverse, names, numbered, slots, out);
    match node {
        Node::Group(_, child) => {
            out.push('(');
            render_child(child, out)?;
            out.push(')');
        }
        Node::Concat(children) | Node::Alt(children) => {
            out.push_str("(?:");
            let alternate = matches!(node, Node::Alt(_));
            for offset in 0..children.len() {
                if alternate && offset > 0 {
                    out.push('|');
                }
                let i = if reverse && !alternate {
                    children.len() - 1 - offset
                } else {
                    offset
                };
                render_child(&children[i], out)?;
            }
            out.push(')');
        }
        Node::Repeat(child, lo, hi, greedy) => {
            out.push_str("(?:");
            render_child(child, out)?;
            out.push(')');
            if *hi == usize::MAX {
                out.push_str(&format!("{{{lo},}}"));
            } else {
                out.push_str(&format!("{{{lo},{hi}}}"));
            }
            if !greedy {
                out.push('?');
            }
        }
        Node::Atomic(child) => {
            out.push_str("(?>");
            render_child(child, out)?;
            out.push(')');
        }
        Node::Look(child, direction) => {
            let prefix = match (direction, reverse) {
                (LookAround::LookAhead, false) | (LookAround::LookBehind, true) => "(?=",
                (LookAround::LookAheadNeg, false) | (LookAround::LookBehindNeg, true) => "(?!",
                (LookAround::LookBehind, false) | (LookAround::LookAhead, true) => "(?<=",
                (LookAround::LookBehindNeg, false) | (LookAround::LookAheadNeg, true) => "(?<!",
            };
            out.push_str(prefix);
            render_child(child, out)?;
            out.push(')');
        }
        Node::Reference(target, casei) => {
            let original = if target.bytes().all(|b| b.is_ascii_digit()) {
                target
                    .parse::<usize>()
                    .ok()
                    .and_then(|i| numbered.get(&i).copied())
            } else {
                names
                    .iter()
                    .position(|name| name.as_deref() == Some(target))
            };
            let original = original.ok_or_else(|| format!("反向引用的分组不存在：{target}"))?;
            out.push_str(if *casei { "(?i:" } else { "(?:" });
            out.push_str(&format!("\\{})", slots[original]));
        }
        Node::Atom(Expr::Literal { val, casei }) => {
            out.push_str(if *casei { "(?i:" } else { "(?:" });
            let literal = if reverse {
                val.chars().rev().collect::<String>()
            } else {
                val.clone()
            };
            out.push_str(&regex::escape(&literal));
            out.push(')');
        }
        Node::Atom(Expr::Assertion(assertion)) => {
            let value = match (assertion, reverse) {
                (Assertion::StartText, false) | (Assertion::EndText, true) => "\\A",
                (Assertion::EndText, false) | (Assertion::StartText, true) => "\\z",
                (Assertion::StartLine { crlf: false }, false)
                | (Assertion::EndLine { crlf: false }, true) => "(?m:^)",
                (Assertion::EndLine { crlf: false }, false)
                | (Assertion::StartLine { crlf: false }, true) => "(?m:$)",
                (Assertion::WordBoundary, _) => "\\b",
                (Assertion::NotWordBoundary, _) => "\\B",
                _ => return Err("暂不支持此边界断言。".into()),
            };
            out.push_str(value);
        }
        Node::Atom(atom) => atom.to_str(out, 0),
        Node::Conditional(..) | Node::CaptureTest(..) | Node::Balance(..) => {
            return Err("条件组和平衡组必须使用方向执行器，已停止替换。".into());
        }
    }
    Ok(())
}

fn children(node: &Node) -> Vec<&Node> {
    match node {
        Node::Concat(nodes) | Node::Alt(nodes) => nodes.iter().collect(),
        Node::Group(_, child)
        | Node::Balance(_, _, child)
        | Node::Look(child, _)
        | Node::Repeat(child, ..)
        | Node::Atomic(child) => vec![child],
        Node::Conditional(condition, yes, no) => vec![condition, yes, no],
        _ => vec![],
    }
}

fn has_capture(node: &Node) -> bool {
    matches!(node, Node::Group(..) | Node::Balance(Some(_), ..))
        || children(node).into_iter().any(has_capture)
}

fn needs_directional_evaluator(node: &Node) -> bool {
    matches!(
        node,
        Node::Conditional(..) | Node::CaptureTest(..) | Node::Balance(..)
    ) || matches!(node, Node::Look(child, _) if has_capture(child))
        || children(node).into_iter().any(needs_directional_evaluator)
}
