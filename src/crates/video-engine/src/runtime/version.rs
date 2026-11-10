//! Version probing for ffmpeg/ffprobe.

use crate::crates::video_engine::error::{VideoError, VideoResult};
use crate::crates::video_engine::ops::{
    FFMPEG, FFPROBE, PROBE_TIMEOUT, failure_detail, find_tool, run_tool,
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
        return Err(VideoError::ExternalTool {
            tool: tool.to_string(),
            detail: failure_detail(&out, "version probe failed"),
        });
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let first = text.lines().next().unwrap_or_default().trim();
    if first.is_empty() {
        // Exit 0 with no stdout means the binary did not behave like
        // ffmpeg/ffprobe; surface it rather than returning "".
        return Err(VideoError::ExternalTool {
            tool: tool.to_string(),
            detail: "empty version output".to_string(),
        });
    }
    Ok(first.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffmpeg_version_reports_version_line() {
        let version = ffmpeg_version().expect("ffmpeg is installed on this machine");
        assert!(
            version.starts_with("ffmpeg version"),
            "unexpected ffmpeg version line: {version:?}"
        );
        // Single line only — the full `-version` banner is multi-line.
        assert!(!version.contains('\n'));
        assert!(!version.contains('\r'));
    }

    #[test]
    fn ffprobe_version_reports_version_line() {
        let version = ffprobe_version().expect("ffprobe is installed on this machine");
        assert!(
            version.starts_with("ffprobe version"),
            "unexpected ffprobe version line: {version:?}"
        );
        assert!(!version.contains('\n'));
        assert!(!version.contains('\r'));
    }

    #[test]
    fn versions_are_stable_across_calls() {
        assert_eq!(ffmpeg_version().unwrap(), ffmpeg_version().unwrap());
        assert_eq!(ffprobe_version().unwrap(), ffprobe_version().unwrap());
    }
}
