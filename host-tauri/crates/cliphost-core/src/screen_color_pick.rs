//! Screen eyedropper for the settings color popover (B2).
//!
//! The whole virtual screen is copied once into memory, then an own full-screen
//! topmost overlay shows that snapshot and reports the clicked pixel. No global
//! mouse hook is installed: the cursor, Esc and focus loss are all handled by the
//! overlay window, and the click cannot reach any other program. The snapshot
//! lives only in process memory, is wiped and freed before this module returns,
//! and is never written to disk or logged.
//!
//! The public entry point runs synchronously on the calling thread (GPUI calls
//! it from a blocking background task). Everything geometric is a pure function
//! in this file so it can be tested without a window; the Win32 code is in
//! `screen_color_pick_overlay.rs` and is compiled out of this crate's tests.

use std::time::Duration;

/// Longest time the overlay may stay up without a decision.
pub(crate) const OVERLAY_TIMEOUT: Duration = Duration::from_secs(60);

/// Magnifier grid: `GRID` x `GRID` source pixels. The sample count never
/// changes with DPI; only how large each cell is drawn.
pub(crate) const GRID: i32 = 11;
/// Metrics at 96 DPI (scale 1.0). Every one is multiplied by dpi / 96 for the
/// monitor under the cursor, see `MagnifierMetrics`.
pub(crate) const CELL: i32 = 10;
/// Gap between the cursor and the magnifier (right and below the cursor).
pub(crate) const MAGNIFIER_OFFSET: i32 = 24;
/// Frame thickness around the magnified grid.
pub(crate) const MAGNIFIER_FRAME: i32 = 2;
/// Height of the strip that shows the `#RRGGBB` text under the grid.
pub(crate) const MAGNIFIER_TEXT_HEIGHT: i32 = 24;
/// Text height in pixels at scale 1.0.
pub(crate) const MAGNIFIER_FONT: i32 = 16;
#[cfg(test)]
pub(crate) const MAGNIFIER_WIDTH: i32 = GRID * CELL + 2 * MAGNIFIER_FRAME;
#[cfg(test)]
pub(crate) const MAGNIFIER_HEIGHT: i32 = GRID * CELL + 2 * MAGNIFIER_FRAME + MAGNIFIER_TEXT_HEIGHT;
/// Largest scale honoured (384 DPI); also sizes the overlay's scratch bitmap.
pub(crate) const MAX_SCALE: f32 = 4.0;

/// `dpi / 96` for a monitor, bounded to `1.0..=MAX_SCALE`; zero or nonsense
/// readings mean no scaling.
pub(crate) fn scale_for_dpi(dpi: u32) -> f32 {
    if dpi == 0 {
        return 1.0;
    }
    (dpi as f32 / 96.0).clamp(1.0, MAX_SCALE)
}

/// All magnifier sizes in physical pixels for one display scale.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MagnifierMetrics {
    pub(crate) cell: i32,
    pub(crate) offset: i32,
    pub(crate) frame: i32,
    pub(crate) text_height: i32,
    pub(crate) font: i32,
}

impl MagnifierMetrics {
    pub(crate) fn for_scale(scale: f32) -> Self {
        let scale = if scale.is_finite() {
            scale.clamp(1.0, MAX_SCALE)
        } else {
            1.0
        };
        let scaled = |base: i32| ((base as f32 * scale).round() as i32).max(1);
        Self {
            cell: scaled(CELL),
            offset: scaled(MAGNIFIER_OFFSET),
            frame: scaled(MAGNIFIER_FRAME),
            text_height: scaled(MAGNIFIER_TEXT_HEIGHT),
            font: scaled(MAGNIFIER_FONT),
        }
    }

    /// Side of the magnified grid.
    pub(crate) fn grid(self) -> i32 {
        GRID * self.cell
    }

    pub(crate) fn width(self) -> i32 {
        self.grid() + 2 * self.frame
    }

    pub(crate) fn height(self) -> i32 {
        self.grid() + 2 * self.frame + self.text_height
    }
}

/// The virtual screen in physical pixels. `x` / `y` are negative when a monitor
/// sits to the left of or above the primary one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VirtualBounds {
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) width: i32,
    pub(crate) height: i32,
}

impl VirtualBounds {
    pub(crate) fn right(self) -> i32 {
        self.x + self.width
    }

    pub(crate) fn bottom(self) -> i32 {
        self.y + self.height
    }

    /// A usable snapshot needs a positive size that a 32-bit DIB can address.
    pub(crate) fn is_capturable(self) -> bool {
        const LIMIT: i64 = 1 << 28; // 256M pixels, 1 GiB of BGRA
        self.width > 0
            && self.height > 0
            && self.width <= 65_536
            && self.height <= 65_536
            && i64::from(self.width) * i64::from(self.height) <= LIMIT
    }

    fn clamp_point(self, x: i32, y: i32) -> (i32, i32) {
        (
            x.clamp(self.x, self.right().max(self.x + 1) - 1),
            y.clamp(self.y, self.bottom().max(self.y + 1) - 1),
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PixelRect {
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) width: i32,
    pub(crate) height: i32,
}

/// Screen coordinates to a pixel index in the snapshot (top-left origin);
/// `None` outside the virtual screen.
pub(crate) fn snapshot_point(screen: (i32, i32), bounds: VirtualBounds) -> Option<(i32, i32)> {
    let (x, y) = (screen.0 - bounds.x, screen.1 - bounds.y);
    (x >= 0 && y >= 0 && x < bounds.width && y < bounds.height).then_some((x, y))
}

/// Reads one pixel of a top-down 32-bit BGRA buffer. Out of range (including a
/// short buffer) is `None`.
pub(crate) fn pixel_at(
    buffer: &[u8],
    width: i32,
    height: i32,
    x: i32,
    y: i32,
) -> Option<(u8, u8, u8)> {
    if width <= 0 || height <= 0 || x < 0 || y < 0 || x >= width || y >= height {
        return None;
    }
    let index = (y as usize)
        .checked_mul(width as usize)?
        .checked_add(x as usize)?
        .checked_mul(4)?;
    let pixel = buffer.get(index..index.checked_add(4)?)?;
    Some((pixel[2], pixel[1], pixel[0]))
}

/// Where the magnifier goes: the scaled offset right of and below the cursor,
/// flipped to the other side of the cursor when it would leave the virtual
/// screen, then clamped so it never leaves it (unless it is larger than it).
/// `scale` is dpi / 96 of the monitor under the cursor.
pub(crate) fn magnifier_rect(cursor: (i32, i32), bounds: VirtualBounds, scale: f32) -> PixelRect {
    let metrics = MagnifierMetrics::for_scale(scale);
    let (width, height, offset) = (metrics.width(), metrics.height(), metrics.offset);
    let mut x = cursor.0 + offset;
    if x + width > bounds.right() {
        x = cursor.0 - offset - width;
    }
    let mut y = cursor.1 + offset;
    if y + height > bounds.bottom() {
        y = cursor.1 - offset - height;
    }
    // `min` first so a magnifier larger than the screen pins to the origin.
    let x = x.min(bounds.right() - width).max(bounds.x);
    let y = y.min(bounds.bottom() - height).max(bounds.y);
    PixelRect {
        x,
        y,
        width,
        height,
    }
}

/// Screen coordinates of the `GRID` x `GRID` cells around the cursor, indexed
/// `[row][column]`. Cells beyond the edge repeat the nearest edge pixel.
pub(crate) fn sample_grid(
    cursor: (i32, i32),
    bounds: VirtualBounds,
) -> [[(i32, i32); GRID as usize]; GRID as usize] {
    let half = GRID / 2;
    let mut cells = [[(0, 0); GRID as usize]; GRID as usize];
    for (row, line) in cells.iter_mut().enumerate() {
        for (column, cell) in line.iter_mut().enumerate() {
            *cell = bounds.clamp_point(
                cursor.0 + column as i32 - half,
                cursor.1 + row as i32 - half,
            );
        }
    }
    cells
}

pub(crate) fn hex_text(rgb: (u8, u8, u8)) -> String {
    format!("#{:02X}{:02X}{:02X}", rgb.0, rgb.1, rgb.2)
}

/// Black or white, whichever stands out against `rgb` (center-cell outline).
pub(crate) fn contrast_color(rgb: (u8, u8, u8)) -> (u8, u8, u8) {
    let luma = 299 * u32::from(rgb.0) + 587 * u32::from(rgb.1) + 114 * u32::from(rgb.2);
    if luma >= 128_000 {
        (0, 0, 0)
    } else {
        (255, 255, 255)
    }
}

/// Pick a color from the screen. `owner` is the settings window's HWND (0 when
/// unknown) and `still_current` is polled about every 100 ms; once it returns
/// false the overlay closes and the result is `Ok(None)`.
///
/// `Ok(Some(rgb))` is the clicked pixel. `Ok(None)` covers Esc, right click,
/// losing activation, the 60 second timeout and a retired caller. Native
/// dialogs being disabled (headless, tests, other platforms) is also
/// `Ok(None)`: no overlay is ever created then.
pub fn pick_screen_color(
    owner: isize,
    still_current: &dyn Fn() -> bool,
) -> Result<Option<(u8, u8, u8)>, String> {
    if !crate::clipboard::native_dialogs_enabled() {
        return Ok(None);
    }
    show_overlay(owner, still_current)
}

#[cfg(all(windows, not(test)))]
#[path = "screen_color_pick_overlay.rs"]
mod overlay;

#[cfg(all(windows, not(test)))]
fn show_overlay(
    owner: isize,
    still_current: &dyn Fn() -> bool,
) -> Result<Option<(u8, u8, u8)>, String> {
    overlay::run(owner, still_current)
}

// Pure tests cannot fall through to a physical overlay, even with dialogs
// enabled in the environment. Other platforms have no overlay.
#[cfg(any(test, not(windows)))]
fn show_overlay(
    _: isize,
    _: &dyn Fn() -> bool,
) -> Result<Option<(u8, u8, u8)>, String> {
    Ok(None)
}

#[cfg(test)]
#[path = "screen_color_pick_tests.rs"]
mod tests;
