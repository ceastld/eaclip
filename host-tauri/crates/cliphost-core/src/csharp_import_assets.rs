//! Rewrite imported rich-text image references without rebuilding its markup.
//! Filesystem ownership, copying and deduplication belong to the import resolver.

use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

use crate::model::{hash_text, ClipKind, ClipRecord};

static TAGS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<!--.*?-->|<!\[CDATA\[.*?\]\]>|<(?:[^>\"']|\"[^\"]*\"|'[^']*')*>"#).unwrap()
});
static TAG_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^<\s*(/?)\s*([a-z][a-z0-9:._-]*)\b").unwrap());
static ATTRIBUTES: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)\s+([a-z_:][a-z0-9_.:-]*)\s*=\s*(\"([^\"]*)\"|'([^']*)'|([^\s\"'=<>`]+))"#)
        .unwrap()
});
static ENTITIES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"&([^;&\s]+);").unwrap());
static CSS_URLS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)url\(\s*(?:\"([^\"]*)\"|'([^']*)'|([^)]*?))\s*\)"#).unwrap()
});

/// Both payloads are prepared before changing the record. An unsupported local
/// reference or resolver failure cannot leave partially rewritten record data.
pub(crate) fn rewrite_embedded_images(
    record: &mut ClipRecord,
    resolve: &mut impl FnMut(&str) -> Result<String, String>,
) -> Result<(), String> {
    let remote_base = record.html_source_url.as_deref().is_some_and(|base| {
        url::Url::parse(base)
            .is_ok_and(|url| matches!(url.scheme(), "http" | "https" | "ftp" | "ftps"))
    });
    let html = record
        .html_data
        .as_deref()
        .map(|raw| {
            let rewritten = rewrite_markup(raw, false, remote_base, &record.source_process_name, resolve)?;
            // Asset migration changes byte lengths. Rebuild only the envelope
            // offsets when necessary, never discard document-level markup.
            Ok::<_, String>(if rewritten != raw && crate::cf_html::looks_like_complete_format(raw) {
                crate::cf_html::build_clipboard_html(&rewritten, crate::cf_html::try_parse_source_url(raw).as_deref())
            } else { rewritten })
        })
        .transpose()?;
    let qq = record
        .qq_rich_text
        .as_deref()
        .map(|raw| rewrite_markup(raw, true, false, &record.source_process_name, resolve))
        .transpose()?;
    record.html_data = html;
    record.qq_rich_text = qq;
    if record.kind == ClipKind::QQRichTextItem {
        record.content_hash = hash_text(&format!(
            "qq:{}",
            record.qq_rich_text.as_deref().unwrap_or("")
        ));
    }
    Ok(())
}

fn rewrite_markup(
    raw: &str,
    qq: bool,
    remote_base: bool,
    source_process: &str,
    resolve: &mut impl FnMut(&str) -> Result<String, String>,
) -> Result<String, String> {
    let mut output = String::with_capacity(raw.len());
    let mut cursor = 0;
    let mut raw_text: Option<String> = None;
    let wechat = qq && (crate::im_rich::uses_wechat_envelope(raw)
        || matches!(crate::im_rich::resolve_origin(source_process, raw),
            crate::im_rich::ImRichOrigin::WeChat | crate::im_rich::ImRichOrigin::Wework));
    for matched in TAGS.find_iter(raw) {
        let gap = &raw[cursor..matched.start()];
        if raw_text.as_deref() == Some("style") {
            reject_local_css(gap, remote_base)?;
        } else if raw_text.is_none() {
            reject_incomplete_image_tag(gap, qq)?;
        }
        output.push_str(gap);
        let tag = matched.as_str();
        let Some(name) = TAG_NAME.captures(tag) else {
            if raw_text.as_deref() == Some("style") {
                reject_local_css(tag, remote_base)?;
            }
            output.push_str(tag); // Comments and CDATA are not active markup.
            cursor = matched.end();
            continue;
        };
        let name_string = name[2].to_ascii_lowercase();
        let closing = &name[1] == "/";
        if let Some(active) = &raw_text {
            output.push_str(tag);
            if closing && active == &name_string {
                raw_text = None;
            }
        } else if closing {
            output.push_str(tag);
        } else {
            output.push_str(&rewrite_tag(
                tag,
                &name_string,
                qq,
                wechat,
                remote_base,
                resolve,
            )?);
            if !qq && matches!(name_string.as_str(), "script" | "style") {
                raw_text = Some(name_string);
            }
        }
        cursor = matched.end();
    }
    let tail = &raw[cursor..];
    if raw_text.as_deref() == Some("style") {
        reject_local_css(tail, remote_base)?;
    } else if raw_text.is_none() {
        reject_incomplete_image_tag(tail, qq)?;
    }
    output.push_str(tail);
    Ok(output)
}

fn rewrite_tag(
    tag: &str,
    name: &str,
    qq: bool,
    wechat: bool,
    remote_base: bool,
    resolve: &mut impl FnMut(&str) -> Result<String, String>,
) -> Result<String, String> {
    let attributes: Vec<_> = ATTRIBUTES.captures_iter(tag).collect();
    let value = |key: &str| {
        attributes
            .iter()
            .find(|attr| attr[1].eq_ignore_ascii_case(key))
            .map(|attr| {
                attr.get(3)
                    .or_else(|| attr.get(4))
                    .or_else(|| attr.get(5))
                    .unwrap()
                    .as_str()
            })
    };
    let is_image = matches!(name, "img" | "image")
        || (name == "input"
            && value("type").is_some_and(|value| value.eq_ignore_ascii_case("image")));
    let edit_type = if qq && name == "editelement" {
        value("type").and_then(|value| value.parse::<i32>().ok())
    } else {
        None
    };
    let rich_image = qq
        && name == "editelement"
        && if wechat {
            edit_type == Some(1)
        } else {
            matches!(edit_type, Some(1 | 3))
        };
    let rich_file = qq && name == "editelement" && edit_type == Some(if wechat { 3 } else { 4 });
    let mut replacements = Vec::new();
    let mut image_attribute_count = 0;
    for attr in &attributes {
        let key = attr[1].to_ascii_lowercase();
        let original = attr
            .get(3)
            .or_else(|| attr.get(4))
            .or_else(|| attr.get(5))
            .unwrap()
            .as_str();
        let key_is_image = (is_image && matches!(key.as_str(), "src" | "href" | "xlink:href"))
            || (rich_image && key == "filepath");
        if key_is_image {
            image_attribute_count += 1;
            if image_attribute_count > 1 {
                return Err("图片标签含重复或冲突的来源属性，无法安全迁移。".into());
            }
            if let Some(path) = local_reference(original, remote_base && !qq, !qq)? {
                let mapped = resolve(&path)?;
                let mapped = if qq { mapped } else { file_url(&mapped)? };
                let token = attr.get(2).unwrap();
                replacements.push((
                    token.start(),
                    token.end(),
                    format!("\"{}\"", escape_attribute(&mapped)),
                ));
            }
        } else if key == "style" {
            reject_local_css(&decode_entities(original)?, remote_base)?;
        } else if key == "srcset" && matches!(name, "img" | "source") {
            // Do not pretend a multi-source list has been migrated by rewriting
            // its first URL. Network-only lists can stay verbatim.
            let decoded = decode_entities(original)?;
            for candidate in srcset_references(&decoded) {
                if local_reference(candidate, remote_base && !qq, !qq)?.is_some() {
                    return Err(
                        "本地图片 srcset 尚不支持迁移，请先在 C# 中转换为普通图片条目。".into(),
                    );
                }
            }
        } else if matches!(key.as_str(), "background" | "poster")
            || (qq && matches!(key.as_str(), "filepath" | "image" | "src") && !rich_file)
            || (name == "object"
                && key == "data"
                && value("type").is_some_and(|kind| kind.starts_with("image/")))
        {
            if local_reference(original, remote_base && !qq, !qq)?.is_some() {
                return Err("富文本含尚不支持的本地图片来源属性，已停止导入。".into());
            }
        }
    }
    if (is_image || rich_image) && image_attribute_count == 0 {
        // An empty/missing src has no file reference; a malformed assignment
        // must not evade parsing and silently retain an old path.
        let lower = tag.to_ascii_lowercase();
        if lower.contains("src=")
            || lower.contains("src =")
            || lower.contains("filepath=")
            || lower.contains("filepath =")
        {
            return Err("图片来源属性格式无效，无法安全迁移。".into());
        }
    }
    let mut output = tag.to_string();
    for (start, end, value) in replacements.into_iter().rev() {
        output.replace_range(start..end, &value);
    }
    Ok(output)
}

fn local_reference(raw: &str, remote_base: bool, html: bool) -> Result<Option<String>, String> {
    let value = decode_entities(raw)?;
    let value = value.trim();
    if value.is_empty() || value.starts_with('#') {
        return Ok(None);
    }
    if value.contains('\0') {
        return Err("图片路径包含空字符。".into());
    }
    let lower = value.to_ascii_lowercase();
    if lower.starts_with("file:") {
        return crate::explorer::file_url_to_local_path(value)
            .map(Some)
            .ok_or_else(|| "图片 file: URL 无效，无法安全迁移。".into());
    }
    let drive_path =
        value.as_bytes().get(1) == Some(&b':') && value.as_bytes()[0].is_ascii_alphabetic();
    if !drive_path {
        if let Some((scheme, _)) = value.split_once(':') {
            if matches!(
                scheme.to_ascii_lowercase().as_str(),
                "http" | "https" | "ftp" | "ftps" | "data" | "blob" | "cid"
            ) {
                return Ok(None);
            }
            return Err("富文本图片使用尚不支持的来源协议，无法安全迁移。".into());
        }
        if html && (value.starts_with("//") || (remote_base && !value.starts_with('\\'))) {
            return Ok(None);
        }
    }
    Ok(Some(value.to_string()))
}

fn file_url(path: &str) -> Result<String, String> {
    // URL serialization accepts ordinary drive/UNC paths. Keep filesystem
    // verbatim prefixes out of HTML, including canonicalize() output on Windows.
    let normalized = if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = path.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        path.to_string()
    };
    url::Url::from_file_path(Path::new(&normalized))
        .map(|url| url.to_string())
        .map_err(|_| "迁移目标图片路径不是可用的绝对文件路径。".into())
}

fn srcset_references(value: &str) -> Vec<&str> {
    // A data URL's comma belongs to the URL. Candidate separators follow the
    // whitespace-separated descriptor (or trail the URL), so splitting every
    // comma would misclassify a network/data-only srcset as a local filename.
    let mut sources = Vec::new();
    let mut rest = value;
    loop {
        rest = rest.trim_start_matches(|ch: char| ch.is_ascii_whitespace() || ch == ',');
        if rest.is_empty() {
            break;
        }
        let end = rest
            .find(|ch: char| ch.is_ascii_whitespace())
            .unwrap_or(rest.len());
        let token = &rest[..end];
        sources.push(token.trim_end_matches(','));
        rest = &rest[end..];
        if !token.ends_with(',') {
            rest = rest.find(',').map(|index| &rest[index + 1..]).unwrap_or("");
        }
    }
    sources
}

fn reject_local_css(raw: &str, remote_base: bool) -> Result<(), String> {
    for capture in CSS_URLS.captures_iter(raw) {
        let value = capture
            .get(1)
            .or_else(|| capture.get(2))
            .or_else(|| capture.get(3))
            .unwrap()
            .as_str();
        if local_reference(value, remote_base, true)?.is_some() {
            return Err(
                "本地 CSS 图片 url() 尚不支持迁移，请先在 C# 中转换为普通图片条目。".into(),
            );
        }
    }
    Ok(())
}

fn reject_incomplete_image_tag(raw: &str, qq: bool) -> Result<(), String> {
    let raw = raw.to_ascii_lowercase();
    if raw.contains("<img")
        || raw.contains("<image")
        || (qq && raw.contains("<editelement") && raw.contains("filepath"))
    {
        return Err("富文本图片标签未闭合，无法安全迁移。".into());
    }
    Ok(())
}

fn decode_entities(raw: &str) -> Result<String, String> {
    let mut output = String::with_capacity(raw.len());
    let mut cursor = 0;
    for capture in ENTITIES.captures_iter(raw) {
        let token = capture.get(0).unwrap();
        output.push_str(&raw[cursor..token.start()]);
        let entity = &capture[1];
        let decoded = match entity {
            "amp" => '&',
            "quot" => '"',
            "apos" => '\'',
            "lt" => '<',
            "gt" => '>',
            "colon" => ':',
            "sol" => '/',
            "bsol" => '\\',
            "nbsp" => '\u{a0}',
            "Tab" => '\t',
            "NewLine" => '\n',
            _ => {
                let number = if let Some(hex) = entity
                    .strip_prefix("#x")
                    .or_else(|| entity.strip_prefix("#X"))
                {
                    u32::from_str_radix(hex, 16).ok()
                } else {
                    entity
                        .strip_prefix('#')
                        .and_then(|number| number.parse::<u32>().ok())
                };
                number
                    .and_then(char::from_u32)
                    .filter(|ch| *ch != '\0')
                    .ok_or("图片来源含尚不支持或无效的字符实体，无法安全迁移。")?
            }
        };
        output.push(decoded);
        cursor = token.end();
    }
    output.push_str(&raw[cursor..]);
    Ok(output)
}

fn escape_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mapped_path() -> String {
        #[cfg(windows)]
        {
            r"D:\Rust data\图&片.png".into()
        }
        #[cfg(not(windows))]
        {
            "/tmp/Rust data/图&片.png".into()
        }
    }

    #[test]
    fn html_preservation_asset_rewrite_refreshes_offsets_but_noop_keeps_raw() {
        let raw = crate::cf_html::build_clipboard_html("<html xmlns:wps='urn:wps'><head><style>table{color:red}</style></head><body><!--StartFragment--><img src='old.png'><!--EndFragment--></body></html>", None);
        let mut record = ClipRecord::new_text("owned", "browser");
        record.kind = ClipKind::HtmlClipItem;
        record.html_data = Some(raw.clone());
        let mapped = mapped_path();
        rewrite_embedded_images(&mut record, &mut |_| Ok(mapped.clone())).unwrap();
        let rewritten = record.html_data.as_deref().unwrap();
        assert!(rewritten.contains("<head><style>table{color:red}</style></head>"));
        let fragment = format!("<img src=\"{}\">", escape_attribute(&file_url(&mapped).unwrap()));
        let offset = |field: &str| rewritten.lines().find_map(|line| line.strip_prefix(field))
            .unwrap().trim().parse::<usize>().unwrap();
        assert_eq!(&rewritten[offset("StartFragment:")..offset("EndFragment:")], fragment);
        let unchanged = raw.replace("old.png", "https://images.test/a.png");
        record.html_data = Some(unchanged.clone());
        rewrite_embedded_images(&mut record, &mut |_| panic!("Network image unexpectedly resolved as a local asset")).unwrap();
        assert_eq!(record.html_data.as_deref(), Some(unchanged.as_str()));
    }

    #[test]
    fn embedded_html_rewrites_owned_images_and_preserves_network_comments_and_text() {
        let mut record = ClipRecord::new_text("plain text", "browser");
        record.kind = ClipKind::HtmlClipItem;
        record.html_data = Some(r#"<!-- <img src="ignored.png"> --><script>let s='<img src="ignored.png">';</script><b>text</b><IMG alt='keep' src='Image/a&amp;b.png'><img src=https://example.test/a.png><img src="data:image/png;base64,abc"><img src="//example.test/b.png">"#.into());
        let mut seen = Vec::new();
        let mapped = mapped_path();
        rewrite_embedded_images(&mut record, &mut |path| {
            seen.push(path.to_string());
            Ok(mapped.clone())
        })
        .unwrap();
        assert_eq!(seen, ["Image/a&b.png"]);
        let html = record.html_data.unwrap();
        assert!(html.contains(&format!(
            "src=\"{}\"",
            escape_attribute(&file_url(&mapped).unwrap())
        )));
        assert!(html.contains("alt='keep'"));
        assert!(html.contains("<img src=https://example.test/a.png>"));
        assert!(html.contains("<img src=\"ignored.png\">"));
        assert_eq!(record.body_text.as_deref(), Some("plain text"));
    }

    #[test]
    fn embedded_qq_and_wechat_rewrite_image_paths_without_rebuilding_envelope_or_file_segments() {
        for (envelope, image_code, file_code, process) in
            [("QQRichEditFormat", 3, 4, ""), ("WeChatRichEditFormat", 1, 3, ""),
             ("QQRichEditFormat", 1, 3, "wechat.exe")]
        {
            let mut record = ClipRecord::new_text("hi", process);
            record.kind = ClipKind::QQRichTextItem;
            record.qq_rich_text = Some(format!(
                r#"<{envelope}><Info keep="yes"/><EditElement type="0"><![CDATA[<img src="literal.png">]]></EditElement><EditElement extra="keep" type="{image_code}" filepath="old/a&amp;b.png"></EditElement><EditElement type="{file_code}" filepath="C:\user\document.txt"></EditElement></{envelope}>"#
            ));
            let mapped = mapped_path();
            let mut seen = Vec::new();
            rewrite_embedded_images(&mut record, &mut |path| {
                seen.push(path.to_string());
                Ok(mapped.clone())
            })
            .unwrap();
            assert_eq!(seen, ["old/a&b.png"]);
            let qq = record.qq_rich_text.as_deref().unwrap();
            assert!(qq.contains(&format!("filepath=\"{}\"", escape_attribute(&mapped))));
            assert!(qq.contains("extra=\"keep\""));
            assert!(qq.contains(r#"filepath="C:\user\document.txt""#));
            assert!(qq.contains(r#"<![CDATA[<img src="literal.png">]]>"#));
            assert_eq!(record.content_hash, hash_text(&format!("qq:{qq}")));
        }
    }

    #[test]
    fn embedded_rewrite_failure_keeps_both_original_payloads() {
        let mut record = ClipRecord::new_text("", "");
        record.html_data = Some("<img src='good.png'>".into());
        record.qq_rich_text = Some(r#"<QQRichEditFormat><EditElement type="3" filepath="bad.png"></EditElement></QQRichEditFormat>"#.into());
        let html = record.html_data.clone();
        let qq = record.qq_rich_text.clone();
        assert!(rewrite_embedded_images(&mut record, &mut |path| {
            if path == "bad.png" {
                Err("missing image".into())
            } else {
                Ok(mapped_path())
            }
        })
        .is_err());
        assert_eq!(record.html_data, html);
        assert_eq!(record.qq_rich_text, qq);
    }

    #[test]
    fn embedded_unsupported_local_image_syntax_fails_explicitly() {
        for html in [
            "<img srcset='old.png 1x, other.png 2x'>",
            "<div style=\"background:url('old.png')\"></div>",
            "<style>body { background:url(old.png) }</style>",
            "<img src='unfinished.png",
        ] {
            let mut record = ClipRecord::new_text("", "");
            record.html_data = Some(html.into());
            assert!(rewrite_embedded_images(&mut record, &mut |_| panic!(
                "unsupported syntax cannot publish"
            ))
            .is_err());
        }
        let mut record = ClipRecord::new_text("", "");
        record.qq_rich_text = Some(r#"<QQRichEditFormat><EditElement type="99" filepath="old.png"></EditElement></QQRichEditFormat>"#.into());
        assert!(rewrite_embedded_images(&mut record, &mut |_| panic!(
            "unknown image element cannot publish"
        ))
        .is_err());
    }

    #[test]
    fn embedded_html_relative_network_urls_keep_their_original_source_base() {
        let mut record = ClipRecord::new_text("", "");
        record.html_source_url = Some("https://example.test/article/".into());
        record.html_data = Some("<img src='../image.png'><img src='/image.png'>".into());
        let original = record.html_data.clone();
        rewrite_embedded_images(&mut record, &mut |_| {
            panic!("network URL is not a local file")
        })
        .unwrap();
        assert_eq!(record.html_data, original);
    }

    #[test]
    fn embedded_network_srcset_with_data_urls_is_preserved_but_mixed_local_sources_fail() {
        let mut record = ClipRecord::new_text("", "");
        record.html_data = Some(
            "<img srcset='data:image/png;base64,abc 1x, https://example.test/a.png 2x'>".into(),
        );
        let original = record.html_data.clone();
        rewrite_embedded_images(&mut record, &mut |_| {
            panic!("network candidates are not local files")
        })
        .unwrap();
        assert_eq!(record.html_data, original);
        record.html_data = Some("<img srcset='data:image/png;base64,abc 1x, old.png 2x'>".into());
        assert!(rewrite_embedded_images(&mut record, &mut |_| panic!(
            "unsupported local srcset cannot publish"
        ))
        .is_err());
    }

    #[cfg(windows)]
    #[test]
    fn embedded_file_urls_decode_and_verbatim_destinations_serialize_without_device_prefix() {
        let mut record = ClipRecord::new_text("", "");
        record.html_data = Some("<img src='file:///C:/Csharp/Image/a%20%26%E5%9B%BE.png'>".into());
        rewrite_embedded_images(&mut record, &mut |path| {
            assert_eq!(path, r"C:\Csharp\Image\a &图.png");
            Ok(r"\\?\D:\Rust data\图&片.png".into())
        })
        .unwrap();
        assert_eq!(
            record.html_data.as_deref(),
            Some("<img src=\"file:///D:/Rust%20data/%E5%9B%BE&amp;%E7%89%87.png\">")
        );
        assert_eq!(
            file_url(r"\\?\UNC\server\share\a b.png").unwrap(),
            "file://server/share/a%20b.png"
        );
        assert!(local_reference("file:relative", false, true).is_err());
    }
}
