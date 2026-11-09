//! Locate the ffmpeg/ffprobe binaries on `PATH`.

use std::path::PathBuf;

use crate::crates::video_engine::error::VideoResult;
use crate::crates::video_engine::ops::{FFMPEG, FFPROBE, find_tool};

/// Resolved paths of the two required binaries.
#[derive(Debug, Clone)]
pub(crate) struct ToolPaths {
    /// Absolute path of the `ffmpeg` binary.
    pub ffmpeg: PathBuf,
    /// Absolute path of the `ffprobe` binary.
    pub ffprobe: PathBuf,
}

/// Resolve both binaries. Fails with
/// [`crate::crates::video_engine::error::VideoError::MissingTool`]
/// naming the first binary that could not be found.
pub(crate) fn discover() -> VideoResult<ToolPaths> {
    Ok(ToolPaths {
        ffmpeg: find_tool(FFMPEG)?,
        ffprobe: find_tool(FFPROBE)?,
    })
}
