//! Delivery packaging: HLS, MPEG-DASH, and faststart MP4.
//!
//! Multi-file outputs return [`MediaOutput`] — segment/playlist
//! names are deterministic (`index.m3u8`, `segment_%03d.ts`, ...).

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::VideoResult;
use crate::crates::video_engine::model::MediaOutput;

/// Package `input` as an HLS stream: `index.m3u8` +
/// `segment_000.ts`-style MPEG-TS segments (default 6s). Video is
/// re-encoded to H.264/AAC when needed for TS compatibility; stream
/// copy is used only when codecs already fit.
pub(crate) fn package_hls(input: &[u8], limits: &EngineLimits) -> VideoResult<MediaOutput> {
    let _ = (input, limits);
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "package_hls not implemented".to_string(),
    ))
}

/// Package `input` as MPEG-DASH: `manifest.mpd` + `chunk_*.m4s`
/// segments via `ffmpeg -f dash`.
pub(crate) fn package_dash(input: &[u8], limits: &EngineLimits) -> VideoResult<MediaOutput> {
    let _ = (input, limits);
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "package_dash not implemented".to_string(),
    ))
}

/// Rewrite an MP4/MOV with the `moov` atom at the front
/// (`-movflags +faststart`) for progressive-download playback.
/// Non-MP4-family inputs are rejected as `InvalidInput`.
pub(crate) fn faststart(input: &[u8], limits: &EngineLimits) -> VideoResult<Vec<u8>> {
    let _ = (input, limits);
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "faststart not implemented".to_string(),
    ))
}
