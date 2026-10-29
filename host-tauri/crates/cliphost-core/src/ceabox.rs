//! Typed, bounded results for the native CeaBox aggregate search.
//! Ranking is pure; storage and Quicker callbacks live in the host adapter.

use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};

use crate::model::{ClipRecord, FavorRecord};

pub const MAX_PAGE_SIZE: usize = 100;
pub const MAX_PAGE_OFFSET: usize = 5_000;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CeaBoxSource {
    #[default]
    All,
    Favorite,
    Clipboard,
    Action,
}

impl CeaBoxSource {
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "favorite" | "favor" | "favorites" => Self::Favorite,
            "clipboard" | "clip" | "history" => Self::Clipboard,
            "action" | "actions" => Self::Action,
            _ => Self::All,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Favorite => "favorite",
            Self::Clipboard => "clipboard",
            Self::Action => "action",
        }
    }

    pub(crate) fn includes(self, source: Self) -> bool {
        self == Self::All || self == source
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CeaBoxExecutionTarget {
    pub source: CeaBoxSource,
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CeaBoxItem {
    pub id: String,
    pub source: CeaBoxSource,
    pub kind: String,
    pub title: String,
    pub context: String,
    pub time: String,
    pub is_folder: bool,
    pub target: CeaBoxExecutionTarget,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CeaBoxSourceStatus {
    pub source: CeaBoxSource,
    pub available: bool,
    /// A source can fail independently without discarding other sources.
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CeaBoxPage {
    pub items: Vec<CeaBoxItem>,
    pub total: usize,
    pub skip: usize,
    pub take: usize,
    pub has_more: bool,
    pub sources: Vec<CeaBoxSourceStatus>,
}

/// A bounded replacement snapshot. Partial totals include sources scanned so
/// far; only `complete` snapshots carry the final aggregate total.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CeaBoxSearchUpdate {
    pub page: CeaBoxPage,
    pub complete: bool,
    pub elapsed_ms: u64,
}

pub(crate) fn field_score(text: &str, query: &str, title: bool) -> i32 {
    if query.is_empty() || text.is_empty() {
        return 0;
    }
    // Match the callback's NFKC/case/whitespace ranking normalization. Source
    // qualification still uses its established matcher before reaching here.
    let text = crate::clip_fuzzy::normalize_for_search(text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    let query = crate::clip_fuzzy::normalize_for_search(query)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    let quality = if text == query {
        5
    } else if text.starts_with(&query) {
        4
    } else if text.contains(&query) {
        3
    } else if query
        .split(' ')
        .filter(|key| !key.is_empty())
        .all(|key| text.contains(key))
    {
        2
    } else if crate::clip_fuzzy::pinyin_search(&query, &[&text]) {
        1
    } else {
        0
    };
    match (title, quality) {
        (true, 5) => 1_000,
        (true, 4) => 800,
        (true, 3) => 700,
        (true, 2) => 650,
        (true, 1) => 500,
        (false, 5) => 450,
        (false, 4) => 350,
        (false, 3) => 300,
        (false, 2) => 250,
        (false, 1) => 200,
        _ => 0,
    }
}

#[derive(Clone)]
struct Ranked {
    item: CeaBoxItem,
    score: i32,
    time: i64,
    ordinal: usize,
}

impl PartialEq for Ranked {
    fn eq(&self, other: &Self) -> bool {
        (self.score, self.time, self.ordinal) == (other.score, other.time, other.ordinal)
    }
}
impl Eq for Ranked {}
impl PartialOrd for Ranked {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Ranked {
    fn cmp(&self, other: &Self) -> Ordering {
        self.score
            .cmp(&other.score)
            .then(self.time.cmp(&other.time))
            .then_with(|| other.ordinal.cmp(&self.ordinal))
    }
}

/// At most one requested prefix is retained, even when all history matches.
#[derive(Clone)]
pub(crate) struct Collector {
    query: String,
    skip: usize,
    take: usize,
    total: usize,
    next_ordinal: usize,
    best: BinaryHeap<Reverse<Ranked>>,
    retain: usize,
}

impl Collector {
    pub(crate) fn new(query: &str, skip: usize, take: usize) -> Result<Self, String> {
        if skip > MAX_PAGE_OFFSET {
            return Err("搜索页码超出范围，请缩小搜索条件。".into());
        }
        Ok(Self {
            query: crate::clip_fuzzy::normalize_for_search(query),
            skip,
            take: take.clamp(1, MAX_PAGE_SIZE),
            total: 0,
            next_ordinal: 0,
            best: BinaryHeap::new(),
            retain: skip + take.clamp(1, MAX_PAGE_SIZE),
        })
    }

    /// Retain the legal pagination prefix once, so subsequent pages can use the
    /// same ranking instead of rescanning and reparsing the entire store.
    pub(crate) fn cached(query: &str, skip: usize, take: usize) -> Result<Self, String> {
        let mut collector = Self::new(query, skip, take)?;
        collector.retain = MAX_PAGE_OFFSET + MAX_PAGE_SIZE;
        Ok(collector)
    }

    pub(crate) fn for_page(&self, skip: usize, take: usize) -> Self {
        let mut collector = self.clone();
        collector.skip = skip;
        collector.take = take.clamp(1, MAX_PAGE_SIZE);
        collector
    }

    pub(crate) fn prefix_len(&self) -> usize {
        self.skip + self.take
    }

    pub(crate) fn has_matches(&self) -> bool {
        self.total > 0
    }

    /// Callers have already qualified the record using the existing source matcher.
    pub(crate) fn add(&mut self, item: CeaBoxItem, secondary: &str, time: i64) {
        let title = item.title.clone();
        self.add_with_title(item, &title, secondary, time);
    }

    pub(crate) fn add_with_title(
        &mut self,
        item: CeaBoxItem,
        title: &str,
        secondary: &str,
        time: i64,
    ) {
        let score =
            field_score(title, &self.query, true).max(field_score(secondary, &self.query, false));
        self.add_with_score(item, score, time);
    }

    pub(crate) fn add_with_score(&mut self, item: CeaBoxItem, score: i32, time: i64) {
        let score = score
            + if item.source == CeaBoxSource::Favorite {
                80
            } else {
                0
            };
        let ranked = Ranked {
            item,
            score,
            time,
            ordinal: self.next_ordinal,
        };
        self.next_ordinal += 1;
        self.total += 1;
        if self.best.len() < self.retain {
            self.best.push(Reverse(ranked));
        } else if self.best.peek().is_some_and(|worst| ranked > worst.0) {
            self.best.pop();
            self.best.push(Reverse(ranked));
        }
    }

    /// Action RPC returns only its own ranked prefix; its total remains exact.
    pub(crate) fn add_unmaterialized(&mut self, count: usize) {
        self.total = self.total.saturating_add(count);
    }

    /// Clone only the requested page, not the retained pagination prefix, when
    /// publishing a stream update.
    pub(crate) fn snapshot(&self, sources: Vec<CeaBoxSourceStatus>) -> CeaBoxPage {
        self.snapshot_merged(None, sources)
    }

    /// Action ranks remain separate from the reusable local ranking. Their
    /// ordinals follow every local ordinal, preserving the original stable tie
    /// order even when the action source finishes before local scanning.
    pub(crate) fn snapshot_merged(
        &self,
        actions: Option<&Self>,
        sources: Vec<CeaBoxSourceStatus>,
    ) -> CeaBoxPage {
        let mut best: Vec<_> = self.best.iter().map(|item| (&item.0, 0usize)).collect();
        if let Some(actions) = actions {
            best.extend(actions.best.iter().map(|item| (&item.0, self.next_ordinal)));
        }
        let compare = |a: &(&Ranked, usize), b: &(&Ranked, usize)| {
            b.0.score
                .cmp(&a.0.score)
                .then(b.0.time.cmp(&a.0.time))
                .then((a.0.ordinal + a.1).cmp(&(b.0.ordinal + b.1)))
        };
        let requested = self.prefix_len();
        if best.len() > requested {
            best.select_nth_unstable_by(requested, compare);
            best.truncate(requested);
        }
        best.sort_unstable_by(compare);
        let total = self
            .total
            .saturating_add(actions.map_or(0, |actions| actions.total));
        CeaBoxPage {
            items: best
                .into_iter()
                .skip(self.skip)
                .take(self.take)
                .map(|(ranked, _)| ranked.item.clone())
                .collect(),
            total,
            skip: self.skip,
            take: self.take,
            has_more: self.skip.saturating_add(self.take) < total,
            sources,
        }
    }

    pub(crate) fn finish(self, sources: Vec<CeaBoxSourceStatus>) -> CeaBoxPage {
        let mut best: Vec<_> = self.best.into_iter().map(|item| item.0).collect();
        best.sort_by(|a, b| b.cmp(a));
        let items = best
            .into_iter()
            .skip(self.skip)
            .take(self.take)
            .map(|ranked| ranked.item)
            .collect();
        CeaBoxPage {
            items,
            total: self.total,
            skip: self.skip,
            take: self.take,
            has_more: self.skip.saturating_add(self.take) < self.total,
            sources,
        }
    }
}

pub(crate) fn clip_item(item: &ClipRecord) -> CeaBoxItem {
    let source = CeaBoxSource::Clipboard;
    CeaBoxItem {
        id: item.id.clone(),
        source,
        kind: item.kind.as_str().into(),
        title: item.preview(160),
        context: item.source_process_name.clone(),
        time: item.create_time.to_rfc3339(),
        is_folder: false,
        target: CeaBoxExecutionTarget {
            source,
            id: item.id.clone(),
        },
    }
}

pub(crate) fn favor_item(item: &FavorRecord, context: String) -> CeaBoxItem {
    let source = CeaBoxSource::Favorite;
    CeaBoxItem {
        id: item.id.clone(),
        source,
        kind: item.kind.as_str().into(),
        title: item.name.clone(),
        context,
        time: item.update_time.to_rfc3339(),
        is_folder: item.is_folder,
        target: CeaBoxExecutionTarget {
            source,
            id: item.id.clone(),
        },
    }
}

/// First reachable folder path, with canonical shared/ParentId-only edges.
/// Cycles, shared children and disconnected folders all have bounded traversal.
pub(crate) fn favorite_contexts(
    items: &[FavorRecord],
    ordered_roots: &[String],
) -> HashMap<String, String> {
    let by_id: HashMap<_, _> = items.iter().map(|item| (item.id.as_str(), item)).collect();
    let children = crate::panel::effective_children(items);
    let mut roots: Vec<_> = items
        .iter()
        .filter(|item| item.is_folder && item.parent_id.is_none())
        .collect();
    roots.sort_by(|a, b| {
        ordered_roots
            .iter()
            .position(|id| id == &a.id)
            .unwrap_or(usize::MAX)
            .cmp(
                &ordered_roots
                    .iter()
                    .position(|id| id == &b.id)
                    .unwrap_or(usize::MAX),
            )
            .then(a.sub_index.cmp(&b.sub_index))
            .then(a.id.cmp(&b.id))
    });
    let mut pending: VecDeque<_> = roots
        .into_iter()
        .map(|item| (item.id.clone(), String::new()))
        .collect();
    let mut visited = HashSet::new();
    let mut contexts = HashMap::new();
    let mut disconnected_cursor = 0;
    loop {
        while let Some((id, parent)) = pending.pop_front() {
            if !visited.insert(id.clone()) {
                continue;
            }
            let Some(item) = by_id.get(id.as_str()) else {
                continue;
            };
            contexts.insert(id.clone(), parent.clone());
            if item.is_folder {
                let path: String = if parent.is_empty() {
                    item.name.clone()
                } else {
                    format!("{parent} / {}", item.name)
                }
                .chars()
                .take(512)
                .collect();
                for child in children.get(&id).into_iter().flatten() {
                    if !visited.contains(child) {
                        pending.push_back((child.clone(), path.clone()));
                    }
                }
            }
        }
        // Include detached items without pretending their missing parent exists.
        while disconnected_cursor < items.len() && visited.contains(&items[disconnected_cursor].id)
        {
            disconnected_cursor += 1;
        }
        let Some(item) = items.get(disconnected_cursor) else {
            break;
        };
        disconnected_cursor += 1;
        pending.push_back((item.id.clone(), String::new()));
    }
    contexts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, source: CeaBoxSource, title: &str) -> CeaBoxItem {
        CeaBoxItem {
            id: id.into(),
            source,
            kind: "TextClipItem".into(),
            title: title.into(),
            context: String::new(),
            time: String::new(),
            is_folder: false,
            target: CeaBoxExecutionTarget {
                source,
                id: id.into(),
            },
        }
    }

    #[test]
    fn matching_favorites_win_but_literal_exact_beats_boosted_partial() {
        let mut c = Collector::new("项目", 0, 20).unwrap();
        c.add(row("clip-exact", CeaBoxSource::Clipboard, "项目"), "", 0);
        c.add(
            row("favor-prefix", CeaBoxSource::Favorite, "项目材料"),
            "",
            1,
        );
        c.add(row("favor-exact", CeaBoxSource::Favorite, "项目"), "", 0);
        let page = c.finish(vec![]);
        assert_eq!(
            page.items
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["favor-exact", "clip-exact", "favor-prefix"]
        );
    }

    #[test]
    fn pinyin_multiword_and_unicode_use_the_existing_matcher() {
        assert_eq!(field_score("项目计划", "xmjh", true), 500);
        assert_eq!(field_score("项目计划 会议", "计划 项目", true), 650);
        assert_eq!(field_score("ＡＢＣ", "abc", true), 1_000);
        assert_eq!(field_score("项目计划", "missing", true), 0);
    }

    #[test]
    fn ranking_keeps_stable_ties_and_only_requested_prefix() {
        let mut c = Collector::new("", 2, 2).unwrap();
        for id in 0..30 {
            c.add(row(&id.to_string(), CeaBoxSource::Action, "动作"), "", 0);
        }
        assert_eq!(c.best.len(), 4);
        let page = c.finish(vec![]);
        assert_eq!(page.total, 30);
        assert!(page.has_more);
        assert_eq!(
            page.items
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["2", "3"]
        );
        assert_eq!(page.items[0].target.id, "2");
    }

    #[test]
    fn streaming_snapshot_partition_equals_the_final_fully_ranked_page() {
        for (skip, take) in [(0, 20), (37, 25), (150, 50), (400, 100)] {
            let mut collector = Collector::cached("项目", skip, take).unwrap();
            for index in 0..550 {
                collector.add_with_score(
                    row(
                        &index.to_string(),
                        if index % 3 == 0 {
                            CeaBoxSource::Favorite
                        } else {
                            CeaBoxSource::Clipboard
                        },
                        "项目",
                    ),
                    (index % 9) * 100,
                    (index % 7) as i64,
                );
            }
            let snapshot = collector.snapshot(Vec::new());
            let final_page = collector.finish(Vec::new());
            assert_eq!(snapshot.items, final_page.items);
            assert_eq!(snapshot.total, final_page.total);
            assert_eq!(snapshot.has_more, final_page.has_more);
        }
    }

    #[test]
    fn concurrent_action_merge_preserves_local_ties_and_full_ranking_pagination() {
        for (skip, take) in [(0, 20), (10, 40), (70, 20)] {
            let mut local = Collector::cached("", skip, take).unwrap();
            let mut actions = Collector::cached("", 0, 100).unwrap();
            for index in 0..60 {
                actions.add_with_score(
                    row(&format!("action-{index}"), CeaBoxSource::Action, ""),
                    500,
                    0,
                );
            }
            for index in 0..60 {
                local.add_with_score(
                    row(&format!("local-{index}"), CeaBoxSource::Clipboard, ""),
                    500,
                    0,
                );
            }
            let mut serial = local.clone();
            for index in 0..60 {
                serial.add_with_score(
                    row(&format!("action-{index}"), CeaBoxSource::Action, ""),
                    500,
                    0,
                );
            }
            let merged = local.snapshot_merged(Some(&actions), Vec::new());
            let final_serial = serial.finish(Vec::new());
            assert_eq!(merged.items, final_serial.items);
            assert_eq!(merged.total, final_serial.total);
            assert_eq!(merged.has_more, final_serial.has_more);
        }
    }

    #[test]
    fn directory_projection_retains_parent_only_shared_and_cyclic_records() {
        let mut root = FavorRecord::from_clip(&ClipRecord::new_text("Root", ""), None);
        root.id = "root".into();
        root.is_folder = true;
        root.name = "工作".into();
        let mut child =
            FavorRecord::from_clip(&ClipRecord::new_text("child", ""), Some("root".into()));
        child.id = "child".into();
        child.is_folder = true;
        child.name = "项目".into();
        let mut leaf =
            FavorRecord::from_clip(&ClipRecord::new_text("leaf", ""), Some("child".into()));
        leaf.id = "leaf".into();
        child.children = vec!["root".into(), "leaf".into()];
        let before = serde_json::to_value([&root, &child, &leaf]).unwrap();
        let paths = favorite_contexts(&[root.clone(), child.clone(), leaf.clone()], &[]);
        assert_eq!(paths["leaf"], "工作 / 项目");
        assert_eq!(paths.len(), 3);
        assert_eq!(
            serde_json::to_value([&root, &child, &leaf]).unwrap(),
            before
        );
    }

    #[test]
    fn page_limits_and_source_aliases_are_explicit() {
        assert!(Collector::new("", MAX_PAGE_OFFSET + 1, 1).is_err());
        assert_eq!(
            Collector::new("", 0, usize::MAX).unwrap().take,
            MAX_PAGE_SIZE
        );
        assert_eq!(CeaBoxSource::parse("favor"), CeaBoxSource::Favorite);
        assert_eq!(CeaBoxSource::parse("clip"), CeaBoxSource::Clipboard);
    }

    #[test]
    fn three_sources_share_one_ranking_and_keep_their_real_execution_ids() {
        let mut collector = Collector::new("项目", 0, 3).unwrap();
        collector.add(
            row("history-real-id", CeaBoxSource::Clipboard, "项目"),
            "",
            3,
        );
        collector.add(
            row("favorite-real-id", CeaBoxSource::Favorite, "项目"),
            "",
            1,
        );
        collector.add_with_score(row("action-real-id", CeaBoxSource::Action, "项目"), 1000, 2);
        let page = collector.finish(vec![]);
        assert_eq!(page.total, 3);
        assert_eq!(
            page.items
                .iter()
                .map(|item| item.source)
                .collect::<Vec<_>>(),
            [
                CeaBoxSource::Favorite,
                CeaBoxSource::Clipboard,
                CeaBoxSource::Action
            ]
        );
        assert_eq!(
            page.items[2].target,
            CeaBoxExecutionTarget {
                source: CeaBoxSource::Action,
                id: "action-real-id".into()
            }
        );
    }
}
