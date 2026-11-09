//! Cut a time range out of a media input.
//!
//! Default mode is stream copy (`-c copy`) — fast, lossless, but cut
//! points snap to keyframes. `reencode = true` re-encodes video as
//! H.264 and audio as AAC for frame-accurate cuts (modern ffmpeg's
//! input-side `-ss` is frame-accurate when decoding).
//!
//! The output container is derived from the probed `format_name` —
//! trimming never changes the container family, because stream copy
//! cannot safely cross containers.

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::{VideoError, VideoResult};
use crate::crates::video_engine::ops::probe::probe;
use crate::crates::video_engine::ops::{
    COMMAND_TIMEOUT, FFMPEG, TempDir, check_input, check_limits, ext_for_format, find_tool,
    h264_aac_args, read_output, run_tool, tool_failure, write_input,
};

/// Validate a requested cut range. Returns `(start, end)` with `end`
/// `None` meaning "to the end of input".
fn checked_range(start_secs: f64, end_secs: Option<f64>) -> VideoResult<(f64, Option<f64>)> {
    if !start_secs.is_finite() || start_secs < 0.0 {
        return Err(VideoError::InvalidInput(format!(
            "start_secs must be a finite non-negative number, got {start_secs}"
        )));
    }
    match end_secs {
        Some(end) => {
            if !end.is_finite() || end <= start_secs {
                return Err(VideoError::InvalidInput(format!(
                    "end_secs must be finite and greater than start_secs \
                     ({start_secs}), got {end}"
                )));
            }
            Ok((start_secs, Some(end)))
        }
        None => Ok((start_secs, None)),
    }
}

/// Trim `input` to `[start_secs, end_secs)` (open-ended when `end_secs`
/// is `None`) and return the encoded bytes.
///
/// `reencode = false` stream-copies (`-c copy`); `true` re-encodes to
/// H.264/AAC for frame accuracy. Duration and input-size limits are
/// enforced on the probed input before ffmpeg runs.
pub(crate) fn trim(
    input: &[u8],
    start_secs: f64,
    end_secs: Option<f64>,
    reencode: bool,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let input_len = check_input(input)?;
    let (start, end) = checked_range(start_secs, end_secs)?;
    let program = find_tool(FFMPEG)?;

    // Probe once: limits, container detection for the output extension,
    // and a sanity check that the cut starts inside the media.
    let info = probe(input)?;
    check_limits(&info, input_len, limits, false)?;
    if let Some(duration) = info.duration_secs {
        if start >= duration {
            return Err(VideoError::InvalidInput(format!(
                "start_secs {start} is at or beyond the media duration {duration:.3}s"
            )));
        }
    }
    let ext = info
        .format_name
        .as_deref()
        .and_then(ext_for_format)
        .ok_or_else(|| {
            VideoError::InvalidInput(
                "cannot determine output container from probed format_name".to_string(),
            )
        })?;

    let dir = TempDir::new("trim")?;
    let input_path = write_input(&dir, "input.bin", input)?;
    let out_path = dir.file(&format!("out.{ext}"));

    let mut args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-ss".to_string(),
        format!("{start:.3}"),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
    ];
    if let Some(end) = end {
        args.push("-to".to_string());
        args.push(format!("{end:.3}"));
    }
    if reencode {
        args.extend(h264_aac_args()?);
    } else {
        args.extend(["-c".to_string(), "copy".to_string()]);
    }
    args.push(out_path.to_string_lossy().into_owned());

    let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(tool_failure(FFMPEG, &output));
    }
    read_output(&out_path)
}
