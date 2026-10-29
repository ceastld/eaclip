//! One UI-thread DirectWrite layout supplies both URL paint and hit testing.
//! Coordinates are physical pixels: the DC target stays at 96 DPI and the
//! requested font size already includes the popup's monitor scale.
use crate::windows_api as windows;
use cliphost_core::content_links::{collect_inline_links, OpenTarget};
use windows::{
    core::w,
    Win32::{
        Foundation::{BOOL, RECT},
        Graphics::{
            Direct2D::{
                Common::{D2D1_ALPHA_MODE_IGNORE, D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_POINT_2F},
                D2D1CreateFactory, ID2D1DCRenderTarget, ID2D1Factory, ID2D1SolidColorBrush,
                D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT,
                D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_RENDER_TARGET_PROPERTIES,
                D2D1_RENDER_TARGET_TYPE_SOFTWARE,
            },
            DirectWrite::*,
            Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
            Gdi::{GdiFlush, HDC},
        },
    },
};

#[derive(Clone, Debug)]
struct Link {
    start: u32,
    length: u32,
    uri: String,
}

fn links(value: &[u16], truncated: bool) -> Vec<Link> {
    let text = String::from_utf16_lossy(value);
    // A recognizer may trim punctuation off a bare URL's range. Exclude the
    // entire cut-off final token, including links ending just before ')' etc.
    let boundary = if truncated {
        text.char_indices().rev().find(|(_, ch)| ch.is_whitespace() || *ch == '\u{feff}')
            .map_or(0, |(index, ch)| index + ch.len_utf8())
    } else { text.len() };
    collect_inline_links(&text).into_iter().filter_map(|link| {
        if truncated && link.range.end > boundary { return None; }
        let OpenTarget::Http(uri) = link.target else { return None; };
        Some(Link {
            start: text[..link.range.start].encode_utf16().count() as u32,
            length: text[link.range].encode_utf16().count() as u32,
            uri,
        })
    }).collect()
}

pub(super) fn has_links(value: &[u16], truncated: bool) -> bool { !links(value, truncated).is_empty() }

/// GDI draws emoji as monochrome outlines; such text uses this engine too.
pub(super) fn has_color_glyphs(value: &[u16]) -> bool {
    char::decode_utf16(value.iter().copied()).flatten().any(|ch| matches!(ch as u32,
        0xFE0F | 0x200D | 0x2300..=0x23FF | 0x2600..=0x27BF | 0x2B00..=0x2BFF
            | 0x1F000..=0x1FAFF | 0xE0020..=0xE007F))
}

struct Layout {
    text: Vec<u16>,
    truncated: bool,
    width: i32,
    px: i32,
    layout: IDWriteTextLayout,
    links: Vec<Link>,
    height: i32,
    measured_width: i32,
}

pub(super) struct TextEngine {
    factory: IDWriteFactory,
    target: ID2D1DCRenderTarget,
    normal: ID2D1SolidColorBrush,
    link: ID2D1SolidColorBrush,
    layout: Option<Layout>,
    /// Exact last successful popup publication, distinct from capture paints.
    painted: Option<(u64, i32, i32, RECT)>,
    prepared: Option<(u64, i32, i32, RECT)>,
}

fn color(red: f32, green: f32, blue: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r: red, g: green, b: blue, a: 1.0 }
}

impl TextEngine {
    pub unsafe fn new() -> windows::core::Result<Self> {
        let factory: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
        let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
        let target = d2d.CreateDCRenderTarget(&D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_IGNORE,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            ..Default::default()
        })?;
        target.SetDpi(96.0, 96.0);
        let normal = target.CreateSolidColorBrush(&color(0.11, 0.11, 0.11), None)?;
        let link = target.CreateSolidColorBrush(&color(0.0, 0.35, 0.8), None)?;
        Ok(Self { factory, target, normal, link, layout: None, painted: None, prepared: None })
    }

    unsafe fn layout(&mut self, text: &[u16], width: i32, px: i32, truncated: bool) -> windows::core::Result<()> {
        let width = width.max(1);
        if self.layout.as_ref().is_some_and(|old| old.text == text && old.width == width && old.px == px && old.truncated == truncated) {
            return Ok(());
        }
        self.clear_hit();
        let format = self.factory.CreateTextFormat(w!("Segoe UI"), None,
            DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL,
            px.max(1) as f32, w!(""))?;
        let layout = self.factory.CreateTextLayout(text, &format, width as f32, 1_000_000.0)?;
        let links = self.layout.as_ref().filter(|old| old.text == text && old.truncated == truncated)
            .map(|old| old.links.clone()).unwrap_or_else(|| links(text, truncated));
        for link in &links {
            let range = DWRITE_TEXT_RANGE { startPosition: link.start, length: link.length };
            layout.SetDrawingEffect(&self.link, range)?;
            layout.SetUnderline(true, range)?;
        }
        let mut metrics = DWRITE_TEXT_METRICS::default();
        layout.GetMetrics(&mut metrics)?;
        self.layout = Some(Layout {
            text: text.to_vec(), truncated, width, px, layout, links,
            height: metrics.height.ceil().max(px as f32) as i32,
            measured_width: metrics.widthIncludingTrailingWhitespace.ceil().max(px as f32) as i32,
        });
        Ok(())
    }

    pub unsafe fn measure(&mut self, text: &[u16], width: i32, px: i32, truncated: bool) -> windows::core::Result<(i32, i32)> {
        self.layout(text, width, px, truncated)?;
        let fitted = self.layout.as_ref().unwrap().measured_width.min(width).max(1);
        self.layout(text, fitted, px, truncated)?;
        Ok((fitted, self.layout.as_ref().unwrap().height))
    }

    pub unsafe fn draw(&mut self, dc: HDC, bounds: RECT, text: &[u16], rect: RECT,
        clip: RECT, px: i32, dark: bool, generation: u64, truncated: bool) -> windows::core::Result<()> {
        self.prepared = None;
        self.layout(text, rect.right - rect.left, px, truncated)?;
        self.normal.SetColor(&if dark { color(0.94, 0.94, 0.94) } else { color(0.11, 0.11, 0.11) });
        self.link.SetColor(&if dark { color(0.40, 0.70, 1.0) } else { color(0.0, 0.35, 0.8) });
        let viewport = RECT {
            left: clip.left.max(rect.left).max(bounds.left),
            top: clip.top.max(rect.top).max(bounds.top),
            right: clip.right.min(rect.right).min(bounds.right),
            bottom: clip.bottom.min(rect.bottom).min(bounds.bottom),
        };
        if viewport.right <= viewport.left || viewport.bottom <= viewport.top { return Ok(()); }
        let _ = GdiFlush();
        // A DC render target owns a backing surface. Bind only the opaque
        // text viewport and initialize its background explicitly; copying it
        // back must never replace the cached shell, shadow or image pixels.
        self.target.BindDC(dc, &viewport)?;
        self.target.BeginDraw();
        let body = if dark { 27.0 / 255.0 } else { 250.0 / 255.0 };
        self.target.Clear(Some(&color(body, body, body)));
        self.target.DrawTextLayout(D2D_POINT_2F { x: (rect.left - viewport.left) as f32, y: (rect.top - viewport.top) as f32 },
            &self.layout.as_ref().unwrap().layout, &self.normal,
            D2D1_DRAW_TEXT_OPTIONS_CLIP | D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT);
        self.target.EndDraw(None, None)?;
        let prepared = (generation, rect.left, rect.top, viewport);
        if self.painted.is_some_and(|old| old != prepared) { self.painted = None; }
        self.prepared = Some(prepared);
        Ok(())
    }

    pub fn clear_hit(&mut self) { self.painted = None; self.prepared = None; }
    pub fn clear_prepared(&mut self) { self.prepared = None; }
    pub fn commit_hit(&mut self) { self.painted = self.prepared.take(); }

    pub unsafe fn hit(&self, generation: u64, x: i32, y: i32) -> Option<String> {
        let (painted, left, top, clip) = self.painted?;
        if painted != generation || x < clip.left || x >= clip.right || y < clip.top || y >= clip.bottom {
            return None;
        }
        let layout = self.layout.as_ref()?;
        let mut trailing = BOOL(0);
        let mut inside = BOOL(0);
        let mut metrics = DWRITE_HIT_TEST_METRICS::default();
        layout.layout.HitTestPoint((x - left) as f32, (y - top) as f32,
            &mut trailing, &mut inside, &mut metrics).ok()?;
        let (x, y) = ((x - left) as f32, (y - top) as f32);
        if !inside.as_bool() || !metrics.isText.as_bool()
            || x < metrics.left || x >= metrics.left + metrics.width
            || y < metrics.top || y >= metrics.top + metrics.height {
            return None;
        }
        layout.links.iter().find(|link|
            metrics.textPosition >= link.start && metrics.textPosition < link.start + link.length)
            .map(|link| link.uri.clone())
    }

    #[cfg(test)]
    pub fn identity(&self) -> (usize, usize) {
        use windows::core::Interface;
        (self.target.as_raw() as usize,
            self.layout.as_ref().map_or(0, |cached| cached.layout.as_raw() as usize))
    }

    #[cfg(test)]
    pub unsafe fn link_rects(&self) -> Vec<(String, RECT)> {
        let Some((_, left, top, _)) = self.painted else { return vec![]; };
        let Some(layout) = &self.layout else { return vec![]; };
        let mut result = Vec::new();
        for link in &layout.links {
            let mut count = 0;
            let _ = layout.layout.HitTestTextRange(link.start, link.length, left as f32, top as f32, None, &mut count);
            let mut metrics = vec![DWRITE_HIT_TEST_METRICS::default(); count as usize];
            if layout.layout.HitTestTextRange(link.start, link.length, left as f32, top as f32,
                Some(&mut metrics), &mut count).is_ok() {
                for metric in metrics.into_iter().take(count as usize) {
                    result.push((link.uri.clone(), RECT {
                        left: metric.left.ceil() as i32, top: metric.top.ceil() as i32,
                        right: (metric.left + metric.width).floor() as i32,
                        bottom: (metric.top + metric.height).floor() as i32,
                    }));
                }
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tooltip_http_ranges_use_utf16_and_leave_local_paths_unlinked() {
        let value = "🦀 中文 https://example.invalid/path\nC:\\Fixture\\note.txt";
        let found = links(&value.encode_utf16().collect::<Vec<_>>(), false);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].start, "🦀 中文 ".encode_utf16().count() as u32);
        assert_eq!(found[0].length, "https://example.invalid/path".encode_utf16().count() as u32);
        assert_eq!(found[0].uri, "https://example.invalid/path");
        assert!(!has_links(&"C:\\Fixture\\note.txt".encode_utf16().collect::<Vec<_>>(), false));
    }

    #[test]
    fn tooltip_emoji_text_is_detected_for_color_rendering() {
        let utf16 = |value: &str| value.encode_utf16().collect::<Vec<_>>();
        for value in ["😆 6XjvEJEPxoX", "❤️", "👍🏽", "🇨🇳", "☀"] {
            assert!(has_color_glyphs(&utf16(value)), "{value}");
        }
        for value in ["中文 abc 123", "C:\\Fixture\\note.txt", "→ — …"] {
            assert!(!has_color_glyphs(&utf16(value)), "{value}");
        }
    }

    #[test]
    fn tooltip_http_ranges_follow_shared_domain_cases() {
        let cases: Vec<serde_json::Value> = serde_json::from_str(include_str!(
            "../../crates/cliphost-core/data/content-link-cases.json")).unwrap();
        for case in cases {
            let text: Vec<_> = case["text"].as_str().unwrap().encode_utf16().collect();
            let actual: Vec<_> = links(&text, false).into_iter().map(|link| serde_json::json!({
                "label": String::from_utf16(&text[link.start as usize..(link.start + link.length) as usize]).unwrap(),
                "target": link.uri,
            })).collect();
            assert_eq!(serde_json::json!(actual), case["links"], "{}", case["text"]);
        }
    }

    #[test]
    fn tooltip_markdown_only_destination_utf16_range_is_clickable() {
        let destination = "https://example.com/a%28b%29";
        for value in [
            "🦀 [网址](https://example.com/a%28b%29 \"title\")",
            "🦀 [https://label.example.com](https://example.com/a%28b%29 \"https://title.example.com\")",
        ] {
            let utf16: Vec<u16> = value.encode_utf16().collect();
            let found = links(&utf16, false);
            assert_eq!(found.len(), 1, "Markdown labels and titles must never become tooltip targets: {value}");
            let start = value[..value.find(destination).unwrap()].encode_utf16().count() as u32;
            let length = destination.encode_utf16().count() as u32;
            assert_eq!((found[0].start, found[0].length), (start, length),
                "emoji and Markdown syntax must not shift the destination's UTF-16 hit range");
            assert_eq!(found[0].uri, destination);
            for position in 0..utf16.len() as u32 {
                let clickable = found.iter().any(|link|
                    position >= link.start && position < link.start + link.length);
                assert_eq!(clickable, position >= start && position < start + length,
                    "only destination UTF-16 units can produce a URL hit at {position}: {value}");
            }
        }
    }

    #[test]
    fn tooltip_parentheses_do_not_absorb_following_prose_without_spaces() {
        let destination = "https://example.com/path";
        for value in [
            "🦀 (https://example.com/path)正文",
            "🦀 （https://example.com/path）正文",
            "🦀 (https://example.com/path)following-text",
            "🦀 [网址](https://example.com/path)正文",
        ] {
            let utf16: Vec<u16> = value.encode_utf16().collect();
            let found = links(&utf16, false);
            assert_eq!(found.len(), 1, "{value}");
            let start = value[..value.find(destination).unwrap()].encode_utf16().count() as u32;
            assert_eq!(found[0].start, start, "{value}");
            assert_eq!(found[0].length, destination.encode_utf16().count() as u32, "{value}");
            assert_eq!(found[0].uri, destination, "{value}");
        }
    }

    #[test]
    fn tooltip_truncation_never_opens_a_partial_boundary_url() {
        let full = format!("https://first.example.invalid/full\nhttps://example.invalid/{}", "segment/".repeat(1600));
        let displayed: Vec<u16> = full.chars().take(8192).collect::<String>().encode_utf16().collect();
        assert!(full.chars().nth(8192).is_some());
        let safe = links(&displayed, true);
        assert_eq!(safe.len(), 1, "the URI cut at the display budget must be excluded");
        assert_eq!(safe[0].uri, "https://first.example.invalid/full");
        let first = "https://first.example.invalid/full\n";
        let final_token = "www.baidu.com/path)";
        let full = format!("{first}{}{final_token}unfinished",
            " ".repeat(8192 - first.chars().count() - final_token.chars().count()));
        let displayed: Vec<u16> = full.chars().take(8192).collect::<String>().encode_utf16().collect();
        let safe = links(&displayed, true);
        assert_eq!(safe.len(), 1, "punctuation trimmed from a bare URL range must not bypass the cut-token guard");
        assert_eq!(safe[0].uri, "https://first.example.invalid/full");
        for final_token in [
            "(https://example.com/partial)正文",
            "（https://example.com/partial）正文",
            "[网址](https://example.com/partial)正文",
        ] {
            let safe = links(&format!("{first}{final_token}").encode_utf16().collect::<Vec<_>>(), true);
            assert_eq!(safe.len(), 1,
                "trimming Markdown/bracket boundaries must not bypass the final-token guard: {final_token}");
            assert_eq!(safe[0].uri, "https://first.example.invalid/full");
        }
        let complete = links(&"https://example.invalid/complete".encode_utf16().collect::<Vec<_>>(), false);
        assert_eq!(complete[0].uri, "https://example.invalid/complete");
    }
}
