//! Quarter-turn rotation (90°/180°/270°).
//!
//! Only right-angle rotations are supported — arbitrary angles would
//! require resampling and are not advertised by the engine.

use crate::crates::core::engine::EngineLimits;

use crate::crates::image_engine::error::{ImageError, ImageResult};

use super::load::{decode, encode};

/// Rotate `input` by `degrees`, which must be exactly 90, 180, or 270.
/// Returns the encoded bytes in the input format.
pub fn rotate(input: &[u8], degrees: u16, limits: &EngineLimits) -> ImageResult<Vec<u8>> {
    let loaded = decode(input, limits)?;
    let rotated = match degrees {
        90 => loaded.image.rotate90(),
        180 => loaded.image.rotate180(),
        270 => loaded.image.rotate270(),
        other => {
            return Err(ImageError::InvalidInput(format!(
                "rotation must be 90, 180, or 270 degrees, got {other}"
            )));
        }
    };
    encode(&rotated, loaded.format)
}

#[cfg(test)]
mod tests {
    use image::{DynamicImage, ImageBuffer, Rgb};

    use crate::crates::core::engine::EngineLimits;
    use crate::crates::image_engine::error::{ImageError, ImageResult};
    use crate::crates::image_engine::ops::format::ImageFormat as EngineFormat;
    use crate::crates::image_engine::ops::load::{decode, encode};

    use super::rotate;

    /// A 3x2 RGB image with a distinct color per pixel, encoded as PNG.
    ///
    /// Row 0: A B C
    /// Row 1: D E F
    fn fixture_png() -> ImageResult<Vec<u8>> {
        let mut img = ImageBuffer::new(3, 2);
        let px = [
            [255, 0, 0],   // A red
            [0, 255, 0],   // B green
            [0, 0, 255],   // C blue
            [255, 255, 0], // D yellow
            [255, 0, 255], // E magenta
            [0, 255, 255], // F cyan
        ];
        for (i, p) in px.iter().enumerate() {
            let (x, y) = (i as u32 % 3, i as u32 / 3);
            img.put_pixel(x, y, Rgb(*p));
        }
        encode(&DynamicImage::ImageRgb8(img), EngineFormat::Png)
    }

    fn pixel_at(decoded: &DynamicImage, x: u32, y: u32) -> [u8; 3] {
        decoded.to_rgb8().get_pixel(x, y).0
    }

    const A: [u8; 3] = [255, 0, 0];
    const B: [u8; 3] = [0, 255, 0];
    const C: [u8; 3] = [0, 0, 255];
    const D: [u8; 3] = [255, 255, 0];
    const E: [u8; 3] = [255, 0, 255];
    const F: [u8; 3] = [0, 255, 255];

    fn roundtrip(input: &[u8], degrees: u16) -> ImageResult<DynamicImage> {
        let out = rotate(input, degrees, &EngineLimits::default())?;
        Ok(decode(&out, &EngineLimits::default())?.image)
    }

    #[test]
    fn rotate_90_swaps_dimensions_and_positions() {
        let input = fixture_png().unwrap();
        let img = roundtrip(&input, 90).unwrap();
        assert_eq!((img.width(), img.height()), (2, 3));
        // `image`'s rotate90 is 90° clockwise: src(x,y) → dst(h-1-y, x).
        // Result (2x3): row0 = D A, row1 = E B, row2 = F C.
        assert_eq!(pixel_at(&img, 0, 0), D);
        assert_eq!(pixel_at(&img, 1, 0), A);
        assert_eq!(pixel_at(&img, 0, 1), E);
        assert_eq!(pixel_at(&img, 1, 1), B);
        assert_eq!(pixel_at(&img, 0, 2), F);
        assert_eq!(pixel_at(&img, 1, 2), C);
    }

    #[test]
    fn rotate_180_is_pixel_exact() {
        let input = fixture_png().unwrap();
        let img = roundtrip(&input, 180).unwrap();
        assert_eq!((img.width(), img.height()), (3, 2));
        // F E D / C B A
        assert_eq!(pixel_at(&img, 0, 0), F);
        assert_eq!(pixel_at(&img, 1, 0), E);
        assert_eq!(pixel_at(&img, 2, 0), D);
        assert_eq!(pixel_at(&img, 0, 1), C);
        assert_eq!(pixel_at(&img, 1, 1), B);
        assert_eq!(pixel_at(&img, 2, 1), A);
    }

    #[test]
    fn rotate_270_swaps_dimensions_and_positions() {
        let input = fixture_png().unwrap();
        let img = roundtrip(&input, 270).unwrap();
        assert_eq!((img.width(), img.height()), (2, 3));
        // rotate270 is 270° clockwise (90° CCW): result rows are
        // C F / B E / A D.
        assert_eq!(pixel_at(&img, 0, 0), C);
        assert_eq!(pixel_at(&img, 1, 0), F);
        assert_eq!(pixel_at(&img, 0, 1), B);
        assert_eq!(pixel_at(&img, 1, 1), E);
        assert_eq!(pixel_at(&img, 0, 2), A);
        assert_eq!(pixel_at(&img, 1, 2), D);
    }

    #[test]
    fn rotate_preserves_input_format() {
        // BMP round-trips losslessly for these exact pixels.
        let png = fixture_png().unwrap();
        let decoded = decode(&png, &EngineLimits::default()).unwrap();
        let bmp = encode(&decoded.image, EngineFormat::Bmp).unwrap();
        let out = rotate(&bmp, 180, &EngineLimits::default()).unwrap();
        let decoded_out = decode(&out, &EngineLimits::default()).unwrap();
        assert_eq!(decoded_out.format, EngineFormat::Bmp);
        assert_eq!(
            (decoded_out.image.width(), decoded_out.image.height()),
            (3, 2)
        );
    }

    #[test]
    fn rotate_rejects_non_quarter_turns() {
        let input = fixture_png().unwrap();
        let limits = EngineLimits::default();
        for bad in [0u16, 45, 89, 91, 360, 450] {
            assert!(
                matches!(
                    rotate(&input, bad, &limits),
                    Err(ImageError::InvalidInput(_))
                ),
                "{bad} must be rejected"
            );
        }
    }

    #[test]
    fn rotate_rejects_garbage_input() {
        assert!(rotate(b"not an image", 90, &EngineLimits::default()).is_err());
    }

    #[test]
    fn rotate_output_remains_decodable_format() {
        // Sanity: output of a PNG rotation still sniffs as PNG.
        let input = fixture_png().unwrap();
        let out = rotate(&input, 90, &EngineLimits::default()).unwrap();
        assert_eq!(&out[..8], b"\x89PNG\r\n\x1a\n");
    }
}
