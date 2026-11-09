//! Cross-domain conversions: audio → video (with waveform
//! visualization), GIF → video, and video → audio-with-artwork.

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::VideoResult;

/// Visualization style for [`audio_to_video`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisualStyle {
    /// Oscilloscope waveform (`showwaves`).
    Waveform,
    /// Frequency spectrum (`showspectrum`).
    Spectrum,
    /// Combined waveform+spectrum split (`showwaves` + `showspectrum`
    /// stacked via `vstack`).
    Both,
}

/// Turn an audio file into a video with a generated visualization.
/// `style` selects the visual; output is `mp4` (or `webm` when no
/// H.264 encoder exists and VP9/Opus do).
pub(crate) fn audio_to_video(
    input: &[u8],
    style: VisualStyle,
    width: u32,
    height: u32,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let _ = (input, style, width, height, limits);
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "audio_to_video not implemented".to_string(),
    ))
}

/// Convert an animated GIF to a real video (`mp4`/`webm`/`mkv`).
/// GIF frame delays are preserved; palettes are converted to
/// yuv420p for compatibility.
pub(crate) fn gif_to_video(
    input: &[u8],
    target_ext: &str,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let _ = (input, target_ext, limits);
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "gif_to_video not implemented".to_string(),
    ))
}

/// Mux `audio` as the soundtrack of `video` *without* re-encoding
/// video (stream copy) — the lightweight counterpart to
/// [`crate::crates::video_engine::ops::stream_ops::merge_av`].
/// Fails when the audio codec cannot be muxed into the video
/// container; caller should fall back to merge_av.
pub(crate) fn replace_audio_track(
    video: &[u8],
    audio: &[u8],
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let _ = (video, audio, limits);
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "replace_audio_track not implemented".to_string(),
    ))
}
