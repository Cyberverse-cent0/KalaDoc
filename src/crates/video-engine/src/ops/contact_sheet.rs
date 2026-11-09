//! Contact sheets and scene-aware keyframe extraction.

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::VideoResult;
use crate::crates::video_engine::model::MediaOutput;

/// Build a single PNG contact sheet: sample one frame every
/// `interval_secs` (default: duration/`cols*rows`), tile into
/// `cols`×`rows` cells of `thumb_width` px wide. Returns PNG bytes.
pub(crate) fn contact_sheet(
    input: &[u8],
    cols: u32,
    rows: u32,
    thumb_width: u32,
    interval_secs: Option<f64>,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let _ = (input, cols, rows, thumb_width, interval_secs, limits);
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "contact_sheet not implemented".to_string(),
    ))
}

/// Extract one frame at each detected scene change (ffmpeg `scdet`
/// filter, `threshold` 0.0–1.0) as `scene_000001.png` files.
pub(crate) fn scene_keyframes(
    input: &[u8],
    threshold: f64,
    limits: &EngineLimits,
) -> VideoResult<MediaOutput> {
    let _ = (input, threshold, limits);
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "scene_keyframes not implemented".to_string(),
    ))
}
