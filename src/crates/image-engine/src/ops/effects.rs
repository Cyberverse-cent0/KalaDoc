//! Scanner-style effects: levels/gamma correction, photocopy
//! simulation, and near-color replacement.
//!
//! All three ops share the [`super::color`] conventions: they decode
//! through [`super::load::decode`] (full `EngineLimits` enforcement),
//! normalize to 8-bit sRGB via [`super::load::normalize_8bit`], apply a
//! fixed per-op pixel pass, and re-encode in the input's sniffed
//! format. Alpha is preserved by every op.
//!
//! # Determinism
//!
//! Every pass is a pure function of the pixel data and options — no
//! RNG, no time, no threads. The photocopy "paper grain" noise is a
//! fixed integer hash of the pixel coordinates, so identical inputs
//! always produce identical outputs.

use serde::{Deserialize, Serialize};

use image::DynamicImage;

use crate::crates::core::engine::EngineLimits;
use crate::crates::image_engine::error::{ImageError, ImageResult};

use super::color::for_each_color_channel;
use super::load::{decode, encode, normalize_8bit};

/// Parameters for [`levels`]. Every field is optional; unset fields
/// keep their neutral defaults (`input_black` = 0, `input_white` =
/// 255, `gamma` = 1.0). At least one field must be set.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct LevelsOptions {
    /// Input black point: gray values at or below this map to 0.
    /// `0`/`None` is neutral.
    pub input_black: Option<u8>,

    /// Input white point: gray values at or above this map to 255.
    /// `255`/`None` is neutral. Must be strictly greater than the
    /// effective `input_black`.
    pub input_white: Option<u8>,

    /// Gamma correction exponent applied after the black/white
    /// stretch, `0.1..=4.0` (finite). Values below 1.0 brighten
    /// midtones, above 1.0 darken them. `1.0`/`None` is neutral.
    pub gamma: Option<f32>,
}

/// Apply a levels/gamma curve to every color channel.
///
/// Each channel is remapped independently through the same LUT:
/// `out = clamp(((in - black) / (white - black))^(1/gamma) * 255)`.
/// Fails with [`ImageError::InvalidInput`] when no field is set, when
/// the effective black point is not below the white point, or when
/// `gamma` is outside `0.1..=4.0`.
pub fn levels(
    input: &[u8],
    options: &LevelsOptions,
    limits: &EngineLimits,
) -> ImageResult<Vec<u8>> {
    let (black, white, gamma) = validate_levels(options)?;

    let decoded = decode(input, limits)?;
    let mut image = normalize_8bit(decoded.image);

    let lut = build_levels_lut(black, white, gamma);
    for_each_color_channel(&mut image, &mut |ch: &mut [u8]| {
        for v in ch.iter_mut() {
            *v = lut[usize::from(*v)];
        }
    });

    encode(&image, decoded.format)
}

/// Validates [`LevelsOptions`] and returns the effective
/// `(black, white, gamma)` triple.
fn validate_levels(options: &LevelsOptions) -> ImageResult<(u8, u8, f32)> {
    if options.input_black.is_none() && options.input_white.is_none() && options.gamma.is_none() {
        return Err(ImageError::InvalidInput(
            "at least one levels option must be set".to_string(),
        ));
    }
    let black = options.input_black.unwrap_or(0);
    let white = options.input_white.unwrap_or(255);
    if black >= white {
        return Err(ImageError::InvalidInput(format!(
            "input_black ({black}) must be less than input_white ({white})"
        )));
    }
    let gamma = options.gamma.unwrap_or(1.0);
    if !gamma.is_finite() || !(0.1..=4.0).contains(&gamma) {
        return Err(ImageError::InvalidInput(format!(
            "gamma must be a finite value in 0.1..=4.0, got {gamma}"
        )));
    }
    Ok((black, white, gamma))
}

/// Builds the 256-entry LUT for the levels curve. `black < white` is
/// guaranteed by [`validate_levels`].
fn build_levels_lut(black: u8, white: u8, gamma: f32) -> [u8; 256] {
    let mut lut = [0u8; 256];
    let span = f32::from(white) - f32::from(black);
    let inv_gamma = 1.0 / gamma;
    for (i, v) in lut.iter_mut().enumerate() {
        let t = ((i as f32 - f32::from(black)) / span).clamp(0.0, 1.0);
        *v = (t.powf(inv_gamma) * 255.0).round().clamp(0.0, 255.0) as u8;
    }
    lut
}

/// Parameters for [`photocopy`]. Every field is optional; unset
/// fields use the defaults documented below. With all defaults the
/// op still produces a hard-thresholded grayscale "scan".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhotocopyOptions {
    /// Binarization threshold on the 0..=255 luma scale.
    /// `None`/unset means 128.
    pub threshold: Option<u8>,

    /// Edge softness in luma levels around the threshold: the ramp
    /// spans `[threshold - softness/2, threshold + softness/2]`.
    /// `0`/unset means a hard black/white cut; larger values produce
    /// a smoother gray transition band.
    pub softness: Option<u8>,

    /// Deterministic "paper grain" noise amplitude in luma levels,
    /// `0..=255`. Each pixel is offset by a hash-derived value in
    /// `[-noise, +noise]` before thresholding. `0`/unset disables
    /// noise. The grain pattern is fixed — identical inputs always
    /// produce identical noise.
    pub noise: Option<u8>,

    /// Apply 4x4 Bayer ordered dithering before thresholding,
    /// breaking up the hard edge into a halftone-like dot pattern.
    #[serde(default)]
    pub dither: bool,
}

/// Simulate a photocopy/scanner pass: grayscale conversion followed
/// by thresholding with optional softness, deterministic grain noise,
/// and ordered dithering.
///
/// The image is first converted to grayscale (Rec. 709 luma, alpha
/// preserved), then each luma sample passes through, in order:
/// noise → Bayer dither → threshold/softness ramp → 0/255 output.
/// Fails with the usual decode/limit errors for malformed or
/// oversized inputs; [`PhotocopyOptions`] has no invalid states.
pub fn photocopy(
    input: &[u8],
    options: &PhotocopyOptions,
    limits: &EngineLimits,
) -> ImageResult<Vec<u8>> {
    let decoded = decode(input, limits)?;
    let mut image = normalize_8bit(decoded.image).grayscale();

    let threshold = options.threshold.unwrap_or(128);
    let softness = options.softness.unwrap_or(0);
    let noise = options.noise.unwrap_or(0);

    apply_scan(&mut image, threshold, softness, noise, options.dither);

    encode(&image, decoded.format)
}

/// Applies the scan simulation to the luma channel of an `L8`/`La8`
/// [`DynamicImage`] (the output of [`DynamicImage::grayscale`]).
fn apply_scan(img: &mut DynamicImage, threshold: u8, softness: u8, noise: u8, dither: bool) {
    match img {
        DynamicImage::ImageLuma8(buf) => {
            for (x, y, px) in buf.enumerate_pixels_mut() {
                let v = scan_value(x, y, px[0], threshold, softness, noise, dither);
                *px = image::Luma([v]);
            }
        }
        DynamicImage::ImageLumaA8(buf) => {
            for (x, y, px) in buf.enumerate_pixels_mut() {
                let v = scan_value(x, y, px[0], threshold, softness, noise, dither);
                *px = image::LumaA([v, px[1]]);
            }
        }
        _ => unreachable!("photocopy runs on grayscale luma buffers"),
    }
}

/// Computes the scanned luma value for one pixel: deterministic
/// grain noise, optional Bayer offset, then the threshold ramp.
#[allow(clippy::too_many_arguments)]
fn scan_value(
    x: u32,
    y: u32,
    luma: u8,
    threshold: u8,
    softness: u8,
    noise: u8,
    dither: bool,
) -> u8 {
    let mut v = i32::from(luma);
    if noise > 0 {
        v += grain_offset(x, y, noise);
    }
    if dither {
        // Ordered dithering: shift each pixel by its Bayer cell so the
        // threshold cuts a dot pattern instead of a flat edge.
        v += bayer_offset(x, y);
    }
    v = v.clamp(0, 255);

    let t = i32::from(threshold);
    let s = i32::from(softness);
    if s == 0 {
        return if v >= t { 255 } else { 0 };
    }
    // Linear ramp centered on the threshold, `s` levels wide.
    let half = s / 2;
    let lo = t - half;
    let hi = t + (s - half);
    if v <= lo {
        0
    } else if v >= hi {
        255
    } else {
        let denom = (hi - lo).max(1);
        (((v - lo) * 255 + denom / 2) / denom).clamp(0, 255) as u8
    }
}

/// Deterministic grain offset in `[-noise, +noise]`, derived from an
/// integer hash of the pixel coordinates (splitmix-style finalizer).
/// Pure function of `(x, y)` — no RNG state.
fn grain_offset(x: u32, y: u32, noise: u8) -> i32 {
    let mut h = (x as u64)
        .wrapping_mul(0x9E37_79B1_85EB_CA77)
        .wrapping_add((y as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F))
        .wrapping_add(0x1656_67B1_9E37_79F9);
    h ^= h >> 30;
    h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94D0_49BB_1331_11EB);
    h ^= h >> 31;
    let span = u64::from(noise);
    // Map the hash into [-noise, +noise].
    ((h % (2 * span + 1)) as i64 - i64::from(noise)) as i32
}

/// 4x4 Bayer ordered-dither matrix (values 0..=15).
const BAYER4: [[u8; 4]; 4] = [[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]];

/// Ordered-dither offset in `[-112, +128]` luma levels for the Bayer
/// cell at `(x, y)`.
fn bayer_offset(x: u32, y: u32) -> i32 {
    let cell = BAYER4[(y % 4) as usize][(x % 4) as usize] as i32;
    // Center the 0..=15 range on zero, scaled to roughly ±half range.
    (cell - 7) * 16
}

/// Parameters for [`replace_color`]. `source` and `target` are 8-bit
/// sRGB triples; `tolerance` is required.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ColorReplaceOptions {
    /// The color to replace, as `[r, g, b]` in 0..=255.
    pub source: [u8; 3],

    /// The color substituted for near-matches of `source`.
    pub target: [u8; 3],

    /// Match radius as a fraction of the maximum possible RGB distance
    /// (`sqrt(3) * 255`), `0.0..=1.0`. `0.0` replaces only exact
    /// matches. Must be finite.
    pub tolerance: f32,

    /// Width of the blend band at the edge of the match radius, as a
    /// fraction of `tolerance`, `0.0..=1.0`. Inside
    /// `tolerance * (1 - feather)` the target is applied fully; the
    /// blend falls linearly to zero at `tolerance`. `None` defaults
    /// to `0.25`; `0.0` gives a hard cut at `tolerance`.
    pub feather: Option<f32>,
}

/// Replace pixels whose color is within `tolerance` of `source` with
/// `target`, blending linearly across the `feather` band.
///
/// Distance is normalized Euclidean distance in RGB space
/// (`dist / (255 * sqrt(3))`). Only the color channels are touched —
/// alpha is preserved, and fully transparent pixels have their RGB
/// replaced like any other. Fails with [`ImageError::InvalidInput`]
/// when `tolerance`/`feather` are non-finite or out of range.
pub fn replace_color(
    input: &[u8],
    options: &ColorReplaceOptions,
    limits: &EngineLimits,
) -> ImageResult<Vec<u8>> {
    validate_replace(options)?;

    let decoded = decode(input, limits)?;
    let mut image = normalize_8bit(decoded.image);

    let tolerance = options.tolerance;
    let feather = options.feather.unwrap_or(0.25);
    let inner = tolerance * (1.0 - feather);
    // Denominator for the feather ramp; guarded against zero.
    let ramp = (tolerance - inner).max(f32::EPSILON);

    for_each_color_channel(&mut image, &mut |ch: &mut [u8]| {
        let dr = f32::from(ch[0]) - f32::from(options.source[0]);
        let dg = f32::from(ch[1]) - f32::from(options.source[1]);
        let db = f32::from(ch[2]) - f32::from(options.source[2]);
        let dist = (dr * dr + dg * dg + db * db).sqrt() / (255.0 * 3.0_f32.sqrt());

        let factor = if tolerance <= 0.0 {
            if dist == 0.0 { 1.0 } else { 0.0 }
        } else if dist <= inner {
            1.0
        } else if dist >= tolerance {
            0.0
        } else {
            ((tolerance - dist) / ramp).clamp(0.0, 1.0)
        };
        if factor == 0.0 {
            return;
        }
        for i in 0..3 {
            let blended =
                f32::from(ch[i]) + (f32::from(options.target[i]) - f32::from(ch[i])) * factor;
            ch[i] = blended.round().clamp(0.0, 255.0) as u8;
        }
    });

    encode(&image, decoded.format)
}

/// Rejects non-finite or out-of-range tolerance/feather values.
fn validate_replace(options: &ColorReplaceOptions) -> ImageResult<()> {
    if !options.tolerance.is_finite() || !(0.0..=1.0).contains(&options.tolerance) {
        return Err(ImageError::InvalidInput(format!(
            "tolerance must be a finite value in 0.0..=1.0, got {}",
            options.tolerance
        )));
    }
    if let Some(f) = options.feather
        && (!f.is_finite() || !(0.0..=1.0).contains(&f))
    {
        return Err(ImageError::InvalidInput(format!(
            "feather must be a finite value in 0.0..=1.0, got {f}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use image::{DynamicImage, Rgb, Rgba, RgbaImage};

    use super::{
        ColorReplaceOptions, LevelsOptions, PhotocopyOptions, levels, photocopy, replace_color,
    };
    use crate::crates::core::engine::EngineLimits;
    use crate::crates::image_engine::error::ImageError;
    use crate::crates::image_engine::ops::load::decode;

    /// Encodes a 4x4 PNG of uniform `rgb` pixels.
    fn png_of(rgb: Rgb<u8>) -> Vec<u8> {
        let mut img = image::RgbImage::new(4, 4);
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

    fn run_levels(input: &[u8], options: &LevelsOptions) -> Vec<u8> {
        levels(input, options, &EngineLimits::default()).expect("levels must succeed")
    }

    fn run_photocopy(input: &[u8], options: &PhotocopyOptions) -> Vec<u8> {
        photocopy(input, options, &EngineLimits::default()).expect("photocopy must succeed")
    }

    fn run_replace(input: &[u8], options: &ColorReplaceOptions) -> Vec<u8> {
        replace_color(input, options, &EngineLimits::default()).expect("replace_color must succeed")
    }

    // ---- levels ----

    #[test]
    fn levels_black_white_points_map_extremes() {
        let mut img = image::RgbImage::new(2, 1);
        img.put_pixel(0, 0, Rgb([50, 50, 50]));
        img.put_pixel(1, 0, Rgb([170, 170, 170]));
        let mut buf = Vec::new();
        DynamicImage::ImageRgb8(img)
            .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)
            .expect("test PNG must encode");

        let out = run_levels(
            &buf,
            &LevelsOptions {
                input_black: Some(50),
                input_white: Some(170),
                gamma: None,
            },
        );
        let decoded = decode(&out, &EngineLimits::default()).expect("must decode");
        let rgb = decoded.image.to_rgb8();
        assert_eq!(*rgb.get_pixel(0, 0), Rgb([0, 0, 0]));
        assert_eq!(*rgb.get_pixel(1, 0), Rgb([255, 255, 255]));
    }

    #[test]
    fn levels_gamma_brightens_midtones() {
        let input = png_of(Rgb([128, 128, 128]));
        let neutral = decode_rgb(&run_levels(
            &input,
            &LevelsOptions {
                gamma: Some(1.0),
                ..LevelsOptions::default()
            },
        ));
        let bright = decode_rgb(&run_levels(
            &input,
            &LevelsOptions {
                gamma: Some(0.5),
                ..LevelsOptions::default()
            },
        ));
        assert!(bright.0[0] > neutral.0[0], "{neutral:?} -> {bright:?}");
        assert_eq!(bright.0[0], bright.0[1]);
        assert_eq!(bright.0[1], bright.0[2]);
    }

    #[test]
    fn levels_rejects_invalid_options() {
        let input = png_of(Rgb([100, 100, 100]));
        let limits = EngineLimits::default();
        for options in [
            LevelsOptions::default(),
            LevelsOptions {
                input_black: Some(200),
                input_white: Some(100),
                gamma: None,
            },
            LevelsOptions {
                input_black: Some(100),
                input_white: Some(100),
                gamma: None,
            },
            LevelsOptions {
                gamma: Some(0.05),
                ..LevelsOptions::default()
            },
            LevelsOptions {
                gamma: Some(f32::NAN),
                ..LevelsOptions::default()
            },
        ] {
            assert!(
                matches!(
                    levels(&input, &options, &limits),
                    Err(ImageError::InvalidInput(_))
                ),
                "options {options:?} must be rejected"
            );
        }
    }

    // ---- photocopy ----

    #[test]
    fn photocopy_threshold_binarizes() {
        // Mid-gray above the default threshold of 128? 200 is above.
        let input = png_of(Rgb([200, 200, 200]));
        let out = run_photocopy(&input, &PhotocopyOptions::default());
        assert_eq!(decode_rgb(&out).0, [255, 255, 255]);

        let dark = png_of(Rgb([100, 100, 100]));
        let out = run_photocopy(&dark, &PhotocopyOptions::default());
        assert_eq!(decode_rgb(&out).0, [0, 0, 0]);
    }

    #[test]
    fn photocopy_custom_threshold_and_softness() {
        // With softness covering the full range, a mid value lands
        // mid-ramp rather than at an extreme.
        let input = png_of(Rgb([128, 128, 128]));
        let out = run_photocopy(
            &input,
            &PhotocopyOptions {
                threshold: Some(128),
                softness: Some(255),
                noise: None,
                dither: false,
            },
        );
        let px = decode_rgb(&out).0[0];
        assert!(px > 100 && px < 155, "soft edge value {px}");
    }

    #[test]
    fn photocopy_is_deterministic() {
        let mut img = image::RgbImage::new(8, 8);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = Rgb([(x * 30) as u8, (y * 30) as u8, 128]);
        }
        let mut buf = Vec::new();
        DynamicImage::ImageRgb8(img)
            .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)
            .expect("test PNG must encode");

        let options = PhotocopyOptions {
            noise: Some(40),
            ..PhotocopyOptions::default()
        };
        let a = run_photocopy(&buf, &options);
        let b = run_photocopy(&buf, &options);
        assert_eq!(a, b, "noise must be deterministic");
    }

    #[test]
    fn photocopy_noise_changes_output() {
        let mut img = image::RgbImage::new(16, 16);
        for p in img.pixels_mut() {
            *p = Rgb([140, 140, 140]); // just above default threshold
        }
        let mut buf = Vec::new();
        DynamicImage::ImageRgb8(img)
            .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)
            .expect("test PNG must encode");

        let clean = run_photocopy(&buf, &PhotocopyOptions::default());
        let noisy = run_photocopy(
            &buf,
            &PhotocopyOptions {
                noise: Some(60),
                ..PhotocopyOptions::default()
            },
        );
        assert_ne!(clean, noisy, "grain noise must alter the scan");
    }

    #[test]
    fn photocopy_preserves_alpha() {
        let mut img = RgbaImage::new(4, 4);
        for p in img.pixels_mut() {
            *p = Rgba([200, 200, 200, 128]);
        }
        let mut buf = Vec::new();
        DynamicImage::ImageRgba8(img)
            .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)
            .expect("test PNG must encode");

        let out = run_photocopy(&buf, &PhotocopyOptions::default());
        let decoded = decode(&out, &EngineLimits::default()).expect("must decode");
        let px = *decoded.image.to_rgba8().get_pixel(0, 0);
        assert_eq!(px.0[3], 128, "alpha must be preserved: {px:?}");
    }

    // ---- replace_color ----

    #[test]
    fn replace_color_swaps_exact_match() {
        let input = png_of(Rgb([10, 20, 30]));
        let out = run_replace(
            &input,
            &ColorReplaceOptions {
                source: [10, 20, 30],
                target: [200, 100, 50],
                tolerance: 0.0,
                feather: None,
            },
        );
        assert_eq!(decode_rgb(&out).0, [200, 100, 50]);
    }

    #[test]
    fn replace_color_tolerance_catches_near_match() {
        // Source (100,100,100); pixel (110,100,100) is distance 10,
        // normalized ~10/441 ≈ 0.023.
        let input = png_of(Rgb([110, 100, 100]));
        let out = run_replace(
            &input,
            &ColorReplaceOptions {
                source: [100, 100, 100],
                target: [0, 0, 0],
                tolerance: 0.05,
                feather: Some(0.0), // hard cut
            },
        );
        assert_eq!(decode_rgb(&out).0, [0, 0, 0]);
    }

    #[test]
    fn replace_color_leaves_far_colors_alone() {
        let input = png_of(Rgb([250, 10, 10]));
        let out = run_replace(
            &input,
            &ColorReplaceOptions {
                source: [10, 20, 30],
                target: [0, 0, 0],
                tolerance: 0.1,
                feather: None,
            },
        );
        let after = decode_rgb(&out).0;
        // PNG is lossless; pixel must be untouched.
        assert_eq!(after, [250, 10, 10]);
    }

    #[test]
    fn replace_color_feather_blends_edge() {
        // dist ≈ 0.023 sits inside a tolerance 0.05 / feather 0.5
        // band: inner = 0.025, so the pixel is at the inner edge.
        let input = png_of(Rgb([110, 100, 100]));
        let out = run_replace(
            &input,
            &ColorReplaceOptions {
                source: [100, 100, 100],
                target: [0, 0, 0],
                tolerance: 0.05,
                feather: Some(0.5),
            },
        );
        let px = decode_rgb(&out).0[0];
        // Fully inside inner radius → black.
        assert_eq!(px, 0);
    }

    #[test]
    fn replace_color_preserves_alpha() {
        let mut img = RgbaImage::new(4, 4);
        for p in img.pixels_mut() {
            *p = Rgba([10, 20, 30, 77]);
        }
        let mut buf = Vec::new();
        DynamicImage::ImageRgba8(img)
            .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)
            .expect("test PNG must encode");

        let out = run_replace(
            &buf,
            &ColorReplaceOptions {
                source: [10, 20, 30],
                target: [255, 0, 0],
                tolerance: 0.0,
                feather: None,
            },
        );
        let decoded = decode(&out, &EngineLimits::default()).expect("must decode");
        let px = *decoded.image.to_rgba8().get_pixel(0, 0);
        assert_eq!(px.0, [255, 0, 0, 77]);
    }

    #[test]
    fn replace_color_rejects_bad_params() {
        let input = png_of(Rgb([100, 100, 100]));
        let limits = EngineLimits::default();
        for options in [
            ColorReplaceOptions {
                source: [0, 0, 0],
                target: [0, 0, 0],
                tolerance: -0.1,
                feather: None,
            },
            ColorReplaceOptions {
                source: [0, 0, 0],
                target: [0, 0, 0],
                tolerance: 1.5,
                feather: None,
            },
            ColorReplaceOptions {
                source: [0, 0, 0],
                target: [0, 0, 0],
                tolerance: f32::NAN,
                feather: None,
            },
            ColorReplaceOptions {
                source: [0, 0, 0],
                target: [0, 0, 0],
                tolerance: 0.5,
                feather: Some(2.0),
            },
        ] {
            assert!(
                matches!(
                    replace_color(&input, &options, &limits),
                    Err(ImageError::InvalidInput(_))
                ),
                "options {options:?} must be rejected"
            );
        }
    }
}
