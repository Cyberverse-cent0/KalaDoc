//! Profile-driven encoding: map an [`EncodeProfile`] to ffmpeg
//! output arguments and run a transcode with them.
//!
//! Covers professional/intermediate codecs (ProRes, DNxHD/HR, FFV1,
//! Huffyuv, MJPEG, MPEG-2, Theora) plus the general quality knobs
//! (CRF, preset, bitrate, pix_fmt).

use std::collections::BTreeMap;

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::{VideoError, VideoResult};
use crate::crates::video_engine::model::EncodeProfile;

/// Named presets callers can select instead of hand-building an
/// [`EncodeProfile`]. Keys are stable identifiers surfaced through
/// the CLI/API.
pub(crate) fn preset_profiles() -> BTreeMap<&'static str, EncodeProfile> {
    BTreeMap::new()
}

/// Convert an [`EncodeProfile`] into the ffmpeg argument tail placed
/// before the output path. Verifies codec/encoder availability via
/// [`crate::crates::video_engine::ops::encoder_available`] and returns
/// [`VideoError::ExternalTool`] when a required encoder is missing —
/// never emits an invocation that is guaranteed to fail.
pub(crate) fn encode_args(profile: &EncodeProfile) -> VideoResult<Vec<String>> {
    let _ = profile;
    Err(VideoError::InvalidInput(
        "encode_args not implemented".to_string(),
    ))
}

/// Transcode `input` to `target_ext` using `profile`'s codec and
/// quality settings. `target_ext` selects the container/muxer; the
/// profile selects codecs. Stream-copy is never assumed — the caller
/// gets an honest error when the profile cannot be honored.
pub(crate) fn transcode_profile(
    input: &[u8],
    profile: &EncodeProfile,
    target_ext: &str,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let _ = (input, profile, target_ext, limits);
    Err(VideoError::InvalidInput(
        "transcode_profile not implemented".to_string(),
    ))
}
