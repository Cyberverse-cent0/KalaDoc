//! Mirror flips — horizontal, vertical, or both.
//!
//! Unlike [`rotate`](super::rotate), flips are lossless: every pixel
//! moves to a mirrored position with its value unchanged, so no
//! resampling is involved.

use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;
use crate::crates::image_engine::error::ImageResult;

use super::load::{decode, encode};

/// Direction of a mirror flip applied by [`flip`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlipDirection {
    /// Mirror left-to-right: the left edge becomes the right edge.
    /// Equivalent to `DynamicImage::fliph`.
    Horizontal,
    /// Mirror top-to-bottom: the top edge becomes the bottom edge.
    /// Equivalent to `DynamicImage::flipv`.
    Vertical,
    /// Apply both mirrors (equivalent to a 180° rotation, pixel-exact).
    Both,
}

/// Flip `input` in the given direction, returning the encoded bytes
/// in the input format. All limits are enforced by [`decode`].
pub fn flip(
    input: &[u8],
    direction: FlipDirection,
    limits: &EngineLimits,
) -> ImageResult<Vec<u8>> {
    let decoded = decode(input, limits)?;
    let flipped = match direction {
        FlipDirection::Horizontal => decoded.image.fliph(),
        FlipDirection::Vertical => decoded.image.flipv(),
        FlipDirection::Both => decoded.image.fliph().flipv(),
    };
    encode(&flipped, decoded.format)
}

/// Convenience wrapper: `flip` with an explicit horizontal flag —
/// `true` mirrors left-to-right, `false` top-to-bottom.
pub fn flip_mirror(input: &[u8], horizontal: bool, limits: &EngineLimits) -> ImageResult<Vec<u8>> {
    flip(
        input,
        if horizontal {
            FlipDirection::Horizontal
        } else {
            FlipDirection::Vertical
        },
        limits,
    )
}

#[cfg(test)]
mod tests {
    use image::{DynamicImage, RgbaImage};

    use crate::crates::core::engine::EngineLimits;
    use crate::crates::image_engine::error::ImageResult;

    use super::{FlipDirection, flip};
    use crate::crates::image_engine::ops::format::ImageFormat;
    use crate::crates::image_engine::ops::load::{decode, encode};

    /// A 3x2 RGBA image where every pixel is a distinct color, so a
    /// flip's effect on positions is unambiguous.
    ///
    /// Row 0: A B C
    /// Row 1: D E F
    fn fixture_png() -> ImageResult<Vec<u8>> {
        let mut img = RgbaImage::new(3, 2);
        let px = [
            [255, 0, 0, 255],    // A
            [0, 255, 0, 255],    // B
            [0, 0, 255, 255],    // C
            [255, 255, 0, 255],  // D
            [255, 0, 255, 255],  // E
            [0, 255, 255, 255],  // F
        ];
        for (i, p) in px.iter().enumerate() {
            let (x, y) = (i as u32 % 3, i as u32 / 3);
            img.put_pixel(x, y, image::Rgba(*p));
        }
        encode(&DynamicImage::ImageRgba8(img), ImageFormat::Png)
    }

    fn pixel_at(decoded: &DynamicImage, x: u32, y: u32) -> [u8; 4] {
        let rgba = decoded.to_rgba8();
        let p = rgba.get_pixel(x, y);
        [p[0], p[1], p[2], p[3]]
    }

    const A: [u8; 4] = [255, 0, 0, 255];
    const B: [u8; 4] = [0, 255, 0, 255];
    const C: [u8; 4] = [0, 0, 255, 255];
    const D: [u8; 4] = [255, 255, 0, 255];
    const E: [u8; 4] = [255, 0, 255, 255];
    const F: [u8; 4] = [0, 255, 255, 255];

    fn roundtrip(input: &[u8], direction: FlipDirection) -> ImageResult<DynamicImage> {
        let out = flip(input, direction, &EngineLimits::default())?;
        Ok(decode(&out, &EngineLimits::default())?.image)
    }

    #[test]
    fn horizontal_mirror_moves_columns() {
        let input = fixture_png().unwrap();
        let img = roundtrip(&input, FlipDirection::Horizontal).unwrap();
        assert_eq!((img.width(), img.height()), (3, 2));
        // Row 0 becomes C B A; row 1 becomes F E D.
        assert_eq!(pixel_at(&img, 0, 0), C);
        assert_eq!(pixel_at(&img, 1, 0), B);
        assert_eq!(pixel_at(&img, 2, 0), A);
        assert_eq!(pixel_at(&img, 0, 1), F);
        assert_eq!(pixel_at(&img, 1, 1), E);
        assert_eq!(pixel_at(&img, 2, 1), D);
    }

    #[test]
    fn vertical_mirror_moves_rows() {
        let input = fixture_png().unwrap();
        let img = roundtrip(&input, FlipDirection::Vertical).unwrap();
        assert_eq!((img.width(), img.height()), (3, 2));
        // Row 0 becomes D E F; row 1 becomes A B C.
        assert_eq!(pixel_at(&img, 0, 0), D);
        assert_eq!(pixel_at(&img, 1, 0), E);
        assert_eq!(pixel_at(&img, 2, 0), F);
        assert_eq!(pixel_at(&img, 0, 1), A);
        assert_eq!(pixel_at(&img, 1, 1), B);
        assert_eq!(pixel_at(&img, 2, 1), C);
    }

    #[test]
    fn both_mirrors_is_pixel_exact_180() {
        let input = fixture_png().unwrap();
        let img = roundtrip(&input, FlipDirection::Both).unwrap();
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
    fn preserves_input_format() {
        // BMP round-trips losslessly enough for exact pixels here.
        let png = fixture_png().unwrap();
        let decoded = decode(&png, &EngineLimits::default()).unwrap();
        let bmp = encode(&decoded.image, ImageFormat::Bmp).unwrap();
        let out = flip(&bmp, FlipDirection::Horizontal, &EngineLimits::default()).unwrap();
        let decoded_out = decode(&out, &EngineLimits::default()).unwrap();
        assert_eq!(decoded_out.format, ImageFormat::Bmp);
        assert_eq!(pixel_at(&decoded_out.image, 0, 0), C);
    }

    #[test]
    fn rejects_garbage_input() {
        let result = flip(b"not an image", FlipDirection::Horizontal, &EngineLimits::default());
        assert!(result.is_err());
    }
}
