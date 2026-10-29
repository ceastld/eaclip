//! Silent, bounded video decoding for the native hover popup.
//!
//! Create, read and drop `Decoder` on the same dedicated background thread. The
//! caller owns playback pacing and keeps only the latest returned frame.
use crate::windows_api::{
    core::{Interface, PCWSTR, PROPVARIANT},
    Win32::{
        Media::MediaFoundation::*,
        System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED},
    },
};
use std::{marker::PhantomData, os::windows::ffi::OsStrExt, path::Path, rc::Rc, time::Duration};

const VIDEO_STREAM: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
const ALL_STREAMS: u32 = MF_SOURCE_READER_ALL_STREAMS.0 as u32;
const SOURCE: u32 = MF_SOURCE_READER_MEDIASOURCE.0 as u32;
const MAX_WIDTH: u32 = 1600;
const MAX_HEIGHT: u32 = 1200;
const MAX_NATIVE_EDGE: u32 = 16384;
const MAX_NATIVE_PIXELS: u64 = 64 * 1024 * 1024;

#[derive(Debug)]
pub(super) struct Frame {
    /// Opaque BGRA, contiguous rows in display order.
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub timestamp: Duration,
}

#[derive(Debug)]
pub(super) struct VideoInfo {
    pub width: u32,
    pub height: u32,
    pub duration: Option<Duration>,
}

struct Runtime(PhantomData<Rc<()>>);

impl Runtime {
    fn start() -> Result<Self, String> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED)
                .ok()
                .map_err(|e| format!("Video COM initialization failed: {e}"))?;
            if let Err(error) = MFStartup(MF_VERSION, MFSTARTUP_FULL) {
                CoUninitialize();
                return Err(format!("Windows video support is unavailable: {error}"));
            }
        }
        Ok(Self(PhantomData))
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        unsafe {
            let _ = MFShutdown();
            CoUninitialize();
        }
    }
}

pub(super) struct Decoder {
    // Fields drop in declaration order: release every COM interface before MF/COM.
    reader: IMFSourceReader,
    info: VideoInfo,
    stride: i32,
    max_width: u32,
    max_height: u32,
    decoded_width: u32,
    decoded_height: u32,
    rotation: u32,
    ended: bool,
    _runtime: Runtime,
}

impl Decoder {
    pub fn open(path: &Path, max_width: u32, max_height: u32) -> Result<Self, String> {
        if !path.is_absolute() || max_width == 0 || max_height == 0 {
            return Err("Invalid local video preview request".into());
        }
        let metadata = path
            .metadata()
            .map_err(|e| format!("Video is unavailable: {e}"))?;
        if !metadata.is_file() {
            return Err("Video preview requires a local file".into());
        }
        // Opening an online-only file from a hover must never trigger a download.
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & (0x1000 | 0x40000 | 0x400000) != 0 {
            return Err("Video is not available locally".into());
        }
        let max_width = max_width.min(MAX_WIDTH);
        let max_height = max_height.min(MAX_HEIGHT);
        let runtime = Runtime::start()?;
        let mut filename: Vec<u16> = path.as_os_str().encode_wide().collect();
        if filename.contains(&0) {
            return Err("Invalid local video path".into());
        }
        filename.push(0);
        unsafe {
            let mut attributes = None;
            MFCreateAttributes(&mut attributes, 1).map_err(video_error)?;
            let attributes = attributes.ok_or("Video attributes were not created")?;
            // Unlike the legacy thumbnail conversion mode, the advanced processor
            // supports frame resizing as part of the decoding pipeline.
            attributes
                .SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)
                .map_err(video_error)?;
            let reader = MFCreateSourceReaderFromURL(PCWSTR(filename.as_ptr()), &attributes)
                .map_err(video_error)?;
            reader
                .SetStreamSelection(ALL_STREAMS, false)
                .map_err(video_error)?;
            reader
                .SetStreamSelection(VIDEO_STREAM, true)
                .map_err(video_error)?;
            // No audio stream is selected, decoded, or connected to an audio sink.
            let native = reader
                .GetNativeMediaType(VIDEO_STREAM, 0)
                .map_err(video_error)?;
            let (width, height) = media_size(&native)?;
            if width > MAX_NATIVE_EDGE
                || height > MAX_NATIVE_EDGE
                || u64::from(width) * u64::from(height) > MAX_NATIVE_PIXELS
            {
                return Err("Video dimensions exceed the preview limit".into());
            }
            let rotation = native.GetUINT32(&MF_MT_VIDEO_ROTATION).unwrap_or(0);
            if !matches!(rotation, 0 | 90 | 180 | 270) {
                return Err("Video has an unsupported rotation".into());
            }
            let aspect = native
                .GetUINT64(&MF_MT_PIXEL_ASPECT_RATIO)
                .unwrap_or((1_u64 << 32) | 1);
            let (decode_max_width, decode_max_height) = if matches!(rotation, 90 | 270) {
                (max_height, max_width)
            } else {
                (max_width, max_height)
            };
            let (width, height) =
                fit_size(width, height, aspect, decode_max_width, decode_max_height);
            let output = MFCreateMediaType().map_err(video_error)?;
            output
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(video_error)?;
            output
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)
                .map_err(video_error)?;
            output
                .SetUINT64(
                    &MF_MT_FRAME_SIZE,
                    (u64::from(width) << 32) | u64::from(height),
                )
                .map_err(video_error)?;
            output
                .SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1_u64 << 32) | 1)
                .map_err(video_error)?;
            // Preserve the source orientation through conversion; normalize it
            // once on our small CPU bitmap, avoiding processor letterboxing.
            output
                .SetUINT32(&MF_MT_VIDEO_ROTATION, rotation)
                .map_err(video_error)?;
            reader
                .SetCurrentMediaType(VIDEO_STREAM, None, &output)
                .map_err(video_error)?;
            disable_processor_rotation(&reader)?;
            let duration = reader
                .GetPresentationAttribute(SOURCE, &MF_PD_DURATION)
                .ok()
                .and_then(|value| u64::try_from(&value).ok())
                .map(|ticks| Duration::from_nanos(ticks.saturating_mul(100)));
            let mut decoder = Self {
                reader,
                info: VideoInfo {
                    width,
                    height,
                    duration,
                },
                stride: 0,
                max_width,
                max_height,
                decoded_width: width,
                decoded_height: height,
                rotation,
                ended: false,
                _runtime: runtime,
            };
            decoder.refresh_format()?;
            Ok(decoder)
        }
    }

    pub fn info(&self) -> &VideoInfo {
        &self.info
    }

    pub fn rewind(&mut self) -> Result<(), String> {
        unsafe {
            self.reader
                .SetCurrentPosition(
                    &crate::windows_api::core::GUID::zeroed(),
                    &PROPVARIANT::from(0_i64),
                )
                .map_err(video_error)?;
        }
        self.ended = false;
        Ok(())
    }

    pub fn read_frame(&mut self) -> Result<Option<Frame>, String> {
        if self.ended {
            return Ok(None);
        }
        // Stream ticks and format notices can have no sample. Bound this loop so
        // corrupt files cannot make the worker spin indefinitely without yielding.
        for _ in 0..64 {
            let mut flags = 0;
            let mut timestamp = 0;
            let mut sample = None;
            unsafe {
                self.reader
                    .ReadSample(
                        VIDEO_STREAM,
                        0,
                        None,
                        Some(&mut flags),
                        Some(&mut timestamp),
                        Some(&mut sample),
                    )
                    .map_err(video_error)?;
            }
            if flags & MF_SOURCE_READERF_ERROR.0 as u32 != 0 {
                return Err("Windows could not decode this video".into());
            }
            if flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32 != 0 {
                self.refresh_format()?;
            }
            self.ended = flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0;
            if let Some(sample) = sample {
                let buffer = unsafe { sample.ConvertToContiguousBuffer() }.map_err(video_error)?;
                let pixels = copy_pixels(
                    &buffer,
                    self.decoded_width,
                    self.decoded_height,
                    self.stride,
                )?;
                let pixels = rotate_pixels(
                    pixels,
                    self.decoded_width,
                    self.decoded_height,
                    self.rotation,
                );
                return Ok(Some(Frame {
                    pixels,
                    width: self.info.width,
                    height: self.info.height,
                    timestamp: Duration::from_nanos((timestamp.max(0) as u64).saturating_mul(100)),
                }));
            }
            if self.ended {
                return Ok(None);
            }
        }
        Err("Video did not produce a frame".into())
    }

    fn refresh_format(&mut self) -> Result<(), String> {
        unsafe {
            let format = self
                .reader
                .GetCurrentMediaType(VIDEO_STREAM)
                .map_err(video_error)?;
            if format.GetGUID(&MF_MT_SUBTYPE).map_err(video_error)? != MFVideoFormat_RGB32 {
                return Err("Video decoder returned an unsupported pixel format".into());
            }
            let (width, height) = media_size(&format)?;
            let (display_width, display_height) = if matches!(self.rotation, 90 | 270) {
                (height, width)
            } else {
                (width, height)
            };
            if display_width > self.max_width || display_height > self.max_height {
                return Err("Video decoder exceeded the preview dimensions".into());
            }
            self.stride = match format.GetUINT32(&MF_MT_DEFAULT_STRIDE) {
                Ok(stride) => stride as i32,
                Err(_) => MFGetStrideForBitmapInfoHeader(MFVideoFormat_RGB32.data1, width)
                    .map_err(video_error)?,
            };
            self.decoded_width = width;
            self.decoded_height = height;
            self.info.width = display_width;
            self.info.height = display_height;
            Ok(())
        }
    }
}

unsafe fn disable_processor_rotation(reader: &IMFSourceReader) -> Result<(), String> {
    if let Ok(extended) = reader.cast::<IMFSourceReaderEx>() {
        for index in 0..16 {
            let mut transform = None;
            if extended
                .GetTransformForStream(VIDEO_STREAM, index, None, &mut transform)
                .is_err()
            {
                break;
            }
            if let Some(transform) = transform {
                if let Ok(control) = transform.cast::<IMFVideoProcessorControl>() {
                    control.SetRotation(ROTATION_NONE).map_err(video_error)?;
                }
            }
        }
    }
    Ok(())
}

/// MF describes how the stored pixels were rotated counter-clockwise; apply
/// the inverse rotation to recover their intended display orientation.
fn rotate_pixels(mut pixels: Vec<u8>, width: u32, height: u32, rotation: u32) -> Vec<u8> {
    if rotation == 0 {
        return pixels;
    }
    if rotation == 180 {
        let count = pixels.len() / 4;
        for index in 0..count / 2 {
            for channel in 0..4 {
                pixels.swap(index * 4 + channel, (count - 1 - index) * 4 + channel);
            }
        }
        return pixels;
    }
    let mut rotated = vec![0; pixels.len()];
    for y in 0..height as usize {
        for x in 0..width as usize {
            let (destination_x, destination_y) = if rotation == 90 {
                (height as usize - 1 - y, x)
            } else {
                (y, width as usize - 1 - x)
            };
            let source = (y * width as usize + x) * 4;
            let destination = (destination_y * height as usize + destination_x) * 4;
            rotated[destination..destination + 4].copy_from_slice(&pixels[source..source + 4]);
        }
    }
    rotated
}

fn video_error(error: crate::windows_api::core::Error) -> String {
    format!("Windows could not preview this video: {error}")
}

unsafe fn media_size(media: &IMFMediaType) -> Result<(u32, u32), String> {
    let packed = media.GetUINT64(&MF_MT_FRAME_SIZE).map_err(video_error)?;
    let (width, height) = ((packed >> 32) as u32, packed as u32);
    if width == 0 || height == 0 {
        return Err("Video has invalid dimensions".into());
    }
    Ok((width, height))
}

fn fit_size(width: u32, height: u32, aspect: u64, max_width: u32, max_height: u32) -> (u32, u32) {
    let numerator = (aspect >> 32).max(1) as u128;
    let denominator = (aspect as u32).max(1) as u128;
    let display_width = u128::from(width) * numerator;
    let display_height = u128::from(height) * denominator;
    let mut fitted_width = width.min(max_width).max(1);
    let mut fitted_height = (u128::from(fitted_width) * display_height / display_width).max(1);
    if fitted_height > u128::from(max_height) {
        fitted_height = u128::from(max_height);
        fitted_width = (fitted_height * display_width / display_height)
            .max(1)
            .min(u128::from(max_width)) as u32;
    }
    (fitted_width, fitted_height as u32)
}

fn copy_pixels(
    buffer: &IMFMediaBuffer,
    width: u32,
    height: u32,
    stride: i32,
) -> Result<Vec<u8>, String> {
    unsafe {
        if let Ok(surface) = buffer.cast::<IMF2DBuffer2>() {
            let mut scanline = std::ptr::null_mut();
            let mut pitch = 0;
            let mut start = std::ptr::null_mut();
            let mut length = 0;
            if surface
                .Lock2DSize(
                    MF2DBuffer_LockFlags_Read,
                    &mut scanline,
                    &mut pitch,
                    &mut start,
                    &mut length,
                )
                .is_ok()
            {
                let result = copy_rows(start, length as usize, scanline, pitch, width, height);
                let unlocked = surface.Unlock2D().map_err(video_error);
                return result.and_then(|pixels| unlocked.map(|_| pixels));
            }
        }
        // IMFMediaBuffer::Lock provides the contiguous representation. The media
        // type's default stride, unlike a surface pitch, describes exactly this.
        let mut start = std::ptr::null_mut();
        let mut length = 0;
        buffer
            .Lock(&mut start, None, Some(&mut length))
            .map_err(video_error)?;
        let row_offset = if stride < 0 {
            u64::from(stride.unsigned_abs()) * u64::from(height - 1)
        } else {
            0
        };
        let result = if row_offset >= u64::from(length) {
            Err("Video frame is truncated".into())
        } else {
            copy_rows(
                start,
                length as usize,
                start.wrapping_add(row_offset as usize),
                stride,
                width,
                height,
            )
        };
        let unlocked = buffer.Unlock().map_err(video_error);
        result.and_then(|pixels| unlocked.map(|_| pixels))
    }
}

/// Validate all row addresses against the locked buffer before copying bytes.
unsafe fn copy_rows(
    start: *const u8,
    length: usize,
    scanline: *const u8,
    stride: i32,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, String> {
    let row_bytes = width as usize * 4;
    if start.is_null()
        || scanline.is_null()
        || width == 0
        || height == 0
        || width > MAX_WIDTH.max(MAX_HEIGHT)
        || height > MAX_WIDTH.max(MAX_HEIGHT)
        || u64::from(width) * u64::from(height) > u64::from(MAX_WIDTH) * u64::from(MAX_HEIGHT)
        || (stride.unsigned_abs() as usize) < row_bytes
    {
        return Err("Video frame has an invalid layout".into());
    }
    let lower = start as usize;
    let upper = lower
        .checked_add(length)
        .ok_or("Invalid video buffer extent")?;
    let first = scanline as usize;
    let last = i128::from(first as u64) + i128::from(stride) * i128::from(height - 1);
    if first < lower
        || first.checked_add(row_bytes).is_none_or(|end| end > upper)
        || last < lower as i128
        || last + row_bytes as i128 > upper as i128
    {
        return Err("Video frame is truncated".into());
    }
    let mut pixels = vec![0; row_bytes * height as usize];
    for row in 0..height as usize {
        let source = scanline.offset(row as isize * stride as isize);
        std::ptr::copy_nonoverlapping(source, pixels.as_mut_ptr().add(row * row_bytes), row_bytes);
    }
    // RGB32's fourth byte is padding, not a trustworthy alpha value.
    for pixel in pixels.chunks_exact_mut(4) {
        pixel[3] = 255;
    }
    Ok(pixels)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_size_preserves_aspect_and_bounds() {
        assert_eq!(
            fit_size(3840, 2160, (1_u64 << 32) | 1, 1600, 1200),
            (1600, 900)
        );
        assert_eq!(
            fit_size(1080, 1920, (1_u64 << 32) | 1, 1600, 1200),
            (675, 1200)
        );
        assert_eq!(
            fit_size(720, 576, (16_u64 << 32) | 15, 1600, 1200),
            (720, 540)
        );
    }

    #[test]
    fn frame_rows_honor_negative_stride_padding_and_opaque_alpha() {
        let source = [30, 20, 10, 0, 0, 0, 0, 0, 60, 50, 40, 0, 0, 0, 0, 0];
        let pixels = unsafe {
            copy_rows(
                source.as_ptr(),
                source.len(),
                source.as_ptr().add(8),
                -8,
                1,
                2,
            )
        }
        .unwrap();
        assert_eq!(pixels, [60, 50, 40, 255, 30, 20, 10, 255]);
        assert!(unsafe { copy_rows(source.as_ptr(), 8, source.as_ptr(), 8, 1, 2) }.is_err());
    }

    #[test]
    fn corrects_mf_counterclockwise_metadata_for_each_orientation() {
        let source: Vec<u8> = (1..=6).flat_map(|value| [value, 0, 0, 255]).collect();
        let values = |rotation| {
            rotate_pixels(source.clone(), 3, 2, rotation)
                .chunks_exact(4)
                .map(|pixel| pixel[0])
                .collect::<Vec<_>>()
        };
        assert_eq!(values(90), [4, 1, 5, 2, 6, 3]);
        assert_eq!(values(180), [6, 5, 4, 3, 2, 1]);
        assert_eq!(values(270), [3, 6, 2, 5, 1, 4]);
    }
}
