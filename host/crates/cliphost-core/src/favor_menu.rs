//! C# `FavorMenuData` / `FavorMenuConfigItem.Match` plus Super Menu target window.

use regex::RegexBuilder;
use serde::{Deserialize, Serialize};

use crate::clipboard::split_process_names;
use crate::model::FavorRecord;

/// C# `WinProperty.IsSaveAsWindow`.
pub const SAVE_AS_CLASS: &str = "#32770";

#[derive(Debug, Clone, Default)]
pub struct MenuTargetWindow {
    pub process_name: String,
    pub class_name: String,
    pub title: String,
    pub browser_url: Option<String>,
}

impl MenuTargetWindow {
    pub fn is_save_as(&self) -> bool {
        self.class_name == SAVE_AS_CLASS
    }

    /// C# `WinProperty.IsBrowser` (`IsProcNameCheck` on chrome/msedge/firefox/vivaldi/360*).
    pub fn is_browser(&self) -> bool {
        [
            "chrome",
            "msedge",
            "firefox",
            "vivaldi",
            "360ChromeX",
            "360Chrome",
            "360se",
        ]
        .iter()
        .any(|name| self.process_name.eq_ignore_ascii_case(name))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct FavorMenuConfigItem {
    #[serde(default)]
    pub proc_names: String,
    #[serde(default)]
    pub class_name_pattern: String,
    #[serde(default)]
    pub title_pattern: String,
    #[serde(default)]
    pub url_pattern: String,
    #[serde(default = "default_match_expression")]
    pub match_expression: String,
    #[serde(default)]
    pub favorite_id: i32,
    #[serde(default)]
    pub favorite_id1: String,
    #[serde(default)]
    pub always_expand: bool,
    #[serde(default = "default_true")]
    pub is_enable: bool,
}

fn default_match_expression() -> String {
    "$=".into()
}

fn default_true() -> bool {
    true
}

impl Default for FavorMenuConfigItem {
    fn default() -> Self {
        Self {
            proc_names: String::new(),
            class_name_pattern: String::new(),
            title_pattern: String::new(),
            url_pattern: String::new(),
            match_expression: default_match_expression(),
            favorite_id: 0,
            favorite_id1: String::new(),
            always_expand: false,
            is_enable: true,
        }
    }
}

impl FavorMenuConfigItem {
    /// C# `ProcessHelper.MatchProcess`: SplitNames + ordinal ignore-case equality (no .exe strip).
    pub fn match_process(names: &str, proc_name: &str) -> bool {
        split_process_names(names)
            .iter()
            .any(|name| name.eq_ignore_ascii_case(proc_name))
    }

    /// C# `FavorMenuConfigItem.Match(ExecuteContext)`.
    ///
    /// `$=.+` expressions use Quicker `RunExpression`. Failed or non-bool results leave
    /// the clause true (C# `catch` / non-bool). The host resolves complex conditions
    /// before menu materialization, so no callback runs while a menu is locked.
    pub fn matches(&self, target: &MenuTargetWindow) -> bool {
        self.matches_window(target)
            && match_expression_ok(&self.match_expression, target)
            && match_url(target, &self.url_pattern)
    }

    pub(crate) fn matches_window(&self, target: &MenuTargetWindow) -> bool {
        let proc_ok = self.proc_names.is_empty()
            || Self::match_process(&self.proc_names, &target.process_name);
        proc_ok
            && regex_is_match(&self.class_name_pattern, &target.class_name)
            && regex_is_match(&self.title_pattern, &target.title)
    }

    pub fn resolve_favor<'a>(&self, favors: &'a [FavorRecord]) -> Option<&'a FavorRecord> {
        if !self.favorite_id1.trim().is_empty() {
            if let Some(item) = favors.iter().find(|f| f.id == self.favorite_id1) {
                return Some(item);
            }
        }
        first_favorites(favors)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub struct FavorMenuData {
    #[serde(default)]
    pub default_favorite: i32,
    #[serde(default)]
    pub default_favorite1: String,
    #[serde(default)]
    pub config_items: Vec<FavorMenuConfigItem>,
}

impl FavorMenuData {
    /// C# `FavorMenuData.GetMatchConfigs`: enabled + Match + resolved favor exists.
    pub fn match_configs<'a>(&'a self, target: &MenuTargetWindow, favors: &'a [FavorRecord]) -> Vec<&'a FavorMenuConfigItem> {
        self.config_items
            .iter()
            .filter(|cfg| cfg.is_enable && cfg.matches(target) && cfg.resolve_favor(favors).is_some())
            .collect()
    }

    /// C# `GetDefaultFavorite`: `GetItem(DefaultFavorite1) ?? GetFirstFavorites()`.
    pub fn default_favorite<'a>(&self, favors: &'a [FavorRecord]) -> Option<&'a FavorRecord> {
        if !self.default_favorite1.trim().is_empty() {
            if let Some(item) = favors.iter().find(|f| f.id == self.default_favorite1) {
                return Some(item);
            }
        }
        first_favorites(favors)
    }
}

/// C# `FavorDataService.GetFirstFavorites`: `IsFavorites` folders, `OrderByDescending(HasChild)`.
pub fn first_favorites(favors: &[FavorRecord]) -> Option<&FavorRecord> {
    let mut folders: Vec<(usize, &FavorRecord)> = favors
        .iter()
        .enumerate()
        .filter(|(_, item)| item.is_folder)
        .collect();
    folders.sort_by(|a, b| {
        let child_a = folder_has_child(favors, a.1);
        let child_b = folder_has_child(favors, b.1);
        child_b.cmp(&child_a).then(a.0.cmp(&b.0))
    });
    folders.first().map(|(_, item)| *item)
}

fn folder_has_child(favors: &[FavorRecord], folder: &FavorRecord) -> bool {
    !folder.children.is_empty()
        || favors
            .iter()
            .any(|item| item.parent_id.as_deref() == Some(folder.id.as_str()))
}

fn regex_is_match(pattern: &str, text: &str) -> bool {
    match RegexBuilder::new(pattern).case_insensitive(true).build() {
        Ok(re) => re.is_match(text),
        Err(_) => false,
    }
}

fn match_expression_ok(expr: &str, target: &MenuTargetWindow) -> bool {
    // C# only evaluates expressions matching ^\$=.+ (dot excludes a leading LF).
    let Some(body) = expr.strip_prefix("$=") else { return true; };
    if body.is_empty() || body.starts_with('\n') { return true; }
    crate::quicker_expression::local(expr, &expression_params(target))
        .and_then(|value| value.as_bool()).unwrap_or(true)
}

pub(crate) fn expression_params(target: &MenuTargetWindow) -> serde_json::Value {
    serde_json::json!({"procName":target.process_name,"className":target.class_name,"title":target.title})
}

fn match_url(target: &MenuTargetWindow, part: &str) -> bool {
    if target.is_browser() {
        match target.browser_url.as_deref() {
            None => true,
            Some(url) => url.contains(part),
        }
    } else {
        part.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ClipKind;
    use chrono::Utc;

    fn folder(id: &str, children: Vec<String>) -> FavorRecord {
        FavorRecord {
            storage_revision: None,
            id: id.into(),
            parent_id: None,
            is_folder: true,
            name: id.into(),
            kind: ClipKind::FavoriteClipItem,
            body_text: None,
            file_paths: Vec::new(),
            image_path: None,
            children,
            create_time: Utc::now(),
            update_time: Utc::now(),
            source_clip_id: None,
            html_data: None,
            html_source_url: None,
            qq_rich_text: None,
            source_process_name: String::new(),
            sub_index: 0,
            icon: String::new(),
            execute_param: String::new(),
        }
    }

    #[test]
    fn match_and_first_favorites_match_csharp() {
        let notepad = MenuTargetWindow {
            process_name: "notepad".into(),
            class_name: "Notepad".into(),
            title: "readme.txt - Notepad".into(),
            browser_url: None,
        };
        let mut cfg = FavorMenuConfigItem {
            proc_names: "notepad,chrome".into(),
            ..FavorMenuConfigItem::default()
        };
        assert!(cfg.matches(&notepad));
        cfg.proc_names = "chrome".into();
        assert!(!cfg.matches(&notepad));
        cfg.proc_names.clear();
        cfg.class_name_pattern = "note".into();
        assert!(cfg.matches(&notepad));
        cfg.title_pattern = "^missing".into();
        assert!(!cfg.matches(&notepad));

        let save_as = MenuTargetWindow {
            class_name: SAVE_AS_CLASS.into(),
            process_name: "notepad".into(),
            title: "另存为".into(),
            browser_url: None,
        };
        assert!(save_as.is_save_as());
        let url_cfg = FavorMenuConfigItem {
            url_pattern: "github.com".into(),
            ..FavorMenuConfigItem::default()
        };
        assert!(!url_cfg.matches(&notepad));
        let chrome = MenuTargetWindow {
            process_name: "chrome".into(),
            class_name: "Chrome_WidgetWin_1".into(),
            title: "GitHub".into(),
            browser_url: None,
        };
        assert!(chrome.is_browser());
        assert!(url_cfg.matches(&chrome));
        let chrome_url = MenuTargetWindow {
            browser_url: Some("https://example.com".into()),
            ..chrome.clone()
        };
        assert!(!url_cfg.matches(&chrome_url));

        let empty = folder("empty", Vec::new());
        let full = folder("full", vec!["child".into()]);
        let folders = [empty.clone(), full.clone()];
        let first = first_favorites(&folders).unwrap();
        assert_eq!(first.id, "full");
        let data = FavorMenuData {
            default_favorite1: "empty".into(),
            config_items: vec![FavorMenuConfigItem {
                favorite_id1: "full".into(),
                proc_names: "notepad".into(),
                ..FavorMenuConfigItem::default()
            }],
            ..FavorMenuData::default()
        };
        let favors = vec![empty, full];
        assert_eq!(data.default_favorite(&favors).unwrap().id, "empty");
        let matched = data.match_configs(&notepad, &favors);
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].favorite_id1, "full");
    }
}
