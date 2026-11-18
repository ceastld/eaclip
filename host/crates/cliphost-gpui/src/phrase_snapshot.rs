//! The right column projects stored favorites in Children order. IDs shared by
//! more than one folder are shown once in an aggregate list.
use super::*;

#[derive(Default)]
pub(crate) struct PhraseSnapshot {
    pub revision: String,
    folder_titles: BTreeMap<String, String>,
    pub(super) tree: FavoriteTree,
    pub all: Vec<String>,
    children: BTreeMap<String, Vec<String>>,
    roots: Vec<String>,
    pub folders: Vec<(String, String)>,
}

impl PhraseSnapshot {
    pub fn parse(value: Value) -> Result<Self, String> {
        let tree: FavoriteTree = serde_json::from_value(value).map_err(|e| e.to_string())?;
        let graph = tree.graph();
        let roots = favorites_tree::direct_members(graph, None);
        let children = favorites_tree::adjacency(graph);
        let mut folders: Vec<_> = favorites_tree::first_occurrence_paths(graph)
            .into_iter()
            .filter(|(id, _)| tree.items[id].is_folder)
            .map(|(id, path)| {
                (
                    id,
                    path.iter()
                        .map(|id| tree.title_of(id))
                        .collect::<Vec<_>>()
                        .join(" / "),
                )
            })
            .collect();
        folders.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
        let mut folder_titles = BTreeMap::new();
        let mut newest = Vec::new();
        for (id, item) in &tree.items {
            if item.is_folder {
                folder_titles.insert(id.clone(), title(item));
            } else {
                newest.push((
                    std::cmp::Reverse(item.create_time),
                    std::cmp::Reverse(item.update_time),
                    id.clone(),
                ));
            }
        }
        newest.sort_unstable();
        Ok(Self {
            revision: tree.drag_revision.clone(),
            folder_titles,
            tree,
            all: newest.into_iter().map(|(_, _, id)| id).collect(),
            roots,
            children,
            folders,
        })
    }

    pub fn folder_exists(&self, id: &str) -> bool {
        id.is_empty() || self.folder_titles.contains_key(id)
    }

    pub fn default_source(&self) -> &str {
        self.roots
            .iter()
            .find(|id| {
                self.folder_titles
                    .get(*id)
                    .is_some_and(|title| title == "常用语")
            })
            .map(String::as_str)
            .unwrap_or("")
    }

    pub fn path_label(&self, id: &str) -> &str {
        if id.is_empty() {
            return tr("全部收藏", "All favorites");
        }
        self.folders
            .iter()
            .find(|(key, _)| key == id)
            .map(|(_, label)| label.as_str())
            .unwrap_or(tr("收藏夹已移除", "Folder removed"))
    }

    fn members(&self, id: &str) -> &[String] {
        if id.is_empty() {
            &self.roots
        } else {
            self.children.get(id).map(Vec::as_slice).unwrap_or(&[])
        }
    }

    pub fn tabs(&self, source: &str) -> Vec<(String, String)> {
        self.members(source)
            .iter()
            .filter_map(|id| {
                self.folder_titles
                    .get(id)
                    .map(|name| (id.clone(), name.clone()))
            })
            .collect()
    }

    pub fn contents(&self, folder: &str) -> (HashSet<String>, Vec<String>) {
        let mut seen = HashSet::from([folder.to_owned()]);
        let mut folders = HashSet::from([folder.to_owned()]);
        let mut rows = Vec::new();
        let mut stack: Vec<_> = self.members(folder).iter().rev().collect();
        while let Some(id) = stack.pop() {
            if !seen.insert(id.clone()) {
                continue;
            }
            if self.folder_titles.contains_key(id) {
                folders.insert(id.clone());
            } else if self.tree.items.contains_key(id) {
                rows.push(id.clone());
            }
            stack.extend(self.members(id).iter().rev());
        }
        (folders, rows)
    }

    pub(crate) fn item(&self, id: &str) -> Option<&FavorItemDto> {
        self.tree.items.get(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phrase_column_includes_all_types_and_keeps_folder_order_across_shared_and_cyclic_folders() {
        let entries: BTreeMap<_, _> = [
            ("common","常用语","Favorite",true), ("day","日常","Favorite",true),
            ("a","确认","Text",false), ("b","提醒","Text",false), ("pic","样图","Image",false),
            ("file","独立文件","File",false), ("url","网址","Link",false), ("cmd","运行","Cmd",false),
        ].into_iter().map(|(id,title,kind,folder)| (id,json!({
            "id":id,"parentId":"","title":title,"summary":"正文","kind":kind,"isFolder":folder,
            "childrenCount":0,"createTime":"2026-10-12T00:00:00Z","updateTime":"2026-10-12T00:00:00Z",
            "sourceProcessName":"","fileCount":0,"filePaths":[],"imRichSegments":[]
        }))).collect();
        let model = PhraseSnapshot::parse(json!({"items":entries,"rootItems":["common"],
            "orderedRootItems":["common"],"childrenById":{"common":["day","b","a","pic"],"day":["a","common"]}})).unwrap();
        assert_eq!(model.default_source(), "common");
        assert_eq!(model.tabs("common"), vec![("day".into(), "日常".into())]);
        assert_eq!(model.contents("common").1, vec!["a", "b", "pic"]);
        assert_eq!(model.all, vec!["a", "b", "cmd", "file", "pic", "url"]);
        assert_eq!(model.path_label("day"), "常用语 / 日常");
        assert!(model.contents("removed").1.is_empty());
    }
}
