//! Authored image-cache fixtures only: no HostState, database, clipboard, API,
//! window or input. Writes one NEW immediate repository .tmp child.
//! Run: cargo run -p cliphost-gpui --example history_image_cache_fixture -- <absolute .tmp/history-image-cache-...>
use std::{fs, io::Cursor, path::Path};

use anyhow::{bail, Context, Result};
use image::{AnimationDecoder, Delay, Frame, ImageEncoder, Rgba, RgbaImage};
use serde_json::{json, Value};

pub(crate) fn png(width: u32, height: u32, seed: u8) -> Result<Vec<u8>> {
    let raster = RgbaImage::from_fn(width, height, |x, y| {
        Rgba([seed.wrapping_add((x / 64) as u8), (y / 32) as u8, seed ^ 0x9a, 255])
    });
    let mut output = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(raster).write_to(&mut output, image::ImageFormat::Png)?;
    Ok(output.into_inner())
}

pub(crate) fn gif(width: u32, height: u32, count: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    {
        let mut encoder = image::codecs::gif::GifEncoder::new(&mut bytes);
        encoder.set_repeat(image::codecs::gif::Repeat::Infinite)?;
        for index in 0..count {
            let raster = RgbaImage::from_pixel(width, height, Rgba([index as u8 * 50, 90, 180, 255]));
            encoder.encode_frame(Frame::from_parts(raster, 0, 0, Delay::from_numer_denom_ms(90, 1)))?;
        }
    }
    let decoded = image::codecs::gif::GifDecoder::new(Cursor::new(&bytes))?
        .into_frames().collect_frames()?;
    anyhow::ensure!(decoded.len() == count, "Generated GIF frame count differs");
    Ok(bytes)
}

pub(crate) fn static_webp(width: u32, height: u32) -> Result<Vec<u8>> {
    let raster = RgbaImage::from_fn(width, height, |x, y| Rgba([(x / 64) as u8, (y / 32) as u8, 119, 255]));
    let mut bytes = Vec::new();
    image::codecs::webp::WebPEncoder::new_lossless(&mut bytes)
        .write_image(raster.as_raw(), width, height, image::ExtendedColorType::Rgba8)?;
    Ok(bytes)
}

fn u24(value: u32) -> [u8; 3] {
    [value as u8, (value >> 8) as u8, (value >> 16) as u8]
}

fn riff_chunk(tag: &[u8; 4], data: &[u8], output: &mut Vec<u8>) {
    output.extend(tag);
    output.extend((data.len() as u32).to_le_bytes());
    output.extend(data);
    if data.len() % 2 != 0 { output.push(0); }
}

/// Assemble an authored animated WebP from independently encoded lossless
/// frames, then verify it with the actual image decoder before publishing.
pub(crate) fn animated_webp(width: u32, height: u32) -> Result<Vec<u8>> {
    let mut body = b"WEBP".to_vec();
    let mut extended = vec![0x02, 0, 0, 0]; // animation flag, opaque pixels
    extended.extend(u24(width - 1));
    extended.extend(u24(height - 1));
    riff_chunk(b"VP8X", &extended, &mut body);
    riff_chunk(b"ANIM", &[0, 0, 0, 0, 0, 0], &mut body);
    for seed in [20u8, 100, 180] {
        let raster = RgbaImage::from_pixel(width, height, Rgba([seed, 40, 220, 255]));
        let mut encoded = Vec::new();
        image::codecs::webp::WebPEncoder::new_lossless(&mut encoded)
            .write_image(raster.as_raw(), width, height, image::ExtendedColorType::Rgba8)?;
        anyhow::ensure!(&encoded[..4] == b"RIFF" && &encoded[8..12] == b"WEBP", "Unexpected frame container");
        let mut frame = vec![0; 6]; // x/y offsets
        frame.extend(u24(width - 1));
        frame.extend(u24(height - 1));
        frame.extend(u24(90));
        frame.push(2); // do not blend this fully opaque frame
        let mut offset = 12;
        while offset + 8 <= encoded.len() {
            let length = u32::from_le_bytes(encoded[offset + 4..offset + 8].try_into()?) as usize;
            let end = offset + 8 + length + length % 2;
            anyhow::ensure!(end <= encoded.len(), "Unexpected lossless frame chunk size");
            if &encoded[offset..offset + 4] == b"VP8L" {
                frame.extend_from_slice(&encoded[offset..end]);
            }
            offset = end;
        }
        anyhow::ensure!(frame.len() > 16, "No encoded lossless frame");
        riff_chunk(b"ANMF", &frame, &mut body);
    }
    let mut output = b"RIFF".to_vec();
    output.extend((body.len() as u32).to_le_bytes());
    output.extend(body);
    let decoder = image::codecs::webp::WebPDecoder::new(Cursor::new(&output))?;
    anyhow::ensure!(decoder.has_animation(), "Authored WebP lost animation flag");
    let frames = decoder.into_frames().collect_frames()?;
    anyhow::ensure!(frames.len() == 3 && frames[0].buffer() != frames[1].buffer(), "Authored WebP is not three distinct frames");
    Ok(output)
}

pub(crate) fn safe_new_root(root: &Path) -> Result<()> {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(3).unwrap();
    let scratch = repository.join(".tmp");
    anyhow::ensure!(root.is_absolute() && root.parent() == Some(scratch.as_path()), "Use an immediate repository .tmp child");
    anyhow::ensure!(root.file_name().and_then(|name| name.to_str()).is_some_and(|name| name.starts_with("history-image-cache-")), "Use a history-image-cache- run name");
    for path in scratch.ancestors().collect::<Vec<_>>().into_iter().rev() {
        let metadata = fs::symlink_metadata(path)?;
        anyhow::ensure!(metadata.is_dir() && !metadata.file_type().is_symlink(), "Linked fixture ancestor refused");
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            anyhow::ensure!(metadata.file_attributes() & 0x441400 == 0, "Cloud/reparse fixture ancestor refused");
        }
    }
    match fs::symlink_metadata(root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        _ => bail!("Fixture output already exists; never overwrite evidence"),
    }
    fs::create_dir(root)?;
    Ok(())
}

fn publish(root: &Path, name: &str, bytes: &[u8], width: u32, height: u32, frames: usize) -> Result<Value> {
    let path = root.join("data/Image/Temp").join(name);
    fs::write(&path, bytes)?;
    Ok(json!({"path":path,"bytes":bytes.len(),"sha256":cliphost_core::model::sha256_hex(bytes),
        "width":width,"height":height,"frames":frames}))
}

fn main() -> Result<()> {
    let root = std::env::args_os().nth(1).context("Pass one new absolute repository .tmp/history-image-cache-... root")?;
    let root = Path::new(&root);
    safe_new_root(root)?;
    fs::create_dir_all(root.join("data/Image/Temp"))?;
    let mut sources = Vec::new();
    for index in 0..8 {
        sources.push(publish(root, &format!("large-{index}.png"), &png(3840, 2160, index * 23)?, 3840, 2160, 1)?);
    }
    sources.push(publish(root, "animated.gif", &gif(320, 180, 4)?, 320, 180, 4)?);
    sources.push(publish(root, "animated.webp", &animated_webp(320, 180)?, 320, 180, 3)?);
    sources.push(publish(root, "static.webp", &static_webp(3840, 2160)?, 3840, 2160, 1)?);
    sources.push(publish(root, "oversized-animation.gif", &gif(2048, 1024, 3)?, 2048, 1024, 3)?);
    sources.push(publish(root, "same-path.png", &png(1920, 1080, 11)?, 1920, 1080, 1)?);
    let replacement = publish(root, "same-path-replacement.png", &png(1920, 1080, 211)?, 1920, 1080, 1)?;
    let items: Vec<_> = sources.iter().enumerate().map(|(index, source)| json!({
        "id":format!("image-cache-{index:02}"),"kind":"ImageClipItem",
        "title":format!("Authored image cache fixture {index:02}"),"imagePath":source["path"],
        "sourceProcessName":"owned-image-cache-fixture"})).collect();
    fs::write(root.join("fixture.json"), serde_json::to_vec_pretty(&json!({"schemaVersion":1,"items":items}))?)?;
    let manifest = json!({"schema":"clip.history-image-cache-fixture.v1","fixture":"fixture.json",
        "sources":sources,"samePathReplacement":replacement,"originalBytesMustRemainUnchanged":true,
        "expectedAnimatedGifFrames":4,"expectedAnimatedWebpFrames":3,
        "oversizedAnimationMustUseBoundedFallbackWithoutChangingOriginal":true,
        "databaseCreated":false,"clipboardAccessed":false,"hostStarted":false,"guiOpened":false});
    fs::write(root.join("original-manifest.json"), serde_json::to_vec_pretty(&manifest)?)?;
    println!("{}", json!({"status":"authored_fixture_created","root":root,"items":items.len(),"hostStarted":false,"guiOpened":false}));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authored_webp_is_actually_multiframe() {
        let bytes = animated_webp(12, 8).unwrap();
        let frames = image::codecs::webp::WebPDecoder::new(Cursor::new(bytes)).unwrap()
            .into_frames().collect_frames().unwrap();
        assert_eq!(frames.len(), 3);
        assert_ne!(frames[0].buffer(), frames[1].buffer());
        assert!(frames.iter().all(|frame| frame.delay() == Delay::from_numer_denom_ms(90, 1)));
    }

    #[test]
    fn authored_gif_is_actually_multiframe() {
        let bytes = gif(12, 8, 4).unwrap();
        let frames = image::codecs::gif::GifDecoder::new(Cursor::new(bytes)).unwrap().into_frames().collect_frames().unwrap();
        assert_eq!(frames.len(), 4);
        assert_ne!(frames[0].buffer(), frames[1].buffer());
        assert_eq!(frames[0].delay(), Delay::from_numer_denom_ms(90, 1));
    }
}
