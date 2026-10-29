//! Font Awesome 5 vectors aligned with C# `FontAwesome5IconPathProvider`.
//! Unknown specs 404 like C# `HandleFaVector`.

use std::collections::HashMap;
use std::sync::OnceLock;

use serde_json::{json, Value};

#[path = "fa_vector_data.rs"]
mod fa_vector_data;

use fa_vector_data::{FaEntry, ICONS};

const DEFAULT_FILL: &str = "#A0A0A0";

fn semantic_fill(color: &str) -> &str {
    match color.to_ascii_lowercase().as_str() {
        "success" => "#22c55e",
        "warning" => "#eab308",
        "danger" | "error" => "#ef4444",
        "info" => "#3b82f6",
        "text" => "#737373",
        "link" => "#2563eb",
        "primary" => "#171717",
        "secondary" => "#a3a3a3",
        _ => color,
    }
}

pub fn parse_spec(raw: &str) -> Option<(String, String)> {
    let mut text = raw.trim();
    if text.is_empty() {
        return None;
    }
    if text.get(..3).is_some_and(|prefix| prefix.eq_ignore_ascii_case("fa:")) {
        text = text[3..].trim();
    }
    if text.is_empty() {
        return None;
    }
    if let Some(split) = text.find(':') {
        let icon = text[..split].trim();
        let color = text[split + 1..].trim();
        if icon.is_empty() {
            return None;
        }
        let fill = if color.is_empty() {
            DEFAULT_FILL.to_string()
        } else {
            semantic_fill(color).to_string()
        };
        Some((icon.to_string(), fill))
    } else {
        Some((text.to_string(), DEFAULT_FILL.to_string()))
    }
}

fn lookup_entry(icon: &str) -> Option<&'static FaEntry> {
    static MAP: OnceLock<HashMap<String, &'static FaEntry>> = OnceLock::new();
    MAP.get_or_init(|| {
        ICONS
            .iter()
            .map(|entry| (entry.icon.to_ascii_lowercase(), entry))
            .collect()
    })
    .get(&icon.to_ascii_lowercase())
    .copied()
}

pub fn try_get_vector(spec: &str) -> Option<Value> {
    let (icon, fill) = parse_spec(spec)?;
    let entry = lookup_entry(&icon)?;
    Some(json!({
        "path": entry.path,
        "width": entry.width,
        "height": entry.height,
        "fill": fill,
        "icon": entry.icon,
        "style": entry.style,
    }))
}

pub fn catalog_len() -> usize {
    ICONS.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_spec_is_none() {
        assert!(try_get_vector("").is_none());
        assert!(try_get_vector("   ").is_none());
    }

    #[test]
    fn light_text_matches_csharp_shape() {
        let vector = try_get_vector("fa:Light_Text:#2563eb").unwrap();
        assert_eq!(vector["icon"], "Light_Text");
        assert_eq!(vector["style"], "Light");
        assert_eq!(vector["fill"], "#2563eb");
        assert!(vector["path"].as_str().unwrap().len() > 20);
        assert_eq!(vector["width"], 448);
    }

    #[test]
    fn semantic_success_fill() {
        let vector = try_get_vector("Light_Bolt:success").unwrap();
        assert_eq!(vector["fill"], "#22c55e");
    }

    #[test]
    fn unknown_icon_is_none() {
        assert!(try_get_vector("NotARealIcon").is_none());
    }

    #[test]
    fn csharp_icon_selection_and_supplemental_are_present() {
        for spec in [
            "Light_FolderPlus",
            "Light_PlusCircle",
            "Light_WindowRestore",
            "Light_MapMarker",
            "Light_Copy",
            "Light_InfoCircle",
            "Regular_SquareFull",
            "Light_Scroll",
            "Solid_Cubes",
            "Light_Bookmark",
        ] {
            assert!(try_get_vector(spec).is_some(), "{spec}");
        }
        assert!(catalog_len() > 5000);
    }
}
