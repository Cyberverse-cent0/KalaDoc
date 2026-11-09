//! Extract an audio-only stream into a standalone audio file.
//!
//! Targets: `mp3` (libmp3lame), `wav` (PCM s16le), `aac`/`m4a` (AAC),
//! `ogg` (libvorbis). Video is always dropped (`-vn`); audio is
//! re-encoded to the target codec because raw stream copy cannot cross
//! container families.

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::{VideoError, VideoResult};
use crate::crates::video_engine::ops::probe::probe;
use crate::crates::video_engine::ops::{
    COMMAND_TIMEOUT, FFMPEG, TempDir, checked_ext, check_input, check_limits, encoder_available,
    find_tool, read_output, run_tool, tool_failure, write_input,
};

/// Audio container/codec targets `extract_audio` accepts.
pub(crate) const AUDIO_TARGETS: &[&str] = &["mp3", "wav", "aac", "m4a", "ogg"];

/// ffmpeg audio encoder + codec arguments per target.
fn audio_args(ext: &str) -> Vec<String> {
    match ext {
        "mp3" => vec!["-c:a".into(), "libmp3lame".into()],
        "wav" => vec!["-c:a".into(), "pcm_s16le".into()],
        "ogg" => vec!["-c:a".into(), "libvorbis".into()],
        // "aac" and "m4a" both carry AAC; the extension picks the muxer.
        _ => vec!["-c:a".into(), "aac".into()],
    }
}

/// Encoders `audio_args` depends on, per target — checked up front so a
/// minimal ffmpeg build fails honestly instead of mid-mux.
fn required_encoder(ext: &str) -> Option<&'static str> {
    match ext {
        "mp3" => Some("libmp3lame"),
        "ogg" => Some("libvorbis"),
        _ => None,
    }
}

/// Extract the audio track of `input` into `target_ext` format bytes.
///
/// `target_ext` is validated against [`AUDIO_TARGETS`]. Duration and
/// input-size limits are enforced on the probed input before ffmpeg
/// runs. Inputs with no audio stream fail inside ffmpeg and surface as
/// [`VideoError::ExternalTool`] with stderr detail.
pub(crate) fn extract_audio(
    input: &[u8],
    target_ext: &str,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    let input_len = check_input(input)?;
    let ext = checked_ext(target_ext, AUDIO_TARGETS, "extract_audio")?;
    let program = find_tool(FFMPEG)?;

    if let Some(enc) = required_encoder(&ext) {
        if !encoder_available(enc) {
            return Err(VideoError::ExternalTool {
                tool: FFMPEG.to_string(),
                detail: format!(
                    "{ext} output requires the {enc} encoder, which this ffmpeg build does not provide"
                ),
            });
        }
    }

    let info = probe(input)?;
    check_limits(&info, input_len, limits, false)?;

    let dir = TempDir::new("audio")?;
    let input_path = write_input(&dir, "input.bin", input)?;
    let out_path = dir.file(&format!("out.{ext}"));

    let mut args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
        "-vn".to_string(),
    ];
    args.extend(audio_args(&ext));
    args.push(out_path.to_string_lossy().into_owned());

    let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(tool_failure(FFMPEG, &output));
    }
    read_output(&out_path)
}
