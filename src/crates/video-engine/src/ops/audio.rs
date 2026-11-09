//! Basic audio operations on media containers: volume, loudness
//! normalization, fades, channel layout, merging/concatenating audio
//! streams, and audio removal.

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::VideoResult;

/// Apply a gain change in dB to all audio streams (`volume` filter).
/// Negative values attenuate; output keeps the input container.
pub(crate) fn audio_volume(
    input: &[u8],
    gain_db: f64,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let _ = (input, gain_db, limits);
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "audio_volume not implemented".to_string(),
    ))
}

/// Normalize loudness toward the EBU R128 target (-23 LUFS
/// integrated by default) using `loudnorm`. Two-pass is preferred
/// when affordable; single-pass linear normalization is acceptable.
pub(crate) fn audio_normalize(
    input: &[u8],
    target_lufs: Option<f64>,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let _ = (input, target_lufs, limits);
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "audio_normalize not implemented".to_string(),
    ))
}

/// Mix `inputs` (audio files or containers with audio) into one
/// output via `amix`. All inputs must share a compatible sample
/// format — ffmpeg resamples where possible.
pub(crate) fn audio_merge(inputs: &[Vec<u8>], limits: &EngineLimits) -> VideoResult<Vec<u8>> {
    let _ = (inputs, limits);
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "audio_merge not implemented".to_string(),
    ))
}

/// Concatenate `inputs` end-to-end into `target_ext` (`mp3`, `wav`,
/// `aac`, `m4a`, `ogg`, `flac`, `opus`). Uses the concat demuxer with
/// stream copy where the codec allows, re-encode otherwise.
pub(crate) fn audio_concat(
    inputs: &[Vec<u8>],
    target_ext: &str,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let _ = (inputs, target_ext, limits);
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "audio_concat not implemented".to_string(),
    ))
}

/// Apply fade-in over `fade_in_secs` and fade-out over
/// `fade_out_secs` (0 disables that side).
pub(crate) fn audio_fade(
    input: &[u8],
    fade_in_secs: f64,
    fade_out_secs: f64,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let _ = (input, fade_in_secs, fade_out_secs, limits);
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "audio_fade not implemented".to_string(),
    ))
}

/// Strip all audio streams, keeping video/subtitle streams; output
/// container follows the input.
pub(crate) fn remove_audio(input: &[u8], limits: &EngineLimits) -> VideoResult<Vec<u8>> {
    let _ = (input, limits);
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "remove_audio not implemented".to_string(),
    ))
}

/// Convert channel layout: `channels = 1` downmixes to mono, `2`
/// upmixes/keeps stereo, `6` targets 5.1 where the codec allows.
pub(crate) fn channel_convert(
    input: &[u8],
    channels: u32,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let _ = (input, channels, limits);
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "channel_convert not implemented".to_string(),
    ))
}
