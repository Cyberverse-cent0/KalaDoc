//! Format conversion — decode `input` and re-encode as `target`.
//!
//! The target is a caller-supplied string (CLI/MCP friendly):
//! surrounding whitespace and case are ignored, `"jpg"` aliases
//! `"jpeg"`, and anything outside the supported set is an
//! [`ImageError::InvalidInput`] — conversion never guesses.

use crate::crates::core::engine::EngineLimits;
use crate::crates::image_engine::error::ImageResult;

use super::format::parse_target;
use super::load::{decode, encode};

/// Convert `input` to the format named by `target`, returning the
/// encoded bytes.
///
/// The image is fully decoded and re-encoded; converting to the same
/// format is allowed (acts as a re-encode/normalize). Animated inputs
/// collapse to their first frame, and alpha is dropped when targeting
/// JPEG.
pub fn convert(input: &[u8], target: &str, limits: &EngineLimits) -> ImageResult<Vec<u8>> {
    let format = parse_target(target)?;
    let decoded = decode(input, limits)?;
    encode(&decoded.image, format)
}
