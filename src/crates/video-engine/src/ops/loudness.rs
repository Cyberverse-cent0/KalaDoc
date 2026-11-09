//! Audio loudness measurement and normalization (EBU R128 family).

use std::collections::BTreeMap;

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::VideoResult;
use crate::crates::video_engine::model::LoudnessReport;

/// Measure integrated loudness, loudness range, and true peak via
/// `ebur128=peak=true` (or `loudnorm print_format=json` fallback).
/// `None` fields indicate values the filter did not report.
pub(crate) fn measure_loudness(
    input: &[u8],
    limits: &EngineLimits,
) -> VideoResult<LoudnessReport> {
    let _ = (input, limits);
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "measure_loudness not implemented".to_string(),
    ))
}

/// Normalize loudness to `target_lufs` (default -23) using
/// `loudnorm`. Dynamic mode when the filter supports it; falls back
/// to `volume` normalization when loudnorm is unavailable.
pub(crate) fn normalize_loudness(
    input: &[u8],
    target_lufs: Option<f64>,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let _ = (input, target_lufs, limits);
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "normalize_loudness not implemented".to_string(),
    ))
}

/// Aggregate audio statistics via `astats`/`volumedetect`: mean/max
/// volume (dB), RMS, DC offset, flat factor, peak level — as a flat
/// `BTreeMap` keyed by stable metric names.
pub(crate) fn audio_stats(
    input: &[u8],
    limits: &EngineLimits,
) -> VideoResult<BTreeMap<String, f64>> {
    let _ = (input, limits);
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "audio_stats not implemented".to_string(),
    ))
}
