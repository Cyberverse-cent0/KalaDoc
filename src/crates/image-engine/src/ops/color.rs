//! Color manipulation: saturation, per-channel RGB gain/offset,
//! grayscale conversion, inversion, and auto-levels (high-contrast)
//! stretching.
//!
//! All adjustments are optional: [`ColorOptions`] fields left unset
//! are skipped entirely. A fixed pipeline order is applied when
//! several are set — channel gain/offset → saturation → high-contrast
//! stretch → grayscale → invert — so results are deterministic
//! regardless of field order in the struct.
//!
//! # Color-type handling
//!
//! Operations are computed in 8-bit sRGB, matching [`super::adjust`]:
//! `Rgb8`/`Rgba8` inputs are processed natively and every other
//! variant is first normalized with `to_rgb8()`/`to_rgba8()`, so
//! re-encoded output is always `Rgb8`, `Rgba8`, `Luma8`, or `La8`
//! pixels. Grayscale conversion uses the `image` crate's
//! [`imageops::grayscale`]/[`imageops::grayscale_alpha`] (Rec. 709
//! luma); alpha is preserved through grayscale and inversion, and is
//! never touched by channel gain/offset, saturation, or the contrast
//! stretch (alpha is channel index 3 and is excluded from all passes).
//!
//! JPEG encoding flattens alpha as usual (see the crate docs).

use serde::{Deserialize, Serialize};

use image::{DynamicImage, Pixel};

use crate::crates::core::engine::EngineLimits;
use crate::crates::image_engine::error::{ImageError, ImageResult};

use super::load::{decode, encode, normalize_8bit};

/// Parameters for [`color`]. Every field is optional; unset fields are
/// skipped. At least one adjustment must be enabled.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct ColorOptions {
    /// Independent per-channel gain and offset, applied first.
    ///
    /// Each channel is computed as `clamp(round(value * gain +
    /// offset))` on the 0..=255 scale. `gain` defaults to `1.0`
    /// (neutral) and `offset` to `0` per channel; a channel whose gain
    /// and offset are both `None` is left untouched.
    pub channels: Option<ChannelAdjust>,

    /// Saturation adjustment as a percentage, `-100.0..=100.0`,
    /// mapped onto an HSV saturation multiplier of
    /// `1.0 + value / 100.0` (so `-100.0` yields factor `0.0` —
    /// full grayscale — and `100.0` doubles saturation).
    ///
    /// Implemented by converting each pixel to HSV, scaling `s`, and
    /// converting back; hue and value are preserved.
    pub saturation: Option<f32>,

    /// Auto-levels stretch ("high contrast"): remaps the darkest and
    /// brightest input values to 0 and 255.
    ///
    /// With [`HighContrastOptions::per_channel`] `false` (the
    /// default), one shared min/max is computed across all three
    /// color channels; when `true`, each channel is stretched
    /// independently. A channel whose min equals its max is left
    /// unchanged. Applied before grayscale conversion.
    pub high_contrast: Option<HighContrastOptions>,

    /// Convert to grayscale (Rec. 709 luma). Alpha is preserved:
    /// `Rgba8` input becomes `La8`, `Rgb8` becomes `Luma8`.
    #[serde(default)]
    pub grayscale: bool,

    /// Invert every color channel (`255 - value`). Alpha is
    /// preserved. Applied last, after grayscale conversion.
    #[serde(default)]
    pub invert: bool,
}

/// Independent gain/offset adjustments for the red, green, and blue
/// channels. Every field is optional; `None` means neutral.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct ChannelAdjust {
    /// Red channel multiplier, `0.0..=4.0`. `1.0`/`None` is neutral.
    pub red_gain: Option<f32>,
    /// Green channel multiplier, `0.0..=4.0`. `1.0`/`None` is neutral.
    pub green_gain: Option<f32>,
    /// Blue channel multiplier, `0.0..=4.0`. `1.0`/`None` is neutral.
    pub blue_gain: Option<f32>,
    /// Red channel offset added after gain, `-255..=255`.
    /// `0`/`None` is neutral.
    pub red_offset: Option<i32>,
    /// Green channel offset added after gain, `-255..=255`.
    /// `0`/`None` is neutral.
    pub green_offset: Option<i32>,
    /// Blue channel offset added after gain, `-255..=255`.
    /// `0`/`None` is neutral.
    pub blue_offset: Option<i32>,
}

impl ChannelAdjust {
    /// True when every field is `None` (nothing to apply).
    fn is_empty(&self) -> bool {
        self.red_gain.is_none()
            && self.green_gain.is_none()
            && self.blue_gain.is_none()
            && self.red_offset.is_none()
            && self.green_offset.is_none()
            && self.blue_offset.is_none()
    }
}

/// Options for the auto-levels (high-contrast) stretch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HighContrastOptions {
    /// When `true`, stretch each of R, G, B independently; when
    /// `false` (default), compute one shared min/max across all
    /// color channels.
    #[serde(default)]
    pub per_channel: bool,
}

/// Apply the requested color adjustments to `input` and return the
/// encoded bytes in the input's sniffed format.
///
/// Fails with [`ImageError::InvalidInput`] when no adjustment is
/// enabled or a value is outside its documented range, and with the
/// usual decode/limit errors for malformed or oversized inputs.
pub fn color(input: &[u8], options: &ColorOptions, limits: &EngineLimits) -> ImageResult<Vec<u8>> {
    validate(options)?;

    let decoded = decode(input, limits)?;
    // Normalize to 8-bit RGB(A), exactly like `adjust`: other color
    // types gain an RGB(A) type and alpha-carrying formats keep alpha.
    let mut image = normalize_8bit(decoded.image);

    if let Some(channels) = options.channels {
        apply_channel_adjust(&mut image, &channels);
    }
    if let Some(saturation) = options.saturation {
        apply_saturation(&mut image, saturation);
    }
    if let Some(high_contrast) = options.high_contrast {
        apply_high_contrast(&mut image, high_contrast.per_channel);
    }
    if options.grayscale {
        image = image.grayscale();
    }
    if options.invert {
        image.invert();
    }

    encode(&image, decoded.format)
}

/// Rejects out-of-range values and option sets with no effective
/// adjustment.
fn validate(options: &ColorOptions) -> ImageResult<()> {
    let any = options.channels.is_some()
        || options.saturation.is_some()
        || options.high_contrast.is_some()
        || options.grayscale
        || options.invert;
    if !any {
        return Err(ImageError::InvalidInput(
            "at least one color adjustment must be enabled".to_string(),
        ));
    }
    if let Some(s) = options.saturation
        && (!s.is_finite() || !(-100.0..=100.0).contains(&s))
    {
        return Err(ImageError::InvalidInput(format!(
            "saturation must be a finite value in -100.0..=100.0, got {s}"
        )));
    }
    if let Some(channels) = options.channels {
        if channels.is_empty() {
            return Err(ImageError::InvalidInput(
                "channels is set but every gain and offset is None".to_string(),
            ));
        }
        for (name, gain, offset) in [
            ("red", channels.red_gain, channels.red_offset),
            ("green", channels.green_gain, channels.green_offset),
            ("blue", channels.blue_gain, channels.blue_offset),
        ] {
            if let Some(g) = gain
                && (!g.is_finite() || !(0.0..=4.0).contains(&g))
            {
                return Err(ImageError::InvalidInput(format!(
                    "{name}_gain must be a finite value in 0.0..=4.0, got {g}"
                )));
            }
            if let Some(o) = offset
                && !(-255..=255).contains(&o)
            {
                return Err(ImageError::InvalidInput(format!(
                    "{name}_offset must be in -255..=255, got {o}"
                )));
            }
        }
    }
    Ok(())
}

/// Calls `f` with a mutable slice of the first three (color) channels
/// of every pixel. Only valid on the normalized `Rgb8`/`Rgba8` forms;
/// the alpha channel (index 3) is never included.
pub(crate) fn for_each_color_channel(img: &mut DynamicImage, mut f: impl FnMut(&mut [u8])) {
    match img {
        DynamicImage::ImageRgb8(buf) => {
            for pixel in buf.pixels_mut() {
                f(&mut pixel.channels_mut()[..3]);
            }
        }
        DynamicImage::ImageRgba8(buf) => {
            for pixel in buf.pixels_mut() {
                f(&mut pixel.channels_mut()[..3]);
            }
        }
        _ => unreachable!("color ops run on normalized rgb8/rgba8 buffers"),
    }
}

/// Read-only counterpart of [`for_each_color_channel`].
pub(crate) fn for_each_color_channel_ref(img: &DynamicImage, mut f: impl FnMut(&[u8])) {
    match img {
        DynamicImage::ImageRgb8(buf) => {
            for pixel in buf.pixels() {
                f(&pixel.channels()[..3]);
            }
        }
        DynamicImage::ImageRgba8(buf) => {
            for pixel in buf.pixels() {
                f(&pixel.channels()[..3]);
            }
        }
        _ => unreachable!("color ops run on normalized rgb8/rgba8 buffers"),
    }
}

/// Applies `value * gain + offset` per channel, clamped to 0..=255.
fn apply_channel_adjust(img: &mut DynamicImage, adj: &ChannelAdjust) {
    let gains = [
        adj.red_gain.unwrap_or(1.0),
        adj.green_gain.unwrap_or(1.0),
        adj.blue_gain.unwrap_or(1.0),
    ];
    let offsets = [
        adj.red_offset.unwrap_or(0),
        adj.green_offset.unwrap_or(0),
        adj.blue_offset.unwrap_or(0),
    ];
    for_each_color_channel(img, &mut |ch: &mut [u8]| {
        for (i, v) in ch.iter_mut().enumerate() {
            let adjusted = f32::from(*v) * gains[i] + offsets[i] as f32;
            *v = adjusted.round().clamp(0.0, 255.0) as u8;
        }
    });
}

/// Scales HSV saturation by `1.0 + percent / 100.0` per pixel.
fn apply_saturation(img: &mut DynamicImage, percent: f32) {
    let factor = 1.0 + percent / 100.0;
    for_each_color_channel(img, &mut |ch: &mut [u8]| {
        let (h, s, v) = rgb_to_hsv(ch[0], ch[1], ch[2]);
        let (r, g, b) = hsv_to_rgb(h, (s * factor).clamp(0.0, 1.0), v);
        ch[0] = r;
        ch[1] = g;
        ch[2] = b;
    });
}

/// Stretches the color-channel range to 0..=255 via a lookup table.
fn apply_high_contrast(img: &mut DynamicImage, per_channel: bool) {
    let mut mins = [u8::MAX; 3];
    let mut maxs = [u8::MIN; 3];
    let mut seen = false;
    for_each_color_channel_ref(img, &mut |ch: &[u8]| {
        seen = true;
        for (i, &v) in ch.iter().enumerate() {
            mins[i] = mins[i].min(v);
            maxs[i] = maxs[i].max(v);
        }
    });
    if !seen {
        return;
    }

    let luts: [[u8; 256]; 3] = if per_channel {
        [
            levels_lut(mins[0], maxs[0]),
            levels_lut(mins[1], maxs[1]),
            levels_lut(mins[2], maxs[2]),
        ]
    } else {
        let shared = levels_lut(
            mins.into_iter().min().unwrap_or(0),
            maxs.into_iter().max().unwrap_or(0),
        );
        [shared, shared, shared]
    };

    for_each_color_channel(img, &mut |ch: &mut [u8]| {
        for (i, v) in ch.iter_mut().enumerate() {
            *v = luts[i][usize::from(*v)];
        }
    });
}

/// Builds a 256-entry stretch LUT mapping `[lo, hi]` onto `0..=255`.
/// Inputs outside the range clamp to 0/255; `lo == hi` is identity.
fn levels_lut(lo: u8, hi: u8) -> [u8; 256] {
    let mut lut = [0u8; 256];
    for (i, v) in lut.iter_mut().enumerate() {
        *v = i as u8;
    }
    let lo = u32::from(lo);
    let hi = u32::from(hi);
    let range = hi.saturating_sub(lo);
    if range == 0 {
        return lut;
    }
    for (i, v) in lut.iter_mut().enumerate() {
        let clamped = (i as u32).clamp(lo, hi) - lo;
        *v = ((clamped * 255 + range / 2) / range).min(255) as u8;
    }
    lut
}

/// Converts an 8-bit RGB triple to `(hue_degrees, saturation, value)`
/// with hue in `0.0..360.0` and s/v in `0.0..=1.0`.
fn rgb_to_hsv(r: u8, g: u8, b: u8) -> (f32, f32, f32) {
    let r = f32::from(r) / 255.0;
    let g = f32::from(g) / 255.0;
    let b = f32::from(b) / 255.0;

    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;

    let h = if d == 0.0 {
        0.0
    } else if max == r {
        60.0 * ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    let s = if max == 0.0 { 0.0 } else { d / max };
    (h, s, max)
}

/// Converts `(hue_degrees, saturation, value)` back to an 8-bit RGB
/// triple. Hue is expected in `0.0..360.0`, s/v in `0.0..=1.0`.
fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (u8, u8, u8) {
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = v - c;
    let (r, g, b) = if h < 60.0 {
        (c, x, 0.0)
    } else if h < 120.0 {
        (x, c, 0.0)
    } else if h < 180.0 {
        (0.0, c, x)
    } else if h < 240.0 {
        (0.0, x, c)
    } else if h < 300.0 {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };
    let to_u8 = |c: f32| ((c + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    (to_u8(r), to_u8(g), to_u8(b))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use image::{DynamicImage, Rgb, Rgba, RgbaImage};

    use super::{ChannelAdjust, ColorOptions, HighContrastOptions, color};
    use crate::crates::core::engine::EngineLimits;
    use crate::crates::image_engine::error::ImageError;
    use crate::crates::image_engine::ops::format::ImageFormat;
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

    fn run(input: &[u8], options: &ColorOptions) -> Vec<u8> {
        color(input, options, &EngineLimits::default()).expect("color must succeed")
    }

    #[test]
    fn saturation_zero_grays_colors() {
        let input = png_of(Rgb([200, 100, 50]));
        let out = run(
            &input,
            &ColorOptions {
                saturation: Some(-100.0),
                ..ColorOptions::default()
            },
        );
        let after = decode_rgb(&out);
        assert_eq!(after.0[0], after.0[1]);
        assert_eq!(after.0[1], after.0[2]);
    }

    #[test]
    fn saturation_boost_increases_channel_spread() {
        let input = png_of(Rgb([180, 120, 90]));
        let before = decode_rgb(&input);
        let out = run(
            &input,
            &ColorOptions {
                saturation: Some(100.0),
                ..ColorOptions::default()
            },
        );
        let after = decode_rgb(&out);
        let spread = |c: [u8; 3]| {
            let max = c.iter().max().unwrap();
            let min = c.iter().min().unwrap();
            max - min
        };
        assert!(
            spread(after.0) > spread(before.0),
            "{before:?} -> {after:?}"
        );
    }

    #[test]
    fn saturation_negative_damps_colors() {
        let input = png_of(Rgb([255, 0, 0]));
        let out = run(
            &input,
            &ColorOptions {
                saturation: Some(-50.0),
                ..ColorOptions::default()
            },
        );
        let after = decode_rgb(&out);
        // Half-saturated red: green and blue lift off zero equally.
        assert!(after.0[1] > 0 && after.0[1] == after.0[2], "{after:?}");
        assert!(after.0[0] > after.0[1], "{after:?}");
    }

    #[test]
    fn channel_gain_scales_only_that_channel() {
        let input = png_of(Rgb([100, 50, 25]));
        let out = run(
            &input,
            &ColorOptions {
                channels: Some(ChannelAdjust {
                    red_gain: Some(2.0),
                    ..ChannelAdjust::default()
                }),
                ..ColorOptions::default()
            },
        );
        let after = decode_rgb(&out);
        assert_eq!(after.0, [200, 50, 25]);
    }

    #[test]
    fn channel_offset_adds_after_gain() {
        let input = png_of(Rgb([100, 100, 100]));
        let out = run(
            &input,
            &ColorOptions {
                channels: Some(ChannelAdjust {
                    green_gain: Some(0.5),
                    green_offset: Some(50),
                    blue_offset: Some(-30),
                    ..ChannelAdjust::default()
                }),
                ..ColorOptions::default()
            },
        );
        let after = decode_rgb(&out);
        assert_eq!(after.0, [100, 100, 70]);
    }

    #[test]
    fn channel_values_clamp_to_range() {
        let input = png_of(Rgb([200, 200, 200]));
        let out = run(
            &input,
            &ColorOptions {
                channels: Some(ChannelAdjust {
                    red_gain: Some(4.0),
                    green_offset: Some(255),
                    blue_offset: Some(-255),
                    ..ChannelAdjust::default()
                }),
                ..ColorOptions::default()
            },
        );
        let after = decode_rgb(&out);
        assert_eq!(after.0, [255, 255, 0]);
    }

    #[test]
    fn grayscale_makes_channels_equal() {
        let input = png_of(Rgb([200, 100, 50]));
        let out = run(
            &input,
            &ColorOptions {
                grayscale: true,
                ..ColorOptions::default()
            },
        );
        let decoded = decode(&out, &EngineLimits::default()).expect("must decode");
        assert_eq!(decoded.image.color(), image::ColorType::L8);
        let luma = *decoded.image.to_luma8().get_pixel(0, 0);
        assert!(luma.0[0] > 80 && luma.0[0] < 160, "luma {luma:?}");
    }

    #[test]
    fn grayscale_preserves_alpha() {
        let mut img = RgbaImage::new(4, 4);
        for p in img.pixels_mut() {
            *p = Rgba([200, 100, 50, 128]);
        }
        let mut buf = Vec::new();
        DynamicImage::ImageRgba8(img)
            .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)
            .expect("test PNG must encode");

        let out = run(
            &buf,
            &ColorOptions {
                grayscale: true,
                ..ColorOptions::default()
            },
        );
        let decoded = decode(&out, &EngineLimits::default()).expect("must decode");
        assert_eq!(decoded.image.color(), image::ColorType::La8);
        let rgba = decoded.image.to_rgba8();
        let px = *rgba.get_pixel(0, 0);
        assert_eq!(px.0[3], 128, "alpha must be preserved: {px:?}");
        assert_eq!(px.0[0], px.0[1]);
        assert_eq!(px.0[1], px.0[2]);
    }

    #[test]
    fn invert_flips_color_channels() {
        let input = png_of(Rgb([10, 128, 250]));
        let out = run(
            &input,
            &ColorOptions {
                invert: true,
                ..ColorOptions::default()
            },
        );
        let after = decode_rgb(&out);
        assert_eq!(after.0, [245, 127, 5]);
    }

    #[test]
    fn high_contrast_stretches_shared_range() {
        // Two gray pixels: dark and light. A shared stretch must map
        // the extremes to 0 and 255.
        let mut img = image::RgbImage::new(2, 1);
        img.put_pixel(0, 0, Rgb([50, 50, 50]));
        img.put_pixel(1, 0, Rgb([170, 170, 170]));
        let mut buf = Vec::new();
        DynamicImage::ImageRgb8(img)
            .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)
            .expect("test PNG must encode");

        let out = run(
            &buf,
            &ColorOptions {
                high_contrast: Some(HighContrastOptions { per_channel: false }),
                ..ColorOptions::default()
            },
        );
        let decoded = decode(&out, &EngineLimits::default()).expect("must decode");
        let rgb = decoded.image.to_rgb8();
        let dark = *rgb.get_pixel(0, 0);
        let light = *rgb.get_pixel(1, 0);
        assert_eq!(dark.0, [0, 0, 0]);
        assert_eq!(light.0, [255, 255, 255]);
    }

    #[test]
    fn high_contrast_per_channel_stretches_independently() {
        let mut img = image::RgbImage::new(2, 1);
        img.put_pixel(0, 0, Rgb([10, 100, 200]));
        img.put_pixel(1, 0, Rgb([20, 110, 250]));
        let mut buf = Vec::new();
        DynamicImage::ImageRgb8(img)
            .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)
            .expect("test PNG must encode");

        let out = run(
            &buf,
            &ColorOptions {
                high_contrast: Some(HighContrastOptions { per_channel: true }),
                ..ColorOptions::default()
            },
        );
        let decoded = decode(&out, &EngineLimits::default()).expect("must decode");
        let rgb = decoded.image.to_rgb8();
        let dark = *rgb.get_pixel(0, 0);
        // Every channel's max maps to 255 under per-channel stretch.
        assert_eq!(dark.0, [0, 0, 0]);
        let light = *rgb.get_pixel(1, 0);
        assert_eq!(light.0, [255, 255, 255]);
    }

    #[test]
    fn output_keeps_input_format() {
        let input = png_of(Rgb([10, 20, 30]));
        let out = run(
            &input,
            &ColorOptions {
                invert: true,
                ..ColorOptions::default()
            },
        );
        let decoded = decode(&out, &EngineLimits::default()).expect("must decode");
        assert_eq!(decoded.format, ImageFormat::Png);
    }

    #[test]
    fn rejects_out_of_range_values() {
        let input = png_of(Rgb([100, 100, 100]));
        let limits = EngineLimits::default();

        for options in [
            ColorOptions {
                saturation: Some(101.0),
                ..ColorOptions::default()
            },
            ColorOptions {
                saturation: Some(f32::NAN),
                ..ColorOptions::default()
            },
            ColorOptions {
                channels: Some(ChannelAdjust {
                    red_gain: Some(-1.0),
                    ..ChannelAdjust::default()
                }),
                ..ColorOptions::default()
            },
            ColorOptions {
                channels: Some(ChannelAdjust {
                    blue_gain: Some(4.1),
                    ..ChannelAdjust::default()
                }),
                ..ColorOptions::default()
            },
            ColorOptions {
                channels: Some(ChannelAdjust {
                    green_offset: Some(256),
                    ..ChannelAdjust::default()
                }),
                ..ColorOptions::default()
            },
            ColorOptions {
                channels: Some(ChannelAdjust::default()),
                ..ColorOptions::default()
            },
        ] {
            assert!(
                matches!(
                    color(&input, &options, &limits),
                    Err(ImageError::InvalidInput(_))
                ),
                "options {options:?} must be rejected"
            );
        }
    }

    #[test]
    fn rejects_empty_options() {
        let input = png_of(Rgb([100, 100, 100]));
        let result = color(&input, &ColorOptions::default(), &EngineLimits::default());
        assert!(matches!(result, Err(ImageError::InvalidInput(_))));
    }

    #[test]
    fn accepts_boundary_values() {
        let input = png_of(Rgb([128, 128, 128]));
        let out = run(
            &input,
            &ColorOptions {
                saturation: Some(-100.0),
                channels: Some(ChannelAdjust {
                    red_gain: Some(0.0),
                    blue_offset: Some(-255),
                    ..ChannelAdjust::default()
                }),
                high_contrast: Some(HighContrastOptions::default()),
                grayscale: true,
                invert: true,
            },
        );
        let _ = decode_rgb(&out);
    }
}
