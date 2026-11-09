//! Aspect-preserving resize.
//!
//! `resize` fits the image *within* the box described by
//! [`ResizeOptions`] while preserving aspect ratio — upscaling is
//! allowed (this is `resize`, not `thumbnail`).

use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;
use crate::crates::image_engine::error::{ImageError, ImageResult};

use super::load::{decode, encode};

/// Bounding box for [`resize`]. Either field may be `None` (unbounded
/// on that axis); both `None` is an [`ImageError::InvalidInput`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResizeOptions {
    /// Maximum output width in pixels.
    pub max_width: Option<u32>,
    /// Maximum output height in pixels.
    pub max_height: Option<u32>,
}

/// Resize `input` to fit within `options`, preserving aspect ratio.
/// Returns the encoded bytes in the input format.
pub fn resize(
    input: &[u8],
    options: ResizeOptions,
    limits: &EngineLimits,
) -> ImageResult<Vec<u8>> {
    if options.max_width.is_none() && options.max_height.is_none() {
        return Err(ImageError::InvalidInput(
            "at least one of max_width or max_height must be given".to_string(),
        ));
    }
    if options.max_width == Some(0) || options.max_height == Some(0) {
        return Err(ImageError::InvalidInput(
            "max_width and max_height must be positive".to_string(),
        ));
    }

    let decoded = decode(input, limits)?;
    let (w, h) = fit_within(
        decoded.image.width(),
        decoded.image.height(),
        options.max_width.unwrap_or(u32::MAX),
        options.max_height.unwrap_or(u32::MAX),
    );

    let resized = decoded
        .image
        .resize_exact(w, h, image::imageops::FilterType::Lanczos3);
    encode(&resized, decoded.format)
}

/// Convenience wrapper: `resize` with explicit optional bounds.
pub fn resize_to(
    input: &[u8],
    width: Option<u32>,
    height: Option<u32>,
    limits: &EngineLimits,
) -> ImageResult<Vec<u8>> {
    resize(
        input,
        ResizeOptions {
            max_width: width,
            max_height: height,
        },
        limits,
    )
}

/// Returns `(w, h)` such that `w/h ≈ src_w/src_h` and `w <= max_w`,
/// `h <= max_h`, maximized. All math in u64; results clamped to
/// `1..=u32::MAX`.
fn fit_within(src_w: u32, src_h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    let (sw, sh) = (u64::from(src_w), u64::from(src_h));
    let (mw, mh) = (u64::from(max_w), u64::from(max_h));

    // Width-bound fits iff mw*sh <= mh*sw (cross-multiplied to stay
    // in integer math).
    let (w, h) = if mw * sh <= mh * sw {
        // Width is the tighter constraint.
        (mw, sh * mw / sw)
    } else {
        // Height is the tighter constraint.
        (sw * mh / sh, mh)
    };

    (
        w.clamp(1, u64::from(u32::MAX)) as u32,
        h.clamp(1, u64::from(u32::MAX)) as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::fit_within;

    #[test]
    fn fits_landscape_into_square() {
        // 200x100 into 50x50 box → 50x25
        assert_eq!(fit_within(200, 100, 50, 50), (50, 25));
    }

    #[test]
    fn fits_portrait_into_square() {
        // 100x200 into 50x50 → 25x50
        assert_eq!(fit_within(100, 200, 50, 50), (25, 50));
    }

    #[test]
    fn unbounded_axis_uses_other() {
        // 200x100, only width bound 40 → 40x20
        assert_eq!(fit_within(200, 100, 40, u32::MAX), (40, 20));
        // only height bound 30 → 60x30
        assert_eq!(fit_within(200, 100, u32::MAX, 30), (60, 30));
    }

    #[test]
    fn never_returns_zero() {
        // Extreme downscale still yields ≥1px.
        let (w, h) = fit_within(10_000, 10_000, 1, 1);
        assert_eq!((w, h), (1, 1));
    }
}
