//! Pure Quick Paste preview rules matching the retained Web UI.
//! These projections select display resources only; they perform no I/O and
//! must never replace the original clipboard payload or an accepted action.

use std::ops::Range;

use cliphost_core::i18n::tr;
use serde_json::Value;

use super::html::HtmlColor;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RasterSource {
    Item(String),
    Local(String),
}

// JavaScript trim()/\s, used by localPathProbe/localImagePath. Rust's Unicode
// whitespace differs for BOM and NEXT LINE, so keep the Web predicate explicit.
fn web_space(ch: char) -> bool {
    matches!(ch, '\u{0009}'..='\u{000d}' | ' ' | '\u{00a0}' | '\u{1680}'
        | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}'
        | '\u{205f}' | '\u{3000}' | '\u{feff}')
}

fn web_trim(value: &str) -> &str {
    value.trim_matches(web_space)
}

fn first_path(item: &Value) -> &str {
    web_trim(item["filePaths"][0].as_str().unwrap_or_default())
}

fn local_raster(path: &str) -> bool {
    let path = web_trim(path);
    if path.is_empty() {
        return false;
    }
    let lower = path.to_ascii_lowercase();
    if lower.starts_with("data:") || lower.starts_with("blob:") {
        return false;
    }
    if let Some((scheme, _)) = path.split_once("://") {
        if scheme
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphabetic)
            && scheme
                .bytes()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, b'+' | b'-' | b'.'))
        {
            return false;
        }
    }
    let basename = lower.rsplit(['\\', '/']).next().unwrap_or_default();
    basename.rsplit_once('.').is_some_and(|(_, extension)| {
        matches!(extension, "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp")
    })
}

fn windows_or_unc_path(path: &str) -> bool {
    let length = path.encode_utf16().count();
    if !(3..=520).contains(&length)
        || path
            .chars()
            .any(|ch| matches!(ch, '\r' | '\n' | '\u{2028}' | '\u{2029}'))
    {
        return false;
    }
    let bytes = path.as_bytes();
    if bytes.first().is_some_and(u8::is_ascii_alphabetic)
        && bytes.get(1) == Some(&b':')
        && bytes.get(2).is_some_and(|ch| matches!(ch, b'\\' | b'/'))
    {
        return true;
    }
    let Some(rest) = path.strip_prefix("\\\\") else {
        return false;
    };
    let Some(separator) = rest.find(['\\', '/']) else {
        return false;
    };
    separator > 0 && !rest[..separator].chars().any(web_space)
}

/// `QuickPasteItemRow.resolveQuickPasteImageSrc`: image paths are passed to
/// the existing local-image service, files require fileCount===1, and other
/// kinds probe only the first Windows/UNC line of displayText→title→summary.
/// No home expansion, unquoting, punctuation stripping or existence checks.
pub fn image_source(item: &Value) -> Option<RasterSource> {
    match item["kind"].as_str().unwrap_or_default() {
        "ImageClipItem" => {
            let path = first_path(item);
            if !path.is_empty() {
                return Some(RasterSource::Local(path.to_owned()));
            }
            item["id"]
                .as_str()
                .filter(|id| !id.is_empty())
                .map(|id| RasterSource::Item(id.to_owned()))
        }
        "FileClipItem" => {
            let path = first_path(item);
            (item["fileCount"].as_f64() == Some(1.0) && local_raster(path))
                .then(|| RasterSource::Local(path.to_owned()))
        }
        _ => {
            // JS || chooses before trim; a whitespace-only primary value must
            // not fall through to a different title/summary.
            let primary = ["displayText", "title", "summary"]
                .into_iter()
                .filter_map(|field| item[field].as_str())
                .find(|text| !text.is_empty())
                .unwrap_or_default();
            let line = web_trim(web_trim(primary).split('\n').next().unwrap_or_default());
            (windows_or_unc_path(line) && local_raster(line))
                .then(|| RasterSource::Local(line.to_owned()))
        }
    }
}

/// Row thumbnails also cover IM rich items, whose list DTO carries the image
/// paths (fileCount = image count). An image-only QQ/WeChat message has no
/// text, so without this its row shows nothing. The detail preview keeps its
/// own segment flow and therefore still uses `image_source`.
pub fn row_image_source(item: &Value) -> Option<RasterSource> {
    image_source(item).or_else(|| {
        if item["kind"].as_str() != Some("QQRichTextItem") {
            return None;
        }
        item["filePaths"]
            .as_array()?
            .iter()
            .filter_map(Value::as_str)
            .map(web_trim)
            .find(|path| local_raster(path))
            .map(|path| RasterSource::Local(path.to_owned()))
    })
}

/// A row never renders blank: when the DTO has no list text (an image-only IM
/// message, an HTML fragment holding just an image), name the content instead.
pub fn row_fallback_label(item: &Value) -> String {
    let images = item["fileCount"].as_u64().unwrap_or(0);
    match item["kind"].as_str().unwrap_or_default() {
        "QQRichTextItem" if images > 1 => {
            cliphost_core::trf!("[图片] ×{images}", "[Image] ×{images}")
        }
        "QQRichTextItem" if images == 1 => tr("[图片]", "[Image]").into(),
        "QQRichTextItem" => tr("[即时通讯图文]", "[IM rich content]").into(),
        "ImageClipItem" => tr("[图片]", "[Image]").into(),
        "FileClipItem" => tr("[文件]", "[File]").into(),
        "HtmlClipItem" => tr("[HTML 富文本]", "[HTML rich text]").into(),
        "LinkClipItem" => tr("[链接]", "[Link]").into(),
        "CmdClipItem" => tr("[命令]", "[Command]").into(),
        "ActionClipItem" => tr("[Quicker 动作]", "[Quicker action]").into(),
        "StepClipItem" => tr("[Quicker 步骤]", "[Quicker steps]").into(),
        _ => tr("[空内容]", "[Empty]").into(),
    }
}

/// File list lead when the row does not already have a raster thumbnail.
/// Count is the DTO's fileCount (zero when absent), as used by the Web row badge.
pub fn file_lead(item: &Value) -> Option<(String, usize)> {
    if item["kind"].as_str() != Some("FileClipItem") || image_source(item).is_some() {
        return None;
    }
    let path = first_path(item);
    (!path.is_empty()).then(|| {
        (
            path.to_owned(),
            item["fileCount"]
                .as_u64()
                .and_then(|value| usize::try_from(value).ok())
                .unwrap_or(0),
        )
    })
}

/// Parse the shared Web swatch families. Recognition and invalid-token
/// placeholders belong to native_content::colors; QP needs a painted color.
pub fn pure_color(text: &str) -> Option<HtmlColor> {
    crate::native_content::colors::parse_swatch_color(text)
}

/// Preserve the shared scanner's original UTF-8 ranges, omitting candidates
/// that cannot paint in the QP color preview.
pub fn inline_colors(text: &str) -> Vec<(Range<usize>, HtmlColor)> {
    crate::native_content::colors::collect_color_tokens(text)
        .into_iter()
        .filter_map(|token| token.color.map(|color| (token.range, color)))
        .collect()
}

/// Fit decoded natural dimensions to a logical-pixel box without stretching or
/// enlarging small images. Invalid natural dimensions/unusable bounds are empty.
pub fn image_display_size(w: f32, h: f32, maxw: f32, maxh: f32) -> (f32, f32) {
    if !w.is_finite()
        || !h.is_finite()
        || w <= 0.0
        || h <= 0.0
        || maxw.is_nan()
        || maxh.is_nan()
        || maxw <= 0.0
        || maxh <= 0.0
    {
        return (0.0, 0.0);
    }
    let scale = (maxw / w).min(maxh / h).min(1.0);
    (w * scale, h * scale)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn image_items_prefer_the_first_path_and_keep_id_fallback() {
        assert_eq!(
            image_source(
                &json!({"kind":"ImageClipItem","id":"x","filePaths":[" C:\\a.PNG ","C:\\b.png"]})
            ),
            Some(RasterSource::Local("C:\\a.PNG".into()))
        );
        assert_eq!(
            image_source(&json!({"kind":"ImageClipItem","id":" x ","filePaths":[""]})),
            Some(RasterSource::Item(" x ".into()))
        );
        assert_eq!(image_source(&json!({"kind":"ImageClipItem","id":""})), None);
        // The Web image branch delegates first-path validation to the service.
        assert_eq!(
            image_source(&json!({"kind":"ImageClipItem","filePaths":["not-a-raster"]})),
            Some(RasterSource::Local("not-a-raster".into()))
        );
    }

    #[test]
    fn files_require_the_declared_single_file_and_supported_raster() {
        for path in ["a.webp", "C:\\a.JPEG", "\\\\server\\share\\x.gif"] {
            assert_eq!(
                image_source(&json!({"kind":"FileClipItem","fileCount":1,"filePaths":[path]})),
                Some(RasterSource::Local(path.into()))
            );
        }
        for (count, path) in [
            (2, "C:\\a.png"),
            (1, "https://x/a.png"),
            (1, "file:///C:/a.png"),
            (1, "data:a.png"),
            (1, "C:\\a.svg"),
            (1, "C:\\a.png?x"),
        ] {
            assert!(image_source(
                &json!({"kind":"FileClipItem","fileCount":count,"filePaths":[path]})
            )
            .is_none());
        }
        assert!(image_source(&json!({"kind":"FileClipItem","filePaths":["a.png"]})).is_none());
        assert!(image_source(
            &json!({"kind":"FileClipItem","fileCount":1,"filePaths":["","a.png"]})
        )
        .is_none());
    }

    #[test]
    fn text_paths_match_web_first_line_precedence_without_extra_normalization() {
        assert_eq!(
            image_source(
                &json!({"kind":"TextClipItem","displayText":" C:\\a.png\r\nnotes","title":"C:\\b.png"})
            ),
            Some(RasterSource::Local("C:\\a.png".into()))
        );
        assert_eq!(
            image_source(
                &json!({"kind":"TextClipItem","displayText":"","title":"\\\\server/share/a.png","summary":"C:\\b.png"})
            ),
            Some(RasterSource::Local("\\\\server/share/a.png".into()))
        );
        for text in [
            "~/a.png",
            "a.png",
            "\"C:\\a.png\"",
            "C:\\a.png.",
            "C:\\a.png\rnotes",
            "C:\\a.png\u{2028}notes",
            "\\\\ server\\a.png",
            "\\\\\\a.png",
            "https://x/a.png",
        ] {
            assert!(
                image_source(&json!({"kind":"TextClipItem","displayText":text})).is_none(),
                "{text}"
            );
        }
        assert!(image_source(
            &json!({"kind":"TextClipItem","displayText":"  ","title":"C:\\a.png"})
        )
        .is_none());
    }

    #[test]
    fn text_path_length_uses_utf16_and_javascript_trim() {
        let path = format!("C:\\{}.png", "😀".repeat(256)); // 519 UTF-16 units.
        assert!(image_source(&json!({"kind":"TextClipItem","displayText":path})).is_some());
        let path = format!("C:\\{}.png", "😀".repeat(257));
        assert!(image_source(&json!({"kind":"TextClipItem","displayText":path})).is_none());
        assert!(image_source(
            &json!({"kind":"TextClipItem","displayText":"\u{feff}C:\\a.png\u{feff}"})
        )
        .is_some());
        assert!(
            image_source(&json!({"kind":"TextClipItem","displayText":"\u{0085}C:\\a.png"}))
                .is_none()
        );
    }

    #[test]
    fn file_lead_has_no_image_duplicate_or_inferred_count() {
        assert_eq!(
            file_lead(
                &json!({"kind":"FileClipItem","fileCount":2,"filePaths":[" C:\\a.png ","C:\\b.png"]})
            ),
            Some(("C:\\a.png".into(), 2))
        );
        assert_eq!(
            file_lead(&json!({"kind":"FileClipItem","filePaths":["C:\\a.txt"]})),
            Some(("C:\\a.txt".into(), 0))
        );
        assert!(
            file_lead(&json!({"kind":"FileClipItem","fileCount":1,"filePaths":["a.png"]}))
                .is_none()
        );
        assert!(file_lead(&json!({"kind":"ImageClipItem","filePaths":["a.txt"]})).is_none());
        assert!(file_lead(&json!({"kind":"FileClipItem","filePaths":[" ","a.txt"]})).is_none());
    }

    #[test]
    fn rows_without_list_text_get_an_im_thumbnail_and_a_label() {
        let item = json!({"kind":"QQRichTextItem","fileCount":2,
            "filePaths":["C:\\a.txt"," C:\\Image\\a.png ","C:\\Image\\b.png"]});
        assert_eq!(
            row_image_source(&item),
            Some(RasterSource::Local("C:\\Image\\a.png".into()))
        );
        // The detail preview renders IM segments itself.
        assert!(image_source(&item).is_none());
        assert!(row_image_source(&json!({"kind":"QQRichTextItem","filePaths":[]})).is_none());
        assert!(
            row_image_source(&json!({"kind":"HtmlClipItem","filePaths":["C:\\a.png"]})).is_none()
        );
        assert!(row_fallback_label(&item).contains('2'));
        for kind in [
            "TextClipItem",
            "HtmlClipItem",
            "QQRichTextItem",
            "ImageClipItem",
            "FileClipItem",
            "LinkClipItem",
            "CmdClipItem",
            "ActionClipItem",
            "StepClipItem",
            "FutureClipItem",
            "",
        ] {
            assert!(
                !row_fallback_label(&json!({ "kind": kind })).trim().is_empty(),
                "{kind}"
            );
        }
    }

    #[test]
    fn hex_swatches_use_css_rgba_and_only_the_web_lengths() {
        assert_eq!(
            pure_color(" #aB3 "),
            Some(HtmlColor {
                red: 170,
                green: 187,
                blue: 51,
                alpha: 255
            })
        );
        assert_eq!(
            pure_color("#11223380"),
            Some(HtmlColor {
                red: 17,
                green: 34,
                blue: 51,
                alpha: 128
            })
        );
        for text in [
            "#abcd",
            "#12",
            "#12345",
            "#1234567",
            "#123456789",
            "#xxx",
            "blue",
            "transparent",
        ] {
            assert!(pure_color(text).is_none(), "{text}");
        }
    }

    #[test]
    fn rgb_swatches_support_legacy_and_space_slash_percent_syntax() {
        let expected = Some(HtmlColor {
            red: 255,
            green: 0,
            blue: 128,
            alpha: 128,
        });
        assert_eq!(pure_color("RGBA(255, 0, 128, .5)"), expected);
        assert_eq!(pure_color("rgb(100% 0% 50% / 50%)"), expected);
        assert_eq!(pure_color("rgb(1e2%\t0\n50% / .5)"), expected);
        assert_eq!(
            pure_color("rgb(999 -100 0 / 200%)"),
            Some(HtmlColor {
                red: 255,
                green: 0,
                blue: 0,
                alpha: 255
            })
        );
    }

    #[test]
    fn hsl_swatches_wrap_hue_and_keep_alpha() {
        assert_eq!(
            pure_color("hsl(120,100%,50%)"),
            Some(HtmlColor {
                red: 0,
                green: 255,
                blue: 0,
                alpha: 255
            })
        );
        assert_eq!(
            pure_color("hsla(-120deg 100% 50% / 25%)"),
            Some(HtmlColor {
                red: 0,
                green: 0,
                blue: 255,
                alpha: 64
            })
        );
        assert_eq!(
            pure_color("hsl(.5turn,100%,50%,.5)"),
            Some(HtmlColor {
                red: 0,
                green: 255,
                blue: 255,
                alpha: 128
            })
        );
        assert_eq!(
            pure_color("hsl(200grad 100% 50%)"),
            pure_color("hsl(180 100% 50%)")
        );
        assert_eq!(
            pure_color("hsl(3.141592653589793rad 100% 50%)"),
            pure_color("hsl(180 100% 50%)")
        );
        assert_eq!(
            pure_color("hsl(0 0% 50%)"),
            Some(HtmlColor {
                red: 128,
                green: 128,
                blue: 128,
                alpha: 255
            })
        );
        // CSS Color 4 modern HSL accepts numeric S/L; legacy comma syntax
        // still requires percentages. Keep QP aligned with the shared parser.
        assert_eq!(pure_color("hsl(0 1 50%)"), pure_color("hsl(0 1% 50%)"));
    }

    #[test]
    fn invalid_function_values_cannot_paint() {
        for text in [
            "rgb()",
            "rgb(1,2)",
            "rgb(1,,2,3)",
            "rgb(1 2 3 .5)",
            "rgb(1,2%,3)",
            "rgb(NaN 0 0)",
            "rgb(1e999 0 0)",
            "rgb(1. 0 0)",
            "rgba(1 2 3 / inf)",
            "hsl(0,1,50%)",
            "hsl(NaN 100% 50%)",
            "hsl(1e308turn 100% 50%)",
            "hsl(0 100% 50% /)",
            "rgb(1 2 3)/x",
            "rgb(1 2 3 / .5 / .6)",
        ] {
            assert!(pure_color(text).is_none(), "{text}");
        }
    }

    #[test]
    fn inline_swatches_keep_exact_unicode_ranges_and_web_hex_boundaries() {
        let text="😀中文 #abc / #12345678 / #abcd #abcg #abcdef_foo #abcdef中文 myRGB(1, 2, 3) hsl(120 100% 50%)";
        let found = inline_colors(text);
        assert_eq!(
            found
                .iter()
                .map(|(range, _)| &text[range.clone()])
                .collect::<Vec<_>>(),
            [
                "#abc",
                "#12345678",
                "#abcdef",
                "RGB(1, 2, 3)",
                "hsl(120 100% 50%)"
            ]
        );
        assert_eq!(found[0].0.start, "😀中文 ".len());
        assert!(found
            .windows(2)
            .all(|pair| pair[0].0.end <= pair[1].0.start));
        assert!(found.iter().all(
            |(range, _)| text.is_char_boundary(range.start) && text.is_char_boundary(range.end)
        ));
    }

    #[test]
    fn invalid_function_token_consumption_matches_the_web_regex() {
        let text = "rgb(bad hsl(120,100%,50%)) then #123 and rgba(0,0,255,.5)";
        assert_eq!(
            inline_colors(text)
                .iter()
                .map(|(range, _)| &text[range.clone()])
                .collect::<Vec<_>>(),
            ["#123", "rgba(0,0,255,.5)"]
        );
    }

    #[test]
    fn image_geometry_keeps_natural_aspect_and_never_enlarges() {
        assert_eq!(
            image_display_size(800.0, 400.0, 200.0, 300.0),
            (200.0, 100.0)
        );
        assert_eq!(
            image_display_size(100.0, 1000.0, 200.0, 250.0),
            (25.0, 250.0)
        );
        assert_eq!(image_display_size(40.0, 20.0, 200.0, 250.0), (40.0, 20.0));
        assert_eq!(
            image_display_size(400.0, 800.0, f32::INFINITY, 200.0),
            (100.0, 200.0)
        );
        for args in [
            (0.0, 2.0, 10.0, 10.0),
            (f32::NAN, 2.0, 10.0, 10.0),
            (2.0, f32::INFINITY, 10.0, 10.0),
            (2.0, 2.0, 0.0, 10.0),
            (2.0, 2.0, 10.0, f32::NAN),
        ] {
            assert_eq!(
                image_display_size(args.0, args.1, args.2, args.3),
                (0.0, 0.0)
            );
        }
    }
}
