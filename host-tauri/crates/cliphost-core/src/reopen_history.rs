//! File Explorer history identities and mutation receipts. No filesystem probing.

use serde::{Deserialize, Serialize};

pub(crate) const EXCLUSIONS_KEY: &str = "cliphost.reopen.exclusions.v1";
pub(crate) const UNDO_KEY: &str = "cliphost.reopen.undo.v1";
pub(crate) const LEGACY_IMPORT_KEY: &str = "cliphost.reopen.legacy-import.v1";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ReopenHistoryEntry {
    pub path: String,
    pub recorded_at: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ReopenExclusion {
    pub path: String,
    pub include_children: bool,
    pub revision: String,
}

impl ReopenExclusion {
    pub fn matches(&self, path: &str) -> bool {
        let key = directory_key(path);
        let rule = directory_key(&self.path);
        !rule.is_empty()
            && (key == rule
                || (self.include_children
                    && !is_shell_location(&self.path)
                    && key
                        .strip_prefix(rule.trim_end_matches('\\'))
                        .is_some_and(|rest| rest.starts_with('\\'))))
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ReopenStoreSnapshot {
    pub entries: Vec<ReopenHistoryEntry>,
    pub exclusions: Vec<ReopenExclusion>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ReopenUndoReceipt {
    pub token: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ReopenRuleUndo {
    pub after: ReopenExclusion,
    pub before: Option<ReopenExclusion>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ReopenUndoRecord {
    pub token: String,
    pub entries: Vec<ReopenHistoryEntry>,
    pub rule: Option<ReopenRuleUndo>,
}

#[derive(Clone, Debug)]
pub(crate) struct ReopenStoreMutation {
    pub changed: usize,
    pub skipped: usize,
    pub rule_changed: bool,
    pub undo: Option<ReopenUndoReceipt>,
    pub snapshot: ReopenStoreSnapshot,
}

pub fn is_shell_location(path: &str) -> bool {
    let path = path.trim().to_ascii_lowercase();
    path.starts_with("shell:") || path.starts_with("::{")
}

/// Pure lexical Windows comparison. This never resolves links, connects a
/// network share, downloads cloud placeholders, or rewrites existing rows.
pub fn directory_key(path: &str) -> String {
    let mut raw = path.trim().replace('/', "\\").to_lowercase();
    if raw.starts_with("shell:::{") {
        raw = raw[6..].to_string();
    }
    if is_shell_location(&raw) {
        return raw.trim_end_matches('\\').to_string();
    }
    if raw.starts_with("\\\\?\\unc\\") {
        raw = format!("\\\\{}", &raw[8..]);
    } else if raw.starts_with("\\\\?\\") {
        raw = raw[4..].to_string();
    }
    let (prefix, tail, floor) = if raw.starts_with("\\\\") {
        ("\\\\".to_string(), &raw[2..], 2)
    } else if raw.as_bytes().get(1) == Some(&b':') && raw.as_bytes().get(2) == Some(&b'\\') {
        (raw[..3].to_string(), &raw[3..], 0)
    } else {
        (String::new(), raw.as_str(), 0)
    };
    let mut parts = Vec::new();
    for part in tail
        .split('\\')
        .filter(|part| !part.is_empty() && *part != ".")
    {
        if part == ".." && parts.len() > floor && parts.last() != Some(&"..") {
            parts.pop();
        } else if part != ".." || prefix.is_empty() {
            parts.push(part);
        }
    }
    format!("{prefix}{}", parts.join("\\"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identities_and_exclusions_are_lexical_and_respect_directory_boundaries() {
        assert_eq!(
            directory_key(r"C:/Work/Docs/"),
            directory_key(r"c:\work\docs")
        );
        assert_eq!(
            directory_key(r"C:\Work\Docs\..\Pictures"),
            directory_key(r"c:\work\pictures")
        );
        assert_eq!(
            directory_key(r"\\?\UNC\Server\Share\Docs\"),
            directory_key(r"\\server\share\docs")
        );
        assert_ne!(directory_key(r"C:\"), directory_key("C:"));
        let mut rule = ReopenExclusion {
            path: r"C:\Work\Docs".into(),
            include_children: false,
            revision: "test".into(),
        };
        assert!(rule.matches(r"c:\work\docs\"));
        assert!(!rule.matches(r"C:\Work\Docs\child"));
        rule.include_children = true;
        assert!(rule.matches(r"C:\Work\Docs\child"));
        assert!(!rule.matches(r"C:\Work\Docs-old"));
        assert!(!rule.matches(r"C:\Work\Docs\..\Other"));
        assert_eq!(directory_key("shell:::{GUID}"), directory_key("::{guid}"));
        let shell = ReopenExclusion {
            path: "shell:::{GUID}".into(),
            include_children: true,
            revision: "test".into(),
        };
        assert!(shell.matches("::{guid}"));
        assert!(!shell.matches("::{guid}\\child"));
    }
}
