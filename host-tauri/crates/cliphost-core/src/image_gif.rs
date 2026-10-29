//! Prepare a complete GIF file before touching the Windows clipboard.
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};

pub(crate) fn prepare(source: &[u8], directory: &Path) -> Result<PathBuf, String> {
    let format = image::guess_format(source).map_err(|error| error.to_string())?;
    // Validate even GIF inputs, but preserve their original animation/metadata.
    let decoded = image::load_from_memory_with_format(source, format).map_err(|error| error.to_string())?;
    let bytes = if format == image::ImageFormat::Gif {
        source.to_vec()
    } else {
        let mut output = Cursor::new(Vec::new());
        decoded.write_to(&mut output, image::ImageFormat::Gif).map_err(|error| error.to_string())?;
        output.into_inner()
    };
    std::fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    let path = directory.join(format!("copy-{}.gif", blake3::hash(&bytes).to_hex()));
    if path.exists() {
        return reuse(&path, &bytes);
    }
    let mut staged = tempfile::NamedTempFile::new_in(directory).map_err(|error| error.to_string())?;
    staged.write_all(&bytes).map_err(|error| error.to_string())?;
    staged.flush().map_err(|error| error.to_string())?;
    match staged.persist_noclobber(&path) {
        Ok(_) => Ok(path),
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => reuse(&path, &bytes),
        Err(error) => Err(error.error.to_string()),
    }
}

fn reuse(path: &Path, expected: &[u8]) -> Result<PathBuf, String> {
    if std::fs::read(path).map_err(|error| error.to_string())? == expected {
        Ok(path.to_path_buf())
    } else {
        Err("生成的 GIF 文件已被修改，未覆盖现有文件。".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gif_copy_converts_image_and_reuses_complete_output_without_overwriting() {
        let root = tempfile::tempdir().unwrap();
        let image = image::RgbaImage::from_pixel(3, 2, image::Rgba([255, 0, 0, 255]));
        let mut source = Cursor::new(Vec::new());
        image.write_to(&mut source, image::ImageFormat::Png).unwrap();
        let gif = prepare(source.get_ref(), root.path()).unwrap();
        let decoded = image::open(&gif).unwrap().to_rgba8();
        assert_eq!(decoded, image);
        assert_eq!(gif.extension().unwrap(), "gif");
        assert_eq!(prepare(source.get_ref(), root.path()).unwrap(), gif);
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
        std::fs::write(&gif, b"changed output").unwrap();
        assert!(prepare(source.get_ref(), root.path()).is_err());
        assert_eq!(std::fs::read(&gif).unwrap(), b"changed output");
        assert!(prepare(b"broken image", root.path()).is_err());
    }

    #[test]
    fn gif_copy_preserves_existing_animation_bytes_and_frame_delays() {
        let root = tempfile::tempdir().unwrap();
        let mut source = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut source);
            encoder.set_repeat(image::codecs::gif::Repeat::Infinite).unwrap();
            for color in [[255, 0, 0, 255], [0, 0, 255, 255]] {
                encoder.encode_frame(image::Frame::from_parts(
                    image::RgbaImage::from_pixel(2, 2, image::Rgba(color)), 0, 0,
                    image::Delay::from_numer_denom_ms(100, 1),
                )).unwrap();
            }
        }
        let copied = prepare(&source, root.path()).unwrap();
        assert_eq!(std::fs::read(copied).unwrap(), source);
    }
}
