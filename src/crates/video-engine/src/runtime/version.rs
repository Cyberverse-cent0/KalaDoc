//! Version probing for ffmpeg/ffprobe.

use crate::crates::video_engine::error::VideoResult;
use crate::crates::video_engine::ops::{
    FFMPEG, FFPROBE, PROBE_TIMEOUT, find_tool, run_tool,
};

/// First line of `ffmpeg -version` output, e.g.
/// `"ffmpeg version 7.1.5-..."`.
pub(crate) fn ffmpeg_version() -> VideoResult<String> {
    version_line(FFMPEG, find_tool(FFMPEG)?)
}

/// First line of `ffprobe -version` output.
pub(crate) fn ffprobe_version() -> VideoResult<String> {
    version_line(FFPROBE, find_tool(FFPROBE)?)
}

fn version_line(tool: &str, program: std::path::PathBuf) -> VideoResult<String> {
    let args = vec!["-version".to_string()];
    let out = run_tool(tool, &program, &args, PROBE_TIMEOUT)?;
    if !out.status.success() {
        return Err(crate::crates::video_engine::error::VideoError::ExternalTool {
            tool: tool.to_string(),
            detail: "version probe failed".to_string(),
        });
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string())
}
