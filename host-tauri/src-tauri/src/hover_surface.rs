//! UI-thread-owned GDI resources for the reusable hover window.
use crate::windows_api as windows;
use windows::{core::w, Win32::Graphics::Gdi::*};

#[derive(Default)]
pub(super) struct Surface {
    dc: HDC,
    bitmap: HBITMAP,
    original_bitmap: HGDIOBJ,
    size: (i32, i32),
    pixels: usize,
    frame: Vec<[u8; 4]>,
    frame_style: Option<(i32, i32, u64, bool)>,
    font: HFONT,
    original_font: HGDIOBJ,
    font_px: i32,
    text: Vec<u16>,
    link_text: Option<super::text_links::TextEngine>,
    link_probe: Vec<u16>,
    link_probe_result: bool,
    link_probe_truncated: bool,
    #[cfg(test)]
    pub allocations: [u32; 3],
}

impl Surface {
    pub unsafe fn ensure_font(&mut self, px: i32) -> Result<HDC, &'static str> {
        if self.dc.is_invalid() {
            self.dc = CreateCompatibleDC(None);
            if self.dc.is_invalid() {
                return Err("Could not create hover drawing context");
            }
            #[cfg(test)]
            {
                self.allocations[0] += 1;
            }
        }
        if self.font_px != px || self.font.is_invalid() {
            let font = CreateFontW(
                -px,
                0,
                0,
                0,
                400,
                0,
                0,
                0,
                DEFAULT_CHARSET.0 as u32,
                0,
                0,
                CLEARTYPE_QUALITY.0 as u32,
                0,
                w!("Segoe UI"),
            );
            if font.is_invalid() {
                return Err("Could not create hover font");
            }
            let old = SelectObject(self.dc, font);
            if old.is_invalid() {
                let _ = DeleteObject(font);
                return Err("Could not select hover font");
            }
            if self.font.is_invalid() {
                self.original_font = old;
            } else {
                let _ = DeleteObject(self.font);
            }
            self.font = font;
            self.font_px = px;
            #[cfg(test)]
            {
                self.allocations[2] += 1;
            }
        }
        Ok(self.dc)
    }

    pub unsafe fn prepare(
        &mut self,
        width: i32,
        height: i32,
        px: i32,
    ) -> Result<HDC, &'static str> {
        if width <= 0 || height <= 0 {
            return Err("Invalid hover drawing size");
        }
        let dc = self.ensure_font(px)?;
        if self.bitmap.is_invalid() || width > self.size.0 || height > self.size.1 {
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    biHeight: -height,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            // DIB pixels are independent of the destination DC (paint or capture).
            // Replace with the requested size instead of combining dimensions
            // from different monitors into an unnecessarily large allocation.
            let mut pixels = std::ptr::null_mut();
            let bitmap = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut pixels, None, 0)
                .map_err(|_| "Could not create hover drawing buffer")?;
            let old = SelectObject(dc, bitmap);
            if old.is_invalid() {
                let _ = DeleteObject(bitmap);
                return Err("Could not select hover drawing buffer");
            }
            if self.bitmap.is_invalid() {
                self.original_bitmap = old;
            } else {
                let _ = DeleteObject(self.bitmap);
            }
            self.bitmap = bitmap;
            self.size = (width, height);
            self.pixels = pixels as usize;
            #[cfg(test)]
            {
                self.allocations[1] += 1;
            }
        }
        // A preceding text draw clips the DC. Clear it before filling the next
        // frame, including when a smaller tooltip replaces an image.
        SelectClipRgn(dc, None);
        if let Some(engine) = &mut self.link_text { engine.clear_prepared(); }
        Ok(dc)
    }

    pub unsafe fn prepare_frame(
        &mut self,
        width: i32,
        height: i32,
        px: i32,
        scale: f64,
        dark: bool,
    ) -> Result<HDC, &'static str> {
        let dc = self.prepare(width, height, px)?;
        let style = (width, height, scale.to_bits(), dark);
        if self.frame_style != Some(style) {
            self.frame.resize(width as usize * height as usize, [0; 4]);
            super::style::draw_frame(width, height, scale, dark, &mut self.frame);
            self.frame_style = Some(style);
        }
        // The allocated DIB may be larger than the current tooltip. Its row
        // stride follows the allocation, not the requested presentation size.
        let pixels = std::slice::from_raw_parts_mut(
            self.pixels as *mut [u8; 4],
            self.size.0 as usize * self.size.1 as usize,
        );
        for (source, target) in self.frame.chunks_exact(width as usize)
            .zip(pixels.chunks_exact_mut(self.size.0 as usize))
        {
            target[..width as usize].copy_from_slice(source);
        }
        Ok(dc)
    }

    pub unsafe fn finish_content(&mut self, rect: windows::Win32::Foundation::RECT) {
        // GDI may clear the alpha channel when it draws text or images. Only
        // the padded content box is painted; the cached antialiased shell and
        // shadow retain their premultiplied pixels.
        let _ = GdiFlush();
        let pixels = std::slice::from_raw_parts_mut(
            self.pixels as *mut [u8; 4],
            self.size.0 as usize * self.size.1 as usize,
        );
        let left = rect.left.clamp(0, self.size.0) as usize;
        let right = rect.right.clamp(left as i32, self.size.0) as usize;
        for y in rect.top.clamp(0, self.size.1)..rect.bottom.clamp(0, self.size.1) {
            let row = &mut pixels[y as usize * self.size.0 as usize..][..self.size.0 as usize];
            for pixel in &mut row[left..right] { pixel[3] = 255; }
        }
    }

    /// Overlay an antialiased capsule after `finish_content` (GDI flushed).
    pub unsafe fn blend_capsule(&mut self, rect: (f64, f64, f64, f64), rgb: [u8; 3], alpha: f64) {
        if self.pixels == 0 || self.size.0 <= 0 || self.size.1 <= 0 {
            return;
        }
        let pixels = std::slice::from_raw_parts_mut(
            self.pixels as *mut [u8; 4],
            self.size.0 as usize * self.size.1 as usize,
        );
        super::style::blend_capsule(pixels, self.size.0 as usize, self.size.1 as usize, rect, rgb, alpha);
    }

    #[cfg(test)]
    pub unsafe fn pixel(&self, x: i32, y: i32) -> [u8; 4] {
        *((self.pixels as *const [u8; 4]).add((y * self.size.0 + x) as usize))
    }

    pub unsafe fn draw_text(
        &mut self,
        value: &[u16],
        rect: &mut windows::Win32::Foundation::RECT,
        flags: DRAW_TEXT_FORMAT,
    ) -> i32 {
        // DrawText's mutable API must never receive shared Content storage.
        // Reuse the scratch allocation across measurements and redraws.
        self.text.clear();
        self.text.extend_from_slice(value);
        DrawTextW(self.dc, &mut self.text, rect, flags)
    }

    fn has_link_text(&mut self, value: &[u16], truncated: bool) -> bool {
        if self.link_probe != value || self.link_probe_truncated != truncated {
            self.link_probe.clear();
            self.link_probe.extend_from_slice(value);
            self.link_probe_result = super::text_links::has_links(value, truncated)
                || super::text_links::has_color_glyphs(value);
            self.link_probe_truncated = truncated;
        }
        self.link_probe_result
    }

    pub unsafe fn measure_links(&mut self, value: &[u16], width: i32, px: i32, truncated: bool) -> Option<(i32, i32)> {
        if !self.has_link_text(value, truncated) { return None; }
        if self.link_text.is_none() {
            self.link_text = super::text_links::TextEngine::new().ok();
        }
        self.link_text.as_mut()?.measure(value, width, px, truncated).ok()
    }

    pub unsafe fn draw_links(&mut self, value: &[u16], bounds: windows::Win32::Foundation::RECT,
        rect: windows::Win32::Foundation::RECT, clip: windows::Win32::Foundation::RECT,
        px: i32, dark: bool, generation: u64, truncated: bool) -> bool {
        if !self.has_link_text(value, truncated) { self.clear_link_hit(); return false; }
        if self.link_text.is_none() {
            self.link_text = super::text_links::TextEngine::new().ok();
        }
        let Some(engine) = &mut self.link_text else { return false; };
        engine.draw(self.dc, bounds, value, rect, clip, px, dark, generation, truncated).is_ok()
    }

    pub fn commit_link_hit(&mut self) {
        if let Some(engine) = &mut self.link_text { engine.commit_hit(); }
    }

    pub fn clear_link_hit(&mut self) {
        if let Some(engine) = &mut self.link_text { engine.clear_hit(); }
    }

    pub unsafe fn hit_link(&self, generation: u64, x: i32, y: i32) -> Option<String> {
        self.link_text.as_ref()?.hit(generation, x, y)
    }

    #[cfg(test)]
    pub fn link_layout_identity(&self) -> Option<(usize, usize)> {
        self.link_text.as_ref().map(|engine| engine.identity())
    }

    #[cfg(test)]
    pub unsafe fn link_rects(&self) -> Vec<(String, windows::Win32::Foundation::RECT)> {
        self.link_text.as_ref().map(|engine| engine.link_rects()).unwrap_or_default()
    }

    #[cfg(test)]
    pub fn handles(&self) -> (HDC, HBITMAP, HFONT, usize) {
        (self.dc, self.bitmap, self.font, self.text.capacity())
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        // Release the DC render target before its bound HDC/DIB are deleted.
        self.link_text = None;
        unsafe {
            if !self.dc.is_invalid() {
                // Deselection is required before deleting the owned objects.
                if !self.original_font.is_invalid() {
                    SelectObject(self.dc, self.original_font);
                }
                if !self.original_bitmap.is_invalid() {
                    SelectObject(self.dc, self.original_bitmap);
                }
                if !self.font.is_invalid() {
                    let _ = DeleteObject(self.font);
                }
                if !self.bitmap.is_invalid() {
                    let _ = DeleteObject(self.bitmap);
                }
                let _ = DeleteDC(self.dc);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::windows_api as windows;
    use windows::Win32::{
        Foundation::{COLORREF, RECT},
        System::Threading::{GetCurrentProcess, GetGuiResources, GR_GDIOBJECTS},
    };

    #[test]
    fn native_surface_reuses_resources_and_resets_clipping() {
        unsafe {
            // Warm up native text/font fallback before measuring steady-state
            // GDI use. Merely creating Segoe UI does not exercise Chinese text.
            {
                let mut warm = Surface::default();
                warm.prepare(20, 20, 14).unwrap();
                let mut rect = RECT {
                    right: 20,
                    bottom: 20,
                    ..Default::default()
                };
                warm.draw_text(&[0x56fa, 0x5b9a], &mut rect, DT_NOPREFIX);
            }
            let _ = GdiFlush();
            let brush = GetStockObject(DC_BRUSH);
            let process = GetCurrentProcess();
            let baseline = GetGuiResources(process, GR_GDIOBJECTS);
            let mut surface = Surface::default();
            let dc = surface.prepare(80, 60, 14).unwrap();
            assert_eq!(GetGuiResources(process, GR_GDIOBJECTS), baseline + 3);
            let original = surface.handles();
            for _ in 0..200 {
                assert_eq!(surface.prepare(80, 60, 14).unwrap(), dc);
                SetDCBrushColor(dc, COLORREF(0x001122ee));
                FillRect(
                    dc,
                    &RECT {
                        right: 80,
                        bottom: 60,
                        ..Default::default()
                    },
                    HBRUSH(brush.0),
                );
                assert_eq!(GetPixel(dc, 0, 0), COLORREF(0x001122ee));
                IntersectClipRect(dc, 10, 10, 30, 30);
                let mut rect = RECT {
                    right: 30,
                    bottom: 30,
                    ..Default::default()
                };
                surface.draw_text(&[0x56fa, 0x5b9a], &mut rect, DT_NOPREFIX);
            }
            assert_eq!(surface.allocations, [1, 1, 1]);
            let handles = surface.handles();
            assert_eq!(
                (handles.0, handles.1, handles.2),
                (original.0, original.1, original.2)
            );
            let capacity = handles.3;
            surface.prepare(400, 240, 14).unwrap();
            let larger = surface.handles();
            surface.prepare(20, 10, 14).unwrap();
            assert_eq!(surface.handles(), larger);
            surface.ensure_font(28).unwrap();
            let _ = GdiFlush();
            let mut rect = RECT {
                right: 20,
                bottom: 10,
                ..Default::default()
            };
            surface.draw_text(&[0x56fa, 0x5b9a], &mut rect, DT_CALCRECT | DT_NOPREFIX);
            assert_eq!(surface.handles().3, capacity);
            assert_eq!(surface.allocations, [1, 2, 2]);
            drop(surface);
            let _ = GdiFlush();
            assert_eq!(GetGuiResources(process, GR_GDIOBJECTS), baseline);
        }
    }

    #[test]
    fn native_surface_paints_emoji_in_color() {
        unsafe {
            let mut surface = Surface::default();
            surface.prepare(120, 40, 24).unwrap();
            let text: Vec<u16> = "😆😆".encode_utf16().collect();
            let rect = RECT { right: 120, bottom: 40, ..Default::default() };
            assert!(surface.draw_links(&text, rect, rect, rect, 24, false, 1, false));
            let _ = GdiFlush();
            // The face is yellow; ClearType fringes on monochrome text are not.
            let yellow = (0..40).flat_map(|y| (0..120).map(move |x| (x, y)))
                .map(|(x, y)| surface.pixel(x, y))
                .filter(|[b, g, r, _]| *r > 180 && *g > 120 && *b < 90)
                .count();
            assert!(yellow > 60, "emoji painted without color ({yellow} yellow pixels)");
        }
    }

    #[test]
    fn native_surface_repeated_resize_and_destruction_do_not_leak_gdi_objects() {
        unsafe {
            // Warm up actual Chinese drawing before taking the baseline.
            {
                let mut warm = Surface::default();
                warm.prepare(20, 20, 14).unwrap();
                let mut rect = RECT {
                    right: 20,
                    bottom: 20,
                    ..Default::default()
                };
                warm.draw_text(&[0x56fa, 0x5b9a], &mut rect, DT_NOPREFIX);
            }
            let _ = GdiFlush();
            let before = GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS);
            for _ in 0..100 {
                let mut surface = Surface::default();
                for size in 1..10 {
                    surface.prepare(20 * size, 15 * size, 14 + size).unwrap();
                    let mut rect = RECT {
                        right: 20 * size,
                        bottom: 15 * size,
                        ..Default::default()
                    };
                    surface.draw_text(&[0x56fa, 0x5b9a], &mut rect, DT_NOPREFIX);
                }
            }
            let _ = GdiFlush();
            assert_eq!(GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS), before);
        }
    }
}
