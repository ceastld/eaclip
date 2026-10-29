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

pub fn load_local_tree() -> Option<BookmarkNode> {
    if crate::clipboard::is_headless() {
        return None;
    }
    let path = bookmark_file()?;
    let text = std::fs::read_to_string(&path).ok()?;
    parse_bookmarks_file(&text)
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

fn bookmark_file() -> Option<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA")?;
    let candidates = [
        PathBuf::from(&local).join("Google/Chrome/User Data/Default/Bookmarks"),
        PathBuf::from(&local).join("Microsoft/Edge/User Data/Default/Bookmarks"),
        PathBuf::from(&local).join("Chromium/User Data/Default/Bookmarks"),
    ];
    candidates.into_iter().find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

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
