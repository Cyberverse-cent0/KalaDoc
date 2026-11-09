//! Raster image operations — synchronous functions over in-memory byte
//! buffers.
//!
//! # Limit enforcement
//!
//! [`EngineLimits`](crate::crates::core::engine::EngineLimits) are enforced in
//! [`load::decode`]: input size (`max_input_bytes`) and decoded pixel
//! count (`max_pixels`, probed from the header before decoding).
//!
//! All ops return [`ImageError`] on malformed input and never panic.

mod convert;
mod format;
mod info;
mod load;
mod resize;

pub use convert::convert;
pub use format::ImageFormat;
pub use info::{ImageInfo, inspect};
pub use resize::{ResizeOptions, resize, resize_to};
