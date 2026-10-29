//! Whole-item drag snapshot (DESIGN 2026-10-27 drag preview). The dragged
//! row's own pixels are captured from its window after one hover-free frame,
//! then shaped off the UI thread like the design stage's CSS: corners masked
//! against the list background, bottom fade, drop shadow, 78% opacity.

use std::sync::{Arc, Mutex};

use gpui::RenderImage;

/// Design values (`ui-design` drag-preview stage), logical pixels.
pub(crate) const OPACITY: f32 = 0.78;
const FADE_MIN_ALPHA: f32 = 0.4;
const FADE_LENGTH: f32 = 64.0;
const FADE_START_RATIO: f32 = 0.55;
const SHADOW_OFFSET_Y: f32 = 8.0;
/// CSS `drop-shadow` blur radius; the Gaussian sigma is half of it.
const SHADOW_BLUR: f32 = 18.0;
const SHADOW_ALPHA: f32 = 0.28;
/// Transparent margin around the row that holds the shadow.
pub(crate) const PAD: f32 = 24.0;
/// Largest corner radius masked (chips use 24 DIP).
const CORNER_MAX: f32 = 26.0;
/// Sum of RGB differences treated as "the same colour".
const SAME_COLOR: u32 = 36;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Pixmap {
    pub width: u32,
    pub height: u32,
    /// Straight-alpha BGRA.
    pub bgra: Vec<u8>,
}

/// A shaped snapshot ready for the popup.
pub(crate) struct Snapshot {
    pub image: Arc<RenderImage>,
    /// Logical size of the image, PAD included on every side.
    pub size: (f32, f32),
    /// Pointer offset inside the row, logical.
    pub grab: (f32, f32),
    /// Physical screen position of the row's top-left when the drag began.
    pub origin: (i32, i32),
    /// Source window scale, for the slide-back target.
    pub scale: f32,
    /// Row surface colour (RGB), for the multi-select stack layers.
    pub surface: [u8; 3],
}

struct Shaped {
    pixmap: Pixmap,
    scale: f32,
    grab: (f32, f32),
    origin: (i32, i32),
    surface: [u8; 3],
}

/// Latest finished capture, keyed by the drag's operation id.
static SLOT: Mutex<Option<(String, Result<Shaped, String>)>> = Mutex::new(None);

fn store(operation: &str, result: Result<Shaped, String>) {
    *SLOT.lock().unwrap() = Some((operation.to_owned(), result));
}

/// The finished capture for `operation`, once. Older captures are dropped.
pub(crate) fn take(operation: &str) -> Option<Result<Snapshot, String>> {
    let mut slot = SLOT.lock().unwrap();
    if slot.as_ref().is_some_and(|(current, _)| current != operation) {
        *slot = None;
    }
    let (_, result) = slot.take()?;
    Some(result.map(|shaped| {
        let Pixmap { width, height, bgra } = shaped.pixmap;
        let buffer = image::RgbaImage::from_raw(width, height, bgra).expect("BGRA buffer size");
        Snapshot {
            image: Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])),
            size: (width as f32 / shaped.scale, height as f32 / shaped.scale),
            grab: shaped.grab,
            origin: shaped.origin,
            scale: shaped.scale,
            surface: shaped.surface,
        }
    }))
}

/// Start capturing the row at `bounds` (window logical) of the drag source
/// window, held at `grab` inside the row. Rows paint without hover for one
/// frame; the capture runs after that frame is presented. False when
/// nothing could be scheduled.
#[cfg(windows)]
pub(crate) fn capture(
    operation: String,
    bounds: gpui::Bounds<gpui::Pixels>,
    grab: (f32, f32),
    window: &mut gpui::Window,
) -> bool {
    use crate::windows_api::Win32::{Foundation::{HWND, POINT}, Graphics::Gdi::ClientToScreen};
    let Ok(hwnd) = crate::platform::hwnd(window) else { return false };
    let scale = window.scale_factor();
    let rect = (
        (f32::from(bounds.origin.x) * scale).round() as i32,
        (f32::from(bounds.origin.y) * scale).round() as i32,
        (f32::from(bounds.size.width) * scale).round() as i32,
        (f32::from(bounds.size.height) * scale).round() as i32,
    );
    if rect.2 <= 0 || rect.3 <= 0 || rect.2 > 4096 || rect.3 > 4096 {
        return false;
    }
    let mut origin = POINT { x: rect.0, y: rect.1 };
    if !unsafe { ClientToScreen(HWND(hwnd.0), &mut origin) }.as_bool() {
        return false;
    }
    let origin = (origin.x, origin.y);
    crate::appearance::set_hover_suppressed(true);
    window.refresh();
    // The first callback runs before the hover-free frame is drawn; the
    // second runs once it has been presented.
    window.on_next_frame(move |window, _| {
        window.on_next_frame(move |window, _| {
            let job_operation = operation.clone();
            let posted = crate::platform::post_window_job(window, move |hwnd| {
                let grabbed = grab_client(hwnd, rect);
                crate::appearance::set_hover_suppressed(false);
                match grabbed {
                    Ok((pixmap, outside)) => {
                        std::thread::spawn(move || {
                            let surface = surface_color(&pixmap, scale);
                            let pixmap = shape(&pixmap, scale, outside);
                            store(&job_operation, Ok(Shaped { pixmap, scale, grab, origin, surface }));
                        });
                    }
                    Err(error) => store(&job_operation, Err(error)),
                }
                Ok(())
            });
            if let Err(error) = posted {
                crate::appearance::set_hover_suppressed(false);
                store(&operation, Err(error));
            }
        });
    });
    true
}

#[cfg(not(windows))]
pub(crate) fn capture(_: String, _: gpui::Bounds<gpui::Pixels>, _: (f32, f32), _: &mut gpui::Window) -> bool {
    false
}

/// The row's pixels from this window's client area, plus the colour just
/// outside the row (the list background) when the row is not at an edge.
#[cfg(windows)]
fn grab_client(hwnd: isize, rect: (i32, i32, i32, i32)) -> Result<(Pixmap, Option<[u8; 3]>), String> {
    use windows::Win32::{
        Foundation::{HWND, RECT},
        Graphics::Gdi::*,
        UI::WindowsAndMessaging::GetClientRect,
    };
    #[link(name = "user32")]
    extern "system" {
        #[link_name = "PrintWindow"]
        fn print_window(hwnd: HWND, dc: HDC, flags: u32) -> i32;
    }
    let hwnd = HWND(hwnd as *mut _);
    unsafe {
        let mut client = RECT::default();
        GetClientRect(hwnd, &mut client).map_err(|error| error.to_string())?;
        let (width, height) = (client.right - client.left, client.bottom - client.top);
        if width <= 0 || height <= 0 {
            return Err("Drag source window has no client area.".into());
        }
        let source = GetDC(Some(hwnd));
        if source.is_invalid() {
            return Err("GetDC failed".into());
        }
        let dc = CreateCompatibleDC(Some(source));
        let bitmap = CreateCompatibleBitmap(source, width, height);
        let previous = SelectObject(dc, bitmap.into());
        // PW_CLIENTONLY | PW_RENDERFULLCONTENT: the composed GPUI frame.
        let printed = print_window(hwnd, dc, 3) != 0;
        SelectObject(dc, previous);
        let mut pixels = vec![0u8; width as usize * height as usize * 4];
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                ..Default::default()
            },
            ..Default::default()
        };
        let rows = if printed {
            GetDIBits(dc, bitmap, 0, height as u32, Some(pixels.as_mut_ptr().cast()), &mut info, DIB_RGB_COLORS)
        } else {
            0
        };
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(dc);
        ReleaseDC(Some(hwnd), source);
        if !printed || rows != height {
            return Err("PrintWindow failed for the drag source.".into());
        }
        crop(&pixels, (width, height), rect).ok_or_else(|| "Dragged row is outside its window.".into())
    }
}

/// Crop `rect` (clamped to the client) and sample the colour beside it.
fn crop(client: &[u8], size: (i32, i32), rect: (i32, i32, i32, i32)) -> Option<(Pixmap, Option<[u8; 3]>)> {
    let (left, top) = (rect.0.max(0), rect.1.max(0));
    let (right, bottom) = ((rect.0 + rect.2).min(size.0), (rect.1 + rect.3).min(size.1));
    if right <= left || bottom <= top {
        return None;
    }
    let pixel = |x: i32, y: i32| {
        let index = (y * size.0 + x) as usize * 4;
        [client[index + 2], client[index + 1], client[index]]
    };
    let middle = (top + bottom) / 2;
    let outside = if left >= 2 {
        Some(pixel(left - 2, middle))
    } else if right + 1 < size.0 {
        Some(pixel(right + 1, middle))
    } else {
        None
    };
    let (width, height) = ((right - left) as u32, (bottom - top) as u32);
    let mut bgra = Vec::with_capacity(width as usize * height as usize * 4);
    for y in top..bottom {
        let start = (y * size.0 + left) as usize * 4;
        bgra.extend_from_slice(&client[start..start + width as usize * 4]);
    }
    for alpha in bgra.iter_mut().skip(3).step_by(4) {
        *alpha = 255;
    }
    Some((Pixmap { width, height, bgra }, outside))
}

fn surface_color(pixmap: &Pixmap, scale: f32) -> [u8; 3] {
    let x = ((6.0 * scale).round() as u32).min(pixmap.width.saturating_sub(1));
    let y = pixmap.height / 2;
    let index = (y * pixmap.width + x) as usize * 4;
    [pixmap.bgra[index + 2], pixmap.bgra[index + 1], pixmap.bgra[index]]
}

fn distance(a: [u8; 3], b: [u8; 3]) -> u32 {
    a.iter().zip(b).map(|(a, b)| u32::from(a.abs_diff(b))).sum()
}

/// Rounded corners, bottom fade, shadow and opacity. `outside` is the list
/// background beside the row; corner pixels connected to a corner and
/// matching it become transparent. Output has PAD on every side.
pub(crate) fn shape(source: &Pixmap, scale: f32, outside: Option<[u8; 3]>) -> Pixmap {
    let scale = if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };
    let (width, height) = (source.width as usize, source.height as usize);
    let rgb = |x: usize, y: usize| {
        let index = (y * width + x) * 4;
        [source.bgra[index + 2], source.bgra[index + 1], source.bgra[index]]
    };
    let mut alpha = vec![1.0f32; width * height];
    if let Some(outside) = outside.filter(|outside| distance(*outside, surface_color(source, scale)) > SAME_COLOR) {
        let reach = ((CORNER_MAX * scale).ceil() as usize).min(width / 2).min(height / 2);
        for (corner_x, corner_y) in [(0, 0), (width - 1, 0), (0, height - 1), (width - 1, height - 1)] {
            let within = |x: usize, y: usize| x.abs_diff(corner_x) < reach && y.abs_diff(corner_y) < reach;
            let mut stack = vec![(corner_x, corner_y)];
            while let Some((x, y)) = stack.pop() {
                if !within(x, y) || alpha[y * width + x] == 0.0 || distance(rgb(x, y), outside) > SAME_COLOR {
                    continue;
                }
                alpha[y * width + x] = 0.0;
                if x > 0 { stack.push((x - 1, y)); }
                if x + 1 < width { stack.push((x + 1, y)); }
                if y > 0 { stack.push((x, y - 1)); }
                if y + 1 < height { stack.push((x, y + 1)); }
            }
        }
    }
    let fade_start = (FADE_START_RATIO * height as f32).max(height as f32 - FADE_LENGTH * scale);
    for y in 0..height {
        let y_f = y as f32;
        if y_f <= fade_start {
            continue;
        }
        let progress = (y_f - fade_start) / (height as f32 - fade_start).max(1.0);
        let factor = 1.0 - (1.0 - FADE_MIN_ALPHA) * progress;
        for value in &mut alpha[y * width..(y + 1) * width] {
            *value *= factor;
        }
    }

    let pad = (PAD * scale).round() as usize;
    let (out_width, out_height) = (width + 2 * pad, height + 2 * pad);
    let offset = (SHADOW_OFFSET_Y * scale).round() as usize;
    let mut shadow = vec![0.0f32; out_width * out_height];
    for y in 0..height {
        let target_y = y + pad + offset;
        if target_y >= out_height {
            break;
        }
        for x in 0..width {
            shadow[target_y * out_width + x + pad] = alpha[y * width + x];
        }
    }
    blur(&mut shadow, out_width, out_height, SHADOW_BLUR * 0.5 * scale);

    let mut bgra = vec![0u8; out_width * out_height * 4];
    for y in 0..out_height {
        for x in 0..out_width {
            let shadow_alpha = shadow[y * out_width + x] * SHADOW_ALPHA;
            let (content, color) = if (pad..pad + width).contains(&x) && (pad..pad + height).contains(&y) {
                let (sx, sy) = (x - pad, y - pad);
                (alpha[sy * width + sx], rgb(sx, sy))
            } else {
                (0.0, [0, 0, 0])
            };
            // Content over a black shadow, then the whole preview at OPACITY.
            let combined = content + shadow_alpha * (1.0 - content);
            let index = (y * out_width + x) * 4;
            if combined > 0.0 {
                let keep = content / combined;
                bgra[index] = (f32::from(color[2]) * keep).round() as u8;
                bgra[index + 1] = (f32::from(color[1]) * keep).round() as u8;
                bgra[index + 2] = (f32::from(color[0]) * keep).round() as u8;
            }
            bgra[index + 3] = (combined * OPACITY * 255.0).round() as u8;
        }
    }
    Pixmap { width: out_width as u32, height: out_height as u32, bgra }
}

/// Gaussian approximation: three box blurs per axis.
fn blur(values: &mut [f32], width: usize, height: usize, sigma: f32) {
    if sigma < 0.5 {
        return;
    }
    let ideal = (12.0 * sigma * sigma / 3.0 + 1.0).sqrt();
    let radius = (((ideal - 1.0) / 2.0).round() as usize).max(1);
    let mut scratch = vec![0.0f32; values.len()];
    for _ in 0..3 {
        box_pass(values, &mut scratch, width, height, radius, true);
        box_pass(&scratch, values, width, height, radius, false);
    }
}

fn box_pass(input: &[f32], output: &mut [f32], width: usize, height: usize, radius: usize, horizontal: bool) {
    let (lines, length) = if horizontal { (height, width) } else { (width, height) };
    let at = |line: usize, i: usize| if horizontal { line * width + i } else { i * width + line };
    let span = (2 * radius + 1) as f32;
    for line in 0..lines {
        let mut sum = 0.0;
        for i in 0..=radius.min(length - 1) {
            sum += input[at(line, i)];
        }
        for i in 0..length {
            output[at(line, i)] = sum / span;
            if i + radius + 1 < length {
                sum += input[at(line, i + radius + 1)];
            }
            if i >= radius {
                sum -= input[at(line, i - radius)];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A rounded row: `surface` inside a radius-`r` rect, `outside` around it.
    fn row(width: u32, height: u32, r: f32, surface: [u8; 3], outside: [u8; 3]) -> Pixmap {
        let mut bgra = Vec::new();
        for y in 0..height {
            for x in 0..width {
                let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
                let cx = fx.clamp(r, width as f32 - r);
                let cy = fy.clamp(r, height as f32 - r);
                let color = if (fx - cx).hypot(fy - cy) > r { outside } else { surface };
                bgra.extend([color[2], color[1], color[0], 255]);
            }
        }
        Pixmap { width, height, bgra }
    }

    fn alpha(pixmap: &Pixmap, x: u32, y: u32) -> u8 {
        pixmap.bgra[((y * pixmap.width + x) * 4 + 3) as usize]
    }

    #[test]
    fn corners_matching_the_list_background_become_transparent() {
        let source = row(120, 60, 8.0, [250, 250, 250], [230, 232, 236]);
        let shaped = shape(&source, 1.0, Some([230, 232, 236]));
        let pad = PAD as u32;
        assert_eq!((shaped.width, shaped.height), (120 + 2 * pad, 60 + 2 * pad));
        // Corner pixel: only shadow remains, far below the content alpha.
        assert!(alpha(&shaped, pad, pad) < 40);
        // Row interior near the top: content at the design opacity.
        let interior = alpha(&shaped, pad + 60, pad + 10);
        assert!((interior as f32 - OPACITY * 255.0).abs() <= 2.0, "{interior}");
    }

    #[test]
    fn rows_without_their_own_background_keep_square_corners() {
        let source = row(120, 60, 8.0, [240, 240, 240], [240, 240, 240]);
        let shaped = shape(&source, 1.0, Some([240, 240, 240]));
        let pad = PAD as u32;
        assert!(alpha(&shaped, pad, pad) as f32 >= OPACITY * 255.0 - 2.0);
    }

    #[test]
    fn short_rows_keep_their_upper_half_and_tall_rows_fade_only_the_last_64px() {
        let pad = PAD as u32;
        let short = shape(&row(100, 60, 0.0, [255, 255, 255], [0, 0, 0]), 1.0, None);
        let top = alpha(&short, pad + 50, pad + 30);
        let bottom = alpha(&short, pad + 50, pad + 59);
        assert!((top as f32 - OPACITY * 255.0).abs() <= 2.0);
        assert!(bottom < top && bottom as f32 >= FADE_MIN_ALPHA * OPACITY * 255.0 - 3.0, "{bottom}");
        let tall = shape(&row(100, 300, 0.0, [255, 255, 255], [0, 0, 0]), 1.0, None);
        assert!((alpha(&tall, pad + 50, pad + 230) as f32 - OPACITY * 255.0).abs() <= 2.0);
        assert!(alpha(&tall, pad + 50, pad + 299) < alpha(&tall, pad + 50, pad + 250));
    }

    #[test]
    fn shadow_falls_below_the_row_and_fades_out_within_the_pad() {
        let pad = PAD as u32;
        let shaped = shape(&row(100, 40, 0.0, [255, 255, 255], [0, 0, 0]), 1.0, None);
        let below = alpha(&shaped, pad + 50, pad + 40 + 6);
        let above = alpha(&shaped, pad + 50, pad - 6);
        assert!(below > above, "{below} {above}");
        assert_eq!(alpha(&shaped, 0, 0), 0);
        // Shadow pixels are black; content keeps its colour.
        let index = (((pad + 40 + 6) * shaped.width + pad + 50) * 4) as usize;
        assert_eq!(&shaped.bgra[index..index + 3], &[0, 0, 0]);
    }

    #[test]
    fn crop_clamps_to_the_client_and_samples_beside_the_row() {
        let size = (10, 4);
        let mut client = vec![0u8; 10 * 4 * 4];
        for (index, pixel) in client.chunks_exact_mut(4).enumerate() {
            pixel[2] = index as u8;
        }
        let (pixmap, outside) = crop(&client, size, (3, 1, 20, 2)).unwrap();
        assert_eq!((pixmap.width, pixmap.height), (7, 2));
        // Two pixels left of the row, on its middle line (y = 2, x = 1).
        assert_eq!(outside, Some([21, 0, 0]));
        assert_eq!(pixmap.bgra[2], 13);
        assert!(crop(&client, size, (20, 0, 4, 4)).is_none());
    }
}
