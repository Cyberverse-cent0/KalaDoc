//! Lossy/lossless re-encoding with an optional quality knob.
//!
//! [`compress`] decodes `input` (enforcing all
//! [`EngineLimits`](crate::crates::core::engine::EngineLimits) via
//! [`load::decode`]) and re-encodes it, optionally switching formats.
//!
//! # Quality semantics per format
//!
//! `quality` is a 1–100 knob; `None` selects the codec default. How it
//! maps depends on what the compiled-in `image` crate encoders expose:
//!
//! - **JPEG** — passed straight to
//!   [`JpegEncoder::new_with_quality`](image::codecs::jpeg::JpegEncoder::new_with_quality):
//!   higher quality means larger files and less loss.
//! - **PNG** — lossless, so quality is interpreted as *compression
//!   effort*: it maps to the DEFLATE level
//!   ([`CompressionType::Level`], 1–9) with adaptive filtering. Higher
//!   quality ⇒ more effort ⇒ *smaller* output. `None` keeps the
//!   encoder's default compression.
//! - **WebP** — the pure-Rust `image-webp` encoder only implements
//!   lossless "VP8L" and exposes no quality parameter, so `quality`
//!   is accepted but ignored (documented no-op).
//! - **GIF / BMP** — the compiled encoders have no quality knob;
//!   `quality` is ignored.
//!
//! A `quality` outside `1..=100` is rejected with
//! [`ImageError::InvalidInput`] regardless of the target format.

use serde::{Deserialize, Serialize};

use crate::crates::core::engine::EngineLimits;
use crate::crates::image_engine::error::{ImageError, ImageResult};

use super::format::ImageFormat;
use super::load::{decode, encode};

/// Options for [`compress`]. Both fields are optional; `None` keeps the
/// input's own format / the codec default quality.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompressOptions {
    /// Output format. `None` re-encodes in the input's sniffed format.
    pub format: Option<ImageFormat>,
    /// Quality/effort knob in `1..=100`; `None` uses codec defaults.
    ///
    /// Lossy formats (JPEG): higher = better quality, larger output.
    /// Lossless formats with a real knob (PNG): higher = more
    /// compression effort, smaller output. Formats without a quality
    /// parameter (WebP, GIF, BMP) ignore this value.
    pub quality: Option<u8>,
}

/// Re-encode `input`, returning the encoded bytes.
///
/// Decodes with [`decode`] (so all input limits apply), then encodes
/// as `options.format` or — when `None` — the input's own format.
/// Animated inputs collapse to their first frame; alpha is dropped
/// when the target cannot represent it (e.g. JPEG), same as
/// [`convert`](super::convert::convert).
pub fn compress(
    input: &[u8],
    options: CompressOptions,
    limits: &EngineLimits,
) -> ImageResult<Vec<u8>> {
    if let Some(q) = options.quality {
        if !(1..=100).contains(&q) {
            return Err(ImageError::InvalidInput(format!(
                "quality must be in 1..=100, got {q}"
            )));
        }
    }

    let decoded = decode(input, limits)?;
    let format = options.format.unwrap_or(decoded.format);

    match format {
        ImageFormat::Jpeg => {
            let mut buf = Vec::new();
            let encoder = match options.quality {
                Some(q) => image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, q),
                None => image::codecs::jpeg::JpegEncoder::new(&mut buf),
            };
            decoded
                .image
                .write_with_encoder(encoder)
                .map_err(ImageError::Encode)?;
            Ok(buf)
        }
        ImageFormat::Png => {
            let mut buf = Vec::new();
            let compression = match options.quality {
                // Map the 1..=100 effort knob onto DEFLATE levels 1..=9
                // (higher quality = more effort = smaller output).
                Some(q) => image::codecs::png::CompressionType::Level(
                    (u16::from(q) * 9).div_ceil(100).clamp(1, 9) as u8,
                ),
                None => image::codecs::png::CompressionType::default(),
            };
            let encoder = image::codecs::png::PngEncoder::new_with_quality(
                &mut buf,
                compression,
                image::codecs::png::FilterType::Adaptive,
            );
            decoded
                .image
                .write_with_encoder(encoder)
                .map_err(ImageError::Encode)?;
            Ok(buf)
        }
        // WebP (lossless-only encoder), GIF, and BMP expose no quality
        // parameter in the compiled `image` crate — `quality` is a
        // documented no-op for these formats.
        _ => encode(&decoded.image, format),
    }
}

#[cfg(test)]
mod tests {
    use image::{DynamicImage, ImageBuffer, Rgb};

    use super::{compress, CompressOptions};
    use crate::crates::core::engine::EngineLimits;
    use crate::crates::image_engine::error::{ImageError, ImageResult};
    use crate::crates::image_engine::ops::format::ImageFormat;
    use crate::crates::image_engine::ops::load::{decode, encode};

    fn limits() -> EngineLimits {
        EngineLimits::default()
    }

    /// Deterministic noisy RGB image — noise is the worst case for
    /// lossy compression, so quality differences show up clearly in
    /// output size.
    fn noisy_image(w: u32, h: u32) -> DynamicImage {
        let mut img = ImageBuffer::<Rgb<u8>, Vec<u8>>::new(w, h);
        let mut state: u32 = 0x1234_5678;
        for px in img.pixels_mut() {
            // xorshift32 — cheap deterministic PRNG.
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            *px = Rgb([(state & 0xff) as u8, ((state >> 8) & 0xff) as u8, ((state >> 16) & 0xff) as u8]);
        }
        DynamicImage::ImageRgb8(img)
    }

    fn noisy_png() -> ImageResult<Vec<u8>> {
        let img = noisy_image(96, 96);
        encode(&img, ImageFormat::Png)
    }

    fn noisy_jpeg() -> ImageResult<Vec<u8>> {
        let img = noisy_image(96, 96);
        encode(&img, ImageFormat::Jpeg)
    }

    #[test]
    fn jpeg_low_quality_is_smaller_and_decodes() -> ImageResult<()> {
        let input = noisy_jpeg()?;
        let low = compress(
            &input,
            CompressOptions {
                format: Some(ImageFormat::Jpeg),
                quality: Some(10),
            },
            &limits(),
        )?;
        let high = compress(
            &input,
            CompressOptions {
                format: Some(ImageFormat::Jpeg),
                quality: Some(95),
            },
            &limits(),
        )?;
        assert!(low.len() < high.len(), "q10 {} should be smaller than q95 {}", low.len(), high.len());
        // Both outputs still decode as JPEG.
        let d = decode(&low, &limits())?;
        assert_eq!(d.format, ImageFormat::Jpeg);
        assert_eq!((d.image.width(), d.image.height()), (96, 96));
        decode(&high, &limits())?;
        Ok(())
    }

    #[test]
    fn png_compress_round_trips_and_is_lossless() -> ImageResult<()> {
        let input = noisy_png()?;
        let out = compress(
            &input,
            CompressOptions {
                format: Some(ImageFormat::Png),
                quality: Some(90),
            },
            &limits(),
        )?;
        let d = decode(&out, &limits())?;
        assert_eq!(d.format, ImageFormat::Png);
        // Lossless: pixels identical to the source image.
        assert_eq!(d.image.as_bytes(), noisy_image(96, 96).as_bytes());
        Ok(())
    }

    #[test]
    fn png_higher_effort_not_larger() -> ImageResult<()> {
        // For PNG, quality = compression effort: higher effort should
        // produce output no larger than minimal effort on noisy data.
        let input = noisy_png()?;
        let fast = compress(
            &input,
            CompressOptions {
                format: Some(ImageFormat::Png),
                quality: Some(1),
            },
            &limits(),
        )?;
        let best = compress(
            &input,
            CompressOptions {
                format: Some(ImageFormat::Png),
                quality: Some(100),
            },
            &limits(),
        )?;
        assert!(
            best.len() <= fast.len(),
            "effort 100 ({} bytes) should not exceed effort 1 ({} bytes)",
            best.len(),
            fast.len()
        );
        Ok(())
    }

    #[test]
    fn format_override_converts() -> ImageResult<()> {
        let input = noisy_png()?;
        let out = compress(
            &input,
            CompressOptions {
                format: Some(ImageFormat::Jpeg),
                quality: Some(80),
            },
            &limits(),
        )?;
        let d = decode(&out, &limits())?;
        assert_eq!(d.format, ImageFormat::Jpeg);
        Ok(())
    }

    #[test]
    fn no_options_reencodes_in_input_format() -> ImageResult<()> {
        let input = noisy_png()?;
        let out = compress(&input, CompressOptions::default(), &limits())?;
        let d = decode(&out, &limits())?;
        assert_eq!(d.format, ImageFormat::Png);
        Ok(())
    }

    #[test]
    fn rejects_out_of_range_quality() {
        let input = noisy_png().expect("test PNG encodes");
        for bad in [0u8, 101, 255] {
            let err = compress(
                &input,
                CompressOptions {
                    format: Some(ImageFormat::Jpeg),
                    quality: Some(bad),
                },
                &limits(),
            )
            .expect_err("out-of-range quality must fail");
            assert!(
                matches!(err, ImageError::InvalidInput(_)),
                "expected InvalidInput, got {err:?}"
            );
        }
        // Boundary values are accepted.
        for good in [1u8, 100] {
            compress(
                &input,
                CompressOptions {
                    format: Some(ImageFormat::Jpeg),
                    quality: Some(good),
                },
                &limits(),
            )
            .expect("boundary quality must be accepted");
        }
    }

    #[test]
    fn rejects_invalid_input() {
        let err = compress(b"not an image", CompressOptions::default(), &limits())
            .expect_err("garbage must fail");
        assert!(matches!(err, ImageError::Decode(_) | ImageError::UnsupportedFormat(_)));
    }
}
