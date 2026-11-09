//! Media analysis: black/freeze/silence detection, scene changes,
//! and aggregate metrics — all via ffmpeg analysis filters writing
//! to `null`/metadata outputs that we parse.

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::VideoResult;
use crate::crates::video_engine::model::MediaAnalysis;

/// Run the analysis suite: `blackdetect`, `freezedetect`,
/// `silencedetect` (audio), `scdet` scene scores, `volumedetect`/
/// `astats` audio stats. Returns a [`MediaAnalysis`]; detectors that
/// the build lacks are skipped (recorded in `metrics` as absent).
pub(crate) fn analyze(input: &[u8], limits: &EngineLimits) -> VideoResult<MediaAnalysis> {
    let _ = (input, limits);
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "analyze not implemented".to_string(),
    ))
}

/// Scene-change timestamps `(time_secs, score)` via `scdet` (or
/// `select='gt(scene,threshold)'`+`showinfo` when scdet is absent).
pub(crate) fn scene_timestamps(
    input: &[u8],
    threshold: f64,
    limits: &EngineLimits,
) -> VideoResult<Vec<(f64, f64)>> {
    let _ = (input, threshold, limits);
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "scene_timestamps not implemented".to_string(),
    ))
}

/// Black-frame intervals `[start, end)` via `blackdetect`.
pub(crate) fn detect_black_frames(
    input: &[u8],
    limits: &EngineLimits,
) -> VideoResult<Vec<(f64, f64)>> {
    let _ = (input, limits);
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "detect_black_frames not implemented".to_string(),
    ))
}

/// Freeze-frame intervals via `freezedetect`.
pub(crate) fn detect_freeze_frames(
    input: &[u8],
    limits: &EngineLimits,
) -> VideoResult<Vec<(f64, f64)>> {
    let _ = (input, limits);
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "detect_freeze_frames not implemented".to_string(),
    ))
}

/// Silence intervals via `silencedetect` (audio only; empty when the
/// input has no audio stream).
pub(crate) fn detect_silence(
    input: &[u8],
    limits: &EngineLimits,
) -> VideoResult<Vec<(f64, f64)>> {
    let _ = (input, limits);
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "detect_silence not implemented".to_string(),
    ))
}
