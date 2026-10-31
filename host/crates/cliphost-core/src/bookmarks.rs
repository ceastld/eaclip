//! Chrome/Edge Bookmarks file tree. Same node shape as C# `chrome.bookmarks.getTree`.

use serde_json::Value;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct BookmarkNode {
    pub id: String,
    pub title: String,
    pub url: String,
    pub children: Vec<BookmarkNode>,
}

impl BookmarkNode {
    pub fn is_folder(&self) -> bool {
        self.url.is_empty()
    }

    pub fn icon(&self) -> String {
        if self.is_folder() {
            return "fa:Light_Folder".into();
        }
        match url_authority(&self.url) {
            Some(host) if host.eq_ignore_ascii_case("settings") => "fa:Solid_Cog:#FF1C75E9".into(),
            Some(host) => format!("https://helperservice.getquicker.cn/favicon/get/{host}"),
            None => "fa:Light_Bookmark".into(),
        }
    }
}

/// C# `ChromeBookMarksNodeWrapper.Execute` for folders.
pub fn chrome_bookmarks_manager_url(id: &str) -> String {
    format!("chrome://bookmarks/?id={id}")
}

pub fn url_authority(url: &str) -> Option<String> {
    let rest = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(url);
    let host = rest.split(['/', '?', '#']).next().unwrap_or("").trim();
    (!host.is_empty()).then(|| host.to_string())
}

/// Chromium browsers read from `%LOCALAPPDATA%`: (setting id, label, user-data dir).
const BROWSERS: &[(&str, &str, &str)] = &[
    ("msedge", "Microsoft Edge", "Microsoft/Edge/User Data"),
    ("chrome", "Google Chrome", "Google/Chrome/User Data"),
    ("chromium", "Chromium", "Chromium/User Data"),
    ("vivaldi", "Vivaldi", "Vivaldi/User Data"),
    ("brave", "Brave", "BraveSoftware/Brave-Browser/User Data"),
    ("360chromex", "360 极速浏览器", "360ChromeX/Chrome/User Data"),
];

/// One browser profile with a Bookmarks file; the super menu shows one at a time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookmarkSource {
    pub browser: String,
    /// Browser name without the profile, e.g. `Microsoft Edge`.
    pub name: String,
    pub profile: String,
    pub label: String,
    /// Links in the file; filled only by `local_sources` for the picker.
    pub count: usize,
    path: PathBuf,
}

/// Which local source the menu reads, and the others it can switch to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BookmarkSources {
    pub sources: Vec<BookmarkSource>,
    pub current: Option<usize>,
    /// A configured Bookmarks file overrides every discovered source.
    pub custom_file: bool,
}

pub fn local_sources(browser: &str, profile: &str, custom_file: &str) -> BookmarkSources {
    if !custom_file.trim().is_empty() {
        return BookmarkSources { custom_file: true, ..Default::default() };
    }
    let Some(local) = std::env::var_os("LOCALAPPDATA") else { return BookmarkSources::default(); };
    let mut sources = discover(std::path::Path::new(&local));
    for source in &mut sources {
        source.count = read_tree(&source.path).map_or(0, |tree| count_links(&tree));
    }
    let current = pick(&sources, browser, profile);
    BookmarkSources { sources, current, custom_file: false }
}

pub fn load_local_tree(browser: &str, profile: &str, custom_file: &str) -> Option<BookmarkNode> {
    if crate::clipboard::is_headless() { return None; }
    let local = std::env::var_os("LOCALAPPDATA")?;
    load_from_root(std::path::Path::new(&local), browser, profile, custom_file)
}

fn local_path(path: &std::path::Path) -> bool {
    for ancestor in path.ancestors() {
        let Ok(meta) = std::fs::symlink_metadata(ancestor) else { return false; };
        if meta.file_type().is_symlink() { return false; }
        #[cfg(windows)] {
            use std::os::windows::fs::MetadataExt;
            if meta.file_attributes() & (0x400 | 0x1000 | 0x40000 | 0x400000) != 0 { return false; }
        }
    }
    true
}

fn discover(local: &std::path::Path) -> Vec<BookmarkSource> {
    let mut found = Vec::new();
    for (browser, label, relative) in BROWSERS {
        let root = local.join(relative);
        if !local_path(&root) { continue; }
        let Ok(entries) = std::fs::read_dir(&root) else { continue; };
        let mut profiles: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        profiles.sort();
        for profile in profiles {
            let name = profile.file_name().and_then(|x| x.to_str()).unwrap_or("");
            let path = profile.join("Bookmarks");
            if (name == "Default" || name.starts_with("Profile ")) && path.is_file() && local_path(&path) {
                found.push(BookmarkSource { browser: (*browser).into(), name: (*label).into(), profile: name.into(),
                    label: format!("{label} · {name}"), count: 0, path });
            }
        }
    }
    found
}

/// Auto picks the first source; a chosen browser never falls back to another
/// browser, and a missing profile falls back to that browser's first one.
fn pick(sources: &[BookmarkSource], browser: &str, profile: &str) -> Option<usize> {
    let browser_matches = |s: &BookmarkSource| browser.is_empty() || s.browser == browser;
    sources.iter().position(|s| browser_matches(s) && s.profile == profile)
        .or_else(|| sources.iter().position(browser_matches))
}

fn read_tree(path: &std::path::Path) -> Option<BookmarkNode> {
    let mut tree = parse_bookmarks_file(&std::fs::read_to_string(path).ok()?)?;
    // An empty Mobile favorites only adds a dead row.
    tree.children.retain(|node| !node.is_folder() || !node.children.is_empty());
    flatten_single_folders(&mut tree);
    tree.title = "浏览器书签".into();
    Some(tree)
}

fn count_links(node: &BookmarkNode) -> usize {
    node.children.iter().map(|child| if child.is_folder() { count_links(child) } else { 1 }).sum()
}

/// A level holding only one folder shows that folder's items instead, so a
/// profile with just 书签栏 opens straight onto its links (2026-10-30).
pub fn flatten_single_folders(node: &mut BookmarkNode) {
    while node.children.len() == 1 && node.children[0].is_folder() {
        let only = node.children.pop().unwrap();
        node.children = only.children;
    }
    node.children.iter_mut().filter(|child| child.is_folder()).for_each(flatten_single_folders);
}

fn load_from_root(local: &std::path::Path, browser: &str, profile: &str, custom_file: &str) -> Option<BookmarkNode> {
    if !custom_file.trim().is_empty() {
        let path = PathBuf::from(custom_file.trim());
        return (path.is_file() && local_path(&path)).then(|| read_tree(&path)).flatten();
    }
    let sources = discover(local);
    read_tree(&sources.get(pick(&sources, browser, profile)?)?.path)
}

pub fn parse_bookmarks_file(text: &str) -> Option<BookmarkNode> {
    let root: Value = serde_json::from_str(text).ok()?;
    let roots = root.get("roots")?.as_object()?;
    let mut children = Vec::new();
    for key in ["bookmark_bar", "other", "synced"] {
        if let Some(node) = roots.get(key).and_then(parse_chrome_node) {
            children.push(node);
        }
    }
    if children.is_empty() {
        return None;
    }
    Some(BookmarkNode {
        id: "0".into(),
        title: "浏览器书签".into(),
        url: String::new(),
        children,
    })
}

/// Quicker serializes chrome.bookmarks.getTree as a JSON string containing an
/// array with one root. An empty root is valid and must not fall back to another profile.
pub fn parse_rpc_tree(text: &str) -> Option<BookmarkNode> {
    let value: Value = serde_json::from_str(text).ok()?;
    let root = value.as_array()?.first()?;
    let children = root.get("children")?.as_array()?;
    let id = root.get("id")?.as_str()?.to_string();
    Some(BookmarkNode {
        id, title: "浏览器书签".into(), url: String::new(),
        children: children.iter().filter_map(parse_chrome_node).collect(),
    })
}

fn parse_chrome_node(value: &Value) -> Option<BookmarkNode> {
    let title = value
        .get("name")
        .or_else(|| value.get("title"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let url = value.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let id = value
        .get("id")
        .or_else(|| value.get("guid"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let children: Vec<BookmarkNode> = value
        .get("children")
        .and_then(|v| v.as_array())
        .map(|items| items.iter().filter_map(parse_chrome_node).collect())
        .unwrap_or_default();
    if title.is_empty() && url.is_empty() && children.is_empty()
        && !(value.get("children").is_some_and(Value::is_array) && !id.is_empty()) {
        return None;
    }
    Some(BookmarkNode { id, title, url, children })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bookmark_sources_pick_one_profile_and_never_fall_back_from_edge() {
        let dir = tempfile::tempdir().unwrap();
        let data = |name: &str| format!(r#"{{"roots":{{"bookmark_bar":{{"name":"书签栏","children":[{{"name":"{name}","url":"https://example.test"}}]}},"other":{{"name":"其他书签","children":[{{"name":"另一个","url":"https://other.test"}}]}},"synced":{{"name":"移动设备书签","children":[]}}}}}}"#);
        for (relative, name) in [("Microsoft/Edge/User Data/Default", "edge-default"), ("Microsoft/Edge/User Data/Profile 1", "edge-p1"), ("Google/Chrome/User Data/Default", "chrome"), ("Microsoft/Edge/User Data/Unrelated", "x")] {
            let profile = dir.path().join(relative);
            std::fs::create_dir_all(&profile).unwrap();
            std::fs::write(profile.join("Bookmarks"), data(name)).unwrap();
        }
        let first_link = |tree: BookmarkNode| tree.children[0].children[0].title.clone();
        let sources = discover(dir.path());
        assert_eq!(sources.iter().map(|s| s.label.as_str()).collect::<Vec<_>>(),
            ["Microsoft Edge · Default", "Microsoft Edge · Profile 1", "Google Chrome · Default"]);
        // Empty 移动设备书签 is dropped; 书签栏 and 其他书签 stay as roots.
        let auto = load_from_root(dir.path(), "", "", "").unwrap();
        assert_eq!(auto.children.iter().map(|n| n.title.as_str()).collect::<Vec<_>>(), ["书签栏", "其他书签"]);
        assert_eq!(first_link(auto), "edge-default");
        assert_eq!(first_link(load_from_root(dir.path(), "msedge", "Profile 1", "").unwrap()), "edge-p1");
        assert_eq!(first_link(load_from_root(dir.path(), "chrome", "Profile 9", "").unwrap()), "chrome");
        assert!(load_from_root(dir.path(), "brave", "", "").is_none());
        let custom = dir.path().join("custom-bookmarks");
        std::fs::write(&custom, data("custom")).unwrap();
        assert_eq!(first_link(load_from_root(dir.path(), "msedge", "", custom.to_str().unwrap()).unwrap()), "custom");
        std::fs::write(&custom, "broken").unwrap();
        assert!(load_from_root(dir.path(), "msedge", "", custom.to_str().unwrap()).is_none());
    }

    #[test]
    fn single_folder_levels_show_their_items() {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("Google/Chrome/User Data/Default");
        std::fs::create_dir_all(&profile).unwrap();
        std::fs::write(profile.join("Bookmarks"), r#"{"roots":{"bookmark_bar":{"name":"书签栏","children":[
            {"name":"工作","children":[{"name":"文档","children":[{"name":"A","url":"https://a.test"},{"name":"B","url":"https://b.test"}]}]},
            {"name":"C","url":"https://c.test"}]},"other":{"name":"其他书签","children":[]}}}"#).unwrap();
        let tree = load_from_root(dir.path(), "chrome", "", "").unwrap();
        // Only 书签栏 remains at the root, so its items take its place.
        assert_eq!(tree.children.iter().map(|n| n.title.as_str()).collect::<Vec<_>>(), ["工作", "C"]);
        // 工作 holds only 文档, so it opens straight onto A and B.
        assert_eq!(tree.children[0].children.iter().map(|n| n.title.as_str()).collect::<Vec<_>>(), ["A", "B"]);
    }

    #[test]
    fn bookmark_settings_round_trip_and_old_defaults() {
        let mut cfg = crate::extra_settings::MenuGlobalSettings::default();
        crate::extra_settings::apply_menu_global(&mut cfg, &serde_json::json!({"BookmarkBrowser":"msedge", "BookmarkProfile":"Profile 1", "BookmarkFilePath":"D:/Portable/Bookmarks"}));
        let value = serde_json::to_value(&cfg).unwrap();
        let saved: crate::extra_settings::MenuGlobalSettings = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(saved.bookmark_browser, "msedge");
        assert_eq!(saved.bookmark_profile, "Profile 1");
        let mut switched = saved.clone();
        crate::extra_settings::apply_menu_global(&mut switched, &serde_json::json!({"BookmarkBrowser":"chrome"}));
        assert!(switched.bookmark_profile.is_empty());
        assert_eq!(saved.bookmark_file_path, "D:/Portable/Bookmarks");
        let mut old = value;
        old.as_object_mut().unwrap().remove("BookmarkBrowser");
        old.as_object_mut().unwrap().remove("BookmarkFilePath");
        old.as_object_mut().unwrap().remove("BookmarkProfile");
        let old: crate::extra_settings::MenuGlobalSettings = serde_json::from_value(old).unwrap();
        assert!(old.bookmark_browser.is_empty());
        assert!(old.bookmark_file_path.is_empty());
    }

    #[test]
    fn rpc_bookmark_tree_retains_empty_folders_and_browser_ids() {
        let tree = parse_rpc_tree(r#"[{"id":"0","children":[{"id":"1","guid":"not-browser-id","title":"书签栏","children":[{"id":"2","title":"站点","url":"https://example.test"}]},{"id":"3","title":"","children":[]}]}]"#).unwrap();
        assert_eq!(tree.children[0].id, "1");
        assert_eq!(tree.children[0].children[0].url, "https://example.test");
        assert_eq!(tree.children[1].id, "3");
        assert!(tree.children[1].is_folder());
        assert!(parse_rpc_tree(r#"[{"id":"0","children":[]}]"#).unwrap().children.is_empty());
        for invalid in ["null", "[]", "{}", r#"[{"id":"0","children":null}]"#, "broken"] {
            assert!(parse_rpc_tree(invalid).is_none());
        }
    }

    #[test]
    fn parses_chrome_bookmarks_file_shape() {
        let json = r#"{
          "roots": {
            "bookmark_bar": {
              "name": "书签栏",
              "guid": "bar",
              "children": [
                { "name": "Quicker", "url": "https://getquicker.net/x", "guid": "1" },
                { "name": "Docs", "guid": "2", "children": [
                  { "name": "Inner", "url": "https://example.com", "guid": "3" }
                ]}
              ]
            },
            "other": { "name": "其他书签", "guid": "other", "children": [] }
          }
        }"#;
        let tree = parse_bookmarks_file(json).unwrap();
        assert_eq!(tree.title, "浏览器书签");
        assert_eq!(tree.children[0].title, "书签栏");
        assert_eq!(tree.children[0].children[0].url, "https://getquicker.net/x");
        assert!(tree.children[0].children[1].is_folder());
        assert_eq!(
            tree.children[0].children[0].icon(),
            "https://helperservice.getquicker.cn/favicon/get/getquicker.net"
        );
        assert_eq!(
            chrome_bookmarks_manager_url("bar"),
            "chrome://bookmarks/?id=bar"
        );
    }
}
