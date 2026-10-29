//! Bounded backtracking for .NET assertions, shared captures and conditionals.
//! Ordinary expressions still use the existing engines.
use super::regex_advanced::{self, Node};
use super::regex_capture_stack::Captures;
use super::regex_captures::Numbers;
use fancy_regex::{Expr, LookAround};
use regex_automata::{meta::Regex, Anchored, Input, Span};

const STEP_LIMIT: usize = 1_000_000;
const FRAME_LIMIT: usize = 16_384;

enum Op {
    Atom(Regex, usize),
    Concat(Vec<Op>),
    Alt(Vec<Op>),
    Group(usize, Box<Op>),
    Balance(Option<usize>, usize, Box<Op>),
    Look(Box<Op>, bool, bool),
    Repeat(Box<Op>, usize, usize, bool),
    Atomic(Box<Op>),
    Reference(usize, bool),
    CaptureTest(usize),
    Conditional(Box<Op>, Box<Op>, Box<Op>),
}

pub(super) struct Program {
    root: Op,
    reversed_input: bool,
    groups: usize,
    ecmascript: bool,
    invariant: bool,
}

#[derive(Clone)]
struct State {
    position: usize,
    groups: Vec<Option<usize>>,
}

#[derive(Clone, Copy)]
enum Task<'a> {
    Eval(&'a Op),
    Save(usize, usize),
    BalanceSave(Option<usize>, usize, usize),
    Repeat(&'a Op, usize, usize, bool, Option<usize>),
}

#[derive(Clone)]
struct Frame<'a> {
    state: State,
    tasks: Vec<Task<'a>>,
}

struct Budget<'a> {
    steps: usize,
    started: std::time::Instant,
    captures: &'a mut Captures,
}

impl Budget<'_> {
    fn step(&mut self) -> Result<(), String> {
        self.steps += 1;
        if self.steps > STEP_LIMIT
            || (self.steps % 1024 == 0 && self.started.elapsed().as_secs() >= 2)
        {
            return Err("正则匹配超过执行限制，已停止替换。".into());
        }
        Ok(())
    }
}

impl Program {
    pub(super) fn captures(&self, text: &str) -> Captures {
        Captures::new(text, self.reversed_input)
    }

    pub(super) fn compile(
        node: Node,
        reversed_input: bool,
        names: &[Option<Box<str>>],
        numbered: &Numbers,
        ecmascript: bool,
        invariant: bool,
    ) -> Result<Self, String> {
        Ok(Self {
            root: compile(node, reversed_input, names, numbered)?,
            reversed_input,
            groups: names.len(),
            ecmascript,
            invariant,
        })
    }

    pub(super) fn search(
        &self,
        text: &str,
        start: usize,
        captures: &mut Captures,
    ) -> Result<Option<Vec<Option<Span>>>, String> {
        captures.clear();
        let mut budget = Budget {
            steps: 0,
            started: std::time::Instant::now(),
            captures,
        };
        for position in text[start..]
            .char_indices()
            .map(|(offset, _)| start + offset)
            .chain(std::iter::once(text.len()))
        {
            let state = State {
                position,
                groups: vec![None; self.groups],
            };
            if let Some(state) =
                self.run(&self.root, text, state, self.reversed_input, &mut budget)?
            {
                let mut groups = state
                    .groups
                    .into_iter()
                    .map(|head| budget.captures.finish(head))
                    .collect::<Result<Vec<_>, _>>()?;
                groups[0] = Some(Span {
                    start: position,
                    end: state.position,
                });
                return Ok(Some(groups));
            }
        }
        Ok(None)
    }

    fn run(
        &self,
        op: &Op,
        text: &str,
        state: State,
        reverse: bool,
        budget: &mut Budget,
    ) -> Result<Option<State>, String> {
        let mut current = Frame {
            state,
            tasks: vec![Task::Eval(op)],
        };
        let mut backtrack = Vec::new();
        loop {
            budget.step()?;
            let Some(task) = current.tasks.pop() else {
                return Ok(Some(current.state));
            };
            let physical_reverse = reverse != self.reversed_input;
            let mut matched = true;
            match task {
                Task::Save(slot, start) => {
                    budget.captures.save(
                        &mut current.state.groups[slot],
                        Span {
                            start: start.min(current.state.position),
                            end: start.max(current.state.position),
                        },
                    );
                }
                Task::BalanceSave(destination, source, start) => {
                    // The body can capture/pop the source itself, so check its
                    // stack after evaluating the body, just like .NET.
                    matched = budget.captures.balance(
                        &mut current.state.groups,
                        destination,
                        source,
                        Span {
                            start: start.min(current.state.position),
                            end: start.max(current.state.position),
                        },
                    )?;
                }
                Task::Repeat(child, lo, hi, greedy, previous) => {
                    if hi != 0 && !(lo == 0 && previous == Some(current.state.position)) {
                        let mut continuation = current.clone();
                        continuation.tasks.push(Task::Repeat(
                            child,
                            lo.saturating_sub(1),
                            hi.saturating_sub(1),
                            greedy,
                            Some(current.state.position),
                        ));
                        continuation.tasks.push(Task::Eval(child));
                        if lo != 0 {
                            current = continuation;
                        } else if greedy {
                            backtrack.push(current);
                            current = continuation;
                        } else {
                            backtrack.push(continuation);
                        }
                    }
                }
                Task::Eval(Op::Atom(regex, width)) => {
                    if let Some(end) =
                        advance(text, current.state.position, *width, physical_reverse)
                    {
                        let range =
                            current.state.position.min(end)..current.state.position.max(end);
                        let found = regex
                            .find(Input::new(text).span(range.clone()).anchored(Anchored::Yes));
                        matched = found.is_some_and(|m| m.range() == range);
                        if matched {
                            current.state.position = end;
                        }
                    } else {
                        matched = false;
                    }
                }
                Task::Eval(Op::Concat(children)) => {
                    if reverse {
                        current.tasks.extend(children.iter().map(Task::Eval));
                    } else {
                        current.tasks.extend(children.iter().rev().map(Task::Eval));
                    }
                }
                Task::Eval(Op::Alt(children)) => {
                    for child in children.iter().skip(1).rev() {
                        let mut alternative = current.clone();
                        alternative.tasks.push(Task::Eval(child));
                        backtrack.push(alternative);
                    }
                    if let Some(first) = children.first() {
                        current.tasks.push(Task::Eval(first));
                    }
                }
                Task::Eval(Op::Group(slot, child)) => {
                    current
                        .tasks
                        .push(Task::Save(*slot, current.state.position));
                    current.tasks.push(Task::Eval(child));
                }
                Task::Eval(Op::Balance(destination, source, child)) => {
                    current.tasks.push(Task::BalanceSave(
                        *destination,
                        *source,
                        current.state.position,
                    ));
                    current.tasks.push(Task::Eval(child));
                }
                Task::Eval(Op::Repeat(child, lo, hi, greedy)) => current
                    .tasks
                    .push(Task::Repeat(child, *lo, *hi, *greedy, None)),
                Task::Eval(Op::Atomic(child)) => {
                    match self.run(child, text, current.state.clone(), reverse, budget)? {
                        Some(next) => current.state = next,
                        None => matched = false,
                    }
                }
                Task::Eval(Op::Look(child, behind, negative)) => {
                    let result = self.run(child, text, current.state.clone(), *behind, budget)?;
                    matched = result.is_some() != *negative;
                    if matched && !negative {
                        current.state.groups = result.unwrap().groups;
                    }
                }
                Task::Eval(Op::Reference(slot, casei)) => {
                    matched = self.ecmascript;
                    if let Some(span) = budget.captures.reference(current.state.groups[*slot])? {
                        matched = false;
                        let captured = &text[span.start..span.end];
                        if let Some(end) = advance(
                            text,
                            current.state.position,
                            captured.chars().count(),
                            physical_reverse,
                        ) {
                            let candidate = &text
                                [current.state.position.min(end)..current.state.position.max(end)];
                            matched = if *casei && self.ecmascript {
                                super::regex_ecmascript::equal_case(
                                    candidate,
                                    captured,
                                    self.invariant,
                                )
                            } else if *casei {
                                regex::RegexBuilder::new(&format!(
                                    "\\A(?:{})\\z",
                                    regex::escape(captured)
                                ))
                                .case_insensitive(true)
                                .build()
                                .map_err(|e| e.to_string())?
                                .is_match(candidate)
                            } else {
                                candidate == captured
                            };
                            if matched {
                                current.state.position = end;
                            }
                        }
                    }
                }
                Task::Eval(Op::CaptureTest(slot)) => {
                    matched = budget.captures.matched(current.state.groups[*slot]);
                }
                Task::Eval(Op::Conditional(condition, yes, no)) => {
                    let result =
                        self.run(condition, text, current.state.clone(), reverse, budget)?;
                    let branch = if let Some(result) = result {
                        // Conditions are atomic zero-width tests: retain captures
                        // but never consume input or retry the other branch.
                        current.state.groups = result.groups;
                        yes
                    } else {
                        no
                    };
                    current.tasks.push(Task::Eval(branch));
                }
            }
            if backtrack.len() > FRAME_LIMIT {
                return Err("正则匹配超过回溯限制，已停止替换。".into());
            }
            if !matched {
                match backtrack.pop() {
                    Some(previous) => current = previous,
                    None => return Ok(None),
                }
            }
        }
    }
}

fn advance(text: &str, position: usize, width: usize, reverse: bool) -> Option<usize> {
    if width == 0 {
        return Some(position);
    }
    if reverse {
        text[..position]
            .char_indices()
            .rev()
            .nth(width - 1)
            .map(|(i, _)| i)
    } else {
        text[position..]
            .char_indices()
            .nth(width - 1)
            .map(|(i, ch)| position + i + ch.len_utf8())
    }
}

fn compile(
    node: Node,
    reverse: bool,
    names: &[Option<Box<str>>],
    numbered: &Numbers,
) -> Result<Op, String> {
    let sub = |child| compile(child, reverse, names, numbered).map(Box::new);
    Ok(match node {
        Node::Concat(children) => Op::Concat(
            children
                .into_iter()
                .map(|child| compile(child, reverse, names, numbered))
                .collect::<Result<_, _>>()?,
        ),
        Node::Alt(children) => Op::Alt(
            children
                .into_iter()
                .map(|child| compile(child, reverse, names, numbered))
                .collect::<Result<_, _>>()?,
        ),
        Node::Group(slot, child) => Op::Group(slot, sub(*child)?),
        Node::Balance(destination, source, child) => {
            let source_slot = if super::regex_mask::is_number(&source) {
                source
                    .parse::<usize>()
                    .ok()
                    .and_then(|i| numbered.get(&i).copied())
            } else {
                names
                    .iter()
                    .position(|name| name.as_deref() == Some(&source))
            }
            .ok_or_else(|| format!("平衡组引用的分组不存在：{source}"))?;
            Op::Balance(destination, source_slot, sub(*child)?)
        }
        Node::Repeat(child, lo, hi, greedy) => Op::Repeat(sub(*child)?, lo, hi, greedy),
        Node::Atomic(child) => Op::Atomic(sub(*child)?),
        Node::Conditional(condition, yes, no) => {
            Op::Conditional(sub(*condition)?, sub(*yes)?, sub(*no)?)
        }
        Node::CaptureTest(target, casei) => {
            let slot = if target.bytes().all(|b| b.is_ascii_digit()) {
                Some(
                    target
                        .parse::<usize>()
                        .ok()
                        .and_then(|i| numbered.get(&i).copied())
                        .ok_or_else(|| format!("条件组引用的分组不存在：{target}"))?,
                )
            } else {
                names
                    .iter()
                    .position(|name| name.as_deref() == Some(&target))
            };
            match slot {
                Some(slot) => Op::CaptureTest(slot),
                // An undeclared name is an expression test in .NET, not an
                // unknown-group error. It keeps the surrounding match direction.
                None => compile(
                    Node::Atom(Expr::Literal { val: target, casei }),
                    reverse,
                    names,
                    numbered,
                )?,
            }
        }
        Node::Look(child, direction) => Op::Look(
            sub(*child)?,
            matches!(
                direction,
                LookAround::LookBehind | LookAround::LookBehindNeg
            ),
            matches!(
                direction,
                LookAround::LookAheadNeg | LookAround::LookBehindNeg
            ),
        ),
        Node::Reference(target, casei) => {
            let slot = if target.bytes().all(|b| b.is_ascii_digit()) {
                target
                    .parse::<usize>()
                    .ok()
                    .and_then(|i| numbered.get(&i).copied())
            } else {
                names
                    .iter()
                    .position(|name| name.as_deref() == Some(&target))
            };
            Op::Reference(
                slot.ok_or_else(|| format!("反向引用的分组不存在：{target}"))?,
                casei,
            )
        }
        Node::Atom(atom) => {
            let width = match &atom {
                Expr::Empty | Expr::Assertion(_) => 0,
                Expr::Literal { val, .. } => val.chars().count(),
                _ => 1,
            };
            let mut pattern = String::new();
            regex_advanced::render(
                &Node::Atom(atom),
                reverse,
                names,
                numbered,
                &[],
                &mut pattern,
            )?;
            Op::Atom(Regex::new(&pattern).map_err(|e| e.to_string())?, width)
        }
    })
}
