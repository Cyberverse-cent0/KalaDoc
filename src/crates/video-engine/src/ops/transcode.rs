//! Container conversion via ffmpeg.
//!
//! Allowed targets: `mp4`, `mkv`, `webm`, `avi`. `mp4` re-encodes to
//! H.264+AAC; `webm` re-encodes to VP9+Opus (requires those encoders —
//! verified up front so the error is honest); `mkv`/`avi` stream-copy
//! (`-c copy`) because both are general-purpose containers that accept
//! the common codec set — ffmpeg errors honestly if a codec cannot be
//! muxed there.

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::{VideoError, VideoResult};
use crate::crates::video_engine::ops::probe::probe;
use crate::crates::video_engine::ops::{
    COMMAND_TIMEOUT, FFMPEG, TempDir, check_input, check_limits, checked_ext, encoder_available,
    find_tool, read_output, run_tool, tool_failure, transcode_args, write_input,
};

/// Container targets `transcode` accepts.
pub(crate) const TRANSCODE_TARGETS: &[&str] = &["mp4", "mkv", "webm", "avi"];

/// Re-encode `input` into the container named by `target_ext`.
///
/// `target_ext` is validated against [`TRANSCODE_TARGETS`]
/// (case/whitespace-insensitive, leading dot tolerated). Duration and
/// input-size limits are enforced on the probed input before ffmpeg
/// runs.
pub(crate) fn transcode(
    input: &[u8],
    target_ext: &str,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let input_len = check_input(input)?;
    let ext = checked_ext(target_ext, TRANSCODE_TARGETS, "transcode")?;
    let program = find_tool(FFMPEG)?;

    if ext == "webm" && (!encoder_available("libvpx-vp9") || !encoder_available("libopus")) {
        return Err(VideoError::ExternalTool {
            tool: FFMPEG.to_string(),
            detail: "webm output requires the libvpx-vp9 and libopus encoders, \
                     which this ffmpeg build does not provide"
                .to_string(),
        });
    }

    let info = probe(input)?;
    check_limits(&info, input_len, limits, false)?;

    let dir = TempDir::new("transcode")?;
    let input_path = write_input(&dir, "input.bin", input)?;
    let out_path = dir.file(&format!("out.{ext}"));

    let mut args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
    ];
    args.extend(transcode_args(&ext)?);
    args.push(out_path.to_string_lossy().into_owned());

    let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(tool_failure(FFMPEG, &output));
    }
    read_output(&out_path)
}
