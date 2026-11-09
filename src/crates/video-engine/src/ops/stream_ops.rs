//! Stream-level container surgery: select/map streams, merge audio
//! from a separate source, and edit/strip metadata including cover
//! art.

use std::collections::BTreeMap;

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::VideoResult;
use crate::crates::video_engine::model::StreamFilter;

/// Keep only the streams allowed by `filter`, remuxing into
/// `target_ext` (stream copy — codecs must be compatible with the
/// target container).
pub(crate) fn select_streams(
    input: &[u8],
    filter: &StreamFilter,
    target_ext: &str,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let _ = (input, filter, target_ext, limits);
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "select_streams not implemented".to_string(),
    ))
}

/// Merge `audio` (any audio-bearing file) into `video` as its audio
/// track — video stream copied, audio re-encoded/converted as
/// needed. Output container follows `target_ext` or the video
/// input's family.
pub(crate) fn merge_av(
    video: &[u8],
    audio: &[u8],
    target_ext: &str,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let _ = (video, audio, target_ext, limits);
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "merge_av not implemented".to_string(),
    ))
}

/// Set container/stream tags. Keys are normalized lowercase;
/// reserved keys (`encoder`) may be rejected. Streams' existing tags
/// are preserved unless overridden.
pub(crate) fn set_metadata(
    input: &[u8],
    tags: &BTreeMap<String, String>,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let _ = (input, tags, limits);
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "set_metadata not implemented".to_string(),
    ))
}

/// Strip all metadata (container tags and per-stream tags) while
/// keeping media streams; remuxes into `target_ext`.
pub(crate) fn strip_metadata(
    input: &[u8],
    target_ext: &str,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let _ = (input, target_ext, limits);
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "strip_metadata not implemented".to_string(),
    ))
}

/// Attach or replace the cover art (`cover` is an image file:
/// png/jpg). Only meaningful for containers supporting attached
/// pictures (mp3, flac, mp4, m4a, ogg).
pub(crate) fn set_cover_art(
    input: &[u8],
    cover: &[u8],
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let _ = (input, cover, limits);
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "set_cover_art not implemented".to_string(),
    ))
}
