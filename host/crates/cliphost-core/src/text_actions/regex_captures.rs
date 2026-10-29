//! .NET capture numbers are sparse and may name the same capture slot twice.
use std::collections::BTreeMap;

pub(super) type Numbers = BTreeMap<usize, usize>;

pub(super) fn assign(names: &[Option<Box<str>>]) -> Result<(Numbers, Vec<usize>), String> {
    let mut numbers = BTreeMap::from([(0, 0)]);
    let mut aliases: Vec<_> = (0..names.len()).collect();
    let mut next = 1;
    for (slot, name) in names.iter().enumerate().skip(1) {
        if name.is_none() {
            numbers.insert(next, slot);
            next += 1;
        }
    }
    // Explicit numbers reserve their slots before automatic named groups. An
    // explicit number matching an unnamed group refers to that same capture.
    for (slot, name) in names.iter().enumerate().skip(1) {
        if let Some(name) = name
            .as_deref()
            .filter(|name| super::regex_mask::is_number(name))
        {
            let number = parse_number(name)?;
            if !name.starts_with('0') {
                aliases[slot] = *numbers.entry(number).or_insert(slot);
            }
        }
    }
    for (slot, name) in names.iter().enumerate().skip(1) {
        if name
            .as_deref()
            .is_some_and(|name| !super::regex_mask::is_number(name))
        {
            while numbers.contains_key(&next) {
                next += 1;
            }
            numbers.insert(next, slot);
            next += 1;
        }
    }
    // .NET's initial declaration scan does not reserve zero-prefixed numbers;
    // they can alias a slot declared elsewhere, but cannot introduce a new one.
    for (slot, name) in names.iter().enumerate().skip(1) {
        if let Some(name) = name.as_deref().filter(|name| name.starts_with('0')) {
            let number = parse_number(name)?;
            aliases[slot] = numbers
                .get(&number)
                .copied()
                .ok_or_else(|| format!("捕获组编号尚未定义：{name}，已停止替换。"))?;
        }
    }
    Ok((numbers, aliases))
}

fn parse_number(name: &str) -> Result<usize, String> {
    name.parse::<usize>()
        .ok()
        .filter(|&n| n > 0 && n <= i32::MAX as usize)
        .ok_or_else(|| format!("捕获组编号必须在 1 到 2147483647 之间：{name}，已停止替换。"))
}
