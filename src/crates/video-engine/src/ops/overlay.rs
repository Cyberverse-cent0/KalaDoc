//! Overlay and compositing ops: image/logo overlay, drawtext,
//! chroma key.

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::VideoResult;

/// Overlay `overlay` (PNG/JPEG with optional alpha) onto `input` at
/// `(x, y)` — negative values anchor from the right/bottom edge.
/// `opacity` (0.0–1.0) applies `colorchannelmixer`/`format=rgba`
/// alpha scaling when `Some`.
pub(crate) fn overlay_image(
    input: &[u8],
    overlay: &[u8],
    x: i32,
    y: i32,
    opacity: Option<f64>,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let _ = (input, overlay, x, y, opacity, limits);
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "overlay_image not implemented".to_string(),
    ))
}

/// Draw `text` onto `input` at `(x, y)` using ffmpeg's `drawtext`
/// filter. `font_size` in pixels; `color` as `0xRRGGBB` or a named
/// color; `None` color uses white with a black outline for legibility.
/// Text is passed through `textfile`-safe expansion — escape `:` `'`
/// `%` per ffmpeg rules.
pub(crate) fn draw_text(
    input: &[u8],
    text: &str,
    x: i32,
    y: i32,
    font_size: u32,
    color: Option<&str>,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let _ = (input, text, x, y, font_size, color, limits);
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "draw_text not implemented".to_string(),
    ))
}

/// Composite `foreground` over `background` removing the
/// `color`-keyed pixels (`colorkey` similarity/blend). Both inputs
/// must have compatible geometry — the foreground is overlaid
/// centered unless filters specify otherwise.
pub(crate) fn chroma_key(
    foreground: &[u8],
    background: &[u8],
    color: &str,
    similarity: f64,
    blend: f64,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let _ = (foreground, background, color, similarity, blend, limits);
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "chroma_key not implemented".to_string(),
    ))
}
