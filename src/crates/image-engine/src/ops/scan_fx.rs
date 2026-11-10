//! Scanner-cleanup / document-imaging operations: impulse denoise,
//! background normalization, best-effort deskew, and one-shot
//! scanner presets.
//!
//! All ops share the crate conventions: they decode through
//! [`super::load::decode`] (full `EngineLimits` enforcement),
//! normalize to 8-bit sRGB via [`super::load::normalize_8bit`], apply
//! a fixed per-op pixel pass, and re-encode in the input's sniffed
//! format. Alpha is preserved by every op.
//!
//! # Deskew
//!
//! [`deskew`] is best-effort and tuned for text pages: it estimates
//! the skew angle by maximizing the variance of the horizontal
//! projection profile (per-row darkness sums) over a ±`max_angle`
//! sweep, then rotates the image about its center. Photographs or
//! pages without strong horizontal structure may report a
//! meaningless angle — check [`DeskewReport::confidence`] before
//! trusting the result.
//!
//! # Determinism
//!
//! Every pass is a pure function of the pixel data — no RNG, no
//! time, no threads.

use serde::{Deserialize, Serialize};

use image::{DynamicImage, GenericImageView, ImageBuffer, Pixel};

use crate::crates::core::engine::EngineLimits;
use crate::crates::image_engine::error::{ImageError, ImageResult};

use super::color::for_each_color_channel;
use super::load::{decode, encode, normalize_8bit};

/// Applies `n` passes of a 3x3 per-channel median filter to `input`,
/// returning the encoded bytes in the input's sniffed format.
///
/// `strength` is the number of passes, `1..=5`: each pass replaces
/// every color channel sample with the median of its 3x3 neighborhood
/// (border pixels replicate the edge). Median filtering removes
/// impulse noise (salt-and-pepper, scanner dropouts) without blurring
/// edges the way a mean filter would; repeated passes widen the
/// effective kernel. Alpha is copied through unchanged.
///
/// Fails with [`ImageError::InvalidInput`] when `strength` is outside
/// `1..=5`, and with the usual decode/limit errors for malformed or
/// oversized inputs.
pub fn denoise(input: &[u8], strength: u8, limits: &EngineLimits) -> ImageResult<Vec<u8>> {
    if !(1..=5).contains(&strength) {
        return Err(ImageError::InvalidInput(format!(
            "strength must be in 1..=5, got {strength}"
        )));
    }

    let decoded = decode(input, limits)?;
    let mut image = normalize_8bit(decoded.image);

    for _ in 0..strength {
        image = match image {
            DynamicImage::ImageRgb8(buf) => DynamicImage::ImageRgb8(median_pass(&buf)),
            DynamicImage::ImageRgba8(buf) => DynamicImage::ImageRgba8(median_pass(&buf)),
            // normalize_8bit guarantees Rgb8/Rgba8; kept for safety.
            other => other,
        };
    }

    encode(&image, decoded.format)
}

/// One 3x3 median-filter pass over an 8-bit RGB(A) buffer.
///
/// Only the first three (color) channels are filtered; any alpha
/// channel is copied verbatim. Border handling replicates the edge
/// pixel.
fn median_pass<P>(img: &ImageBuffer<P, Vec<u8>>) -> ImageBuffer<P, Vec<u8>>
where
    P: Pixel<Subpixel = u8> + 'static,
{
    let (w, h) = img.dimensions();
    // Clamp bounds as i32 without overflow for absurdly wide images.
    let max_x = w.min(i32::MAX as u32) as i32 - 1;
    let max_y = h.min(i32::MAX as u32) as i32 - 1;
    let n_channels = usize::from(P::CHANNEL_COUNT);
    let mut out = ImageBuffer::new(w, h);
    let mut window = [0u8; 9];

    for y in 0..h {
        for x in 0..w {
            let mut result = [0u8; 4];
            for ch in 0..n_channels.min(4) {
                if ch < 3 {
                    let mut i = 0;
                    for dy in -1i32..=1 {
                        for dx in -1i32..=1 {
                            let sx = (x as i32 + dx).clamp(0, max_x) as u32;
                            let sy = (y as i32 + dy).clamp(0, max_y) as u32;
                            window[i] = img.get_pixel(sx, sy).channels()[ch];
                            i += 1;
                        }
                    }
                    window.sort_unstable();
                    result[ch] = window[4];
                } else {
                    // Alpha (and any further channel) passes through.
                    result[ch] = img.get_pixel(x, y).channels()[ch];
                }
            }
            *out.get_pixel_mut(x, y) = *P::from_slice(&result[..n_channels]);
        }
    }
    out
}

/// Estimated background level per channel, sampled on a grid.
struct BackgroundEstimate {
    /// Per-channel background gray level (0..=255).
    levels: [u8; 3],
}

/// Samples the image on a regular grid (at most ~4096 samples per
/// channel) and returns the 90th-percentile value per color channel —
/// a robust estimate of the "paper" level that ignores dark text and
/// marks.
fn estimate_background(img: &DynamicImage) -> BackgroundEstimate {
    let (w, h) = img.dimensions();
    // Aim for ~4096 samples: step = sqrt(total / 4096).
    let step = (((u64::from(w) * u64::from(h)) / 4096).max(1) as f64)
        .sqrt()
        .ceil() as u32;

    let mut samples = [Vec::<u8>::new(), Vec::new(), Vec::new()];
    let mut y = 0;
    while y < h {
        let mut x = 0;
        while x < w {
            let px = img.get_pixel(x, y);
            let ch = px.channels();
            for c in 0..3 {
                samples[c].push(ch[c]);
            }
            x += step;
        }
        y += step;
    }

    let mut levels = [255u8; 3];
    for (c, vals) in samples.iter_mut().enumerate() {
        if vals.is_empty() {
            continue;
        }
        vals.sort_unstable();
        // 90th percentile: the bright end of the distribution.
        let idx = (vals.len() * 9 / 10).min(vals.len() - 1);
        levels[c] = vals[idx];
    }
    BackgroundEstimate { levels }
}

/// Upper bound on the per-channel gain applied by [`clean_background`]
/// so a very dark "background" estimate cannot blow up the image.
const MAX_BACKGROUND_GAIN: f32 = 4.0;

/// Normalize the background to white.
///
/// The background level of each color channel is estimated as the
/// 90th percentile of a ~4096-point grid sample (see
/// [`estimate_background`]), then every channel is multiplied by
/// `255 / background`, clamped to a maximum gain of
/// [`MAX_BACKGROUND_GAIN`]. Multiplicative rescaling preserves the
/// dynamic range (contrast ratios between tones) while pushing the
/// estimated paper color toward pure white; dark foreground content
/// brightens proportionally. Channels whose estimate is already
/// ~white are effectively unchanged. Alpha is preserved.
///
/// This suits scans and photocopies with uneven illumination (darker
/// edges, gray paper); it is not a shadow-removal filter and will not
/// recover detail crushed to black.
pub fn clean_background(input: &[u8], limits: &EngineLimits) -> ImageResult<Vec<u8>> {
    let decoded = decode(input, limits)?;
    let mut image = normalize_8bit(decoded.image);

    let bg = estimate_background(&image);
    let gains = [
        background_gain(bg.levels[0]),
        background_gain(bg.levels[1]),
        background_gain(bg.levels[2]),
    ];

    for_each_color_channel(&mut image, &mut |ch: &mut [u8]| {
        for i in 0..3 {
            let v = f32::from(ch[i]) * gains[i];
            ch[i] = v.round().clamp(0.0, 255.0) as u8;
        }
    });

    encode(&image, decoded.format)
}

/// Per-channel rescale factor mapping the estimated background to
/// white: `255 / bg`, capped at [`MAX_BACKGROUND_GAIN`]. Since
/// `bg <= 255` the factor is always >= 1.0 (pure brightening).
fn background_gain(bg: u8) -> f32 {
    if bg == 0 {
        return MAX_BACKGROUND_GAIN;
    }
    (255.0 / f32::from(bg)).min(MAX_BACKGROUND_GAIN)
}

/// Report returned by [`deskew`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DeskewReport {
    /// Best-estimate skew angle in degrees. Positive/negative follow
    /// the sweep convention of the rotation used internally; the
    /// magnitude is what matters for text pages.
    pub angle_deg: f64,

    /// Confidence in `[0.0, 1.0]`, derived from how much the winning
    /// angle's projection-profile variance stands out from the mean
    /// across the sweep. Near 0 means the image has no clear
    /// horizontal structure (blank page, photo, or angle beyond
    /// `max_angle`).
    pub confidence: f32,
}

/// Output of [`deskew`]: the deskewed image bytes plus its report.
#[derive(Debug, Clone, PartialEq)]
pub struct DeskewResult {
    /// Deskewed image encoded in the input's sniffed format.
    pub image: Vec<u8>,
    /// The estimated angle and confidence.
    pub report: DeskewReport,
}

/// Default `max_angle` for [`deskew`]: ±5 degrees.
pub const DEFAULT_DESKEW_ANGLE: f64 = 5.0;

/// Largest accepted `max_angle` for [`deskew`].
const MAX_DESKEW_ANGLE: f64 = 45.0;

/// Sweep step for the angle search, in degrees.
const ANGLE_STEP: f64 = 0.25;

/// Largest dimension of the grayscale buffer used for skew analysis.
const ANALYSIS_MAX_DIM: u32 = 256;

/// Estimate and remove skew from `input`, returning the rotated image
/// bytes and a [`DeskewReport`].
///
/// The angle is found by sweeping `-max_angle..=max_angle` in
/// [`ANGLE_STEP`] steps; at each step a small grayscale copy of the
/// image is rotated about its center and the variance of its
/// horizontal projection profile (row sums of darkness) is measured.
/// Text lines produce a sharply peaked profile when the page is
/// level, so the angle with maximum variance wins.
///
/// `max_angle` of `None` uses [`DEFAULT_DESKEW_ANGLE`] (±5°). Must be
/// finite and within `(0.0, MAX_DESKEW_ANGLE]`.
///
/// Best-effort: suited to text pages. The output has the same
/// dimensions as the input; corners exposed by rotation are filled
/// white.
pub fn deskew(
    input: &[u8],
    max_angle: Option<f64>,
    limits: &EngineLimits,
) -> ImageResult<DeskewResult> {
    let max_angle = max_angle.unwrap_or(DEFAULT_DESKEW_ANGLE);
    if !max_angle.is_finite() || max_angle <= 0.0 || max_angle > MAX_DESKEW_ANGLE {
        return Err(ImageError::InvalidInput(format!(
            "max_angle must be a finite value in 0.0..={MAX_DESKEW_ANGLE}, got {max_angle}"
        )));
    }

    let decoded = decode(input, limits)?;
    let image = normalize_8bit(decoded.image);

    let best = estimate_skew(&image, max_angle);

    let rotated = if best.angle_deg.abs() < f64::EPSILON {
        image.clone()
    } else {
        rotate_image(&image, best.angle_deg)
    };

    Ok(DeskewResult {
        image: encode(&rotated, decoded.format)?,
        report: DeskewReport {
            angle_deg: best.angle_deg,
            confidence: best.confidence,
        },
    })
}

/// Internal result of the skew sweep.
struct SkewEstimate {
    angle_deg: f64,
    confidence: f32,
}

/// Builds a small "darkness" buffer (0 = white background, 255 = black
/// ink) for analysis, sampled at most [`ANALYSIS_MAX_DIM`] pixels on
/// the long side.
fn darkness_buffer(img: &DynamicImage) -> (Vec<u8>, u32, u32) {
    let (w, h) = img.dimensions();
    let step = (w.max(h) + ANALYSIS_MAX_DIM - 1) / ANALYSIS_MAX_DIM;
    let step = step.max(1);
    let sw = w.div_ceil(step);
    let sh = h.div_ceil(step);

    let mut dark = vec![0u8; (sw * sh) as usize];
    for sy in 0..sh {
        for sx in 0..sw {
            // Sample the top-left pixel of each `step`-sized cell.
            let px = img.get_pixel(sx * step, sy * step);
            let ch = px.channels();
            // Rec. 709 luma, matching `imageops::grayscale`.
            let luma =
                0.2126 * f32::from(ch[0]) + 0.7152 * f32::from(ch[1]) + 0.0722 * f32::from(ch[2]);
            dark[(sy * sw + sx) as usize] = (255.0 - luma).round().clamp(0.0, 255.0) as u8;
        }
    }
    (dark, sw, sh)
}

/// Rotates a grayscale darkness buffer about its center by `deg`
/// degrees using nearest-neighbor sampling; out-of-bounds samples
/// read as 0 (white). Output keeps the input dimensions.
fn rotate_darkness(src: &[u8], w: u32, h: u32, deg: f64) -> Vec<u8> {
    let rad = deg.to_radians();
    let (sin, cos) = rad.sin_cos();
    let cx = f64::from(w) / 2.0 - 0.5;
    let cy = f64::from(h) / 2.0 - 0.5;
    let mut out = vec![0u8; src.len()];

    for y in 0..h {
        for x in 0..w {
            let dx = f64::from(x) - cx;
            let dy = f64::from(y) - cy;
            // Inverse rotation: map output coords back into the source.
            let sx = cx + dx * cos + dy * sin;
            let sy = cy - dx * sin + dy * cos;
            let ix = sx.round() as i64;
            let iy = sy.round() as i64;
            let v = if ix >= 0 && iy >= 0 && ix < i64::from(w) && iy < i64::from(h) {
                src[(iy as u32 * w + ix as u32) as usize]
            } else {
                0
            };
            out[(y * w + x) as usize] = v;
        }
    }
    out
}

/// Variance of the horizontal projection profile: per-row sums of
/// darkness. High when dark content forms horizontal bands (level
/// text lines); low when content is diagonal or uniform.
fn projection_variance(rows: &[u64]) -> f64 {
    if rows.is_empty() {
        return 0.0;
    }
    let n = rows.len() as f64;
    let mean = rows.iter().sum::<u64>() as f64 / n;
    let var = rows
        .iter()
        .map(|&v| {
            let d = v as f64 - mean;
            d * d
        })
        .sum::<f64>()
        / n;
    var
}

/// Sweeps angles and returns the one maximizing projection-profile
/// variance, plus a confidence score.
fn estimate_skew(img: &DynamicImage, max_angle: f64) -> SkewEstimate {
    let (dark, w, h) = darkness_buffer(img);

    let steps = (max_angle / ANGLE_STEP).round() as i64;
    let mut best_angle = 0.0f64;
    let mut best_var = -1.0f64;
    let mut total_var = 0.0f64;
    let mut count = 0u64;

    for k in -steps..=steps {
        let angle = k as f64 * ANGLE_STEP;
        let rotated = rotate_darkness(&dark, w, h, angle);

        // Row sums of darkness.
        let mut rows = vec![0u64; h as usize];
        for (i, &v) in rotated.iter().enumerate() {
            rows[i / w as usize] += u64::from(v);
        }
        let var = projection_variance(&rows);
        total_var += var;
        count += 1;
        if var > best_var {
            best_var = var;
            best_angle = angle;
        }
    }

    let mean_var = if count > 0 {
        total_var / count as f64
    } else {
        0.0
    };
    // Confidence: how far the winner stands out from the average.
    // A flat image gives best ≈ mean → ~0; a strong text page gives a
    // large ratio → approaching 1.
    let confidence = if best_var > 0.0 {
        ((best_var - mean_var) / best_var).clamp(0.0, 1.0) as f32
    } else {
        0.0
    };

    SkewEstimate {
        angle_deg: best_angle,
        confidence,
    }
}

/// Rotates `img` (normalized `Rgb8`/`Rgba8`) about its center by
/// `deg` degrees using bilinear interpolation; out-of-bounds samples
/// read as white (255 per channel, including alpha → opaque).
fn rotate_image(img: &DynamicImage, deg: f64) -> DynamicImage {
    match img {
        DynamicImage::ImageRgb8(buf) => DynamicImage::ImageRgb8(rotate_buffer(buf, deg)),
        DynamicImage::ImageRgba8(buf) => DynamicImage::ImageRgba8(rotate_buffer(buf, deg)),
        other => other.clone(),
    }
}

/// Bilinear rotation of an 8-bit buffer about its center. The output
/// keeps the input dimensions; corners exposed by the rotation are
/// filled with 255 in every channel.
fn rotate_buffer<P>(img: &ImageBuffer<P, Vec<u8>>, deg: f64) -> ImageBuffer<P, Vec<u8>>
where
    P: Pixel<Subpixel = u8> + 'static,
{
    let (w, h) = img.dimensions();
    let n_channels = usize::from(P::CHANNEL_COUNT).min(4);
    let rad = deg.to_radians();
    let (sin, cos) = rad.sin_cos();
    let cx = f64::from(w) / 2.0 - 0.5;
    let cy = f64::from(h) / 2.0 - 0.5;
    let mut out = ImageBuffer::new(w, h);

    for y in 0..h {
        for x in 0..w {
            let dx = f64::from(x) - cx;
            let dy = f64::from(y) - cy;
            let sx = cx + dx * cos + dy * sin;
            let sy = cy - dx * sin + dy * cos;

            let x0 = sx.floor() as i64;
            let y0 = sy.floor() as i64;
            let tx = sx - x0 as f64;
            let ty = sy - y0 as f64;

            let sample = |ix: i64, iy: i64, ch: usize| -> f64 {
                if ix >= 0 && iy >= 0 && ix < i64::from(w) && iy < i64::from(h) {
                    f64::from(img.get_pixel(ix as u32, iy as u32).channels()[ch])
                } else {
                    255.0 // white fill
                }
            };

            let mut result = [0u8; 4];
            for ch in 0..n_channels {
                let top = sample(x0, y0, ch) * (1.0 - tx) + sample(x0 + 1, y0, ch) * tx;
                let bot = sample(x0, y0 + 1, ch) * (1.0 - tx) + sample(x0 + 1, y0 + 1, ch) * tx;
                let v = top * (1.0 - ty) + bot * ty;
                result[ch] = v.round().clamp(0.0, 255.0) as u8;
            }
            *out.get_pixel_mut(x, y) = *P::from_slice(&result[..n_channels]);
        }
    }
    out
}

/// Scanner-cleanup preset modes for [`scanner_preset`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScannerMode {
    /// General document cleanup: background normalization followed by
    /// a light median denoise. Suited to ordinary scans and photos of
    /// pages.
    Clean,

    /// Photocopy simulation: background normalization, light denoise,
    /// then a hard-threshold grayscale "scan" via the
    /// [`super::effects::photocopy`] op with default options.
    Photocopy,

    /// High-contrast document mode: background normalization, light
    /// denoise, then an auto-levels contrast stretch (shared across
    /// color channels) via the [`super::color::color`] op.
    HighContrast,
}

/// Apply a scanner-cleanup preset to `input`, returning the encoded
/// bytes in the input's sniffed format.
///
/// Each mode composes the ops in this module (plus the existing
/// [`super::effects::photocopy`] and [`super::color::color`] ops):
///
/// - [`ScannerMode::Clean`]: [`clean_background`] → [`denoise`] (2 passes)
/// - [`ScannerMode::Photocopy`]: [`clean_background`] → [`denoise`] (1 pass)
///   → grayscale threshold scan
/// - [`ScannerMode::HighContrast`]: [`clean_background`] → [`denoise`] (1 pass)
///   → auto-levels stretch
///
/// Fails with the usual decode/limit errors for malformed or
/// oversized inputs.
pub fn scanner_preset(
    input: &[u8],
    mode: ScannerMode,
    limits: &EngineLimits,
) -> ImageResult<Vec<u8>> {
    match mode {
        ScannerMode::Clean => {
            let cleaned = clean_background(input, limits)?;
            denoise(&cleaned, 2, limits)
        }
        ScannerMode::Photocopy => {
            let cleaned = clean_background(input, limits)?;
            let denoised = denoise(&cleaned, 1, limits)?;
            super::effects::photocopy(
                &denoised,
                &super::effects::PhotocopyOptions::default(),
                limits,
            )
        }
        ScannerMode::HighContrast => {
            let cleaned = clean_background(input, limits)?;
            let denoised = denoise(&cleaned, 1, limits)?;
            super::color::color(
                &denoised,
                &super::color::ColorOptions {
                    high_contrast: Some(super::color::HighContrastOptions { per_channel: false }),
                    ..Default::default()
                },
                limits,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use image::{DynamicImage, Rgb, RgbImage};

    use super::{
        DEFAULT_DESKEW_ANGLE, ScannerMode, clean_background, denoise, deskew, scanner_preset,
    };
    use crate::crates::core::engine::EngineLimits;
    use crate::crates::image_engine::error::ImageError;
    use crate::crates::image_engine::ops::load::decode;

    fn encode_png(img: DynamicImage) -> Vec<u8> {
        let mut buf = Vec::new();
        img.write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)
            .expect("test PNG must encode");
        buf
    }

    fn decode_rgb(bytes: &[u8]) -> image::RgbImage {
        decode(bytes, &EngineLimits::default())
            .expect("output must decode")
            .image
            .to_rgb8()
    }

    /// 48x48 mid-gray image with deterministic impulse noise.
    fn noisy_gray_png() -> Vec<u8> {
        let mut img = RgbImage::new(48, 48);
        for (x, y, px) in img.enumerate_pixels_mut() {
            let base = 128u8;
            // Sparse deterministic impulses: salt and pepper.
            let impulse = (x.wrapping_mul(7).wrapping_add(y.wrapping_mul(13)) % 11 == 0) as u8;
            let v = if impulse == 1 {
                if (x + y) % 2 == 0 { 255 } else { 0 }
            } else {
                base
            };
            *px = Rgb([v, v, v]);
        }
        encode_png(DynamicImage::ImageRgb8(img))
    }

    // ---- denoise ----

    #[test]
    fn denoise_reduces_impulse_noise() {
        let input = noisy_gray_png();
        let before = decode_rgb(&input);
        let metric = |img: &image::RgbImage| -> f64 {
            let mut sum = 0f64;
            let mut n = 0u64;
            for px in img.pixels() {
                sum += f64::from(px.0[0].abs_diff(128));
                n += 1;
            }
            sum / n as f64
        };
        let before_metric = metric(&before);

        let out = denoise(&input, 1, &EngineLimits::default()).expect("denoise must succeed");
        let after = decode_rgb(&out);
        let after_metric = metric(&after);

        assert!(
            after_metric < before_metric,
            "noise metric must drop: {before_metric} -> {after_metric}"
        );
    }

    #[test]
    fn denoise_preserves_smooth_regions() {
        // A flat gray field has nothing to remove; output stays flat.
        let mut img = RgbImage::new(16, 16);
        for p in img.pixels_mut() {
            *p = Rgb([100, 120, 140]);
        }
        let input = encode_png(DynamicImage::ImageRgb8(img));
        let out = denoise(&input, 3, &EngineLimits::default()).expect("denoise must succeed");
        let rgb = decode_rgb(&out);
        assert_eq!(*rgb.get_pixel(8, 8), Rgb([100, 120, 140]));
    }

    #[test]
    fn denoise_rejects_invalid_strength() {
        let input = noisy_gray_png();
        let limits = EngineLimits::default();
        for strength in [0u8, 6, 255] {
            assert!(matches!(
                denoise(&input, strength, &limits),
                Err(ImageError::InvalidInput(_))
            ));
        }
    }

    // ---- clean_background ----

    #[test]
    fn clean_background_brightens_gradient_edges() {
        // Horizontal gradient: bright (220) on the left fading to 150
        // on the right. The estimated background is ~220, so the dark
        // right edge should be rescaled upward.
        let mut img = RgbImage::new(64, 64);
        for (x, _, px) in img.enumerate_pixels_mut() {
            // 220 on the left fading linearly to 150 on the right.
            let v = 220i32 - x as i32 * 70 / 63;
            *px = Rgb([v as u8, v as u8, v as u8]);
        }
        let input = encode_png(DynamicImage::ImageRgb8(img));

        let out = clean_background(&input, &EngineLimits::default())
            .expect("clean_background must succeed");
        let after = decode_rgb(&out);

        let before_right = 150u8; // rightmost column value
        let after_right = after.get_pixel(63, 32).0[0];
        assert!(
            after_right > before_right,
            "dark edge should brighten: {before_right} -> {after_right}"
        );
        // Left edge was already near-white; it may clip at 255.
        assert!(after.get_pixel(0, 32).0[0] >= 220);
    }

    #[test]
    fn clean_background_is_stable_on_white() {
        let mut img = RgbImage::new(16, 16);
        for p in img.pixels_mut() {
            *p = Rgb([250, 250, 250]);
        }
        let input = encode_png(DynamicImage::ImageRgb8(img));
        let out = clean_background(&input, &EngineLimits::default())
            .expect("clean_background must succeed");
        let rgb = decode_rgb(&out);
        // bg ≈ 250 → gain ≈ 1.02 → still essentially white.
        assert!(rgb.get_pixel(8, 8).0[0] >= 250);
    }

    // ---- deskew ----

    /// Horizontal black stripes on white: perfectly level text-like
    /// structure.
    fn striped_png(tilt_per_x: f64) -> Vec<u8> {
        let (w, h) = (200u32, 200u32);
        let mut img = RgbImage::new(w, h);
        for (x, y, px) in img.enumerate_pixels_mut() {
            // Dark bands where the tilted coordinate crosses a period.
            let coord = y as f64 + tilt_per_x * x as f64;
            let band = (coord % 10.0) < 3.0;
            let v = if band { 0u8 } else { 255u8 };
            *px = Rgb([v, v, v]);
        }
        encode_png(DynamicImage::ImageRgb8(img))
    }

    #[test]
    fn deskew_level_stripes_reports_small_angle() {
        let input = striped_png(0.0);
        let result = deskew(&input, None, &EngineLimits::default()).expect("deskew must succeed");
        assert!(
            result.report.angle_deg.abs() <= 0.5,
            "level stripes should give ~0°, got {}",
            result.report.angle_deg
        );
        assert!(result.report.confidence > 0.0);
        // Output must be a decodable image of the same size.
        let decoded = decode(&result.image, &EngineLimits::default()).expect("must decode");
        assert_eq!(decoded.image.width(), 200);
        assert_eq!(decoded.image.height(), 200);
    }

    #[test]
    fn deskew_tilted_stripes_detects_angle() {
        // Lines tilted by ~3° (tan(3°) ≈ 0.0524).
        let input = striped_png(0.0524);
        let result =
            deskew(&input, Some(5.0), &EngineLimits::default()).expect("deskew must succeed");
        let a = result.report.angle_deg.abs();
        assert!(
            (1.0..=5.0).contains(&a),
            "expected ~3° correction, got {}",
            result.report.angle_deg
        );
    }

    #[test]
    fn deskew_rejects_invalid_angle() {
        let input = striped_png(0.0);
        let limits = EngineLimits::default();
        for bad in [
            Some(f64::NAN),
            Some(f64::INFINITY),
            Some(0.0),
            Some(-1.0),
            Some(46.0),
        ] {
            assert!(
                matches!(
                    deskew(&input, bad, &limits),
                    Err(ImageError::InvalidInput(_))
                ),
                "max_angle {bad:?} must be rejected"
            );
        }
        // Boundary values accepted.
        assert!(deskew(&input, Some(0.25), &limits).is_ok());
        assert!(deskew(&input, Some(45.0), &limits).is_ok());
        let default_angle = deskew(&input, None, &limits)
            .expect("default must work")
            .report
            .angle_deg;
        assert!(default_angle.abs() <= DEFAULT_DESKEW_ANGLE + ANGLE_STEP_FOR_TEST);
    }

    const ANGLE_STEP_FOR_TEST: f64 = 0.25;

    // ---- scanner_preset ----

    fn small_doc_png() -> Vec<u8> {
        let mut img = RgbImage::new(32, 32);
        for (_x, y, px) in img.enumerate_pixels_mut() {
            let v = if (y / 4) % 2 == 0 { 60 } else { 200 };
            *px = Rgb([v as u8, v as u8, v as u8]);
        }
        encode_png(DynamicImage::ImageRgb8(img))
    }

    #[test]
    fn presets_produce_decodable_bytes() {
        let input = small_doc_png();
        let limits = EngineLimits::default();
        for mode in [
            ScannerMode::Clean,
            ScannerMode::Photocopy,
            ScannerMode::HighContrast,
        ] {
            let out = scanner_preset(&input, mode, &limits).expect("preset must succeed");
            assert!(!out.is_empty());
            decode(&out, &limits).expect("preset output must decode");
        }
    }
}
