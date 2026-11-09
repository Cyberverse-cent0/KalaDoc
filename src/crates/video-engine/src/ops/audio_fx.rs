//! Advanced audio effects: EQ, denoise, compression, pitch shift,
//! and spectrogram image generation.

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::VideoResult;

/// Apply a 3-band EQ (`bass_db`, `mid_db`, `treble_db`; `None` skips
/// a band). Uses `firequalizer`/`equalizer` filters as appropriate.
pub(crate) fn audio_eq(
    input: &[u8],
    bass_db: f64,
    mid_db: Option<f64>,
    treble_db: f64,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let _ = (input, bass_db, mid_db, treble_db, limits);
    Err(
        crate::crates::video_engine::error::VideoError::InvalidInput(
            "audio_eq not implemented".to_string(),
        ),
    )
}

/// Spectral noise reduction via `afftdn`; `strength` (0.0–1.0) maps
/// to the `nr` parameter.
pub(crate) fn audio_denoise(
    input: &[u8],
    strength: f64,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let _ = (input, strength, limits);
    Err(
        crate::crates::video_engine::error::VideoError::InvalidInput(
            "audio_denoise not implemented".to_string(),
        ),
    )
}

/// Dynamic-range compression via `acompressor` with the given
/// threshold (dB) and ratio.
pub(crate) fn audio_compress(
    input: &[u8],
    threshold_db: f64,
    ratio: f64,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let _ = (input, threshold_db, ratio, limits);
    Err(
        crate::crates::video_engine::error::VideoError::InvalidInput(
            "audio_compress not implemented".to_string(),
        ),
    )
}

/// Shift pitch by `semitones` without changing duration
/// (`asetrate` + `atempo` + `aresample` chain).
pub(crate) fn audio_pitch(
    input: &[u8],
    semitones: f64,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let _ = (input, semitones, limits);
    Err(
        crate::crates::video_engine::error::VideoError::InvalidInput(
            "audio_pitch not implemented".to_string(),
        ),
    )
}

/// Render the audio as a spectrogram PNG via `showspectrumpic`.
/// Returns the PNG bytes.
pub(crate) fn audio_spectrogram(
    input: &[u8],
    width: u32,
    height: u32,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let _ = (input, width, height, limits);
    Err(
        crate::crates::video_engine::error::VideoError::InvalidInput(
            "audio_spectrogram not implemented".to_string(),
        ),
    )
}
