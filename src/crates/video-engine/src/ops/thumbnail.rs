//! Frame extraction: produce a PNG thumbnail at a chosen timestamp.
//!
//! `at_secs = None` seeks to ~10% of the probed duration (fallback 1s
//! when the container reports no duration). The image is written via
//! ffmpeg's `image2` muxer as PNG and read back as bytes.

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::{VideoError, VideoResult};
use crate::crates::video_engine::ops::probe::probe;
use crate::crates::video_engine::ops::{
    COMMAND_TIMEOUT, FFMPEG, TempDir, check_input, check_limits, find_tool, read_output, run_tool,
    tool_failure, write_input,
};

/// PNG signature — verified on the produced bytes so a silent
/// ffmpeg failure (e.g. audio-only input) cannot return garbage.
const PNG_MAGIC: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

/// Extract a PNG thumbnail from `input`.
///
/// `at_secs` selects the seek point; `None` uses ~10% of the probed
/// duration (1s fallback). `limits.max_pixels` bounds the probed frame
/// size and `limits.max_duration_secs` bounds the input duration —
/// both checked before ffmpeg runs.
pub(crate) fn thumbnail(
    input: &[u8],
    at_secs: Option<f64>,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let input_len = check_input(input)?;
    let program = find_tool(FFMPEG)?;

    // Probe first: limits need duration/dimensions, and the default
    // seek point needs the duration.
    let info = probe(input)?;
    check_limits(&info, input_len, limits, true)?;

    let at = match at_secs {
        Some(t) => {
            if !t.is_finite() || t < 0.0 {
                return Err(VideoError::InvalidInput(format!(
                    "at_secs must be a finite non-negative number, got {t}"
                )));
            }
            t
        }
        None => info
            .duration_secs
            .map(|d| d * 0.1)
            .filter(|t| *t > 0.0)
            .unwrap_or(1.0),
    };

    let dir = TempDir::new("thumb")?;
    let input_path = write_input(&dir, "input.bin", input)?;
    let out_path = dir.file("thumb.png");

    let args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-ss".to_string(),
        format!("{at:.3}"),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
        "-frames:v".to_string(),
        "1".to_string(),
        "-f".to_string(),
        "image2".to_string(),
        out_path.to_string_lossy().into_owned(),
    ];
    let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(tool_failure(FFMPEG, &output));
    }

    let png = read_output(&out_path)?;
    if png.len() < PNG_MAGIC.len() || &png[..PNG_MAGIC.len()] != PNG_MAGIC {
        return Err(VideoError::ExternalTool {
            tool: FFMPEG.to_string(),
            detail: "no video frame produced (input may have no video stream)".to_string(),
        });
    }
    Ok(png)
}
