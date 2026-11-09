//! Image inspection.
//!
//! `inspect` fully decodes the input — header-only probing would miss
//! truncated or corrupt pixel data, and the [`ImageInfo`] report is
//! cheap enough that decode cost is acceptable for the sizes the
//! `max_input_bytes`/`max_pixels` limits allow.

use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;
use crate::crates::image_engine::error::ImageResult;

use super::load::decode;

/// Result of [`inspect`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageInfo {
    /// Format identifier (`"png"`, `"jpeg"`, ...).
    pub format: String,
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// `width * height`.
    pub pixel_count: u64,
    /// Size of the input buffer in bytes.
    pub file_size_bytes: u64,
    /// Decoder color type (e.g. `"Rgb8"`), when known.
    pub color_type: Option<String>,
    /// Whether the decoded image carries an alpha channel.
    pub has_alpha: bool,
}

/// Inspect `input`, returning an [`ImageInfo`] report.
///
/// Returns `Err` when the format cannot be sniffed, the codec is not
/// supported, the data is corrupt, or a limit is exceeded.
pub fn inspect(input: &[u8], limits: &EngineLimits) -> ImageResult<ImageInfo> {
    let decoded = decode(input, limits)?;
    let (width, height) = (decoded.image.width(), decoded.image.height());
    Ok(ImageInfo {
        format: decoded.format.as_str().to_string(),
        width,
        height,
        pixel_count: u64::from(width) * u64::from(height),
        file_size_bytes: input.len() as u64,
        color_type: Some(format!("{:?}", decoded.image.color())),
        has_alpha: decoded.image.has_alpha(),
    })
}
