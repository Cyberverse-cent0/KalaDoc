//! Metadata extraction.
//!
//! `metadata` reports descriptive properties of an image — format,
//! dimensions, color type, alpha presence — without attempting to
//! transcode or transform it. It fully decodes like
//! [`inspect`](super::info::inspect) because decoder color type is only
//! known after decode; the distinction from `inspect` is semantic:
//! `metadata` is the stable, minimal contract for callers that only
//! need descriptive fields, while `inspect` may grow richer diagnostics.
//!
//! Container-level metadata (EXIF, XMP, ICC profiles, PNG text chunks)
//! is not yet extracted — the `image` crate does not expose it through
//! its safe API. When support lands it will be added here as optional
//! fields rather than changing existing ones.

use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;
use crate::crates::image_engine::error::ImageResult;

use super::load::decode;

/// Descriptive metadata for an image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageMetadata {
    /// Format identifier (`"png"`, `"jpeg"`, ...).
    pub format: String,
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// `width * height`.
    pub pixel_count: u64,
    /// Decoder color type (e.g. `"Rgba8"`), when known.
    pub color_type: Option<String>,
    /// Whether the decoded image carries an alpha channel.
    pub has_alpha: bool,
    /// Size of the input buffer in bytes.
    pub file_size_bytes: u64,
}

/// Extract [`ImageMetadata`] for `input`.
///
/// Returns `Err` when the format cannot be sniffed, the codec is not
/// supported, the data is corrupt, or a limit is exceeded.
pub fn metadata(input: &[u8], limits: &EngineLimits) -> ImageResult<ImageMetadata> {
    let decoded = decode(input, limits)?;
    let (width, height) = (decoded.image.width(), decoded.image.height());
    Ok(ImageMetadata {
        format: decoded.format.as_str().to_string(),
        width,
        height,
        pixel_count: u64::from(width) * u64::from(height),
        color_type: Some(format!("{:?}", decoded.image.color())),
        has_alpha: decoded.image.has_alpha(),
        file_size_bytes: input.len() as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, ImageBuffer, Rgba};

    #[test]
    fn metadata_reports_png_fixture() {
        let img: DynamicImage = ImageBuffer::from_pixel(2, 2, Rgba([1u8, 2, 3, 255])).into();
        let mut buf = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        let meta = metadata(&buf, &EngineLimits::default()).unwrap();
        assert_eq!(meta.format, "png");
        assert_eq!((meta.width, meta.height), (2, 2));
        assert_eq!(meta.pixel_count, 4);
        assert_eq!(meta.file_size_bytes, buf.len() as u64);
    }

    #[test]
    fn metadata_rejects_garbage() {
        assert!(metadata(b"\x00\x01\x02", &EngineLimits::default()).is_err());
    }
}
