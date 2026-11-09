//! Frame extraction: timed intervals, fixed fps, all frames, and
//! keyframe-only pulls — emitted as a [`MediaOutput`] of named image
//! files.

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::VideoResult;
use crate::crates::video_engine::model::{FramesSpec, MediaOutput};

/// Extract frames per `spec` into `frames_000001.<ext>`-style named
/// files. `spec.format` selects `png` or `jpg`. Enforces
/// `limits.max_outputs` (default cap 10000 frames) and
/// `max_input_bytes`/`max_duration_secs`.
pub(crate) fn extract_frames(
    input: &[u8],
    spec: &FramesSpec,
    limits: &EngineLimits,
) -> VideoResult<MediaOutput> {
    let _ = (input, spec, limits);
    Err(
        crate::crates::video_engine::error::VideoError::InvalidInput(
            "extract_frames not implemented".to_string(),
        ),
    )
}

/// Extract every frame as a numbered image sequence
/// (`frame_000000.png`, ...). Same guards as [`extract_frames`].
pub(crate) fn video_to_image_sequence(
    input: &[u8],
    format: &str,
    limits: &EngineLimits,
) -> VideoResult<MediaOutput> {
    let _ = (input, format, limits);
    Err(
        crate::crates::video_engine::error::VideoError::InvalidInput(
            "video_to_image_sequence not implemented".to_string(),
        ),
    )
}
