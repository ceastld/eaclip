//! Pure, occurrence-based projection of favorite graphs into virtual-list rows.
//!
//! Business records contain relationships and content only. Expansion, selection
//! and scroll state can use `OccurrencePath`, so a shared item under two parents
//! never shares UI state accidentally. No projection step mutates a DTO.

use cliphost_core::dto::FavorItemDto;
use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet, HashSet},
};

pub(crate) type OccurrencePath = Vec<String>;

/// Explicit relationships are authoritative, including an explicitly empty list.
/// `parent_id` is a compatibility fallback only when that parent's adjacency is
/// absent. Both roots and children retain their source order unless requested.
#[derive(Clone, Copy)]
pub(crate) struct TreeGraph<'a> {
    pub items: &'a BTreeMap<String, FavorItemDto>,
    pub roots: &'a [String],
    pub children_by_id: &'a BTreeMap<String, Vec<String>>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ProjectionLimits {
    pub max_rows: usize,
    pub max_occurrences: usize,
    pub max_depth: usize,
}

impl Default for ProjectionLimits {
    fn default() -> Self {
        Self {
            max_rows: 20_000,
            max_occurrences: 100_000,
            max_depth: 128,
        }
    }
}

pub(crate) struct ProjectionOptions<'a> {
    /// Hide this occurrence itself and display its children at depth zero. Keep
    /// the complete prefix in row paths to retain occurrence identity in scope.
    pub scope: Option<&'a [String]>,
    pub expanded: &'a HashSet<OccurrencePath>,
    /// An active search/type filter. Matching descendants retain their ancestors
    /// and temporarily expand them without writing the saved expansion set.
    pub matches: Option<&'a dyn Fn(&FavorItemDto) -> bool>,
    /// Presentation-only expansion, for example traversing non-folder parents
    /// in a folder-only sidebar. This does not change occurrence memory.
    pub auto_expand: Option<&'a dyn Fn(&FavorItemDto) -> bool>,
    /// Stable sibling sorting, never a sort of the flattened graph. `None`
    /// preserves the relationship order, including after filtering.
    pub compare: Option<&'a dyn Fn(&FavorItemDto, &FavorItemDto) -> Ordering>,
    pub limits: ProjectionLimits,
}

impl<'a> ProjectionOptions<'a> {
    pub fn new(expanded: &'a HashSet<OccurrencePath>) -> Self {
        Self {
            scope: None,
            expanded,
            matches: None,
            auto_expand: None,
            compare: None,
            limits: ProjectionLimits::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TreeRow {
    pub item_id: String,
    pub path: OccurrencePath,
    /// Can refer to the hidden scope occurrence for a depth-zero row.
    pub parent_path: Option<OccurrencePath>,
    pub depth: usize,
    pub has_children: bool,
    pub expanded: bool,
    /// A back-edge remains inspectable as a terminal row.
    pub is_cycle: bool,
    /// A broken explicit reference remains inspectable as a terminal row.
    pub is_missing: bool,
    /// This row survives an active filter only to explain a matching descendant.
    pub context_only: bool,
    pub depth_limited: bool,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct TreeProjection {
    pub rows: Vec<TreeRow>,
    pub truncated: bool,
    /// The requested scope no longer exists or its path no longer has these edges.
    pub scope_missing: bool,
    pub examined_occurrences: usize,
}

fn unique_ids(ids: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    ids.into_iter()
        .filter(|id| seen.insert(id.clone()))
        .collect()
}

fn adjacency(graph: TreeGraph<'_>) -> BTreeMap<String, Vec<String>> {
    let mut children: BTreeMap<_, _> = graph
        .children_by_id
        .iter()
        .map(|(id, ids)| (id.clone(), unique_ids(ids.iter().cloned())))
        .collect();
    for (id, item) in graph.items {
        if !item.parent_id.is_empty() && !graph.children_by_id.contains_key(&item.parent_id) {
            children
                .entry(item.parent_id.clone())
                .or_default()
                .push(id.clone());
        }
    }
    children
}

fn mark_reachable(
    roots: impl IntoIterator<Item = String>,
    graph: TreeGraph<'_>,
    children: &BTreeMap<String, Vec<String>>,
    reachable: &mut HashSet<String>,
) {
    let mut pending: Vec<_> = roots.into_iter().collect();
    while let Some(id) = pending.pop() {
        if !reachable.insert(id.clone()) || !graph.items.contains_key(&id) {
            continue;
        }
        if let Some(ids) = children.get(&id) {
            pending.extend(ids.iter().cloned());
        }
    }
}

/// Supplement declared roots with disconnected components, not with collapsed
/// descendants. The ID walk is independent of occurrence expansion and cycles.
fn complete_roots(graph: TreeGraph<'_>, children: &BTreeMap<String, Vec<String>>) -> Vec<String> {
    let incoming: HashSet<_> = children
        .iter()
        .filter(|(parent, _)| graph.items.contains_key(*parent))
        .flat_map(|(_, ids)| ids.iter().cloned())
        .collect();
    let mut roots = unique_ids(graph.roots.iter().cloned());
    let mut root_set: HashSet<_> = roots.iter().cloned().collect();
    for id in graph.items.keys().filter(|id| !incoming.contains(*id)) {
        if root_set.insert(id.clone()) {
            roots.push(id.clone());
        }
    }
    let mut reachable = HashSet::new();
    mark_reachable(roots.iter().cloned(), graph, children, &mut reachable);
    for id in graph.items.keys() {
        if !reachable.contains(id) {
            roots.push(id.clone());
            mark_reachable([id.clone()], graph, children, &mut reachable);
        }
    }
    roots
}

/// Direct membership in stored order for list navigation. Unlike a search
/// projection this never flattens descendants into the root directory.
pub(crate) fn direct_members(graph: TreeGraph<'_>, parent: Option<&str>) -> Vec<String> {
    let children = adjacency(graph);
    let ids = match parent {
        Some(id) => children.get(id).cloned().unwrap_or_default(),
        None => complete_roots(graph, &children),
    };
    ids.into_iter().filter(|id| graph.items.contains_key(id)).collect()
}

/// Cycle checks must use Children edges, including shared references whose
/// compatibility ParentId points to a different occurrence.
pub(crate) fn descendant_ids(graph: TreeGraph<'_>, id: &str) -> HashSet<String> {
    let children = adjacency(graph);
    let mut visited = HashSet::new();
    let mut pending = vec![id.to_owned()];
    while let Some(id) = pending.pop() {
        if visited.insert(id.clone()) {
            pending.extend(children.get(&id).into_iter().flatten().cloned());
        }
    }
    visited
}

/// Validate only remembered paths, independently of current expansion/filtering.
/// A move invalidates the old edge instead of transferring state to the same ID
/// elsewhere. Valid prefixes survive a deleted tail. Build the relationship
/// index once, without enumerating every possible occurrence in a shared graph.
pub(crate) fn valid_remembered_paths<'a>(
    graph: TreeGraph<'_>,
    paths: impl IntoIterator<Item = &'a OccurrencePath>,
) -> BTreeSet<OccurrencePath> {
    let children = adjacency(graph);
    let roots: HashSet<_> = complete_roots(graph, &children).into_iter().collect();
    let mut valid = BTreeSet::new();
    for path in paths {
        if path.first().is_none_or(|id| !roots.contains(id)) {
            continue;
        }
        let mut prefix = Vec::new();
        let mut ancestors = HashSet::new();
        for id in path {
            if let Some(parent) = prefix.last() {
                if !children.get(parent).is_some_and(|ids| ids.contains(id)) {
                    break;
                }
            }
            prefix.push(id.clone());
            valid.insert(prefix.clone());
            if !graph.items.contains_key(id) || !ancestors.insert(id) {
                // These occurrences are visible terminals, not navigable paths.
                break;
            }
        }
    }
    valid
}

fn sort_siblings(
    graph: TreeGraph<'_>,
    ids: &mut [String],
    compare: Option<&dyn Fn(&FavorItemDto, &FavorItemDto) -> Ordering>,
) {
    if let Some(compare) = compare {
        // Missing references retain their relation position relative to other
        // missing references; they do not call a comparator that expects a DTO.
        ids.sort_by(|a, b| match (graph.items.get(a), graph.items.get(b)) {
            (Some(a), Some(b)) => compare(a, b),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        });
    }
}

struct Frame<'a> {
    ids: &'a [String],
    next: usize,
    parent_path: OccurrencePath,
    parent_index: Option<usize>,
    depth: usize,
}

struct Candidate {
    row: TreeRow,
    parent_index: Option<usize>,
    matches: bool,
}

/// First source-order DFS path for each canonical item. This index is for ID
/// navigation only: global visitation here must never be used for visible rows,
/// where each occurrence has its own expansion. Bound path allocation to the
/// same depth as browsing, without spending an occurrence budget on shared DAGs.
pub(crate) fn first_occurrence_paths(graph: TreeGraph<'_>) -> BTreeMap<String, OccurrencePath> {
    let children = adjacency(graph);
    let roots = complete_roots(graph, &children);
    let max_depth = ProjectionLimits::default().max_depth;
    let mut paths = BTreeMap::new();
    let mut visited = HashSet::new();
    let mut frames = vec![Frame {
        ids: &roots,
        next: 0,
        parent_path: Vec::new(),
        parent_index: None,
        depth: 0,
    }];
    while let Some(frame) = frames.last_mut() {
        let Some(id) = frame.ids.get(frame.next).cloned() else {
            frames.pop();
            continue;
        };
        frame.next += 1;
        if visited.contains(&id) || !graph.items.contains_key(&id) {
            continue;
        }
        let mut path = frame.parent_path.clone();
        path.push(id.clone());
        paths.entry(id.clone()).or_insert_with(|| path.clone());
        let depth = frame.depth;
        if depth < max_depth {
            visited.insert(id.clone());
            frames.push(Frame {
                ids: children.get(&id).map(Vec::as_slice).unwrap_or_default(),
                next: 0,
                parent_path: path,
                parent_index: None,
                depth: depth + 1,
            });
        }
        // A depth-cutoff node is indexed but not marked visited. If another
        // parent reaches it at a shallower depth, its children remain reachable.
    }
    paths
}

/// Project only expanded occurrences in normal browsing. Active filtering walks
/// bounded occurrences first and retains ancestor context in a reverse pass.
/// Both walks are iterative, so old deep or cyclic graphs cannot recurse forever.
pub(crate) fn project_tree(graph: TreeGraph<'_>, options: ProjectionOptions<'_>) -> TreeProjection {
    let mut result = TreeProjection::default();
    let mut children = adjacency(graph);
    let mut roots = complete_roots(graph, &children);
    sort_siblings(graph, &mut roots, options.compare);
    for ids in children.values_mut() {
        sort_siblings(graph, ids, options.compare);
    }

    let prefix = options.scope.unwrap_or_default();
    let initial_ids = if let Some(scope) = options.scope {
        let valid = !scope.is_empty()
            && scope.len() <= 512
            && scope.iter().all(|id| graph.items.contains_key(id))
            && scope.windows(2).all(|pair| {
                children
                    .get(&pair[0])
                    .is_some_and(|ids| ids.contains(&pair[1]))
            })
            && scope.iter().collect::<HashSet<_>>().len() == scope.len();
        if !valid {
            result.scope_missing = true;
            return result;
        }
        children
            .get(scope.last().expect("nonempty validated scope"))
            .map(Vec::as_slice)
            .unwrap_or_default()
    } else {
        roots.as_slice()
    };

    let filtering = options.matches.is_some();
    let candidate_limit = if filtering {
        options.limits.max_occurrences
    } else {
        options.limits.max_occurrences.min(options.limits.max_rows)
    };
    let mut frames = vec![Frame {
        ids: initial_ids,
        next: 0,
        parent_path: prefix.to_vec(),
        parent_index: None,
        depth: 0,
    }];
    let mut candidates: Vec<Candidate> = Vec::new();

    while !frames.is_empty() {
        if frames
            .last()
            .is_some_and(|frame| frame.next >= frame.ids.len())
        {
            frames.pop();
            continue;
        }
        if candidates.len() >= candidate_limit {
            result.truncated = true;
            break;
        }
        let frame = frames.last_mut().expect("nonempty frame stack");
        let id = frame.ids[frame.next].clone();
        frame.next += 1;
        let is_cycle = frame.parent_path.contains(&id);
        let parent_path = (!frame.parent_path.is_empty()).then(|| frame.parent_path.clone());
        let mut path = frame.parent_path.clone();
        path.push(id.clone());
        let depth = frame.depth;
        let parent_index = frame.parent_index;
        let item = graph.items.get(&id);
        let child_ids = children.get(&id).map(Vec::as_slice).unwrap_or_default();
        let real_children = item.is_some() && !is_cycle && !child_ids.is_empty();
        let depth_limited = real_children && depth >= options.limits.max_depth.min(512);
        let has_children = real_children && !depth_limited;
        let expanded = has_children
            && (filtering
                || options.expanded.contains(&path)
                || options
                    .auto_expand
                    .is_some_and(|auto_expand| item.is_some_and(auto_expand)));
        result.truncated |= depth_limited;
        let index = candidates.len();
        candidates.push(Candidate {
            row: TreeRow {
                item_id: id,
                path: path.clone(),
                parent_path,
                depth,
                has_children,
                expanded,
                is_cycle,
                is_missing: item.is_none(),
                context_only: false,
                depth_limited,
            },
            parent_index,
            matches: options
                .matches
                .is_none_or(|matches| item.is_some_and(matches)),
        });
        if expanded {
            frames.push(Frame {
                ids: child_ids,
                next: 0,
                parent_path: path,
                parent_index: Some(index),
                depth: depth + 1,
            });
        }
    }
    result.examined_occurrences = candidates.len();

    if filtering {
        let mut keep: Vec<_> = candidates.iter().map(|node| node.matches).collect();
        let mut visible_children = vec![false; candidates.len()];
        for index in (0..candidates.len()).rev() {
            if keep[index] {
                if let Some(parent) = candidates[index].parent_index {
                    keep[parent] = true;
                    visible_children[parent] = true;
                }
            }
        }
        for (index, mut node) in candidates.into_iter().enumerate() {
            if !keep[index] {
                continue;
            }
            if result.rows.len() >= options.limits.max_rows {
                result.truncated = true;
                break;
            }
            node.row.context_only = !node.matches;
            node.row.has_children = visible_children[index];
            node.row.expanded = visible_children[index];
            result.rows.push(node.row);
        }
    } else {
        result.rows = candidates.into_iter().map(|node| node.row).collect();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, parent: &str, folder: bool) -> FavorItemDto {
        serde_json::from_value(serde_json::json!({
            "id":id,"parentId":parent,"title":id,"summary":"","kind":"Text",
            "isFolder":folder,"childrenCount":0,"createTime":"2026-10-12T00:00:00Z",
            "updateTime":"2026-10-12T00:00:00Z","sourceProcessName":"","fileCount":0,
            "filePaths":[],"textLength":null,"imRichOrigin":null,"imRichSegments":[]
        }))
        .unwrap()
    }

    fn path(ids: &[&str]) -> OccurrencePath {
        ids.iter().map(|id| (*id).into()).collect()
    }
    fn items(ids: &[&str]) -> BTreeMap<String, FavorItemDto> {
        ids.iter()
            .map(|id| ((*id).into(), item(id, "", false)))
            .collect()
    }
    fn edges(rows: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        rows.iter()
            .map(|(id, children)| ((*id).into(), path(children)))
            .collect()
    }

    #[test]
    fn shared_nonfolder_items_have_independent_occurrences_and_saved_child_order() {
        let items = items(&["a", "b", "shared", "leaf", "z"]);
        let roots = path(&["a", "b"]);
        let children = edges(&[
            ("a", &["shared", "z"]),
            ("b", &["shared"]),
            ("shared", &["leaf"]),
        ]);
        let expanded = [path(&["a"]), path(&["b"]), path(&["a", "shared"])]
            .into_iter()
            .collect();
        let result = project_tree(
            TreeGraph {
                items: &items,
                roots: &roots,
                children_by_id: &children,
            },
            ProjectionOptions::new(&expanded),
        );
        assert_eq!(
            result
                .rows
                .iter()
                .map(|row| row.item_id.as_str())
                .collect::<Vec<_>>(),
            ["a", "shared", "leaf", "z", "b", "shared"]
        );
        assert!(result.rows[1].expanded);
        assert!(!result.rows[5].expanded);
        assert_ne!(result.rows[1].path, result.rows[5].path);
        assert!(!items["shared"].is_folder);
        assert_eq!(result.rows[2].parent_path, Some(path(&["a", "shared"])));
        assert!(!result.truncated);
    }

    #[test]
    fn filtering_retains_ancestors_without_changing_expansion_or_child_order() {
        let items = items(&["root", "z-parent", "a-other", "wanted", "hidden"]);
        let roots = path(&["root"]);
        let children = edges(&[
            ("root", &["z-parent", "a-other"]),
            ("z-parent", &["hidden", "wanted"]),
        ]);
        let expanded = HashSet::new();
        let matches = |item: &FavorItemDto| item.id == "wanted";
        let mut options = ProjectionOptions::new(&expanded);
        options.matches = Some(&matches);
        let result = project_tree(
            TreeGraph {
                items: &items,
                roots: &roots,
                children_by_id: &children,
            },
            options,
        );
        assert_eq!(
            result
                .rows
                .iter()
                .map(|row| row.item_id.as_str())
                .collect::<Vec<_>>(),
            ["root", "z-parent", "wanted"]
        );
        assert!(result.rows[0].context_only && result.rows[0].expanded);
        assert!(result.rows[1].context_only && result.rows[1].expanded);
        assert!(!result.rows[2].context_only && !result.rows[2].expanded);
        assert!(!result.rows[2].has_children);
        assert!(expanded.is_empty());
    }

    #[test]
    fn sibling_sort_is_stable_and_does_not_mix_descendants_into_other_roots() {
        let items = items(&["root-z", "root-a", "z", "a", "child"]);
        let roots = path(&["root-z", "root-a"]);
        let children = edges(&[("root-z", &["z", "a"]), ("a", &["child"])]);
        let expanded = [path(&["root-z"]), path(&["root-z", "a"])]
            .into_iter()
            .collect();
        let compare = |a: &FavorItemDto, b: &FavorItemDto| a.title.cmp(&b.title);
        let mut options = ProjectionOptions::new(&expanded);
        options.compare = Some(&compare);
        let result = project_tree(
            TreeGraph {
                items: &items,
                roots: &roots,
                children_by_id: &children,
            },
            options,
        );
        assert_eq!(
            result
                .rows
                .iter()
                .map(|row| row.item_id.as_str())
                .collect::<Vec<_>>(),
            ["root-a", "root-z", "a", "child", "z"]
        );
        assert_eq!(result.rows[3].depth, 2);
        let equal = |_: &FavorItemDto, _: &FavorItemDto| Ordering::Equal;
        let mut options = ProjectionOptions::new(&expanded);
        options.compare = Some(&equal);
        let equal_result = project_tree(
            TreeGraph {
                items: &items,
                roots: &roots,
                children_by_id: &children,
            },
            options,
        );
        assert_eq!(equal_result.rows[1].item_id, "z");
    }

    #[test]
    fn explicit_empty_edges_win_over_parent_fallback_and_duplicates_keep_first_position() {
        let mut items = items(&["parent", "fallback", "z", "a"]);
        items.get_mut("fallback").unwrap().parent_id = "parent".into();
        let roots = path(&["parent", "parent"]);
        let children = edges(&[("parent", &["z", "z", "a", "z"])]);
        let expanded = [path(&["parent"])].into_iter().collect();
        let result = project_tree(
            TreeGraph {
                items: &items,
                roots: &roots,
                children_by_id: &children,
            },
            ProjectionOptions::new(&expanded),
        );
        assert_eq!(
            result
                .rows
                .iter()
                .map(|row| row.item_id.as_str())
                .collect::<Vec<_>>(),
            ["parent", "z", "a", "fallback"]
        );
        assert_eq!(result.rows[3].depth, 0);
        let children = edges(&[("parent", &[])]);
        let result = project_tree(
            TreeGraph {
                items: &items,
                roots: &roots,
                children_by_id: &children,
            },
            ProjectionOptions::new(&expanded),
        );
        assert!(!result.rows[0].has_children);
        assert_eq!(
            result
                .rows
                .iter()
                .filter(|row| row.item_id == "fallback")
                .count(),
            1
        );
    }

    #[test]
    fn missing_references_disconnected_cycles_and_orphans_remain_finite_and_inspectable() {
        let mut items = items(&["root", "cycle-a", "cycle-b", "orphan"]);
        items.get_mut("orphan").unwrap().parent_id = "missing-parent".into();
        let roots = path(&["root"]);
        let children = edges(&[
            ("root", &["missing-item"]),
            ("cycle-a", &["cycle-b"]),
            ("cycle-b", &["cycle-a"]),
        ]);
        let expanded = [
            path(&["root"]),
            path(&["cycle-a"]),
            path(&["cycle-a", "cycle-b"]),
        ]
        .into_iter()
        .collect();
        let result = project_tree(
            TreeGraph {
                items: &items,
                roots: &roots,
                children_by_id: &children,
            },
            ProjectionOptions::new(&expanded),
        );
        assert_eq!(result.rows.len(), 6);
        let missing = result
            .rows
            .iter()
            .find(|row| row.item_id == "missing-item")
            .unwrap();
        assert!(missing.is_missing && !missing.has_children);
        let cycle = result.rows.iter().find(|row| row.is_cycle).unwrap();
        assert_eq!(cycle.path, path(&["cycle-a", "cycle-b", "cycle-a"]));
        assert!(!cycle.has_children && !cycle.expanded);
        assert_eq!(
            result
                .rows
                .iter()
                .filter(|row| row.item_id == "orphan")
                .count(),
            1
        );
        assert!(!result.truncated);
    }

    #[test]
    fn scope_keeps_the_real_occurrence_prefix_and_rejects_stale_relationships() {
        let items = items(&["left", "right", "shared", "leaf"]);
        let roots = path(&["left", "right"]);
        let children = edges(&[
            ("left", &["shared"]),
            ("right", &["shared"]),
            ("shared", &["leaf"]),
        ]);
        let expanded = HashSet::new();
        let scope = path(&["right", "shared"]);
        let mut options = ProjectionOptions::new(&expanded);
        options.scope = Some(&scope);
        let graph = TreeGraph {
            items: &items,
            roots: &roots,
            children_by_id: &children,
        };
        let result = project_tree(graph, options);
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].depth, 0);
        assert_eq!(result.rows[0].path, path(&["right", "shared", "leaf"]));
        assert_eq!(result.rows[0].parent_path, Some(scope));
        let stale = path(&["right", "leaf"]);
        let mut options = ProjectionOptions::new(&expanded);
        options.scope = Some(&stale);
        let result = project_tree(graph, options);
        assert!(result.scope_missing && result.rows.is_empty());
    }

    #[test]
    fn deep_graphs_obey_explicit_budgets_without_recursion() {
        let mut items = BTreeMap::new();
        let mut children = BTreeMap::new();
        for index in 0..2048 {
            let id = index.to_string();
            items.insert(id.clone(), item(&id, "", false));
            if index < 2047 {
                children.insert(id, vec![(index + 1).to_string()]);
            }
        }
        let roots = path(&["0"]);
        let expanded = HashSet::new();
        let all = |_: &FavorItemDto| true;
        let mut options = ProjectionOptions::new(&expanded);
        options.matches = Some(&all);
        options.limits = ProjectionLimits {
            max_rows: 100,
            max_occurrences: 100,
            max_depth: 64,
        };
        let graph = TreeGraph {
            items: &items,
            roots: &roots,
            children_by_id: &children,
        };
        let result = project_tree(graph, options);
        assert_eq!(result.rows.len(), 65);
        assert!(result.truncated && result.rows.last().unwrap().depth_limited);
        assert!(!result.rows.last().unwrap().has_children);
        let mut options = ProjectionOptions::new(&expanded);
        options.matches = Some(&all);
        options.limits = ProjectionLimits {
            max_rows: 10,
            max_occurrences: 12,
            max_depth: 512,
        };
        let result = project_tree(graph, options);
        assert_eq!(result.examined_occurrences, 12);
        assert_eq!(result.rows.len(), 10);
        assert!(result.truncated);
    }

    #[test]
    fn collapsed_remembered_paths_survive_and_moved_edges_do_not_follow_item_ids() {
        let items = items(&["left", "right", "parent", "leaf"]);
        let roots = path(&["left", "right"]);
        let mut children = edges(&[("left", &["parent"]), ("parent", &["leaf"])]);
        let remembered = [path(&["left", "parent", "leaf"]), path(&["parent", "leaf"])];
        let graph = TreeGraph {
            items: &items,
            roots: &roots,
            children_by_id: &children,
        };
        let visible = project_tree(graph, ProjectionOptions::new(&HashSet::new()));
        assert!(!visible.rows.iter().any(|row| row.item_id == "leaf"));
        let valid = valid_remembered_paths(graph, &remembered);
        assert_eq!(
            valid,
            [
                path(&["left"]),
                path(&["left", "parent"]),
                path(&["left", "parent", "leaf"])
            ]
            .into_iter()
            .collect()
        );
        children.insert("left".into(), Vec::new());
        children.insert("right".into(), path(&["parent"]));
        let moved = valid_remembered_paths(
            TreeGraph {
                items: &items,
                roots: &roots,
                children_by_id: &children,
            },
            &remembered,
        );
        assert_eq!(moved, [path(&["left"])].into_iter().collect());
        assert!(!moved.contains(&path(&["right", "parent", "leaf"])));
    }

    #[test]
    fn exponentially_shared_graph_stops_at_occurrence_budget() {
        let mut items = BTreeMap::new();
        let mut children = BTreeMap::new();
        for level in 0..25 {
            for side in 0..2 {
                let id = format!("{level}-{side}");
                items.insert(id.clone(), item(&id, "", false));
                if level < 24 {
                    children.insert(
                        id,
                        vec![format!("{}-0", level + 1), format!("{}-1", level + 1)],
                    );
                }
            }
        }
        let roots = path(&["0-0", "0-1"]);
        let expanded = HashSet::new();
        let matches = |_: &FavorItemDto| true;
        let mut options = ProjectionOptions::new(&expanded);
        options.matches = Some(&matches);
        options.limits = ProjectionLimits {
            max_rows: 16,
            max_occurrences: 32,
            max_depth: 128,
        };
        let result = project_tree(
            TreeGraph {
                items: &items,
                roots: &roots,
                children_by_id: &children,
            },
            options,
        );
        assert_eq!(result.examined_occurrences, 32);
        assert_eq!(result.rows.len(), 16);
        assert!(result.truncated);
    }

    #[test]
    fn canonical_index_reaches_later_roots_after_an_exponentially_shared_dag() {
        let mut items = items(&["later-root", "later-leaf"]);
        let mut children = edges(&[("later-root", &["later-leaf"])]);
        for level in 0..25 {
            for side in 0..2 {
                let id = format!("{level}-{side}");
                items.insert(id.clone(), item(&id, "", false));
                if level < 24 {
                    children.insert(
                        id,
                        vec![format!("{}-0", level + 1), format!("{}-1", level + 1)],
                    );
                }
            }
        }
        let roots = path(&["0-0", "0-1", "later-root"]);
        let graph = TreeGraph {
            items: &items,
            roots: &roots,
            children_by_id: &children,
        };
        let index = first_occurrence_paths(graph);
        assert_eq!(index.len(), items.len());
        assert_eq!(index["later-leaf"], path(&["later-root", "later-leaf"]));
        assert_eq!(index["1-0"], path(&["0-0", "1-0"]));
        assert_eq!(index["1-1"], path(&["0-0", "1-1"]));
    }

    #[test]
    fn canonical_index_bounds_deep_paths_but_later_shortcuts_can_reach_descendants() {
        let max_depth = ProjectionLimits::default().max_depth;
        let mut items = items(&["shortcut"]);
        let mut children = edges(&[("shortcut", &[])]);
        for depth in 0..max_depth + 2 {
            let id = depth.to_string();
            items.insert(id.clone(), item(&id, "", false));
            children.insert(id, vec![(depth + 1).to_string()]);
        }
        children.insert("shortcut".into(), vec![max_depth.to_string()]);
        let roots = path(&["0", "shortcut"]);
        let graph = TreeGraph {
            items: &items,
            roots: &roots,
            children_by_id: &children,
        };
        let index = first_occurrence_paths(graph);
        let last = (max_depth + 1).to_string();
        assert_eq!(
            index[&last],
            vec!["shortcut".to_owned(), max_depth.to_string(), last]
        );
        assert!(index.values().all(|path| path.len() <= max_depth + 1));
    }

    #[test]
    fn automatic_nonfolder_expansion_preserves_folder_collapse_and_occurrence_paths() {
        let mut items = items(&["text-root", "folder", "text-child", "nested-folder"]);
        items.get_mut("folder").unwrap().is_folder = true;
        items.get_mut("nested-folder").unwrap().is_folder = true;
        let roots = path(&["text-root"]);
        let children = edges(&[
            ("text-root", &["folder"]),
            ("folder", &["text-child"]),
            ("text-child", &["nested-folder"]),
        ]);
        let expanded = HashSet::new();
        let auto_expand = |item: &FavorItemDto| !item.is_folder;
        let mut options = ProjectionOptions::new(&expanded);
        options.auto_expand = Some(&auto_expand);
        let graph = TreeGraph {
            items: &items,
            roots: &roots,
            children_by_id: &children,
        };
        let result = project_tree(graph, options);
        assert_eq!(
            result
                .rows
                .iter()
                .map(|row| row.item_id.as_str())
                .collect::<Vec<_>>(),
            ["text-root", "folder"]
        );
        assert!(result.rows[0].expanded);
        assert!(!result.rows[1].expanded);
        assert_eq!(result.rows[1].path, path(&["text-root", "folder"]));
        assert!(expanded.is_empty());
        let expanded = [path(&["text-root", "folder"])].into_iter().collect();
        let mut options = ProjectionOptions::new(&expanded);
        options.auto_expand = Some(&auto_expand);
        let result = project_tree(graph, options);
        assert_eq!(
            result.rows.last().unwrap().path,
            path(&["text-root", "folder", "text-child", "nested-folder"])
        );
        assert_eq!(expanded.len(), 1);
    }
}
