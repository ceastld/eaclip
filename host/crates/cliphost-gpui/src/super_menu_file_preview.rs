//! File-menu hover projection. Rendering never probes paths or opens files.
use super::Rect;
use crate::hover::{content, Content, FileCard, ImageSource};
use serde_json::Value;

pub(super) fn content_for_node(node: &Value) -> Option<Content> {
    if !super::enabled(node)
        || super::expandable(node)
        || node["kind"].as_str() == Some("Folder")
        || !matches!(node["role"].as_str(), Some("file" | "reopen"))
    {
        return None;
    }
    let path = node["payload"]["path"].as_str()?.trim();
    if path.contains('\0')
        || path.to_ascii_lowercase().starts_with("shell:")
        || path.starts_with("::{")
    {
        return None;
    }
    if content::local_raster_path(path) {
        Some(Content::Image {
            source: ImageSource::Local(path.into()),
            meta_line: String::new(),
        })
    } else if content::local_video_path(path) {
        Some(Content::Video { path: path.into() })
    } else if content::local_text_path(path) {
        Some(Content::Files(FileCard {
            paths: vec![path.into()],
            focus: Some(0),
        }))
    } else {
        None
    }
}

/// Avoid all visible columns, including a cascade folded back to the left.
pub(super) fn avoid_bounds(rects: impl Iterator<Item = Rect>) -> Option<[f64; 4]> {
    let mut rects = rects.filter(|r| {
        [r.x, r.y, r.width, r.height]
            .into_iter()
            .all(f32::is_finite)
            && r.width > 0.0
            && r.height > 0.0
    });
    let first = rects.next()?;
    let (mut left, mut top, mut right, mut bottom) = (
        first.x,
        first.y,
        first.x + first.width,
        first.y + first.height,
    );
    for rect in rects {
        left = left.min(rect.x);
        top = top.min(rect.y);
        right = right.max(rect.x + rect.width);
        bottom = bottom.max(rect.y + rect.height);
    }
    Some([
        left as f64,
        top as f64,
        (right - left) as f64,
        (bottom - top) as f64,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn file(path: &str) -> Value {
        json!({"kind":"Command", "role":"file", "isEnabled":true, "payload":{"path":path}})
    }

    #[test]
    fn menu_files_reuse_image_video_and_bounded_text_previews() {
        assert!(matches!(
            content_for_node(&file(r"C:\not-probed\photo.GIF")),
            Some(Content::Image { .. })
        ));
        assert!(matches!(
            content_for_node(&file(r"C:\not-probed\movie.mp4")),
            Some(Content::Video { .. })
        ));
        assert!(matches!(
            content_for_node(&file(r"C:\not-probed\main.rs")),
            Some(Content::Files(FileCard { focus: Some(0), .. }))
        ));
        assert!(content_for_node(&file("https://example.test/photo.png")).is_none());
        assert!(content_for_node(&file(r"C:\app.exe")).is_none());
    }

    #[test]
    fn folders_actions_disabled_nodes_and_shell_locations_do_not_preview() {
        let mut node = file(r"C:\folder.txt");
        node["kind"] = json!("Folder");
        assert!(content_for_node(&node).is_none());
        node["kind"] = json!("Command");
        node["hasChildren"] = json!(true);
        assert!(content_for_node(&node).is_none());
        node["hasChildren"] = json!(false);
        node["role"] = json!("file-copy-path");
        assert!(content_for_node(&node).is_none());
        node["role"] = json!("file");
        node["isEnabled"] = json!(false);
        assert!(content_for_node(&node).is_none());
        assert!(content_for_node(&file("shell:Downloads")).is_none());
        assert!(content_for_node(&file("shell:Downloads/preview.png")).is_none());
    }

    #[test]
    fn exclusion_spans_both_directions_and_ignores_empty_columns() {
        let bounds = avoid_bounds(
            [
                Rect {
                    x: 300.0,
                    y: 80.0,
                    width: 240.0,
                    height: 300.0,
                },
                Rect {
                    x: 54.0,
                    y: 120.0,
                    width: 240.0,
                    height: 150.0,
                },
                Rect {
                    x: 0.0,
                    y: 0.0,
                    width: 0.0,
                    height: 0.0,
                },
            ]
            .into_iter(),
        );
        assert_eq!(bounds, Some([54.0, 80.0, 486.0, 300.0]));
        assert_eq!(avoid_bounds(std::iter::empty()), None);
    }
}
