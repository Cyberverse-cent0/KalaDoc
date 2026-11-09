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

mod adjust;
mod cmyk;
mod color;
mod compress;
mod convert;
mod crop;
mod effects;
mod flip;
mod format;
mod info;
mod load;
mod metadata;
mod resize;
mod rotate;
mod thumbnail;
mod validate;
mod watermark;

pub use adjust::{AdjustOptions, adjust};
pub use cmyk::cmyk;
pub use color::{ChannelAdjust, ColorOptions, HighContrastOptions, color};
pub use compress::{CompressOptions, compress};
pub use convert::convert;
pub use crop::crop;
pub use effects::{
    ColorReplaceOptions, LevelsOptions, PhotocopyOptions, levels, photocopy, replace_color,
};
pub use flip::{FlipDirection, flip, flip_mirror};
pub use format::ImageFormat;
pub use info::{ImageInfo, inspect};
pub use metadata::{ImageMetadata, metadata};
pub use resize::{ResizeOptions, resize, resize_to};
pub use rotate::rotate;
pub use thumbnail::thumbnail;
pub use validate::{ValidationReport, validate};
pub use watermark::{WatermarkOptions, WatermarkPosition, watermark};
