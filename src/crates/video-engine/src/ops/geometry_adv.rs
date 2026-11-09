//! Advanced geometry: letterbox/pillarbox, autocrop, transpose,
//! aspect-ratio conversion (portrait↔landscape).

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::VideoResult;

/// Scale `input` to `width`×`height` and pad (letterbox) to exactly
/// that frame, preserving aspect ratio.
pub(crate) fn letterbox(
    input: &[u8],
    width: u32,
    height: u32,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let _ = (input, width, height, limits);
    Err(
        crate::crates::video_engine::error::VideoError::InvalidInput(
            "letterbox not implemented".to_string(),
        ),
    )
}

/// Same as [`letterbox`] but pillarbox is the same operation for
/// landscape content — kept as a separate entry point for API
/// symmetry (both produce a `width`×`height` frame with bars).
pub(crate) fn pillarbox(
    input: &[u8],
    width: u32,
    height: u32,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    letterbox(input, width, height, limits)
}

/// Detect and remove black borders via ffmpeg's `cropdetect` filter;
/// returns the cropped video. Fails honestly when no crop is found.
pub(crate) fn autocrop(input: &[u8], limits: &EngineLimits) -> VideoResult<Vec<u8>> {
    let _ = (input, limits);
    Err(
        crate::crates::video_engine::error::VideoError::InvalidInput(
            "autocrop not implemented".to_string(),
        ),
    )
}

/// Rotate 90° clockwise (`clockwise = true`) or counter-clockwise
/// using the `transpose` filter (lossless for most codecs).
pub(crate) fn transpose(
    input: &[u8],
    clockwise: bool,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let _ = (input, clockwise, limits);
    Err(
        crate::crates::video_engine::error::VideoError::InvalidInput(
            "transpose not implemented".to_string(),
        ),
    )
}

/// Convert between portrait and landscape orientation by rotating
/// 90° and swapping to the requested orientation.
pub(crate) fn orient(input: &[u8], landscape: bool, limits: &EngineLimits) -> VideoResult<Vec<u8>> {
    let _ = (input, landscape, limits);
    Err(
        crate::crates::video_engine::error::VideoError::InvalidInput(
            "orient not implemented".to_string(),
        ),
    )
}
