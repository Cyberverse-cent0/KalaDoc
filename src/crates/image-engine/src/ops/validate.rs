//! Input validation.
//!
//! `validate` is the cheap pre-flight check callers run before queueing
//! an image for processing: it sniffs the format, probes the header for
//! dimensions, and enforces `max_input_bytes`/`max_pixels` — all without
//! decoding pixel data, so it is safe to run on untrusted uploads.
//!
//! A successful return means the header is well-formed and within
//! limits; it does *not* guarantee the pixel data is intact — use
//! [`inspect`](super::info::inspect) or a real op for that.

use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;
use crate::crates::image_engine::error::ImageResult;

use super::load::{check_input_limits, pixel_dimensions};

/// Result of [`validate`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationReport {
    /// `true` when the header parsed and limits passed.
    pub valid: bool,
    /// Format identifier (`"png"`, `"jpeg"`, ...).
    pub format: String,
    /// Image width in pixels, from the header.
    pub width: u32,
    /// Image height in pixels, from the header.
    pub height: u32,
    /// `width * height`.
    pub pixel_count: u64,
    /// Size of the input buffer in bytes.
    pub file_size_bytes: u64,
}

/// Validate `input` without decoding pixels.
///
/// Returns `Err` with [`ImageError::UnsupportedFormat`] for unrecognized
/// or unsupported codecs, [`ImageError::Decode`] for malformed headers,
/// and [`ImageError::LimitExceeded`] when `max_input_bytes` or
/// `max_pixels` is exceeded.
pub fn validate(input: &[u8], limits: &EngineLimits) -> ImageResult<ValidationReport> {
    check_input_limits(input.len(), limits)?;
    let (format, width, height) = pixel_dimensions(input, limits)?;
    Ok(ValidationReport {
        valid: true,
        format: format.as_str().to_string(),
        width,
        height,
        pixel_count: u64::from(width) * u64::from(height),
        file_size_bytes: input.len() as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::image_engine::ops::format::ImageFormat;
    use image::{DynamicImage, ImageBuffer, Rgba};

    fn png_fixture() -> Vec<u8> {
        let img: DynamicImage = ImageBuffer::from_pixel(4, 3, Rgba([10u8, 20, 30, 255])).into();
        let mut buf = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        buf
    }

    #[test]
    fn validate_accepts_png() {
        let report = validate(&png_fixture(), &EngineLimits::default()).unwrap();
        assert!(report.valid);
        assert_eq!(report.format, "png");
        assert_eq!((report.width, report.height), (4, 3));
        assert_eq!(report.pixel_count, 12);
        assert_eq!(report.file_size_bytes, report.file_size_bytes.max(1));
    }

    #[test]
    fn validate_rejects_garbage() {
        assert!(validate(b"not an image", &EngineLimits::default()).is_err());
        assert!(validate(&[], &EngineLimits::default()).is_err());
    }

    #[test]
    fn validate_enforces_input_size_limit() {
        let mut limits = EngineLimits::default();
        limits.max_input_bytes = Some(4);
        assert!(validate(&png_fixture(), &limits).is_err());
    }

    #[test]
    fn validate_enforces_pixel_limit() {
        let mut limits = EngineLimits::default();
        limits.max_pixels = Some(8); // fixture is 12 pixels
        assert!(validate(&png_fixture(), &limits).is_err());
    }

    #[test]
    fn formats_round_trip_through_as_str() {
        for f in ImageFormat::all() {
            assert!(!f.as_str().is_empty());
        }
    }
}
