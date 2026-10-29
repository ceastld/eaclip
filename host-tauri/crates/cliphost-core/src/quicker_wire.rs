//! Data-only reader for Quicker V1's WPF NRBF action payloads (MS-NRBF).
//! Never loads assemblies, constructs CLR objects, or recursively expands references.
//! Keep IDs in a flat table so shared references and cyclic action children are safe.
//! Wire specification: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-nrbf/75b9fe09-be15-475f-85b8-ae7b7558cfe5

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

const MAX_BYTES: usize = 32 * 1024 * 1024;
const MAX_OBJECTS: usize = 32_768;
const MAX_VALUES: usize = 262_144;
const MAX_MEMBERS: usize = 4_096;
const MAX_DEPTH: usize = 64;

pub(crate) fn decode(bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() || bytes.len() > MAX_BYTES {
        return None;
    }
    // Text and WPF root strings retain their original JSON, including V2 fields.
    if let Some(text) = crate::clipboard_text::decode(bytes) {
        crate::quicker_action::action_id_from_payload(&text).ok()?;
        return Some(text);
    }
    let payload = bytes
        .strip_prefix(crate::clipboard_text::MANAGED_PREFIX)
        .unwrap_or(bytes);
    let mut reader = Reader::new(payload);
    let root = reader.parse()?;
    let mut action = reader.class(root)?;
    if action.0.name == "Quicker.Common.ActionItemDragObject" {
        action = reader.class(
            action
                .1
                .get(action.0.field("Action")?)
                .copied()?
                .reference()?,
        )?;
    }
    if action.0.name != "Quicker.Common.ActionItem" {
        return None;
    }
    let mut json = serde_json::Map::new();
    // Same metadata persisted by C# SimpleActionItem; action execution stays in Quicker.
    for key in [
        "Id",
        "Title",
        "Description",
        "Icon",
        "TemplateId",
        "SharedActionId",
    ] {
        if let Some(index) = action.0.field(key) {
            match action.1[index] {
                Value::Null => {}
                Value::Ref(id) => {
                    let Node::String(text) = reader.objects.get(&id)? else {
                        return None;
                    };
                    json.insert(key.into(), serde_json::Value::String((*text).into()));
                }
                _ => return None,
            }
        }
    }
    let text = serde_json::Value::Object(json).to_string();
    crate::quicker_action::action_id_from_payload(&text).ok()?;
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    const ACTION: &[u8] = include_bytes!("../tests/fixtures/quicker-action-v1-wpf.bin");
    const DRAG: &[u8] = include_bytes!("../tests/fixtures/quicker-drag-v1-wpf.bin");

    #[test]
    fn quicker_wire_v1_real_wpf_objects_preserve_csharp_action_metadata() {
        let expected = serde_json::json!({
            "Id":"08c06820-416c-4463-99b0-18c711634b3a", "Title":"Rust 动作 🦀",
            "Description":"native-v1-fixture", "Icon":"fa:Light_Bolt",
            "TemplateId":"5042d4d2-2979-4fff-4a25-08da404596c9", "SharedActionId":"fixture-share"
        });
        for bytes in [ACTION, DRAG, &ACTION[16..], &DRAG[16..]] {
            let json = decode(bytes).expect("real Quicker.Common object payload");
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&json).unwrap(),
                expected
            );
            assert_eq!(
                crate::quicker_action::payload_menu_action_template(&json, ""),
                Some(crate::quicker_action::MenuActionTemplate::File)
            );
        }
        // The drag fixture has repeated child references, a cycle back to the
        // action, boxed DateTime/enum/bool values, and shared string references.
        // They must not be expanded or included in the persisted SimpleActionItem.
        assert_eq!(decode(ACTION), decode(DRAG));
    }

    fn header() -> Vec<u8> {
        vec![0, 1, 0, 0, 0, 255, 255, 255, 255, 1, 0, 0, 0, 0, 0, 0, 0]
    }
    fn int(bytes: &mut Vec<u8>, value: i32) {
        bytes.extend(value.to_le_bytes());
    }
    fn string(bytes: &mut Vec<u8>, value: &str) {
        assert!(value.len() < 128);
        bytes.push(value.len() as u8);
        bytes.extend(value.as_bytes());
    }

    #[test]
    fn quicker_wire_v1_rejects_damaged_or_unrelated_objects() {
        for bytes in [ACTION, DRAG] {
            for end in 16..bytes.len() {
                assert!(decode(&bytes[..end]).is_none(), "truncated at {end}");
            }
            let mut extra = bytes.to_vec();
            extra.push(1);
            assert!(decode(&extra).is_none());
            let mut padding = bytes.to_vec();
            padding.extend([0; 16]);
            assert_eq!(decode(&padding), decode(bytes));
        }
        let mut unrelated = ACTION.to_vec();
        let class = b"Quicker.Common.ActionItem";
        let index = unrelated
            .windows(class.len())
            .position(|p| p == class)
            .unwrap();
        unrelated[index] = b'X';
        assert!(decode(&unrelated).is_none());
        // A valid stream with a dangling member reference cannot produce a partial action.
        let mut dangling = header();
        dangling.push(2);
        int(&mut dangling, 1);
        string(&mut dangling, "Quicker.Common.ActionItem");
        int(&mut dangling, 1);
        string(&mut dangling, "Id");
        dangling.push(9);
        int(&mut dangling, 999);
        dangling.push(11);
        assert!(Reader::new(&dangling).parse().is_none());
        assert!(decode(&dangling).is_none());
    }

    #[test]
    fn quicker_wire_v1_bounds_allocations_nesting_and_null_runs() {
        let mut huge_array = header();
        huge_array.push(16);
        int(&mut huge_array, 1);
        int(&mut huge_array, i32::MAX);
        assert!(Reader::new(&huge_array).parse().is_none());
        let mut null_run = header();
        null_run.push(16);
        int(&mut null_run, 1);
        int(&mut null_run, 1);
        null_run.push(14);
        int(&mut null_run, 2);
        null_run.push(11);
        assert!(Reader::new(&null_run).parse().is_none());
        let mut duplicate = header();
        duplicate.push(16);
        int(&mut duplicate, 1);
        int(&mut duplicate, 2);
        for _ in 0..2 {
            duplicate.push(6);
            int(&mut duplicate, 2);
            string(&mut duplicate, "same");
        }
        duplicate.push(11);
        assert!(Reader::new(&duplicate).parse().is_none());
        let mut deep = header();
        for id in 1..=MAX_DEPTH as i32 + 2 {
            deep.push(2);
            int(&mut deep, id);
            string(&mut deep, "node");
            int(&mut deep, 1);
            string(&mut deep, "next");
        }
        deep.extend([10, 11]);
        assert!(Reader::new(&deep).parse().is_none());
        assert!(decode(&vec![0; MAX_BYTES + 1]).is_none());
    }
}

#[derive(Clone, Copy)]
enum Value {
    Null,
    Ref(i32),
    Primitive,
}
impl Value {
    fn reference(self) -> Option<i32> {
        if let Self::Ref(id) = self {
            Some(id)
        } else {
            None
        }
    }
}
enum Node<'a> {
    String(&'a str),
    Class(Arc<Schema<'a>>, Vec<Value>),
    Array,
}
struct Schema<'a> {
    name: &'a str,
    names: Vec<&'a str>,
    types: Vec<Option<u8>>,
}
impl Schema<'_> {
    fn field(&self, key: &str) -> Option<usize> {
        self.names.iter().position(|name| {
            let name = name
                .strip_prefix('<')
                .and_then(|v| v.strip_suffix(">k__BackingField"))
                .unwrap_or(name);
            name == key
        })
    }
}
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
    budget: usize,
    ids: HashSet<i32>,
    libraries: HashSet<i32>,
    refs: Vec<i32>,
    schemas: HashMap<i32, Arc<Schema<'a>>>,
    objects: HashMap<i32, Node<'a>>,
}
impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            pos: 0,
            budget: MAX_VALUES,
            ids: HashSet::new(),
            libraries: HashSet::new(),
            refs: Vec::new(),
            schemas: HashMap::new(),
            objects: HashMap::new(),
        }
    }
    fn charge(&mut self, count: usize) -> Option<()> {
        self.budget = self.budget.checked_sub(count)?;
        Some(())
    }
    fn take(&mut self, count: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(count)?;
        let bytes = self.bytes.get(self.pos..end)?;
        self.pos = end;
        Some(bytes)
    }
    fn byte(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn int(&mut self) -> Option<i32> {
        Some(i32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn count(&mut self, max: usize) -> Option<usize> {
        let value = usize::try_from(self.int()?).ok()?;
        (value <= max).then_some(value)
    }
    fn string(&mut self) -> Option<&'a str> {
        let mut length = 0usize;
        for shift in (0..=28).step_by(7) {
            let byte = self.byte()?;
            if shift == 28 && byte > 7 {
                return None;
            }
            length |= usize::from(byte & 127) << shift;
            if byte < 128 {
                return std::str::from_utf8(self.take(length)?).ok();
            }
        }
        None
    }
    fn object_id(&mut self) -> Option<i32> {
        let id = self.int()?;
        // BinaryFormatter also uses negative IDs for inline value types.
        if id == 0 || self.ids.len() >= MAX_OBJECTS || !self.ids.insert(id) {
            return None;
        }
        Some(id)
    }
    fn library(&mut self) -> Option<()> {
        self.charge(1)?;
        let id = self.int()?;
        if id <= 0 || !self.libraries.insert(id) {
            return None;
        }
        self.string()?;
        Some(())
    }
    fn parse(&mut self) -> Option<i32> {
        if self.byte()? != 0 {
            return None;
        }
        let root = self.int()?;
        if root <= 0 || self.int()? != -1 || self.int()? != 1 || self.int()? != 0 {
            return None;
        }
        loop {
            let tag = self.byte()?;
            if tag == 11 {
                break;
            }
            if tag == 12 {
                self.library()?;
            } else if matches!(tag, 1..=7 | 15..=17) {
                self.record(tag, 0)?;
            } else {
                return None;
            }
        }
        if self.bytes[self.pos..].iter().any(|&b| b != 0)
            || !self.objects.contains_key(&root)
            || self.refs.iter().any(|id| !self.objects.contains_key(id))
        {
            return None;
        }
        Some(root)
    }
    fn class(&self, id: i32) -> Option<(&Schema<'a>, &[Value])> {
        match self.objects.get(&id)? {
            Node::Class(schema, values) => Some((schema, values)),
            _ => None,
        }
    }
    // Return a primitive code only for inline primitive members; other types are records.
    fn type_info(&mut self, kind: u8) -> Option<Option<u8>> {
        match kind {
            0 | 7 => {
                let code = self.byte()?;
                if !(1..=16).contains(&code) || code == 4 {
                    return None;
                }
                Some((kind == 0).then_some(code))
            }
            1 | 2 | 5 | 6 => Some(None),
            3 => {
                self.string()?;
                Some(None)
            }
            4 => {
                self.string()?;
                let library = self.int()?;
                self.libraries.contains(&library).then_some(None)
            }
            _ => None,
        }
    }
    fn primitive(&mut self, code: u8) -> Option<Value> {
        self.charge(1)?;
        match code {
            1 => {
                if self.byte()? > 1 {
                    return None;
                }
            }
            2 | 10 => {
                self.take(1)?;
            }
            7 | 14 => {
                self.take(2)?;
            }
            8 | 11 | 15 => {
                self.take(4)?;
            }
            6 | 9 | 12 | 13 | 16 => {
                self.take(8)?;
            }
            3 => {
                let first = *self.bytes.get(self.pos)?;
                let count = match first {
                    0..=127 => 1,
                    194..=223 => 2,
                    224..=239 => 3,
                    _ => return None,
                };
                std::str::from_utf8(self.take(count)?).ok()?;
            }
            5 => {
                self.string()?;
            } // Decimal is length-prefixed text in NRBF.
            _ => return None,
        }
        Some(Value::Primitive)
    }
    fn value(&mut self, kind: Option<u8>, depth: usize) -> Option<Value> {
        if let Some(code) = kind {
            return self.primitive(code);
        }
        loop {
            let tag = self.byte()?;
            if tag == 12 {
                self.library()?;
            } else {
                return self.record(tag, depth);
            }
        }
    }
    fn record(&mut self, tag: u8, depth: usize) -> Option<Value> {
        if depth > MAX_DEPTH {
            return None;
        }
        self.charge(1)?;
        match tag {
            1..=5 => {
                let id = self.object_id()?;
                let schema = if tag == 1 {
                    let metadata = self.int()?;
                    self.schemas.get(&metadata)?.clone()
                } else {
                    let name = self.string()?;
                    let count = self.count(MAX_MEMBERS)?;
                    self.charge(count)?;
                    let names = (0..count)
                        .map(|_| self.string())
                        .collect::<Option<Vec<_>>>()?;
                    if names.iter().copied().collect::<HashSet<_>>().len() != count {
                        return None;
                    }
                    let types = if tag >= 4 {
                        let kinds = self.take(count)?;
                        kinds
                            .iter()
                            .map(|&kind| self.type_info(kind))
                            .collect::<Option<Vec<_>>>()?
                    } else {
                        vec![None; count]
                    };
                    if tag == 3 || tag == 5 {
                        let library = self.int()?;
                        if !self.libraries.contains(&library) {
                            return None;
                        }
                    }
                    let schema = Arc::new(Schema { name, names, types });
                    self.schemas.insert(id, schema.clone());
                    schema
                };
                let values = schema
                    .types
                    .iter()
                    .map(|&kind| self.value(kind, depth + 1))
                    .collect::<Option<Vec<_>>>()?;
                self.objects.insert(id, Node::Class(schema, values));
                Some(Value::Ref(id))
            }
            6 => {
                let id = self.object_id()?;
                let text = self.string()?;
                self.objects.insert(id, Node::String(text));
                Some(Value::Ref(id))
            }
            7 | 15..=17 => self.array(tag, depth),
            8 => {
                let code = self.byte()?;
                self.primitive(code)
            }
            9 => {
                let id = self.int()?;
                if id == 0 {
                    return None;
                }
                self.refs.push(id);
                Some(Value::Ref(id))
            }
            10 => Some(Value::Null),
            _ => None,
        }
    }
    fn array(&mut self, tag: u8, depth: usize) -> Option<Value> {
        let id = self.object_id()?;
        let (length, primitive) = if tag == 7 {
            let shape = self.byte()?;
            if shape > 5 {
                return None;
            }
            let rank = self.count(32)?;
            if rank == 0 || (matches!(shape, 0 | 1 | 3 | 4) && rank != 1) {
                return None;
            }
            let mut length = 1usize;
            for _ in 0..rank {
                length = length.checked_mul(self.count(MAX_VALUES)?)?;
            }
            if length > MAX_VALUES {
                return None;
            }
            if shape >= 3 {
                for _ in 0..rank {
                    self.int()?;
                }
            }
            let kind = self.byte()?;
            (length, self.type_info(kind)?)
        } else {
            let length = self.count(MAX_VALUES)?;
            let kind = if tag == 15 { self.type_info(0)? } else { None };
            (length, kind)
        };
        self.charge(length)?;
        self.objects.insert(id, Node::Array);
        let mut index = 0;
        while index < length {
            if let Some(code) = primitive {
                self.primitive(code)?;
                index += 1;
            } else {
                let tag = self.byte()?;
                let count = match tag {
                    12 => {
                        self.library()?;
                        continue;
                    }
                    13 => usize::from(self.byte()?),
                    14 => self.count(length)?,
                    _ => {
                        self.record(tag, depth + 1)?;
                        1
                    }
                };
                if count == 0 {
                    return None;
                }
                index = index.checked_add(count)?;
                if index > length {
                    return None;
                }
            }
        }
        Some(Value::Ref(id))
    }
}
