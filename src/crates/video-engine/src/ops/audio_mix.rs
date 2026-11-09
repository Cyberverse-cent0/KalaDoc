//! Multi-track mixing: weighted amix, crossfade, delay/sync.

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::VideoResult;

/// Mix `inputs` with per-track `weights` (linear gain multipliers,
/// must match `inputs.len()`). `normalize` applies `amix`'s
/// auto-leveling (`normalize=1`).
pub(crate) fn audio_mix_weighted(
    inputs: &[Vec<u8>],
    weights: &[f64],
    normalize: bool,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let _ = (inputs, weights, normalize, limits);
    Err(
        crate::crates::video_engine::error::VideoError::InvalidInput(
            "audio_mix_weighted not implemented".to_string(),
        ),
    )
}

/// Crossfade between two inputs over `duration_secs` (`acrossfade`).
/// Output length ≈ sum of inputs minus the overlap.
pub(crate) fn audio_crossfade(
    inputs: &[Vec<u8>],
    duration_secs: f64,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let _ = (inputs, duration_secs, limits);
    Err(
        crate::crates::video_engine::error::VideoError::InvalidInput(
            "audio_crossfade not implemented".to_string(),
        ),
    )
}

/// Shift the audio track by `offset_ms` (positive = later; uses
/// `adelay`/`atrim`+`asetpts` as needed, padding silence at the head).
pub(crate) fn audio_delay(
    input: &[u8],
    offset_ms: i64,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let _ = (input, offset_ms, limits);
    Err(
        crate::crates::video_engine::error::VideoError::InvalidInput(
            "audio_delay not implemented".to_string(),
        ),
    )
}
