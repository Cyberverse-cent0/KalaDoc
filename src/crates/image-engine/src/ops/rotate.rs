//! Quarter-turn rotation (90°/180°/270°).
//!
//! Only right-angle rotations are supported — arbitrary angles would
//! require resampling and are not advertised by the engine.

use crate::crates::core::engine::EngineLimits;

use crate::crates::image_engine::error::{ImageError, ImageResult};

use super::load::{encode, load_image};

/// Rotate `input` by `degrees`, which must be exactly 90, 180, or 270.
/// Returns the encoded bytes in the input format.
pub fn rotate(
    input: &[u8],
    degrees: u16,
    limits: &EngineLimits,
) -> ImageResult<Vec<u8>> {
    let loaded = load_image(input, limits)?;
    let rotated = match degrees {
        90 => loaded.image.rotate90(),
        180 => loaded.image.rotate180(),
        270 => loaded.image.rotate270(),
        other => {
            return Err(ImageError::InvalidInput(format!(
                "rotation must be 90, 180, or 270 degrees, got {other}"
            )));
        }
    };
    encode(&rotated, loaded.format, None)
}
