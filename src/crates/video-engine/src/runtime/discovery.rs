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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_finds_both_tools_on_path() {
        // ffmpeg/ffprobe are required dev dependencies of this engine;
        // they are installed in CI/dev images, so discovery must succeed.
        let paths = discover().expect("ffmpeg and ffprobe are installed");
        assert_eq!(paths.ffmpeg.file_name().unwrap(), FFMPEG);
        assert_eq!(paths.ffprobe.file_name().unwrap(), FFPROBE);
        assert!(paths.ffmpeg.is_file());
        assert!(paths.ffprobe.is_file());
    }

    #[test]
    fn discover_is_deterministic() {
        let a = discover().unwrap();
        let b = discover().unwrap();
        assert_eq!(a.ffmpeg, b.ffmpeg);
        assert_eq!(a.ffprobe, b.ffprobe);
    }
}
