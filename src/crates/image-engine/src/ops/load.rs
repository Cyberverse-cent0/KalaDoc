//! Input decoding, format sniffing, and output encoding.
//!
//! # Limit enforcement
//!
//! [`EngineLimits`](crate::crates::core::engine::EngineLimits) are enforced here:
//!
//! 1. **Input size** — [`check_input_limits`] rejects buffers over
//!    `max_input_bytes` before any parsing.
//! 2. **Pixel count** — [`pixel_dimensions`] reads width × height from
//!    the image header (no pixel decode) and rejects totals over
//!    `max_pixels`. [`decode`] runs this probe *before* decoding so
//!    decompression bombs are rejected before allocation.
//!
//! `max_pages`/`max_outputs` have no meaning for raster images and are
//! ignored.

use std::io::Cursor;

use image::{ColorType, DynamicImage, ImageFormat as ImageCodecFormat, ImageReader};

use crate::crates::core::engine::EngineLimits;
use crate::crates::image_engine::error::{ImageError, ImageResult};

use super::format::ImageFormat;

/// Rejects inputs over `max_input_bytes`.
pub(crate) fn check_input_limits(len: usize, limits: &EngineLimits) -> ImageResult<()> {
    if let Some(max) = limits.max_input_bytes {
        if len as u64 > max {
            return Err(ImageError::LimitExceeded(format!(
                "input is {len} bytes, exceeding max_input_bytes {max}"
            )));
        }
    }
    Ok(())
}

/// Builds an [`ImageReader`] over `bytes` with the format guessed from
/// magic bytes, returning it together with the sniffed codec format.
///
/// Fails with [`ImageError::UnsupportedFormat`] when no format can be
/// sniffed or the codec is not compiled into this engine.
fn open(bytes: &[u8]) -> ImageResult<(ImageReader<Cursor<&[u8]>>, ImageCodecFormat)> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(image::ImageError::IoError)
        .map_err(ImageError::Decode)?;
    let sniffed = reader
        .format()
        .ok_or_else(|| ImageError::UnsupportedFormat("unrecognized image format".to_string()))?;
    if ImageFormat::from_image_format(sniffed).is_none() {
        return Err(ImageError::UnsupportedFormat(format!(
            "{} codec",
            sniffed.to_mime_type()
        )));
    }
    reader.set_format(sniffed);
    Ok((reader, sniffed))
}

/// Reads `(width, height)` from the image header without decoding
/// pixels. Enforces `max_input_bytes` and `max_pixels`.
pub(crate) fn pixel_dimensions(
    bytes: &[u8],
    limits: &EngineLimits,
) -> ImageResult<(ImageFormat, u32, u32)> {
    check_input_limits(bytes.len(), limits)?;
    let (reader, sniffed) = open(bytes)?;
    let (width, height) = reader.into_dimensions().map_err(ImageError::Decode)?;
    if let Some(max) = limits.max_pixels {
        let pixels = u64::from(width) * u64::from(height);
        if pixels > max {
            return Err(ImageError::LimitExceeded(format!(
                "image is {width}x{height} = {pixels} pixels, exceeding max_pixels {max}"
            )));
        }
    }
    Ok((
        ImageFormat::from_image_format(sniffed).expect("sniffed format was validated as supported"),
        width,
        height,
    ))
}

/// A fully decoded image plus its sniffed format.
pub(crate) struct Decoded {
    pub format: ImageFormat,
    pub image: DynamicImage,
}

/// Decodes `bytes` into a [`DynamicImage`], enforcing all input limits
/// first (size, then pixel count from the header before decoding).
pub(crate) fn decode(bytes: &[u8], limits: &EngineLimits) -> ImageResult<Decoded> {
    // Header probe first: rejects oversized pixel counts before the
    // decoder allocates anything.
    let (format, _, _) = pixel_dimensions(bytes, limits)?;

    let (reader, _) = open(bytes)?;
    let image = reader.decode().map_err(ImageError::Decode)?;
    Ok(Decoded { format, image })
}

/// Normalizes any decoded [`DynamicImage`] to 8-bit sRGB pixels:
/// `Rgb8`/`Rgba8` pass through untouched; every other variant gains an
/// RGB(A) type, preserving alpha for alpha-carrying formats. Used by
/// the color pipeline ops (`color`, `effects`, `cmyk`) so adjustments
/// always run on the same pixel representation.
pub(crate) fn normalize_8bit(image: DynamicImage) -> DynamicImage {
    match image {
        DynamicImage::ImageRgb8(buf) => DynamicImage::ImageRgb8(buf),
        DynamicImage::ImageRgba8(buf) => DynamicImage::ImageRgba8(buf),
        other => {
            let has_alpha = matches!(
                other.color(),
                ColorType::La8 | ColorType::La16 | ColorType::Rgba16 | ColorType::Rgba32F
            );
            if has_alpha {
                DynamicImage::ImageRgba8(other.to_rgba8())
            } else {
                DynamicImage::ImageRgb8(other.to_rgb8())
            }
        }
    }
}

/// Encodes `image` as `format`, returning the serialized bytes.
pub(crate) fn encode(image: &DynamicImage, format: ImageFormat) -> ImageResult<Vec<u8>> {
    let mut buf = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut buf), format.to_image_format())
        .map_err(ImageError::Encode)?;
    Ok(buf)
}
