//! Concatenate multiple media inputs into one file.
//!
//! All inputs are probed first. When every input reports the same
//! `format_name` the ffmpeg concat *demuxer* is used with `-c copy` —
//! fast and lossless. When containers differ the demuxer still feeds
//! the inputs but codecs are re-encoded to `target_ext`'s profile
//! (same mapping as [`transcode`](super::transcode)).
//!
//! Caveat, surfaced honestly rather than hidden: concat-demuxer input
//! must share stream layout (codec, resolution, sample rate) for the
//! output to be valid — differing geometries fail inside ffmpeg and
//! surface as [`VideoError::ExternalTool`].

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::{VideoError, VideoResult};
use crate::crates::video_engine::model::VideoInfo;
use crate::crates::video_engine::ops::probe::probe;
use crate::crates::video_engine::ops::transcode::TRANSCODE_TARGETS;
use crate::crates::video_engine::ops::{
    COMMAND_TIMEOUT, FFMPEG, TempDir, check_input, check_limits, checked_ext, encoder_available,
    find_tool, read_output, run_tool, tool_failure, transcode_args, write_input,
};

/// Concatenate `inputs` (at least one) into `target_ext` bytes.
///
/// `target_ext` is validated against the transcode target set
/// (`mp4`, `mkv`, `webm`, `avi`). Every input is probed and checked
/// against `limits` before any ffmpeg invocation.
pub(crate) fn concat(
    inputs: &[Vec<u8>],
    target_ext: &str,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    if inputs.is_empty() {
        return Err(VideoError::InvalidInput(
            "concat requires at least one input".to_string(),
        ));
    }
    let ext = checked_ext(target_ext, TRANSCODE_TARGETS, "concat")?;
    let program = find_tool(FFMPEG)?;

    if ext == "webm" && (!encoder_available("libvpx-vp9") || !encoder_available("libopus")) {
        return Err(VideoError::ExternalTool {
            tool: FFMPEG.to_string(),
            detail: "webm output requires the libvpx-vp9 and libopus encoders, \
                     which this ffmpeg build does not provide"
                .to_string(),
        });
    }

    // Probe every input up front: limits apply per input, and the
    // container comparison decides copy vs re-encode.
    let mut infos: Vec<VideoInfo> = Vec::with_capacity(inputs.len());
    for input in inputs {
        let len = check_input(input)?;
        let info = probe(input)?;
        check_limits(&info, len, limits, false)?;
        infos.push(info);
    }
    let same_container = infos.iter().all(|i| i.format_name == infos[0].format_name);

    let dir = TempDir::new("concat")?;

    // Write inputs with a sniffable extension so the concat demuxer
    // picks the right demuxer for each file.
    let mut list = String::new();
    for (idx, input) in inputs.iter().enumerate() {
        let in_ext = infos[idx]
            .format_name
            .as_deref()
            .and_then(crate::crates::video_engine::ops::ext_for_format)
            .unwrap_or("bin");
        let path = write_input(&dir, &format!("in{idx}.{in_ext}"), input)?;
        // Temp paths are generated (pid/counter/nanos) and never contain
        // quotes, so single-quoting is safe for the concat list format.
        list.push_str(&format!("file '{}'\n", path.display()));
    }
    let list_path = dir.file("list.txt");
    std::fs::write(&list_path, list)?;

    let out_path = dir.file(&format!("out.{ext}"));
    let mut args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-f".to_string(),
        "concat".to_string(),
        "-safe".to_string(),
        "0".to_string(),
        "-i".to_string(),
        list_path.to_string_lossy().into_owned(),
    ];
    if same_container {
        args.extend(["-c".to_string(), "copy".to_string()]);
    } else {
        args.extend(transcode_args(&ext)?);
    }
    args.push(out_path.to_string_lossy().into_owned());

    let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(tool_failure(FFMPEG, &output));
    }
    read_output(&out_path)
}
