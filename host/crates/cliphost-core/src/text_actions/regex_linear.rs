//! Compile .NET NonBacktracking expressions without a backtracking fallback.
use super::regex_advanced::{self, Node};
use super::regex_captures::Numbers;
use regex_automata::meta::Regex;
use regex_syntax::hir::{Capture, Hir, Repetition};

pub(super) fn compile(
    node: Node,
    names: &[Option<Box<str>>],
    numbers: &Numbers,
) -> Result<Regex, String> {
    let hir = to_hir(node, names, numbers)?;
    Regex::builder()
        .build_from_hir(&hir)
        .map_err(|error| error.to_string())
}

fn to_hir(node: Node, names: &[Option<Box<str>>], numbers: &Numbers) -> Result<Hir, String> {
    let child = |node| to_hir(node, names, numbers);
    Ok(match node {
        Node::Concat(children) => {
            Hir::concat(children.into_iter().map(child).collect::<Result<_, _>>()?)
        }
        Node::Alt(children) => {
            Hir::alternation(children.into_iter().map(child).collect::<Result<_, _>>()?)
        }
        Node::Group(slot, inner) => Hir::capture(Capture {
            index: u32::try_from(slot).map_err(|_| "捕获组数量过多，已停止替换。")?,
            name: None,
            sub: Box::new(child(*inner)?),
        }),
        Node::Repeat(inner, lo, hi, greedy) => Hir::repetition(Repetition {
            min: u32::try_from(lo).map_err(|_| "重复次数过大，已停止替换。")?,
            max: if hi == usize::MAX {
                None
            } else {
                Some(u32::try_from(hi).map_err(|_| "重复次数过大，已停止替换。")?)
            },
            greedy,
            sub: Box::new(child(*inner)?),
        }),
        Node::Atom(atom) => {
            let mut pattern = String::new();
            regex_advanced::render(&Node::Atom(atom), false, names, numbers, &[], &mut pattern)?;
            regex_syntax::Parser::new()
                .parse(&pattern)
                .map_err(|error| error.to_string())?
        }
        Node::Look(..)
        | Node::Balance(..)
        | Node::Atomic(..)
        | Node::Reference(..)
        | Node::CaptureTest(..)
        | Node::Conditional(..) => {
            return Err(
                "非回溯模式不支持断言、反向引用、原子组、条件组或平衡组，已停止替换。".into(),
            );
        }
    })
}
