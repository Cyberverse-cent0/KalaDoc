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
//! | `inspect` | [`ImageEngine::inspect`] | [`ImageInfo`] report; fully decodes so truncated data is caught |
//! | `resize`   | [`ImageEngine::resize`] / [`ImageEngine::resize_to`] | Aspect-preserving fit within a bounding box |
//! | `convert`  | [`ImageEngine::convert`] | Re-encode to another supported format |
//!
//! Unsupported operations (`merge`, `split`, `rotate`, `compress`,
//! `watermark`, `ocr`, `encrypt`, `decrypt`, ...) are **not**
//! advertised in
//! [`EngineCapabilities`](crate::crates::core::engine::EngineCapabilities) and have no entry points here — the engine
//! never silently approximates unsupported behavior.
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
pub use ops::{ImageFormat, ImageInfo, ResizeOptions, convert, inspect, resize, resize_to};
