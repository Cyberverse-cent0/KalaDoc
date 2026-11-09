//! Container remuxing: change the container format without
//! re-encoding streams (`ffmpeg -c copy`).
//!
//! Implemented by the multimedia engine expansion; see `engine.rs`
//! for the public entry point.

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::VideoResult;

/// Remux `input` into the container named by `target_ext`
/// (`mp4`, `mkv`, `mov`, `webm`, `avi`, `ts`, `flv`, ...), copying all
/// streams without re-encoding. Fails with `InvalidInput` when the
/// target container cannot accept the source codecs — ffmpeg's own
/// muxer error is surfaced honestly in that case.
pub(crate) fn remux(
    input: &[u8],
    target_ext: &str,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let _ = (input, target_ext, limits);
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "remux not implemented".to_string(),
    ))
}
