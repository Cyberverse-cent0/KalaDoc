//! Image format identifiers shared by the engine and its ops.
//!
//! [`ImageFormat`] is the crate's own closed enum — it deliberately
//! does not expose `image::ImageFormat` so the supported set is
//! exactly what this crate advertises in
//! [`EngineCapabilities`](crate::crates::core::engine::EngineCapabilities).

use serde::{Deserialize, Serialize};

/// A raster image format this engine can decode and encode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageFormat {
    /// PNG — lossless, alpha-capable.
    Png,
    /// JPEG — lossy, no alpha.
    Jpeg,
    /// GIF — palette-based; only the first frame is handled.
    Gif,
    /// WebP — encoded with codec defaults.
    WebP,
    /// BMP — uncompressed or RLE.
    Bmp,
}

impl ImageFormat {
    /// Stable lowercase identifier (`"png"`, `"jpeg"`, ...).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpeg",
            Self::Gif => "gif",
            Self::WebP => "webp",
            Self::Bmp => "bmp",
        }
    }

    /// Every format the engine supports, for capability reporting.
    pub fn all() -> &'static [ImageFormat] {
        &[Self::Bmp, Self::Gif, Self::Jpeg, Self::Png, Self::WebP]
    }

    /// Format identifiers as owned strings.
    pub fn identifiers() -> Vec<String> {
        Self::all().iter().map(|f| f.as_str().to_string()).collect()
    }

    pub(crate) fn to_image_format(self) -> image::ImageFormat {
        match self {
            Self::Png => image::ImageFormat::Png,
            Self::Jpeg => image::ImageFormat::Jpeg,
            Self::Gif => image::ImageFormat::Gif,
            Self::WebP => image::ImageFormat::WebP,
            Self::Bmp => image::ImageFormat::Bmp,
        }
    }

    /// Maps a sniffed `image` codec format onto the supported set.
    /// `None` means the codec is not compiled in or not supported.
    pub(crate) fn from_image_format(format: image::ImageFormat) -> Option<Self> {
        match format {
            image::ImageFormat::Png => Some(Self::Png),
            image::ImageFormat::Jpeg => Some(Self::Jpeg),
            image::ImageFormat::Gif => Some(Self::Gif),
            image::ImageFormat::WebP => Some(Self::WebP),
            image::ImageFormat::Bmp => Some(Self::Bmp),
            _ => None,
        }
    }
}

/// Parses a caller-supplied format identifier for *output*.
///
/// Accepts surrounding whitespace and mixed case (`" JPG "` works);
/// `"jpg"` is an alias for [`ImageFormat::Jpeg`]. Anything outside the
/// supported set — including real formats this engine does not compile
/// (e.g. `"tiff"`) — is rejected with [`ImageError::InvalidInput`].
pub(crate) fn parse_target(
    s: &str,
) -> crate::crates::image_engine::error::ImageResult<ImageFormat> {
    use crate::crates::image_engine::error::ImageError;
    match s.trim().to_ascii_lowercase().as_str() {
        "png" => Ok(ImageFormat::Png),
        "jpg" | "jpeg" => Ok(ImageFormat::Jpeg),
        "gif" => Ok(ImageFormat::Gif),
        "webp" => Ok(ImageFormat::WebP),
        "bmp" => Ok(ImageFormat::Bmp),
        other => Err(ImageError::InvalidInput(format!(
            "unsupported target image format {other:?}"
        ))),
    }
}
