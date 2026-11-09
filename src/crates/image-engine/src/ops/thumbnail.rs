//! Thumbnail generation — a fast, aspect-preserving downscale.
//!
//! Unlike [`resize`](super::resize), [`thumbnail`] **never upscales**:
//! an image that already fits within the requested box is re-encoded
//! at its original size. This matches the usual thumbnail semantic —
//! the operation exists to make images smaller, not bigger.
//!
//! The actual resampling uses [`DynamicImage::thumbnail`], which takes
//! a fast sampling path (area-average for downscales) rather than the
//! high-quality Lanczos filter used by `resize` — thumbnails are
//! preview-sized, so speed wins over fidelity.

use crate::crates::core::engine::EngineLimits;
use crate::crates::image_engine::error::{ImageError, ImageResult};

use super::load::{decode, encode};

/// Generate a thumbnail of `input`, fitting within `max_width` ×
/// `max_height` while preserving aspect ratio.
///
/// Returns the encoded bytes in the input format. The image is never
/// enlarged: if it already fits inside the box, the original pixels
/// are re-encoded unchanged. Both bounds must be non-zero.
///
/// All [`EngineLimits`] are enforced by [`decode`] on the input;
/// because the output is never larger than the input, `max_pixels`
/// cannot be exceeded by the result.
pub fn thumbnail(
    input: &[u8],
    max_width: u32,
    max_height: u32,
    limits: &EngineLimits,
) -> ImageResult<Vec<u8>> {
    if max_width == 0 || max_height == 0 {
        return Err(ImageError::InvalidInput(
            "max_width and max_height must be positive".to_string(),
        ));
    }

    let decoded = decode(input, limits)?;
    let (src_w, src_h) = (decoded.image.width(), decoded.image.height());
    let (w, h) = fit_within(src_w, src_h, max_width, max_height);

    // fit_within maximizes within the box, so a result larger than the
    // source on either axis means the box permits upscaling — skip the
    // resample entirely and encode the original pixels.
    let out = if w >= src_w && h >= src_h {
        decoded.image
    } else {
        decoded.image.thumbnail(w, h)
    };
    encode(&out, decoded.format)
}

/// Returns `(w, h)` such that `w/h ≈ src_w/src_h`, `w <= max_w`,
/// `h <= max_h`, maximized — the same integer math as
/// `ops::resize::fit_within` (kept private there, replicated here).
/// All math in u64; results clamped to `1..=u32::MAX`.
fn fit_within(src_w: u32, src_h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    let (sw, sh) = (u64::from(src_w), u64::from(src_h));
    let (mw, mh) = (u64::from(max_w), u64::from(max_h));

    let (w, h) = if mw * sh <= mh * sw {
        (mw, sh * mw / sw)
    } else {
        (sw * mh / sh, mh)
    };

    (
        w.clamp(1, u64::from(u32::MAX)) as u32,
        h.clamp(1, u64::from(u32::MAX)) as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::core::engine::EngineLimits;
    use crate::crates::image_engine::ops::load::pixel_dimensions;

    /// Encodes a solid-color `w`×`h` PNG for tests.
    fn test_png(w: u32, h: u32) -> Vec<u8> {
        let img = image::DynamicImage::new_rgb8(w, h);
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::write_to(&img, &mut buf, image::ImageFormat::Png).unwrap();
        buf.into_inner()
    }

    #[test]
    fn downscales_preserving_aspect() {
        // 200x100 into a 50x50 box → 50x25.
        let input = test_png(200, 100);
        let out = thumbnail(&input, 50, 50, &EngineLimits::default()).unwrap();
        let (_, w, h) = pixel_dimensions(&out, &EngineLimits::default()).unwrap();
        assert_eq!((w, h), (50, 25));
    }

    #[test]
    fn portrait_image_fits_height_bound() {
        // 100x200 into 50x50 → 25x50.
        let input = test_png(100, 200);
        let out = thumbnail(&input, 50, 50, &EngineLimits::default()).unwrap();
        let (_, w, h) = pixel_dimensions(&out, &EngineLimits::default()).unwrap();
        assert_eq!((w, h), (25, 50));
    }

    #[test]
    fn small_image_is_not_upscaled() {
        // 40x20 requested as 100x100 → stays 40x20.
        let input = test_png(40, 20);
        let out = thumbnail(&input, 100, 100, &EngineLimits::default()).unwrap();
        let (_, w, h) = pixel_dimensions(&out, &EngineLimits::default()).unwrap();
        assert_eq!((w, h), (40, 20));
    }

    #[test]
    fn rejects_zero_dimensions() {
        let input = test_png(200, 100);
        assert!(matches!(
            thumbnail(&input, 0, 50, &EngineLimits::default()),
            Err(ImageError::InvalidInput(_))
        ));
        assert!(matches!(
            thumbnail(&input, 50, 0, &EngineLimits::default()),
            Err(ImageError::InvalidInput(_))
        ));
    }

    #[test]
    fn enforces_input_limits() {
        // max_pixels of 1 rejects the 200x100 input before decoding.
        let input = test_png(200, 100);
        let limits = EngineLimits {
            max_pixels: Some(1),
            ..EngineLimits::default()
        };
        assert!(matches!(
            thumbnail(&input, 50, 50, &limits),
            Err(ImageError::LimitExceeded(_))
        ));
    }
}
