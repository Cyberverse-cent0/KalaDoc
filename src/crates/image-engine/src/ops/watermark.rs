//! Image watermarking — overlaying a second image onto a base image.
//!
//! There is no text/font support in this engine (the `image` crate is
//! built without font rasterization), so a "watermark" here is always
//! another raster image: a logo, a stamp, a tiled pattern, etc.
//!
//! The watermark is decoded with the same [`EngineLimits`] as the base
//! (via [`decode`]), optionally rescaled, alpha-adjusted for
//! [`WatermarkOptions::opacity`], and composited with
//! [`image::imageops::overlay`] (source-over alpha blending). The result
//! is encoded in the **base** image's format — the watermark's own
//! format only matters for decoding it.

use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;
use crate::crates::image_engine::error::{ImageError, ImageResult};

use super::load::{decode, encode};

/// Where on the base image the watermark is placed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatermarkPosition {
    /// Top-left corner, offset by [`WatermarkOptions::margin`].
    TopLeft,
    /// Top-right corner, offset by [`margin`](WatermarkOptions::margin).
    TopRight,
    /// Bottom-left corner, offset by [`margin`](WatermarkOptions::margin).
    BottomLeft,
    /// Bottom-right corner (the default), offset by
    /// [`margin`](WatermarkOptions::margin).
    #[default]
    BottomRight,
    /// Centered on the base image; `margin` is ignored.
    Center,
    /// Repeated in a grid covering the image. Spacing between tiles is
    /// [`WatermarkOptions::tile_spacing`] pixels; `margin` is the offset
    /// of the first tile from the top-left corner.
    Tile,
}

/// Parameters for [`watermark`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WatermarkOptions {
    /// Placement of the watermark.
    pub position: WatermarkPosition,
    /// Opacity multiplier applied to the watermark's alpha channel,
    /// `0.0..=1.0`. `1.0` (fully opaque where the mark is opaque) is the
    /// usual choice; `0.0` leaves the base image unchanged.
    pub opacity: f32,
    /// Optional scale factor. When set, the watermark is resized so its
    /// **longest side** equals `scale × min(base_width, base_height)`
    /// pixels, preserving aspect ratio. When `None`, the watermark is
    /// used at its native size and must fit inside the base image.
    pub scale: Option<f32>,
    /// Pixel margin from the edges for the corner positions (and the
    /// first tile for [`WatermarkPosition::Tile`]). Ignored for `Center`.
    pub margin: u32,
    /// Gap in pixels between adjacent tiles for
    /// [`WatermarkPosition::Tile`]. Ignored by other positions.
    pub tile_spacing: u32,
}

impl Default for WatermarkOptions {
    fn default() -> Self {
        Self {
            position: WatermarkPosition::default(),
            opacity: 1.0,
            scale: None,
            margin: 0,
            tile_spacing: 0,
        }
    }
}

/// Overlays `mark` onto `base` according to `options` and returns the
/// result encoded in the base image's format.
///
/// # Errors
///
/// - [`ImageError::InvalidInput`] — `opacity` outside `0.0..=1.0` or
///   non-finite, `scale` non-positive/non-finite, the watermark fails to
///   decode, or the (unscaled) watermark is larger than the base image.
/// - Decode/encode errors and [`EngineLimits`] violations propagate from
///   [`decode`]/[`encode`].
pub fn watermark(
    base: &[u8],
    mark: &[u8],
    options: &WatermarkOptions,
    limits: &EngineLimits,
) -> ImageResult<Vec<u8>> {
    if !(0.0..=1.0).contains(&options.opacity) {
        return Err(ImageError::InvalidInput(format!(
            "opacity must be in 0.0..=1.0, got {}",
            options.opacity
        )));
    }
    if let Some(scale) = options.scale {
        if !scale.is_finite() || scale <= 0.0 {
            return Err(ImageError::InvalidInput(format!(
                "scale must be a positive finite number, got {scale}"
            )));
        }
    }

    let base_decoded = decode(base, limits)?;
    let mark_decoded = decode(mark, limits)?;

    // Fully transparent watermark: nothing to composite.
    if options.opacity == 0.0 {
        return encode(&base_decoded.image, base_decoded.format);
    }

    let (bw, bh) = (base_decoded.image.width(), base_decoded.image.height());
    let mut mark_img = mark_decoded.image;

    // Optional rescale: longest side → scale × min(base dims).
    if let Some(scale) = options.scale {
        let (mw, mh) = (mark_img.width(), mark_img.height());
        let base_min = bw.min(bh) as f64;
        let mark_max = mw.max(mh) as f64;
        let factor = base_min * f64::from(scale) / mark_max;
        let nw = ((mw as f64 * factor).round() as u64).clamp(1, u64::from(u32::MAX)) as u32;
        let nh = ((mh as f64 * factor).round() as u64).clamp(1, u64::from(u32::MAX)) as u32;
        if (nw, nh) != (mw, mh) {
            mark_img = mark_img.resize_exact(nw, nh, image::imageops::FilterType::Lanczos3);
        }
    } else {
        let (mw, mh) = (mark_img.width(), mark_img.height());
        if mw > bw || mh > bh {
            return Err(ImageError::InvalidInput(format!(
                "watermark is {mw}x{mh}, larger than base {bw}x{bh}; \
                 provide options.scale to resize it"
            )));
        }
    }

    let (mw, mh) = (mark_img.width(), mark_img.height());
    if mw == 0 || mh == 0 {
        return Err(ImageError::InvalidInput(
            "watermark has zero dimensions".to_string(),
        ));
    }

    // Work in RGBA8 so `overlay`'s alpha blending applies regardless of
    // the source color types. The output format is unchanged — only the
    // pixel buffer representation differs.
    let mut canvas = base_decoded.image.to_rgba8();
    let mut mark_buf = mark_img.to_rgba8();

    // Bake `opacity` into the mark's alpha channel with a 256-entry LUT.
    let opacity_q = (options.opacity * 255.0).round() as u32;
    let lut: [u8; 256] =
        std::array::from_fn(|i| ((i as u32 * opacity_q + 127) / 255).min(255) as u8);
    for px in mark_buf.pixels_mut() {
        let a = px[3] as usize;
        px[3] = lut[a];
    }

    let margin = i64::from(options.margin);
    let mut place = |x: i64, y: i64| image::imageops::overlay(&mut canvas, &mark_buf, x, y);

    match options.position {
        WatermarkPosition::TopLeft => place(margin, margin),
        WatermarkPosition::TopRight => place(i64::from(bw) - i64::from(mw) - margin, margin),
        WatermarkPosition::BottomLeft => place(margin, i64::from(bh) - i64::from(mh) - margin),
        WatermarkPosition::BottomRight => place(
            i64::from(bw) - i64::from(mw) - margin,
            i64::from(bh) - i64::from(mh) - margin,
        ),
        WatermarkPosition::Center => place(
            (i64::from(bw) - i64::from(mw)) / 2,
            (i64::from(bh) - i64::from(mh)) / 2,
        ),
        WatermarkPosition::Tile => {
            // Step = tile size + gap; saturating to avoid overflow, and
            // bail if a degenerate step would loop forever.
            let step_x = u64::from(mw) + u64::from(options.tile_spacing);
            let step_y = u64::from(mh) + u64::from(options.tile_spacing);
            if step_x == 0 || step_y == 0 {
                return Err(ImageError::InvalidInput("tile step is zero".to_string()));
            }
            let mut y = margin;
            while y < i64::from(bh) {
                let mut x = margin;
                while x < i64::from(bw) {
                    place(x, y);
                    x = x.saturating_add(step_x.min(i64::MAX as u64) as i64);
                }
                y = y.saturating_add(step_y.min(i64::MAX as u64) as i64);
            }
        }
    }

    encode(
        &image::DynamicImage::ImageRgba8(canvas),
        base_decoded.format,
    )
}

#[cfg(test)]
mod tests {
    use super::super::format::ImageFormat;
    use super::super::load::{decode, encode};
    use super::{WatermarkOptions, WatermarkPosition, watermark};
    use crate::crates::core::engine::EngineLimits;
    use crate::crates::image_engine::error::ImageError;
    use image::{DynamicImage, ImageBuffer, Rgba};

    /// Encodes a solid-color RGBA image as PNG.
    fn solid(w: u32, h: u32, color: [u8; 4]) -> Vec<u8> {
        let img: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::from_pixel(w, h, Rgba(color));
        encode(&DynamicImage::ImageRgba8(img), ImageFormat::Png).unwrap()
    }

    fn opts(position: WatermarkPosition) -> WatermarkOptions {
        WatermarkOptions {
            position,
            ..WatermarkOptions::default()
        }
    }

    #[test]
    fn bottom_right_margin_places_mark() {
        let base = solid(100, 100, [0, 0, 255, 255]); // blue
        let mark = solid(10, 10, [255, 0, 0, 255]); // red
        let limits = EngineLimits::default();

        let out = watermark(
            &base,
            &mark,
            &WatermarkOptions {
                position: WatermarkPosition::BottomRight,
                margin: 5,
                ..WatermarkOptions::default()
            },
            &limits,
        )
        .unwrap();

        let decoded = decode(&out, &limits).unwrap();
        let img = decoded.image.to_rgba8();
        // Inside the mark (bottom-right, 5px margin): red.
        let p = img.get_pixel(94, 94).0;
        assert!(p[0] > 200 && p[1] < 60 && p[2] < 60, "pixel in mark: {p:?}");
        // Top-left corner untouched: still blue.
        let q = img.get_pixel(0, 0).0;
        assert!(q[2] > 200 && q[0] < 60, "corner pixel: {q:?}");
    }

    #[test]
    fn opacity_validation_rejects_out_of_range() {
        let base = solid(20, 20, [0, 0, 255, 255]);
        let mark = solid(5, 5, [255, 0, 0, 255]);
        let limits = EngineLimits::default();

        for bad in [1.5f32, -0.1, f32::NAN, f32::INFINITY] {
            let o = WatermarkOptions {
                opacity: bad,
                ..WatermarkOptions::default()
            };
            assert!(
                matches!(
                    watermark(&base, &mark, &o, &limits),
                    Err(ImageError::InvalidInput(_))
                ),
                "opacity {bad} should be rejected"
            );
        }
    }

    #[test]
    fn oversized_mark_rejected_without_scale() {
        let base = solid(50, 50, [0, 0, 255, 255]);
        let mark = solid(200, 200, [255, 0, 0, 255]);
        let limits = EngineLimits::default();

        assert!(matches!(
            watermark(&base, &mark, &opts(WatermarkPosition::Center), &limits),
            Err(ImageError::InvalidInput(_))
        ));

        // With scale, the same pair succeeds.
        let o = WatermarkOptions {
            position: WatermarkPosition::Center,
            scale: Some(0.5),
            ..WatermarkOptions::default()
        };
        assert!(watermark(&base, &mark, &o, &limits).is_ok());
    }

    #[test]
    fn tile_repeats_mark() {
        let base = solid(60, 60, [0, 0, 255, 255]);
        let mark = solid(10, 10, [255, 0, 0, 255]);
        let limits = EngineLimits::default();

        let o = WatermarkOptions {
            position: WatermarkPosition::Tile,
            tile_spacing: 10,
            ..WatermarkOptions::default()
        };
        let out = watermark(&base, &mark, &o, &limits).unwrap();
        let img = decode(&out, &limits).unwrap().image.to_rgba8();

        // Tiles at (0,0) and (20,0) (step = 10px mark + 10px gap).
        for (x, y) in [(5u32, 5u32), (25, 5), (5, 25)] {
            let p = img.get_pixel(x, y).0;
            assert!(p[0] > 200 && p[2] < 60, "tile pixel ({x},{y}): {p:?}");
        }
        // Gap between tiles stays blue: x=15 is inside the first tile's
        // right edge? mark spans 0..10, gap 10..20, next tile 20..30.
        let g = img.get_pixel(15, 5).0;
        assert!(g[2] > 200 && g[0] < 60, "gap pixel: {g:?}");
    }

    #[test]
    fn semi_transparent_mark_blends() {
        let base = solid(20, 20, [0, 0, 255, 255]);
        let mark = solid(20, 20, [255, 0, 0, 128]); // half-alpha red
        let limits = EngineLimits::default();

        let o = WatermarkOptions {
            position: WatermarkPosition::Center,
            opacity: 0.5,
            ..WatermarkOptions::default()
        };
        let out = watermark(&base, &mark, &o, &limits).unwrap();
        let img = decode(&out, &limits).unwrap().image.to_rgba8();
        let p = img.get_pixel(10, 10).0;
        // Blue base blended with ~25%-effective red → both channels present.
        assert!(p[0] > 40 && p[2] > 40, "blended pixel: {p:?}");
    }
}
