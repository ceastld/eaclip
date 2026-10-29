//! Background-only SVG icon admission and bounded rasterization.
//!
//! HTTP SVG source is never returned as `Image::Svg`. The shared HTML5 tokenizer
//! checks decoded attributes, fragments and dimensions; usvg then parses XML.
//! usvg's file resolver is disabled even after admission. Embedded raster images
//! are decoded with limits and normalized to small PNGs before usvg receives them.
//! Font discovery, if text needs it, happens once on this background path.
//! The direct resvg dependency must enable `raster-images` (its default feature)
//! because GPUI's existing resvg dependency disables embedded raster rendering.
//!
//! Complex CSS root sizing, prefixed roots, external references, active foreign
//! content, pattern/filter/use expansion and missing intrinsic dimensions use
//! the caller's generic fallback. Ordinary masks/clips have a conservative
//! budget for every intermediate pixel buffer in the actual expanded tree.

use std::{
    cell::RefCell,
    collections::HashMap,
    io::Cursor,
    sync::{Arc, OnceLock},
};

use html5ever::{
    tendril::StrTendril,
    tokenizer::{BufferQueue, Tag, TagKind, Token, TokenSink, TokenSinkResult, Tokenizer},
};
use image::{ImageFormat, ImageReader, Limits};
use resvg::{tiny_skia, usvg};

use crate::overlay::html::{classify_html_image_source, decode_embedded_image};

const SOURCE_LIMIT: usize = 8 * 1024 * 1024;
const RASTER_LIMIT: usize = 32 * 1024 * 1024;
const ICON_SIDE: u32 = 56;
const NODE_LIMIT: usize = 8192;
const DEPTH_LIMIT: usize = 128;
const INTRINSIC_LIMIT: f64 = 1_000_000_000.0;

fn xml_space(ch: char) -> bool {
    matches!(ch, ' ' | '\t' | '\r' | '\n')
}

/// Strip only XML's inert prolog and leading comments so the original root can
/// be nested intact. DTDs/stylesheet instructions are deliberately unsupported.
fn document(bytes: &[u8]) -> Result<&str, String> {
    if bytes.is_empty() || bytes.len() > SOURCE_LIMIT {
        return Err("SVG icon source exceeds the 8 MiB limit".into());
    }
    let mut text = std::str::from_utf8(bytes)
        .map_err(|_| "SVG icon must be UTF-8".to_owned())?
        .trim_start_matches('\u{feff}')
        .trim_start_matches(xml_space);
    if text.starts_with("<?xml") && text[5..].chars().next().is_some_and(xml_space) {
        let end = text
            .find("?>")
            .ok_or_else(|| "Incomplete SVG XML declaration".to_owned())?;
        text = text[end + 2..].trim_start_matches(xml_space);
    }
    while text.starts_with("<!--") {
        let end = text
            .find("-->")
            .ok_or_else(|| "Incomplete SVG leading comment".to_owned())?;
        text = text[end + 3..].trim_start_matches(xml_space);
    }
    if !text.starts_with("<svg")
        || !text[4..]
            .chars()
            .next()
            .is_some_and(|ch| xml_space(ch) || matches!(ch, '>' | '/'))
    {
        return Err("Image source has no SVG root".into());
    }
    Ok(text)
}

/// Cheap format sniff only. XML/reference/dimension validation belongs to the
/// background rasterizer; malformed SVG can sniff true and still fail safely.
pub(super) fn is_svg_bytes(bytes: &[u8]) -> bool {
    document(bytes).is_ok()
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Intrinsic {
    width: f64,
    height: f64,
}

fn positive(value: f64) -> Result<f64, String> {
    if value.is_finite() && value > 0.0 && value <= INTRINSIC_LIMIT {
        Ok(value)
    } else {
        Err("SVG intrinsic dimensions must be positive and finite".into())
    }
}

fn attr<'a>(tag: &'a Tag, name: &str) -> Option<&'a str> {
    tag.attrs
        .iter()
        .find(|attribute| attribute.name.local.as_ref() == name)
        .map(|attribute| attribute.value.as_ref())
}

fn length(value: &str) -> Result<Option<f64>, String> {
    let value = value.trim_matches(xml_space);
    if let Some(percent) = value.strip_suffix('%') {
        if percent.parse::<f64>().ok() == Some(100.0) {
            return Ok(None);
        }
        return Err("SVG percentage dimensions require the generic fallback".into());
    }
    let lower = value.to_ascii_lowercase();
    positive(
        lower
            .strip_suffix("px")
            .unwrap_or(&lower)
            .parse()
            .map_err(|_| "SVG CSS/unit dimensions require the generic fallback".to_owned())?,
    )
    .map(Some)
}

fn css_dimensions(value: &str) -> bool {
    value.split(';').any(|declaration| {
        declaration.split_once(':').is_some_and(|(property, _)| {
            matches!(
                property.trim().to_ascii_lowercase().as_str(),
                "width" | "height"
            )
        })
    })
}

fn intrinsic(tag: &Tag) -> Result<Intrinsic, String> {
    if attr(tag, "xmlns").is_some_and(|ns| ns != "http://www.w3.org/2000/svg") {
        return Err("SVG root uses an unsupported namespace".into());
    }
    if attr(tag, "style").is_some_and(css_dimensions)
        || ["x", "y"]
            .into_iter()
            .any(|name| attr(tag, name).is_some_and(|v| !matches!(v.trim(), "0" | "0px")))
    {
        return Err("SVG CSS root sizing/position requires the generic fallback".into());
    }
    let view_box = attr(tag, "viewbox")
        .map(|value| {
            let values: Vec<f64> = value
                .split(|ch| xml_space(ch) || ch == ',')
                .filter(|v| !v.is_empty())
                .map(|v| v.parse::<f64>())
                .collect::<Result<_, _>>()
                .map_err(|_| "Invalid SVG viewBox".to_owned())?;
            if values.len() != 4 || !values.iter().all(|v| v.is_finite()) {
                return Err("Invalid SVG viewBox".to_owned());
            }
            Ok(Intrinsic {
                width: positive(values[2])?,
                height: positive(values[3])?,
            })
        })
        .transpose()?;
    let width = attr(tag, "width").map(length).transpose()?.flatten();
    let height = attr(tag, "height").map(length).transpose()?.flatten();
    let dimensions = match (width, height, view_box) {
        (Some(width), Some(height), _) => Intrinsic { width, height },
        (Some(width), None, Some(view_box)) => Intrinsic {
            width,
            height: positive(width * view_box.height / view_box.width)?,
        },
        (None, Some(height), Some(view_box)) => Intrinsic {
            width: positive(height * view_box.width / view_box.height)?,
            height,
        },
        (None, None, Some(view_box)) => view_box,
        _ => return Err("SVG icon has no usable intrinsic dimensions".into()),
    };
    Ok(dimensions)
}

fn css_references(value: &str) -> Result<(), String> {
    // Escaped CSS can disguise a URL token. We use a visible fallback instead
    // of implementing another CSS engine or leaving a path resolver enabled.
    let lower = value.to_ascii_lowercase();
    if value.contains('\\') || lower.contains("@import") {
        return Err("SVG CSS external/escaped references are unsupported".into());
    }
    let mut remaining = lower.as_str();
    while let Some(start) = remaining.find("url(") {
        remaining = &remaining[start + 4..];
        let end = remaining
            .find(')')
            .ok_or_else(|| "Incomplete SVG CSS URL".to_owned())?;
        let target = remaining[..end].trim().trim_matches(['\'', '"']).trim();
        if !target.starts_with('#') || target.len() < 2 {
            return Err("SVG CSS URL must be a local fragment".into());
        }
        remaining = &remaining[end + 1..];
    }
    Ok(())
}

#[derive(Default)]
struct Scan {
    stack: Vec<String>,
    nodes: usize,
    root: Option<Intrinsic>,
    root_closed: bool,
    embedded: Vec<String>,
    style: String,
    error: Option<String>,
}

impl Scan {
    fn accept_tag(&mut self, tag: Tag) -> Result<(), String> {
        let name = tag.name.as_ref();
        if tag.kind == TagKind::EndTag {
            if self.stack.last().is_none_or(|open| open != name) {
                return Err("Unbalanced SVG markup".into());
            }
            if name == "style" {
                css_references(&self.style)?;
                // Stylesheet root sizing could override the bounded wrapper.
                if css_dimensions(&self.style.replace(['{', '}'], ";")) {
                    return Err("SVG stylesheet sizing requires the generic fallback".into());
                }
                self.style.clear();
            }
            self.stack.pop();
            self.root_closed = self.stack.is_empty();
            return Ok(());
        }
        if self.root_closed {
            return Err("SVG icon must have exactly one root".into());
        }
        self.nodes += 1;
        if self.nodes > NODE_LIMIT || self.stack.len() >= DEPTH_LIMIT || tag.attrs.len() > 128 {
            return Err("SVG icon exceeds markup limits".into());
        }
        if self.stack.is_empty() {
            if name != "svg" || self.root.is_some() {
                return Err("SVG icon must have exactly one SVG root".into());
            }
            self.root = Some(intrinsic(&tag)?);
        }
        if matches!(
            name,
            "script" | "foreignobject" | "pattern" | "filter" | "use"
        ) {
            // resvg allocates patterns and primitive filter results using their
            // original regions, independently of its clipped layer max_bbox.
            // usvg's 1,000,000-node use-expansion limit is too late/large for
            // this icon worker. Refuse these before parsing/expanding the tree.
            return Err("SVG active/unbounded content requires the generic fallback".into());
        }
        for attribute in &tag.attrs {
            let name = attribute.name.local.as_ref();
            let value = attribute.value.trim();
            if name == "href" || name.ends_with(":href") {
                if value.starts_with('#') && value.len() > 1 {
                    continue;
                }
                if matches!(tag.name.as_ref(), "image" | "feimage")
                    && value
                        .get(..5)
                        .is_some_and(|v| v.eq_ignore_ascii_case("data:"))
                {
                    let source = classify_html_image_source(value);
                    if decode_embedded_image(&source).is_none() {
                        return Err(
                            "SVG embedded reference must be a supported raster image".into()
                        );
                    }
                    self.embedded.push(value.to_owned());
                } else {
                    return Err("SVG reference must be a local fragment".into());
                }
            } else if name == "style" || value.to_ascii_lowercase().contains("url(") {
                css_references(value)?;
            }
        }
        if tag.self_closing {
            if self.stack.is_empty() {
                self.root_closed = true;
            }
        } else {
            self.stack.push(name.to_owned());
        }
        Ok(())
    }
}

#[derive(Default)]
struct Validator(RefCell<Scan>);

impl TokenSink for Validator {
    type Handle = ();

    fn process_token(&self, token: Token, _: u64) -> TokenSinkResult<()> {
        let mut scan = self.0.borrow_mut();
        if scan.error.is_some() {
            return TokenSinkResult::Continue;
        }
        let result = match token {
            Token::TagToken(tag) => scan.accept_tag(tag),
            Token::CharacterTokens(text) => {
                if scan.stack.last().is_some_and(|name| name == "style") {
                    scan.style.push_str(&text);
                    Ok(())
                } else if scan.stack.is_empty() && !text.chars().all(xml_space) {
                    Err("Text outside SVG root".into())
                } else {
                    Ok(())
                }
            }
            Token::DoctypeToken(_) | Token::NullCharacterToken => {
                Err("SVG DTD/null markup is unsupported".into())
            }
            Token::ParseError(error) => Err(format!("Invalid SVG markup: {error}")),
            _ => Ok(()),
        };
        if let Err(error) = result {
            scan.error = Some(error);
        }
        TokenSinkResult::Continue
    }

    fn adjusted_current_node_present_but_not_in_html_namespace(&self) -> bool {
        !self.0.borrow().stack.is_empty()
    }
}

fn validate(text: &str) -> Result<Scan, String> {
    let input = BufferQueue::default();
    input.push_back(StrTendril::from(text));
    let tokenizer = Tokenizer::new(Validator::default(), Default::default());
    let _ = tokenizer.feed(&input);
    tokenizer.end();
    let mut scan = tokenizer.sink.0.into_inner();
    if let Some(error) = scan.error.take() {
        return Err(error);
    }
    if scan.root.is_none() || !scan.root_closed || !scan.stack.is_empty() {
        return Err("Incomplete SVG root".into());
    }
    Ok(scan)
}

type RasterMap = HashMap<blake3::Hash, Arc<Vec<u8>>>;

fn embedded_rasters(uris: &[String]) -> Result<RasterMap, String> {
    let mut prepared = HashMap::new();
    let mut decoded_bytes = 0usize;
    for uri in uris {
        let (bytes, _) = decode_embedded_image(&classify_html_image_source(uri))
            .ok_or_else(|| "Invalid embedded SVG raster".to_owned())?;
        let key = blake3::hash(&bytes);
        if prepared.contains_key(&key) {
            continue;
        }
        let reader = ImageReader::new(Cursor::new(&bytes))
            .with_guessed_format()
            .map_err(|error| format!("Invalid embedded SVG raster: {error}"))?;
        let format = reader.format();
        let (width, height) = reader
            .into_dimensions()
            .map_err(|error| format!("Invalid embedded SVG raster dimensions: {error}"))?;
        let cost = if format == Some(ImageFormat::Gif) {
            super::gif_decoded_bytes(&bytes)
                .ok_or_else(|| "Invalid or oversized embedded SVG GIF".to_owned())?
        } else {
            (width as usize)
                .checked_mul(height as usize)
                .and_then(|pixels| pixels.checked_mul(4))
                .ok_or_else(|| "Embedded SVG raster dimensions overflow".to_owned())?
        };
        decoded_bytes = decoded_bytes.saturating_add(cost);
        if width == 0
            || height == 0
            || width > 8192
            || height > 8192
            || decoded_bytes > RASTER_LIMIT
        {
            return Err("Embedded SVG rasters exceed the 32 MiB decode budget".into());
        }
        let mut reader = ImageReader::new(Cursor::new(&bytes))
            .with_guessed_format()
            .map_err(|error| format!("Invalid embedded SVG raster: {error}"))?;
        let mut limits = Limits::default();
        limits.max_image_width = Some(8192);
        limits.max_image_height = Some(8192);
        limits.max_alloc = Some(RASTER_LIMIT as u64);
        reader.limits(limits);
        let image = reader
            .decode()
            .map_err(|error| format!("Embedded SVG raster decode failed: {error}"))?
            .thumbnail(ICON_SIDE, ICON_SIDE);
        let mut output = Cursor::new(Vec::new());
        image
            .write_to(&mut output, ImageFormat::Png)
            .map_err(|error| format!("Embedded SVG raster normalization failed: {error}"))?;
        prepared.insert(key, Arc::new(output.into_inner()));
    }
    Ok(prepared)
}

fn font_selector() -> usvg::FontResolver<'static> {
    let default = usvg::FontResolver::default_font_selector();
    usvg::FontResolver {
        select_font: Box::new(move |font, db| {
            static FONTS: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
            if db.is_empty() {
                *db = FONTS
                    .get_or_init(|| {
                        let mut db = usvg::fontdb::Database::new();
                        db.load_system_fonts();
                        Arc::new(db)
                    })
                    .clone();
            }
            default(font, db)
        }),
        select_fallback: usvg::FontResolver::default_fallback_selector(),
    }
}

/// resvg::render clamps ordinary isolated group layers to a max_bbox five
/// times the output dimensions. Masks/clip canvases inherit that layer size;
/// their RGBA canvas and alpha masks fit within two RGBA buffers. Counting all
/// occurrences (even sequential/shared ones) overestimates their simultaneous
/// lifetime, and bounds chains that would otherwise accumulate many layers.
struct PixelBudget {
    bytes: usize,
    nodes: usize,
}

impl PixelBudget {
    fn add(&mut self, bytes: usize) -> Result<(), String> {
        self.bytes = self.bytes.saturating_add(bytes);
        if self.bytes > RASTER_LIMIT {
            return Err("SVG intermediate pixel buffers exceed the 32 MiB budget".into());
        }
        Ok(())
    }

    fn layer(&mut self, count: usize) -> Result<(), String> {
        self.add(count * (ICON_SIDE as usize * 5).pow(2) * 4)
    }

    fn visit(&mut self, depth: usize) -> Result<(), String> {
        self.nodes += 1;
        if depth >= DEPTH_LIMIT || self.nodes > NODE_LIMIT {
            return Err("SVG expanded resource tree exceeds limits".into());
        }
        Ok(())
    }

    fn children(
        &mut self,
        group: &usvg::Group,
        transform: usvg::Transform,
        depth: usize,
    ) -> Result<(), String> {
        for node in group.children() {
            match node {
                usvg::Node::Group(group) => self.group(group, transform, depth + 1)?,
                usvg::Node::Text(text) => self.group(text.flattened(), transform, depth + 1)?,
                usvg::Node::Image(image) => {
                    self.visit(depth + 1)?;
                    if matches!(image.kind(), usvg::ImageKind::SVG(_)) {
                        return Err("Nested SVG images require the generic fallback".into());
                    }
                    // Only admitted normalized PNGs reach this branch. Include
                    // decoder output and row/format-conversion work buffers.
                    self.add(3 * (ICON_SIDE as usize).pow(2) * 4)?;
                }
                usvg::Node::Path(_) => self.visit(depth + 1)?,
            }
        }
        Ok(())
    }

    fn group(
        &mut self,
        group: &usvg::Group,
        transform: usvg::Transform,
        depth: usize,
    ) -> Result<(), String> {
        self.visit(depth)?;
        let transform = transform.pre_concat(group.transform());
        if !group.filters().is_empty() {
            // Also catches filters synthesized by CSS blur()/drop-shadow().
            return Err("SVG filters require the generic fallback".into());
        }
        if group.should_isolate() {
            let rect = group
                .layer_bounding_box()
                .transform(transform)
                .ok_or_else(|| "Invalid SVG layer bounds".to_owned())?;
            // render_group rounds bounds to i32/u32 and adds antialias margins
            // before its max_bbox clamp. Reject saturation/overflow first.
            if [rect.x(), rect.y(), rect.width(), rect.height()]
                .into_iter()
                .any(|value| !value.is_finite() || value.abs() > INTRINSIC_LIMIT as f32)
            {
                return Err("SVG layer bounds exceed safe renderer coordinates".into());
            }
            self.layer(1)?;
        }
        self.children(group, transform, depth)?;
        if let Some(mask) = group.mask() {
            self.mask(mask, transform, depth + 1)?;
        }
        if let Some(clip) = group.clip_path() {
            self.clip(clip, transform, depth + 1)?;
        }
        Ok(())
    }

    fn mask(
        &mut self,
        mask: &usvg::Mask,
        transform: usvg::Transform,
        depth: usize,
    ) -> Result<(), String> {
        self.visit(depth)?;
        self.layer(2)?;
        self.children(mask.root(), transform, depth)?;
        if let Some(mask) = mask.mask() {
            self.mask(mask, transform, depth + 1)?;
        }
        Ok(())
    }

    fn clip(
        &mut self,
        clip: &usvg::ClipPath,
        transform: usvg::Transform,
        depth: usize,
    ) -> Result<(), String> {
        self.visit(depth)?;
        self.layer(2)?;
        self.children(clip.root(), transform.pre_concat(clip.transform()), depth)?;
        if let Some(clip) = clip.clip_path() {
            self.clip(clip, transform, depth + 1)?;
        }
        Ok(())
    }
}

fn admit_pixel_buffers(tree: &usvg::Tree, rasters: &RasterMap) -> Result<(), String> {
    if !tree.patterns().is_empty() || !tree.filters().is_empty() {
        return Err("SVG patterns/filters require the generic fallback".into());
    }
    let mut budget = PixelBudget { bytes: 0, nodes: 0 };
    // Final canvas and PNG encoder staging; known normalized source PNG storage
    // is kept alive by the resolver throughout tree parsing and rendering.
    budget.add(4 * (ICON_SIDE as usize).pow(2) * 4)?;
    for raster in rasters.values() {
        budget.add(raster.capacity())?;
    }
    budget.group(tree.root(), usvg::Transform::identity(), 0)
}

/// Run only on the parent's background resource worker. The output is a 56×56
/// PNG with the original viewport ratio centered, never executable SVG bytes.
pub(super) fn rasterize_icon_svg(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let source = document(bytes)?;
    let scan = validate(source)?;
    let intrinsic = scan.root.unwrap();
    let rasters = embedded_rasters(&scan.embedded)?;
    let wrapper = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"56\" height=\"56\" viewBox=\"0 0 {} {}\" preserveAspectRatio=\"xMidYMid meet\">{source}</svg>",
        intrinsic.width, intrinsic.height
    );
    let resolver_rasters = rasters.clone();
    let options = usvg::Options {
        font_resolver: font_selector(),
        image_href_resolver: usvg::ImageHrefResolver {
            resolve_string: Box::new(|_, _| None),
            resolve_data: Box::new(move |_, data, _| {
                resolver_rasters
                    .get(&blake3::hash(&data))
                    .cloned()
                    .map(usvg::ImageKind::PNG)
            }),
        },
        ..Default::default()
    };
    let tree = usvg::Tree::from_data(wrapper.as_bytes(), &options)
        .map_err(|error| format!("SVG icon parse failed: {error}"))?;
    if tree.size().width() != ICON_SIDE as f32 || tree.size().height() != ICON_SIDE as f32 {
        return Err("SVG stylesheet changed the bounded viewport".into());
    }
    admit_pixel_buffers(&tree, &rasters)?;
    let mut pixmap = tiny_skia::Pixmap::new(ICON_SIDE, ICON_SIDE)
        .ok_or_else(|| "Cannot allocate the SVG icon raster".to_owned())?;
    resvg::render(
        &tree,
        tiny_skia::Transform::identity(),
        &mut pixmap.as_mut(),
    );
    pixmap
        .encode_png()
        .map_err(|error| format!("SVG icon PNG encoding failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(svg: &str) -> Result<Scan, String> {
        validate(document(svg.as_bytes())?)
    }

    #[test]
    fn svg_sniff_accepts_utf8_prolog_comments_and_rejects_other_sources() {
        assert!(is_svg_bytes(b"<svg viewBox='0 0 16 16'/ >"));
        assert!(is_svg_bytes(b"<?xml version='1.0'?><!-- icon --><svg/>"));
        assert!(is_svg_bytes("\u{feff}<svg/>".as_bytes()));
        for source in [
            "<svgish/>",
            "<html><svg/></html>",
            "<!DOCTYPE svg><svg/>",
            "<s:svg/>",
            "<?xml-stylesheet href='a'?><svg/>",
        ] {
            assert!(!is_svg_bytes(source.as_bytes()), "{source}");
        }
        assert!(!is_svg_bytes(&vec![b'x'; SOURCE_LIMIT + 1]));
    }

    #[test]
    fn root_dimensions_preserve_natural_viewport_ratio_and_reject_ambiguous_css() {
        for (svg, expected) in [
            (
                "<svg viewBox='-10 2 64 32'/>",
                Intrinsic {
                    width: 64.0,
                    height: 32.0,
                },
            ),
            (
                "<svg width='120px' height='60' viewBox='0,0,64,32'/>",
                Intrinsic {
                    width: 120.0,
                    height: 60.0,
                },
            ),
            (
                "<svg width='80' viewBox='0 0 64 32'/>",
                Intrinsic {
                    width: 80.0,
                    height: 40.0,
                },
            ),
            (
                "<svg width='100%' height='100%' viewBox='0 0 24 24'/>",
                Intrinsic {
                    width: 24.0,
                    height: 24.0,
                },
            ),
        ] {
            assert_eq!(scan(svg).unwrap().root.unwrap(), expected);
        }
        for svg in [
            "<svg/>",
            "<svg width='0' height='1'/>",
            "<svg width='NaN' height='1'/>",
            "<svg width='1e300' height='1'/>",
            "<svg viewBox='0 0 -1 1'/>",
            "<svg viewBox='0 0 1 NaN'/>",
            "<svg width='1em' viewBox='0 0 24 24'/>",
            "<svg style='width:24px' viewBox='0 0 24 24'/>",
        ] {
            assert!(scan(svg).is_err(), "{svg}");
        }
    }

    #[test]
    fn decoded_href_external_images_uses_and_nested_svg_are_rejected() {
        for href in [
            "file:///C:/a.png",
            "C:/a.png",
            "../a.png",
            "https://example.test/a.svg",
            "&#x66;ile:///C:/a.png",
            "data:image/svg+xml;base64,PHN2Zy8+",
            "data:text/plain;base64,PHN2Zy8+",
        ] {
            assert!(
                scan(&format!(
                    "<svg viewBox='0 0 16 16'><image href='{href}'/></svg>"
                ))
                .is_err(),
                "{href}"
            );
        }
        assert!(scan("<svg viewBox='0 0 16 16'><use xlink:href='other.svg#id'/></svg>").is_err());
        assert!(scan("<svg viewBox='0 0 16 16'><use href='#id'/></svg>").is_err());
        assert!(
            scan("<svg viewBox='0 0 16 16'><image href='data:image/png;base64,AA=='/></svg>")
                .is_ok()
        );
        assert!(
            scan("<svg viewBox='0 0 16 16'><use href='data:image/png;base64,AA=='/></svg>")
                .is_err()
        );
    }

    #[test]
    fn css_external_references_and_active_foreign_markup_have_visible_fallbacks() {
        assert!(scan("<svg viewBox='0 0 16 16'><path fill='url(#gradient)'/></svg>").is_ok());
        for content in [
            "<path fill='url(file:///C:/a.png)'/>",
            "<path style='fill:url(https://example.test/x)'/>",
            "<style>path{fill:url(../x)}</style>",
            "<style>svg{width:1000000px;height:2px}</style>",
            "<style>@import 'remote.css';</style>",
            "<script href='#id'/>",
            "<foreignObject/>",
        ] {
            assert!(
                scan(&format!("<svg viewBox='0 0 16 16'>{content}</svg>")).is_err(),
                "{content}"
            );
        }
        assert!(
            scan("<svg viewBox='0 0 16 16'><style><![CDATA[.c{fill:url(#g)}]]></style></svg>")
                .is_ok()
        );
    }

    #[test]
    fn malformed_multiple_roots_and_depth_are_bounded() {
        for svg in [
            "<svg viewBox='0 0 16 16'>",
            "<svg viewBox='0 0 16 16'/><svg/>",
            "<svg viewBox='0 0 16 16'><g></svg>",
            "<svg viewBox='0 0 16 16'><!DOCTYPE svg></svg>",
        ] {
            assert!(scan(svg).is_err(), "{svg}");
        }
        let svg = format!(
            "<svg viewBox='0 0 16 16'>{}{}</svg>",
            "<g>".repeat(DEPTH_LIMIT),
            "</g>".repeat(DEPTH_LIMIT)
        );
        assert!(scan(&svg).is_err());
    }

    #[test]
    fn svg_output_is_bounded_png_and_huge_intrinsic_rect_keeps_ratio() {
        let bytes = rasterize_icon_svg(b"<svg xmlns='http://www.w3.org/2000/svg' width='100000000' height='50000000'><rect width='100000000' height='50000000' fill='#ff0000'/></svg>").unwrap();
        assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
        let png = image::load_from_memory(&bytes).unwrap().to_rgba8();
        assert_eq!(png.dimensions(), (56, 56));
        assert_eq!(png.get_pixel(28, 28).0, [255, 0, 0, 255]);
        assert_eq!(png.get_pixel(28, 0).0[3], 0);
        assert!(bytes.len() < 56 * 56 * 4 + 4096);
    }

    fn data_uri(bytes: &[u8]) -> String {
        let escaped: String = bytes.iter().map(|byte| format!("%{byte:02X}")).collect();
        format!("data:image/png,{escaped}")
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(width, height, image::Rgba([0, 255, 0, 255]));
        let mut bytes = Cursor::new(Vec::new());
        image.write_to(&mut bytes, ImageFormat::Png).unwrap();
        bytes.into_inner()
    }

    #[test]
    fn admitted_embedded_raster_is_normalized_and_renders_through_closed_resolver() {
        let uri = data_uri(&png(2, 1));
        let embedded = embedded_rasters(&[uri.clone(), uri.clone()]).unwrap();
        assert_eq!(embedded.len(), 1);
        let svg =
            format!("<svg viewBox='0 0 2 1'><image width='2' height='1' href='{uri}'/></svg>");
        let bytes = rasterize_icon_svg(svg.as_bytes()).unwrap();
        let image = image::load_from_memory(&bytes).unwrap().to_rgba8();
        assert_eq!(image.dimensions(), (56, 56));
        assert_eq!(image.get_pixel(28, 28).0, [0, 255, 0, 255]);
        assert_eq!(image.get_pixel(28, 0).0[3], 0);
    }

    #[test]
    fn malformed_disguised_svg_and_oversized_rasters_fail_before_usvg_decode() {
        assert!(embedded_rasters(&[data_uri(b"<svg viewBox='0 0 1 1'/>")]).is_err());
        assert!(embedded_rasters(&["data:image/png;base64,AA==".into()]).is_err());
        // A real, tiny encoded PNG with an inadmissible natural width. No huge
        // decoded allocation is necessary to verify the header admission limit.
        assert!(embedded_rasters(&[data_uri(&png(8193, 1))]).is_err());
    }

    #[test]
    fn pattern_filter_and_use_allocations_are_refused_before_tree_expansion() {
        for content in [
            "<defs><pattern id='p' patternUnits='userSpaceOnUse' width='100000000' height='100000000'><rect width='1' height='1'/></pattern></defs><rect width='16' height='16' fill='url(#p)'/>",
            "<defs><filter id='f' filterUnits='userSpaceOnUse' width='100000000' height='100000000'><feFlood/></filter></defs><rect width='16' height='16' filter='url(#f)'/>",
            "<defs><g id='a'><path d='M0 0L1 1'/></g><g id='b'><use href='#a'/><use href='#a'/></g></defs><use href='#b'/>",
        ] {
            let svg = format!("<svg viewBox='0 0 16 16'>{content}</svg>");
            let error = rasterize_icon_svg(svg.as_bytes()).unwrap_err();
            assert!(error.contains("unbounded"), "{error}");
        }
        // The actual-tree guard also admits no CSS-generated filter region.
        let error = rasterize_icon_svg(b"<svg viewBox='0 0 16 16'><rect width='16' height='16' style='filter:blur(1000000px)'/></svg>").unwrap_err();
        assert!(error.contains("filters"), "{error}");
    }

    #[test]
    fn normal_clip_and_mask_icons_keep_visible_pixels_within_budget() {
        for (definition, attribute) in [
            ("<clipPath id='clip'><rect width='8' height='16'/></clipPath>", "clip-path='url(#clip)'"),
            ("<mask id='mask' maskUnits='userSpaceOnUse' x='0' y='0' width='16' height='16'><rect width='8' height='16' fill='white'/></mask>", "mask='url(#mask)'"),
        ] {
            let svg = format!("<svg viewBox='0 0 16 16'><defs>{definition}</defs><rect width='16' height='16' fill='#00ff00' {attribute}/></svg>");
            let bytes = rasterize_icon_svg(svg.as_bytes()).unwrap();
            let pixels = image::load_from_memory(&bytes).unwrap().to_rgba8();
            assert_eq!(pixels.get_pixel(10, 28).0, [0, 255, 0, 255]);
            assert_eq!(pixels.get_pixel(45, 28).0[3], 0);
        }
    }

    #[test]
    fn cumulative_isolated_layers_and_extreme_child_bounds_fail_before_pixmaps() {
        // resvg retains ancestor layers while drawing each descendant; each
        // large bbox clamps to 280×280 but enough ancestors exceed 32 MiB.
        let rect = "<rect x='-1000' y='-1000' width='2000' height='2000'/>";
        let mut content = rect.to_owned();
        for _ in 0..110 {
            content = format!("<g opacity='.9'>{content}{rect}</g>");
        }
        let svg = format!("<svg viewBox='0 0 16 16'>{content}</svg>");
        let error = rasterize_icon_svg(svg.as_bytes()).unwrap_err();
        assert!(error.contains("pixel buffers"), "{error}");
        // render_group casts/adds antialias margins before its bbox clamp.
        let error = rasterize_icon_svg(b"<svg viewBox='0 0 16 16'><g opacity='.9'><rect x='-100000000000' y='-100000000000' width='200000000000' height='200000000000'/><rect width='1' height='1'/></g></svg>").unwrap_err();
        assert!(error.contains("safe renderer coordinates"), "{error}");
    }
}
