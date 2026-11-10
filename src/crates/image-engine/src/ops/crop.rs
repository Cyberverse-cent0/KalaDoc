//! Rectangular crop.
//!
//! `crop` extracts the sub-rectangle `(x, y, width, height)` from the
//! decoded image and re-encodes it in the input's sniffed format. No
//! resampling occurs — pixel values are copied verbatim.

use crate::crates::core::engine::EngineLimits;
use crate::crates::image_engine::error::{ImageError, ImageResult};

use super::load::{decode, encode};

/// Crop `input` to the rectangle whose top-left corner is `(x, y)` with
/// size `width` × `height` pixels.
///
/// The rectangle must lie entirely within the image and have non-zero
/// width and height; violations are rejected with
/// [`ImageError::InvalidInput`]. Returns the encoded bytes in the
/// input format.
pub fn crop(
    input: &[u8],
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    limits: &EngineLimits,
) -> ImageResult<Vec<u8>> {
    if width == 0 || height == 0 {
        return Err(ImageError::InvalidInput(format!(
            "crop rectangle must be non-empty, got {width}x{height}"
        )));
    }

    let decoded = decode(input, limits)?;
    let (img_w, img_h) = (decoded.image.width(), decoded.image.height());

    // Bounds in u64 so x+width can't overflow u32 before comparison.
    let (right, bottom) = (
        u64::from(x) + u64::from(width),
        u64::from(y) + u64::from(height),
    );
    if right > u64::from(img_w) || bottom > u64::from(img_h) {
        return Err(ImageError::InvalidInput(format!(
            "crop rectangle ({x}, {y}, {width}, {height}) is out of bounds \
             for a {img_w}x{img_h} image"
        )));
    }

    let cropped = decoded.image.crop_imm(x, y, width, height);
    encode(&cropped, decoded.format)
}

#[cfg(test)]
mod tests {
    use image::{DynamicImage, ImageBuffer, ImageFormat, Rgb};

    use crate::crates::core::engine::EngineLimits;
    use crate::crates::image_engine::error::ImageError;

    use super::crop;

    fn unlimited() -> EngineLimits {
        EngineLimits::default()
    }

    /// A 4×2 RGB image with a distinct color per pixel, encoded as PNG.
    fn png_bytes() -> Vec<u8> {
        let mut img = ImageBuffer::new(4, 2);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = Rgb([(x * 60) as u8, (y * 120) as u8, 200]);
        }
        let mut buf = std::io::Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(img)
            .write_to(&mut buf, ImageFormat::Png)
            .unwrap();
        buf.into_inner()
    }

    #[test]
    fn crop_produces_correct_dimensions_and_format() {
        let out = crop(&png_bytes(), 1, 0, 2, 2, &unlimited()).unwrap();
        // Still a PNG (magic bytes) with the cropped size.
        assert_eq!(&out[..8], b"\x89PNG\r\n\x1a\n");
        let decoded = image::load_from_memory(&out).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (2, 2));
    }

    #[test]
    fn crop_full_image_is_identity() {
        let out = crop(&png_bytes(), 0, 0, 4, 2, &unlimited()).unwrap();
        let decoded = image::load_from_memory(&out).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (4, 2));
    }

    #[test]
    fn crop_out_of_bounds_rejected() {
        // x+width past right edge.
        let err = crop(&png_bytes(), 3, 0, 2, 1, &unlimited()).unwrap_err();
        assert!(matches!(err, ImageError::InvalidInput(_)), "{err:?}");
        // y+height past bottom edge.
        let err = crop(&png_bytes(), 0, 1, 1, 2, &unlimited()).unwrap_err();
        assert!(matches!(err, ImageError::InvalidInput(_)), "{err:?}");
        // Origin itself out of bounds.
        let err = crop(&png_bytes(), 4, 0, 1, 1, &unlimited()).unwrap_err();
        assert!(matches!(err, ImageError::InvalidInput(_)), "{err:?}");
    }

    #[test]
    fn crop_zero_size_rejected() {
        let err = crop(&png_bytes(), 0, 0, 0, 1, &unlimited()).unwrap_err();
        assert!(matches!(err, ImageError::InvalidInput(_)), "{err:?}");
        let err = crop(&png_bytes(), 0, 0, 1, 0, &unlimited()).unwrap_err();
        assert!(matches!(err, ImageError::InvalidInput(_)), "{err:?}");
    }
}
