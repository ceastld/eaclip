//! Bounded immutable preview frames. Decode off the UI thread; playback only selects pixels.
use image::{AnimationDecoder, ImageDecoder};
use std::{io::Cursor, time::Duration};

pub(super) const FRAME_BYTES: usize = 32 * 1024 * 1024;
const MAX_FRAMES: usize = 512;

pub(super) struct Picture {
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
    tail: Vec<Vec<u8>>,
    ends: Vec<Duration>,
    plays: Option<u32>,
    bytes: usize,
}

impl Picture {
    pub fn still(pixels: Vec<u8>, width: u32, height: u32) -> Self {
        let bytes = pixels.len();
        Self {
            pixels,
            width,
            height,
            tail: vec![],
            ends: vec![],
            plays: Some(1),
            bytes,
        }
    }
    pub fn byte_len(&self) -> usize {
        self.bytes
    }
    pub fn pixels_at(&self, frame: usize) -> &[u8] {
        if frame == 0 {
            &self.pixels
        } else {
            &self.tail[frame - 1]
        }
    }
    pub fn frame_at(&self, elapsed: Duration) -> (usize, Option<Duration>) {
        if self.tail.is_empty() {
            return (0, None);
        }
        let cycle = self.ends.last().unwrap().as_nanos();
        let elapsed = elapsed.as_nanos();
        if self
            .plays
            .is_some_and(|plays| elapsed / cycle >= plays as u128)
        {
            return (self.tail.len(), None);
        }
        let within = elapsed % cycle;
        let frame = self.ends.partition_point(|end| end.as_nanos() <= within);
        let remaining = self.ends[frame].as_nanos() - within;
        (
            frame,
            Some(Duration::from_nanos(remaining.min(u64::MAX as u128) as u64)),
        )
    }
}

fn limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(128 * 1024 * 1024);
    limits.max_image_width = Some(16384);
    limits.max_image_height = Some(16384);
    limits
}

fn animation_canvas(width: u32, height: u32) -> Result<(), String> {
    limits()
        .check_dimensions(width, height)
        .map_err(|e| e.to_string())?;
    // Compositing codecs keep current/previous canvases and temporary frames.
    if u64::from(width) * u64::from(height) * 4 * 4 > 128 * 1024 * 1024 {
        return Err("Hover animation exceeds the canvas budget".into());
    }
    Ok(())
}

fn pixels(image: image::DynamicImage, dark: bool) -> Picture {
    let rgba = if image.width() > 1600 || image.height() > 1200 {
        image.thumbnail(1600, 1200)
    } else {
        image
    }
    .into_rgba8();
    let (width, height) = rgba.dimensions();
    let mut pixels = rgba.into_raw();
    let background = if dark { 27 } else { 250 };
    for pixel in pixels.chunks_exact_mut(4) {
        let alpha = pixel[3] as u32;
        for channel in &mut pixel[..3] {
            *channel = ((*channel as u32 * alpha + background * (255 - alpha) + 127) / 255) as u8;
        }
        pixel.swap(0, 2);
        pixel[3] = 255;
    }
    Picture::still(pixels, width, height)
}

fn duration(delay: image::Delay) -> Duration {
    let (numerator, denominator) = delay.numer_denom_ms();
    let nanos = numerator as u64 * 1_000_000 / u64::from(denominator.max(1));
    // Match Chromium's decoded-image timing normalization for <=10 ms frames.
    if nanos <= 10_000_000 {
        Duration::from_millis(100)
    } else {
        Duration::from_nanos(nanos)
    }
}

fn collect(
    mut frames: image::Frames<'_>,
    plays: Option<u32>,
    dark: bool,
    active: &mut impl FnMut() -> bool,
    byte_budget: usize,
    frame_budget: usize,
) -> Result<Option<Picture>, String> {
    let mut result: Option<Picture> = None;
    let mut total = Duration::ZERO;
    loop {
        if !active() {
            return Ok(None);
        }
        let Some(frame) = frames.next() else {
            break;
        };
        let frame = frame.map_err(|e| e.to_string())?;
        let delay = duration(frame.delay());
        let picture = pixels(image::DynamicImage::ImageRgba8(frame.into_buffer()), dark);
        if !active() {
            return Ok(None);
        }
        total += delay;
        if let Some(result) = &mut result {
            if (picture.width, picture.height) != (result.width, result.height) {
                return Err("Inconsistent hover animation canvas".into());
            }
            if result.ends.len() >= frame_budget || result.bytes + picture.bytes > byte_budget {
                return Err("Hover animation exceeds the frame budget".into());
            }
            result.bytes += picture.bytes;
            result.tail.push(picture.pixels);
            result.ends.push(total);
        } else {
            if frame_budget == 0 || picture.bytes > byte_budget {
                return Err("Hover animation exceeds the frame budget".into());
            }
            result = Some(Picture {
                plays,
                ends: vec![total],
                ..picture
            });
        }
    }
    result
        .map(Some)
        .ok_or_else(|| "Hover animation has no frames".into())
}

fn take<'a>(bytes: &'a [u8], at: &mut usize, count: usize) -> Result<&'a [u8], String> {
    let end = at.checked_add(count).ok_or("Invalid GIF block size")?;
    let block = bytes.get(*at..end).ok_or("Incomplete GIF metadata")?;
    *at = end;
    Ok(block)
}

// image 0.25's GIF loop_count conflates an absent extension with infinity and
// exposes repeat counts as play counts. Read the structured application block
// so a no-loop GIF plays once and finite GIF repetitions add the initial play.
fn gif_metadata(bytes: &[u8]) -> Result<(Option<u32>, usize), String> {
    let mut at = 0;
    let header = take(bytes, &mut at, 13)?;
    if &header[..6] != b"GIF87a" && &header[..6] != b"GIF89a" {
        return Err("Invalid GIF header".into());
    }
    if header[10] & 128 != 0 {
        take(bytes, &mut at, 3usize << ((header[10] & 7) + 1))?;
    }
    let mut plays = Some(1);
    let mut frames = 0;
    while at < bytes.len() {
        match take(bytes, &mut at, 1)?[0] {
            0x3b => break,
            0x2c => {
                frames += 1;
                if frames > MAX_FRAMES {
                    return Err("Hover animation exceeds the frame budget".into());
                }
                let descriptor = take(bytes, &mut at, 9)?;
                if descriptor[8] & 128 != 0 {
                    take(bytes, &mut at, 3usize << ((descriptor[8] & 7) + 1))?;
                }
                take(bytes, &mut at, 1)?; // LZW minimum code size.
                loop {
                    let size = take(bytes, &mut at, 1)?[0] as usize;
                    if size == 0 {
                        break;
                    }
                    take(bytes, &mut at, size)?;
                }
            }
            0x21 => {
                let label = take(bytes, &mut at, 1)?[0];
                let mut application = false;
                let mut first = true;
                loop {
                    let size = take(bytes, &mut at, 1)?[0] as usize;
                    if size == 0 {
                        break;
                    }
                    let block = take(bytes, &mut at, size)?;
                    if first {
                        application =
                            label == 0xff && (block == b"NETSCAPE2.0" || block == b"ANIMEXTS1.0");
                        first = false;
                    } else if application && block.len() == 3 && block[0] == 1 {
                        let repeats = u16::from_le_bytes([block[1], block[2]]);
                        plays = if repeats == 0 {
                            None
                        } else {
                            Some(u32::from(repeats) + 1)
                        };
                    }
                }
            }
            _ => return Err("Invalid GIF metadata block".into()),
        }
    }
    Ok((plays, frames))
}

fn plays(count: image::metadata::LoopCount) -> Option<u32> {
    match count {
        image::metadata::LoopCount::Infinite => None,
        image::metadata::LoopCount::Finite(count) => Some(count.get()),
    }
}

pub(super) fn decode(
    bytes: &[u8],
    dark: bool,
    mut active: impl FnMut() -> bool,
) -> Result<Option<Picture>, String> {
    if !active() {
        return Ok(None);
    }
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    match reader.format() {
        Some(image::ImageFormat::Gif) => {
            let (plays, count) = gif_metadata(bytes)?;
            if count > 1 {
                let mut decoder = image::codecs::gif::GifDecoder::new(Cursor::new(bytes))
                    .map_err(|e| e.to_string())?;
                decoder.set_limits(limits()).map_err(|e| e.to_string())?;
                let (width, height) = decoder.dimensions();
                animation_canvas(width, height)?;
                return collect(
                    decoder.into_frames(),
                    plays,
                    dark,
                    &mut active,
                    FRAME_BYTES,
                    MAX_FRAMES,
                );
            }
        }
        Some(image::ImageFormat::Png) => {
            let decoder = image::codecs::png::PngDecoder::with_limits(Cursor::new(bytes), limits())
                .map_err(|e| e.to_string())?;
            if decoder.is_apng().map_err(|e| e.to_string())? {
                let (width, height) = decoder.dimensions();
                animation_canvas(width, height)?;
                let decoder = decoder.apng().map_err(|e| e.to_string())?;
                let plays = plays(decoder.loop_count());
                return collect(
                    decoder.into_frames(),
                    plays,
                    dark,
                    &mut active,
                    FRAME_BYTES,
                    MAX_FRAMES,
                );
            }
        }
        Some(image::ImageFormat::WebP) => {
            let decoder = image::codecs::webp::WebPDecoder::new(Cursor::new(bytes))
                .map_err(|e| e.to_string())?;
            if decoder.has_animation() {
                let (width, height) = decoder.dimensions();
                animation_canvas(width, height)?;
                let plays = plays(decoder.loop_count());
                return collect(
                    decoder.into_frames(),
                    plays,
                    dark,
                    &mut active,
                    FRAME_BYTES,
                    MAX_FRAMES,
                );
            }
        }
        _ => {}
    }
    reader.limits(limits());
    let picture = pixels(reader.decode().map_err(|e| e.to_string())?, dark);
    Ok(active().then_some(picture))
}

#[cfg(test)]
mod tests {
    use super::*;
    const GIF: &[u8] = include_bytes!("../tests/fixtures/hover-animated.gif");
    const PNG: &[u8] = include_bytes!("../tests/fixtures/hover-animated.png");
    const WEBP: &[u8] = include_bytes!("../tests/fixtures/hover-animated.webp");
    #[test]
    fn animated_formats_preserve_pixels_delays_and_loop_without_drift() {
        for (name, bytes) in [("GIF", GIF), ("APNG", PNG), ("WebP", WEBP)] {
            let picture = decode(bytes, true, || true).unwrap().unwrap();
            assert_eq!((picture.width, picture.height), (32, 20));
            assert_eq!(picture.byte_len(), 32 * 20 * 4 * 2);
            assert_eq!(&picture.pixels_at(0)[..4], &[80, 30, 210, 255]);
            // image-webp 0.2.4 uses a truncated fixed-point reciprocal in
            // source-over compositing. Allow one RGB LSB relative to libwebp
            // only for this opaque WebP fixture; alpha and other codecs exact.
            let tolerance = u8::from(name == "WebP");
            let actual = &picture.pixels_at(1)[..4];
            for (channel, expected) in [80u8, 210, 30, 255].into_iter().enumerate() {
                assert!(actual[channel].abs_diff(expected) <= if channel == 3 { 0 } else { tolerance },
                    "{name} frame 1 channel {channel}: {actual:?}");
            }
            println!("{name}: decoded first={:?}, second={actual:?}", &picture.pixels[..4]);
            for (ms, frame, remaining) in [
                (0, 0, 80),
                (79, 0, 1),
                (80, 1, 240),
                (319, 1, 1),
                (320, 0, 80),
                (400, 1, 240),
                (32320, 0, 80),
            ] {
                assert_eq!(
                    picture.frame_at(Duration::from_millis(ms)),
                    (frame, Some(Duration::from_millis(remaining)))
                );
            }
        }
    }
    #[test]
    fn finite_gif_and_apng_stop_on_last_frame_and_poster_is_not_animation() {
        for (bytes, cycles) in [
            (
                include_bytes!("../tests/fixtures/hover-once.gif").as_slice(),
                1,
            ),
            (
                include_bytes!("../tests/fixtures/hover-finite.gif").as_slice(),
                2,
            ),
            (
                include_bytes!("../tests/fixtures/hover-poster.png").as_slice(),
                2,
            ),
        ] {
            let picture = decode(bytes, false, || true).unwrap().unwrap();
            assert_eq!(&picture.pixels[..4], &[80, 30, 210, 255]);
            assert_eq!(picture.tail.len(), 1);
            assert_eq!(
                picture.frame_at(Duration::from_millis(cycles * 320)),
                (1, None)
            );
            assert_eq!(picture.frame_at(Duration::from_secs(600)), (1, None));
        }
    }
    #[test]
    fn short_frame_delays_follow_browser_normalization() {
        for ms in [0, 1, 10] {
            assert_eq!(
                duration(image::Delay::from_numer_denom_ms(ms, 1)),
                Duration::from_millis(100)
            );
        }
        assert_eq!(
            duration(image::Delay::from_numer_denom_ms(11, 1)),
            Duration::from_millis(11)
        );
        assert_eq!(
            duration(image::Delay::from_numer_denom_ms(100, 3)),
            Duration::from_nanos(33_333_333)
        );
    }
    #[test]
    fn cancelled_and_over_budget_animations_do_not_return_partial_previews() {
        let decoder = image::codecs::gif::GifDecoder::new(Cursor::new(GIF)).unwrap();
        assert!(collect(
            decoder.into_frames(),
            None,
            true,
            &mut || true,
            32 * 20 * 4 * 2 - 1,
            MAX_FRAMES
        )
        .is_err());
        let decoder = image::codecs::gif::GifDecoder::new(Cursor::new(GIF)).unwrap();
        assert!(collect(
            decoder.into_frames(),
            None,
            true,
            &mut || true,
            FRAME_BYTES,
            1
        )
        .is_err());
        let mut calls = 0;
        assert!(decode(GIF, true, || {
            calls += 1;
            calls < 4
        })
        .unwrap()
        .is_none());
        assert!(animation_canvas(16384, 16384).is_err());
        assert!(decode(b"GIF89a", true, || true).is_err());
    }
}
