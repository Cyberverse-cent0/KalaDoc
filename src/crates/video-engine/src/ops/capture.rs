//! Device/source/protocol enumeration. This engine does NOT perform
//! live capture in v1 — it reports what the ffmpeg build *could*
//! capture so callers and future versions can decide.

use crate::crates::video_engine::error::VideoResult;
use crate::crates::video_engine::model::CaptureDevice;

/// Enumerate capture-capable devices and sources compiled into this
/// ffmpeg build: `ffmpeg -devices` (indevs) plus platform sources
/// (`avfoundation` on macOS, `dshow` on Windows, `x11grab`,
/// `alsa`/`pulse` on Linux — whichever the build lists via
/// `ffmpeg -sources`).
pub(crate) fn list_devices() -> VideoResult<Vec<CaptureDevice>> {
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "list_devices not implemented".to_string(),
    ))
}

/// List input/output protocols the build supports (`ffmpeg
/// -protocols`), filtered to plausible live/streaming ones plus a
/// count of the rest.
pub(crate) fn list_protocols() -> VideoResult<Vec<CaptureDevice>> {
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "list_protocols not implemented".to_string(),
    ))
}

/// List hardware accelerations (`ffmpeg -hwaccels`) as
/// [`CaptureDevice`] entries of kind `"hwaccel"`.
pub(crate) fn list_hwaccels() -> VideoResult<Vec<CaptureDevice>> {
    Err(crate::crates::video_engine::error::VideoError::InvalidInput(
        "list_hwaccels not implemented".to_string(),
    ))
}

/// Pick the best available hardware acceleration for this host:
/// probe `ffmpeg -hwaccels` and prefer `cuda`/`vaapi`/`videotoolbox`/
/// `qsv`/`amf` in that order. `None` when nothing is usable — callers
/// must then run software-only.
pub(crate) fn detect_hwaccel() -> Option<String> {
    None
}
