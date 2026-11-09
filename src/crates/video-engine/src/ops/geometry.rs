//! Basic video geometry: resize/scale, crop, pad, rotate, flip.
//!
//! All ops re-encode (geometry filters are lossy by nature) using the
//! engine's standard H.264/AAC output path and preserve the input's
//! container family.

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::VideoResult;
use crate::crates::video_engine::model::FitMode;

/// Resize `input` to fit `width`×`height` per `fit`
/// ([`FitMode::Fit`]/`Cover`/`Contain`/`Stretch`). `0` in either
/// dimension means "auto" (preserve aspect). Output keeps the input
/// container; re-encodes via the standard H.264/AAC path.
pub(crate) fn resize(
    input: &[u8],
    width: u32,
    height: u32,
    fit: FitMode,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let _ = (input, width, height, fit, limits);
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "resize not implemented".to_string(),
    ))
}

/// Crop `input` to the rectangle `(x, y, w, h)` in source pixels.
pub(crate) fn crop(
    input: &[u8],
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let _ = (input, x, y, w, h, limits);
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "crop not implemented".to_string(),
    ))
}

/// Pad `input` with `color` (e.g. `"black"`, `"0xRRGGBB"`) by the
/// given pixel amounts on each side.
pub(crate) fn pad(
    input: &[u8],
    left: u32,
    right: u32,
    top: u32,
    bottom: u32,
    color: Option<&str>,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let _ = (input, left, right, top, bottom, color, limits);
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "pad not implemented".to_string(),
    ))
}

/// Rotate `input` by `degrees` (arbitrary angle; 90/180/270 should
/// prefer the lossless `transpose` path in `geometry_adv`). Fill
/// color defaults to black.
pub(crate) fn rotate(
    input: &[u8],
    degrees: f64,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let _ = (input, degrees, limits);
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "rotate not implemented".to_string(),
    ))
}

/// Mirror `input` horizontally or vertically.
pub(crate) fn flip(input: &[u8], horizontal: bool, limits: &EngineLimits) -> VideoResult<Vec<u8>> {
    let _ = (input, horizontal, limits);
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "flip not implemented".to_string(),
    ))
}
