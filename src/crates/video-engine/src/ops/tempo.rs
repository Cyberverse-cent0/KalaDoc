//! Tempo and time-domain video ops: speed change, reverse, loop.
//!
//! Speed changes re-encode (setpts + atempo); reverse requires a
//! full decode/re-encode with the `reverse` filter; loop uses the
//! concat demuxer with repeated inputs.

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::VideoResult;

/// Change playback speed by `factor` (`2.0` = 2× fast, `0.5` = half
/// speed). Audio tempo follows via `atempo` (chained for factors
/// outside 0.5–100). Re-encodes.
pub(crate) fn set_speed(
    input: &[u8],
    factor: f64,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let _ = (input, factor, limits);
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "set_speed not implemented".to_string(),
    ))
}

/// Reverse the clip (video reversed, audio reversed too — full
/// decode/re-encode; bounded by `limits.max_duration_secs`).
pub(crate) fn reverse(input: &[u8], limits: &EngineLimits) -> VideoResult<Vec<u8>> {
    let _ = (input, limits);
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "reverse not implemented".to_string(),
    ))
}

/// Loop `input` `count` additional times (`count = 1` → plays twice).
/// Stream-copies via the concat demuxer when possible.
pub(crate) fn loop_clip(
    input: &[u8],
    count: u32,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let _ = (input, count, limits);
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "loop_clip not implemented".to_string(),
    ))
}

/// Freeze the frame at `at_secs` for `duration_secs` (the preceding
/// video is kept; the frozen segment is appended).
pub(crate) fn freeze_frame(
    input: &[u8],
    at_secs: f64,
    duration_secs: f64,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let _ = (input, at_secs, duration_secs, limits);
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "freeze_frame not implemented".to_string(),
    ))
}
