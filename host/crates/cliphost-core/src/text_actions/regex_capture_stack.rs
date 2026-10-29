//! Persistent capture logs: backtracking copies heads, never capture histories.
//! Signed markers follow .NET's RegexRunner.TransferCapture and Match capture log:
//! https://github.com/dotnet/runtime/tree/v10.0.0/src/libraries/System.Text.RegularExpressions/src/System/Text/RegularExpressions
use regex_automata::Span;

struct Entry {
    start: isize,
    length: isize,
    previous: Option<usize>,
    skip: Option<usize>,
    count: usize,
}

pub(super) struct Captures {
    entries: Vec<Entry>,
    // Original UTF-16 positions paired with positions in the execution string.
    boundaries: Vec<(usize, usize)>,
    reversed: bool,
}

impl Captures {
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn new(text: &str, reversed: bool) -> Self {
        let mut boundaries = vec![(0, if reversed { text.len() } else { 0 })];
        let mut units = 0;
        let chars: Box<dyn Iterator<Item = (usize, char)>> = if reversed {
            Box::new(text.char_indices().rev())
        } else {
            Box::new(text.char_indices())
        };
        for (byte, ch) in chars {
            units += ch.len_utf16();
            boundaries.push((units, if reversed { byte } else { byte + ch.len_utf8() }));
        }
        Self {
            entries: Vec::new(),
            boundaries,
            reversed,
        }
    }

    fn record_at(&self, mut head: usize, count: usize) -> Option<usize> {
        if count == 0 || count > self.entries[head].count {
            return None;
        }
        while self.entries[head].count != count {
            let entry = &self.entries[head];
            head = match entry.skip {
                Some(skip) if self.entries[skip].count >= count => skip,
                _ => entry.previous?,
            };
        }
        Some(head)
    }

    fn raw(&self, head: usize, index: isize) -> Result<isize, String> {
        let index = usize::try_from(index).map_err(|_| invalid_capture())?;
        let entry = self
            .record_at(head, index / 2 + 1)
            .ok_or_else(invalid_capture)?;
        let entry = &self.entries[entry];
        Ok(if index % 2 == 0 {
            entry.start
        } else {
            entry.length
        })
    }

    fn push(&mut self, head: &mut Option<usize>, start: isize, length: isize) {
        let count = head.map_or(1, |i| self.entries[i].count + 1);
        // A skip to the count with its lowest set bit cleared keeps old-log
        // lookups short when many captures are removed consecutively.
        let skip = head.and_then(|i| self.record_at(i, count & (count - 1)));
        let index = self.entries.len();
        self.entries.push(Entry {
            start,
            length,
            previous: *head,
            skip,
            count,
        });
        *head = Some(index);
    }

    fn coordinate(&self, byte: usize) -> isize {
        let index = self
            .boundaries
            .binary_search_by(|(_, value)| {
                let order = value.cmp(&byte);
                if self.reversed {
                    order.reverse()
                } else {
                    order
                }
            })
            .expect("capture endpoint is a character boundary");
        self.boundaries[index].0 as isize
    }

    fn span(&self, start: isize, length: isize) -> Result<Span, String> {
        let byte = |unit: isize| {
            let unit = usize::try_from(unit).map_err(|_| invalid_capture())?;
            self.boundaries
                .binary_search_by_key(&unit, |&(u, _)| u)
                .map(|i| self.boundaries[i].1)
                .map_err(|_| invalid_capture())
        };
        if length < 0 {
            return Err(invalid_capture());
        }
        let a = byte(start)?;
        let b = byte(start.checked_add(length).ok_or_else(invalid_capture)?)?;
        Ok(Span {
            start: a.min(b),
            end: a.max(b),
        })
    }

    pub fn save(&mut self, head: &mut Option<usize>, span: Span) {
        let a = self.coordinate(span.start);
        let b = self.coordinate(span.end);
        self.push(head, a.min(b), (a - b).abs());
    }

    pub fn matched(&self, head: Option<usize>) -> bool {
        head.is_some_and(|i| self.entries[i].length != -2)
    }

    fn current(&self, head: usize) -> Result<(isize, isize), String> {
        let entry = &self.entries[head];
        let resolve = |value| {
            if value < 0 {
                self.raw(head, -3 - value)
            } else {
                Ok(value)
            }
        };
        Ok((resolve(entry.start)?, resolve(entry.length)?))
    }

    pub fn reference(&self, head: Option<usize>) -> Result<Option<Span>, String> {
        if !self.matched(head) {
            return Ok(None);
        }
        let (start, length) = self.current(head.unwrap())?;
        self.span(start, length).map(Some)
    }

    pub fn balance(
        &mut self,
        heads: &mut [Option<usize>],
        destination: Option<usize>,
        source: usize,
        span: Span,
    ) -> Result<bool, String> {
        if !self.matched(heads[source]) {
            return Ok(false);
        }
        let head = heads[source].unwrap();
        let (source_start, source_length) = self.current(head)?;
        let source_end = source_start + source_length;
        let a = self.coordinate(span.start);
        let b = self.coordinate(span.end);
        let (mut start, mut end) = (a.min(b), a.max(b));
        // Match .NET TransferCapture, including leftward intervals. A negative
        // interval is a transient log entry, never an unchecked string slice.
        if start >= source_end {
            end = start;
            start = source_end;
        } else if end <= source_start {
            start = source_start;
        } else {
            end = end.min(source_end);
            start = start.max(source_start);
        }

        let entry = &self.entries[head];
        let target = if entry.start < 0 {
            -3 - entry.start
        } else {
            entry.count as isize * 2 - 2
        } - 2;
        let (back_start, back_length) = if target >= 0 && self.raw(head, target)? < 0 {
            (self.raw(head, target)?, self.raw(head, target + 1)?)
        } else {
            (-3 - target, -4 - target)
        };
        self.push(&mut heads[source], back_start, back_length);
        if let Some(destination) = destination {
            self.push(&mut heads[destination], start, end - start);
        }
        Ok(true)
    }

    pub fn finish(&self, head: Option<usize>) -> Result<Option<Span>, String> {
        let Some(mut head) = head else {
            return Ok(None);
        };
        let mut entries = Vec::with_capacity(self.entries[head].count);
        loop {
            let entry = &self.entries[head];
            entries.push((entry.start, entry.length));
            match entry.previous {
                Some(previous) => head = previous,
                None => break,
            }
        }
        // .NET compacts the signed capture log before exposing replacement
        // values. This also preserves its right-to-left transfer behavior.
        let mut values = Vec::new();
        for (start, length) in entries.into_iter().rev() {
            for value in [start, length] {
                if value < 0 {
                    values.pop();
                } else {
                    values.push(value);
                }
            }
        }
        let count = values.len() / 2;
        if count == 0 {
            return Ok(None);
        }
        self.span(values[count * 2 - 2], values[count * 2 - 1])
            .map(Some)
    }
}

fn invalid_capture() -> String {
    "平衡组产生无效捕获范围，已停止替换。".into()
}
