//! Raster image engine — inspect, resize, and format conversion.
//!
//! This crate implements a [`ProcessingEngine`](crate::crates::core::engine::ProcessingEngine)
//! for raster images, backed by the in-process, pure-Rust `image`
//! crate. There are no external binaries or services — everything
//! operates on in-memory byte buffers and returns `Vec<u8>` (or
//! structured reports).
//!
//! # Supported formats
//!
//! Only the codecs compiled into the `image` dependency are handled:
//! PNG, JPEG, GIF, WebP and BMP — both as inputs and outputs.
//! Everything else is rejected rather than approximated: unrecognized
//! input fails with [`ImageError::UnsupportedFormat`], an unsupported
//! *target* format with [`ImageError::InvalidInput`].
//!
//! # Operations
//!
//! | Engine operation | Method | Notes |
//! |---|---|---|
//! | `inspect`          | [`ImageEngine::inspect`] | [`ImageInfo`] report; fully decodes so truncated data is caught |
//! | `resize`           | [`ImageEngine::resize`] / [`ImageEngine::resize_to`] | Aspect-preserving fit within a bounding box |
//! | `convert`          | [`ImageEngine::convert`] | Re-encode to another supported format |
//! | `rotate`           | [`ImageEngine::rotate`] | Quarter turns only (90/180/270) — no arbitrary angles |
//! | `crop`             | [`ImageEngine::crop`] | Pixel-exact rectangle crop |
//! | `adjust`           | [`ImageEngine::adjust`] | Brightness, contrast, hue rotation |
//! | `color`            | [`ImageEngine::color`] | Saturation, per-channel RGB, grayscale, invert, high-contrast |
//! | `levels`           | [`ImageEngine::levels`] | Input black/white points + gamma curve |
//! | `photocopy`        | [`ImageEngine::photocopy`] | Scanner look: grayscale, threshold, grain, dithering |
//! | `replace_color`    | [`ImageEngine::replace_color`] | Tolerance-based color keying to a target color |
//! | `cmyk`             | [`ImageEngine::cmyk`] | RGB→CMYK as uncompressed TIFF (output is always TIFF) |
//! | `flip`             | [`ImageEngine::flip`] / [`ImageEngine::flip_mirror`] | Lossless mirror, no resampling |
//! | `thumbnail`        | [`ImageEngine::thumbnail`] | Aspect-fit thumbnail; never upscales |
//! | `compress`         | [`ImageEngine::compress`] | Quality/effort re-encode per codec |
//! | `watermark`        | [`ImageEngine::watermark`] | Image overlay with position/opacity/scale/tile |
//! | `validate`         | [`ImageEngine::validate`] | Header-only pre-flight check, no pixel decode |
//! | `extract_metadata` | [`ImageEngine::metadata`] | Descriptive [`ImageMetadata`] report |
//!
//! Operations with no [`EngineOperation`] analogue are still exposed as
//! methods but only the advertised subset appears in
//! [`EngineCapabilities`]. Unsupported operations (`merge`, `split`,
//! `ocr`, `encrypt`, `decrypt`, ...) are **not** advertised and have no
//! entry points here — the engine never silently approximates
//! unsupported behavior.
//!
//! # Contracts
//!
//! - **Limits** ([`EngineLimits`](crate::crates::core::engine::EngineLimits)):
//!   `max_input_bytes` bounds the input buffer; `max_pixels` bounds
//!   width × height, checked from the header *before* pixels are
//!   decoded (decompression-bomb guard). `max_pages`/`max_outputs`
//!   are meaningless for raster images and ignored.
//! - **Animated images** (GIF, animated WebP) collapse to their first
//!   frame — `DynamicImage` has no frame concept.
//! - **Alpha** is lost when converting to JPEG (no alpha channel);
//!   `DynamicImage` handles the flattening during encode.
//! - **No panics**: malformed input returns [`ImageError`].

mod engine;
mod error;
mod ops;
#[cfg(test)]
mod tests;

pub use engine::ImageEngine;
pub use error::{ImageError, ImageResult};
pub use ops::{
    AdjustOptions, ChannelAdjust, ColorOptions, ColorReplaceOptions, CompressOptions,
    FlipDirection, HighContrastOptions, ImageFormat, ImageInfo, ImageMetadata, LevelsOptions,
    PhotocopyOptions, ResizeOptions, ValidationReport, WatermarkOptions, WatermarkPosition, cmyk,
    convert, flip, flip_mirror, inspect, levels, metadata, photocopy, replace_color, resize,
    resize_to, thumbnail, validate,
};
