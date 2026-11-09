//! Brightness, contrast, and hue-rotate color adjustments.
//!
//! Each adjustment is optional: [`AdjustOptions`] fields left as `None`
//! are skipped entirely (no pixel pass is made for them).
//!
//! # Color-type handling
//!
//! Adjustments are computed in 8-bit sRGB. `Rgb8`/`Rgba8` inputs are
//! processed natively; every other variant (16-bit, float, grayscale,
//! gray+alpha) is first converted with `to_rgb8()`/`to_rgba8()`, so
//! re-encoded output is always `Rgb8` or `Rgba8` pixels. Grayscale
//! inputs therefore gain an RGB(A) color type, and 16-bit/float
//! inputs are reduced to 8 bits — the same trade-off the `image`
//! crate's own color ops make (their hue-rotate matrix is hardcoded
//! for an 8-bit range).
//!
//! Alpha is preserved where the input has it; JPEG encoding flattens
//! it as usual (see the crate docs).

use serde::{Deserialize, Serialize};

use image::{ColorType, DynamicImage, ImageBuffer, Pixel, Primitive, imageops};

use crate::crates::core::engine::EngineLimits;
use crate::crates::image_engine::error::{ImageError, ImageResult};

use super::load::{decode, encode};

/// Parameters for [`adjust`]. Every field is optional; `None` means
/// "leave this adjustment untouched". At least one field must be set.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct AdjustOptions {
    /// Brightness shift as a percentage, `-100..=100`.
    ///
    /// Mapped onto the per-channel delta used by
    /// [`imageops::brighten`]: `100` adds 255 to every channel (full
    /// white), `-100` subtracts 255 (full black), `0`/`None` is a
    /// no-op. Intermediate values scale linearly.
    pub brightness: Option<i32>,

    /// Contrast factor in percent, `-100.0..=100.0`, passed straight
    /// to [`imageops::contrast`]: `0.0`/`None` is a no-op, positive
    /// values increase contrast, negative values decrease it
    /// (`-100.0` collapses everything to mid-gray).
    pub contrast: Option<f32>,

    /// Hue rotation in degrees, `-180..=180`, passed to
    /// [`imageops::huerotate`]. Positive values rotate the same way
    /// CSS `hue-rotate()` does. No-op on grayscale pixels is *not*
    /// guaranteed — convert color spaces yourself if that matters.
    pub hue_degrees: Option<i32>,
}

/// Apply the requested adjustments to `input` and return the encoded
/// bytes in the input's sniffed format.
///
/// Fails with [`ImageError::InvalidInput`] when no adjustment is set
/// or a value is outside its documented range, and with the usual
/// decode/limit errors for malformed or oversized inputs.
pub fn adjust(
    input: &[u8],
    options: &AdjustOptions,
    limits: &EngineLimits,
) -> ImageResult<Vec<u8>> {
    validate(options)?;

    let decoded = decode(input, limits)?;
    let adjusted = match decoded.image {
        DynamicImage::ImageRgb8(buf) => DynamicImage::ImageRgb8(adjust_buffer(&buf, options)),
        DynamicImage::ImageRgba8(buf) => DynamicImage::ImageRgba8(adjust_buffer(&buf, options)),
        other => {
            // 16-bit, float, grayscale, and gray+alpha variants are
            // normalized to 8-bit sRGB before adjustment (see module
            // docs). Alpha-carrying formats keep an alpha channel.
            let has_alpha = matches!(
                other.color(),
                ColorType::La8 | ColorType::La16 | ColorType::Rgba16 | ColorType::Rgba32F
            );
            if has_alpha {
                DynamicImage::ImageRgba8(adjust_buffer(&other.to_rgba8(), options))
            } else {
                DynamicImage::ImageRgb8(adjust_buffer(&other.to_rgb8(), options))
            }
        }
    };
    encode(&adjusted, decoded.format)
}

/// Rejects out-of-range or empty option sets.
fn validate(options: &AdjustOptions) -> ImageResult<()> {
    if options.brightness.is_none() && options.contrast.is_none() && options.hue_degrees.is_none() {
        return Err(ImageError::InvalidInput(
            "at least one of brightness, contrast, or hue_degrees must be set".to_string(),
        ));
    }
    if let Some(b) = options.brightness {
        if !(-100..=100).contains(&b) {
            return Err(ImageError::InvalidInput(format!(
                "brightness must be in -100..=100, got {b}"
            )));
        }
    }
    if let Some(c) = options.contrast {
        if !c.is_finite() || !(-100.0..=100.0).contains(&c) {
            return Err(ImageError::InvalidInput(format!(
                "contrast must be a finite value in -100.0..=100.0, got {c}"
            )));
        }
    }
    if let Some(h) = options.hue_degrees {
        if !(-180..=180).contains(&h) {
            return Err(ImageError::InvalidInput(format!(
                "hue_degrees must be in -180..=180, got {h}"
            )));
        }
    }
    Ok(())
}

/// Applies the `Some` adjustments, in order brightness → contrast →
/// hue, to an 8-bit pixel buffer. Each step returns a new buffer of
/// the same pixel type so the chain stays type-stable.
fn adjust_buffer<P, S>(
    image: &ImageBuffer<P, Vec<S>>,
    options: &AdjustOptions,
) -> ImageBuffer<P, Vec<S>>
where
    P: Pixel<Subpixel = S> + 'static,
    S: Primitive + 'static,
{
    let mut out = image.clone();
    if let Some(b) = options.brightness {
        // Percent → per-channel delta on the 0..=255 scale.
        out = imageops::brighten(&out, b * 255 / 100);
    }
    if let Some(c) = options.contrast {
        out = imageops::contrast(&out, c);
    }
    if let Some(h) = options.hue_degrees {
        out = imageops::huerotate(&out, h);
    }
    out
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use image::{DynamicImage, Rgb, RgbImage};

    use super::{AdjustOptions, adjust};
    use crate::crates::core::engine::EngineLimits;
    use crate::crates::image_engine::error::ImageError;
    use crate::crates::image_engine::ops::format::ImageFormat;
    use crate::crates::image_engine::ops::load::decode;

    /// Encodes a 4x4 PNG of uniform `rgb` pixels.
    fn png_of(rgb: Rgb<u8>) -> Vec<u8> {
        let mut img = RgbImage::new(4, 4);
        for p in img.pixels_mut() {
            *p = rgb;
        }
        let mut buf = Vec::new();
        DynamicImage::ImageRgb8(img)
            .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)
            .expect("test PNG must encode");
        buf
    }

    fn decode_rgb(bytes: &[u8]) -> Rgb<u8> {
        *decode(bytes, &EngineLimits::default())
            .expect("output must decode")
            .image
            .to_rgb8()
            .get_pixel(0, 0)
    }

    #[test]
    fn brightness_increases_luminance() {
        let input = png_of(Rgb([100, 150, 200]));
        let out = adjust(
            &input,
            &AdjustOptions {
                brightness: Some(50),
                contrast: None,
                hue_degrees: None,
            },
            &EngineLimits::default(),
        )
        .expect("adjust must succeed");

        let before = decode_rgb(&input);
        let after = decode_rgb(&out);
        let lum = |c: [u8; 3]| c[0] as u32 + c[1] as u32 + c[2] as u32;
        assert!(lum(after.0) > lum(before.0), "{before:?} -> {after:?}");
    }

    #[test]
    fn brightness_negative_decreases_luminance() {
        let input = png_of(Rgb([150, 150, 150]));
        let out = adjust(
            &input,
            &AdjustOptions {
                brightness: Some(-50),
                contrast: None,
                hue_degrees: None,
            },
            &EngineLimits::default(),
        )
        .expect("adjust must succeed");

        let after = decode_rgb(&out);
        // Delta is -127 per channel: 150 -> 23.
        assert_eq!(after.0, [23, 23, 23]);
    }

    #[test]
    fn contrast_pushes_channels_away_from_midpoint() {
        let input = png_of(Rgb([100, 150, 200]));
        let out = adjust(
            &input,
            &AdjustOptions {
                brightness: None,
                contrast: Some(100.0),
                hue_degrees: None,
            },
            &EngineLimits::default(),
        )
        .expect("adjust must succeed");

        let after = decode_rgb(&out);
        // Dark channel darker, light channel lighter than before.
        assert!(after.0[0] < 100, "red should darken: {after:?}");
        assert!(after.0[2] > 200, "blue should brighten: {after:?}");
    }

    #[test]
    fn hue_rotation_changes_color() {
        let input = png_of(Rgb([255, 0, 0]));
        let out = adjust(
            &input,
            &AdjustOptions {
                brightness: None,
                contrast: None,
                hue_degrees: Some(120),
            },
            &EngineLimits::default(),
        )
        .expect("adjust must succeed");

        let after = decode_rgb(&out);
        assert_ne!(after.0, [255, 0, 0], "red must rotate: {after:?}");
        assert!(
            after.0[1] > after.0[0],
            "expected greenish result: {after:?}"
        );
    }

    #[test]
    fn output_keeps_input_format() {
        let input = png_of(Rgb([10, 20, 30]));
        let out = adjust(
            &input,
            &AdjustOptions {
                brightness: Some(10),
                contrast: None,
                hue_degrees: None,
            },
            &EngineLimits::default(),
        )
        .expect("adjust must succeed");

        let decoded = decode(&out, &EngineLimits::default()).expect("must decode");
        assert_eq!(decoded.format, ImageFormat::Png);
    }

    #[test]
    fn rejects_out_of_range_values() {
        let input = png_of(Rgb([100, 100, 100]));
        let limits = EngineLimits::default();

        for options in [
            AdjustOptions {
                brightness: Some(101),
                contrast: None,
                hue_degrees: None,
            },
            AdjustOptions {
                brightness: Some(-101),
                contrast: None,
                hue_degrees: None,
            },
            AdjustOptions {
                brightness: None,
                contrast: Some(f32::NAN),
                hue_degrees: None,
            },
            AdjustOptions {
                brightness: None,
                contrast: Some(101.0),
                hue_degrees: None,
            },
            AdjustOptions {
                brightness: None,
                contrast: None,
                hue_degrees: Some(181),
            },
            AdjustOptions {
                brightness: None,
                contrast: None,
                hue_degrees: Some(-181),
            },
        ] {
            assert!(
                matches!(
                    adjust(&input, &options, &limits),
                    Err(ImageError::InvalidInput(_))
                ),
                "options {options:?} must be rejected"
            );
        }
    }

    #[test]
    fn rejects_empty_options() {
        let input = png_of(Rgb([100, 100, 100]));
        let result = adjust(&input, &AdjustOptions::default(), &EngineLimits::default());
        assert!(matches!(result, Err(ImageError::InvalidInput(_))));
    }

    #[test]
    fn accepts_boundary_values() {
        let input = png_of(Rgb([128, 128, 128]));
        let out = adjust(
            &input,
            &AdjustOptions {
                brightness: Some(-100),
                contrast: Some(-100.0),
                hue_degrees: Some(-180),
            },
            &EngineLimits::default(),
        )
        .expect("boundary values must be accepted");
        // brightness -100 alone would blacken; combined ops still decode.
        let _ = decode_rgb(&out);
    }
}
