//! General transcode — re-encode audio/video to a target container
//! and/or codecs.
//!
//! [`transcode`] writes the input buffer to a temp file, runs
//! `ffmpeg` with an argv vector built from [`TranscodeOptions`], and
//! reads the produced file back under [`MediaLimits::max_output_bytes`].
//!
//! Codec names in [`TranscodeOptions`] are **FFmpeg encoder names**
//! (the tokens from `ffmpeg -encoders`, e.g. `"libx264"`, `"aac"`,
//! `"libvpx-vp9"`) and are validated against
//! [`Runtime::capabilities`] before ffmpeg is invoked — a build
//! without the encoder yields [`MediaError::Unsupported`], never a
//! silent fallback.

use std::ffi::OsString;

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Options for [`transcode`].
///
/// Every field is optional except `output_format`; unset fields keep
/// ffmpeg's defaults (codec chosen by the muxer, source bitrate/fps).
///
/// `extra_args` is an **escape hatch**: each entry is appended
/// verbatim to the ffmpeg argv immediately before the output path.
/// Use it for options this struct does not model (`-map`,
/// `-metadata`, per-stream options, filters, ...). Entries are passed
/// as single argv elements — no shell splitting occurs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TranscodeOptions {
    /// Output container extension / format name without a leading dot
    /// (e.g. `"mp4"`, `"mkv"`, `"webm"`). Becomes `output.<format>`.
    pub output_format: String,
    /// Video encoder name (`ffmpeg -encoders` token, e.g. `"libx264"`).
    /// `None` lets ffmpeg pick the muxer's default.
    pub video_codec: Option<String>,
    /// Audio encoder name (e.g. `"aac"`, `"libopus"`).
    pub audio_codec: Option<String>,
    /// Video bitrate as an ffmpeg bitrate string (e.g. `"1M"`,
    /// `"500k"`). Passed as `-b:v`.
    pub video_bitrate: Option<String>,
    /// Audio bitrate string (e.g. `"128k"`). Passed as `-b:a`.
    pub audio_bitrate: Option<String>,
    /// Constant-quality CRF value `0..=51`, passed as `-crf`. Honoured
    /// by `libx264`, `libx265`, `libvpx`, `libaom-av1`, etc.; ignored
    /// (with a warning) by encoders that do not support it.
    pub crf: Option<u8>,
    /// Encoder preset name (e.g. `"medium"`, `"fast"`). Passed as
    /// `-preset`; only meaningful for encoders that accept one.
    pub preset: Option<String>,
    /// ffmpeg `scale` filter expression (e.g. `"1280:-2"`,
    /// `"iw/2:ih/2"`). Passed as `-vf scale=<expr>`.
    pub scale: Option<String>,
    /// Output frame rate in fps. Passed as `-r`.
    pub fps: Option<f64>,
    /// Extra ffmpeg arguments appended verbatim before the output
    /// path. Escape hatch for anything not modeled above.
    #[serde(default)]
    pub extra_args: Vec<String>,
}

/// Re-encode `input` into `options.output_format`.
///
/// Runs `ffmpeg -hide_banner -loglevel error -nostdin -y -i <in>`
/// followed by the codec/filter options implied by `options`, then
/// reads the output back under `limits`.
///
/// # Errors
///
/// - [`MediaError::InvalidInput`] — empty/oversized input, `crf` out
///   of range, non-positive `fps`.
/// - [`MediaError::Unsupported`] — a requested encoder is not in this
///   FFmpeg build's capability set.
/// - [`MediaError::ToolFailed`] / [`MediaError::Timeout`] — ffmpeg
///   rejected the input or options.
pub fn transcode(
    input: &[u8],
    options: &TranscodeOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    if input.is_empty() {
        return Err(MediaError::InvalidInput("empty input buffer".to_string()));
    }
    if let Some(max) = limits.max_input_bytes
        && (input.len() as u64) > max
    {
        return Err(MediaError::LimitExceeded(format!(
            "input is {} bytes, exceeding max_input_bytes {max}",
            input.len()
        )));
    }
    if options.output_format.trim().is_empty() {
        return Err(MediaError::InvalidInput(
            "output_format must not be empty".to_string(),
        ));
    }
    if let Some(crf) = options.crf
        && crf > 51
    {
        return Err(MediaError::InvalidInput(format!(
            "crf must be in 0..=51, got {crf}"
        )));
    }
    if let Some(fps) = options.fps
        && (!fps.is_finite() || fps <= 0.0)
    {
        return Err(MediaError::InvalidInput(format!(
            "fps must be a positive finite number, got {fps}"
        )));
    }

    // Honest capabilities: requested encoders must exist in this build.
    let caps = rt.capabilities()?;
    for (kind, codec) in [
        ("video", options.video_codec.as_deref()),
        ("audio", options.audio_codec.as_deref()),
    ] {
        if let Some(codec) = codec
            && !caps.has_encoder(codec)
        {
            return Err(MediaError::Unsupported(format!(
                "{kind} encoder '{codec}' is not available in this FFmpeg build"
            )));
        }
    }

    let dir = TempDir::new("transcode")?;
    let input_name = "input.bin";
    // ffmpeg probes by content; a neutral extension avoids misleading
    // the demuxer when callers pass raw elementary streams.
    dir.write(input_name, input)?;

    let output_ext = options.output_format.trim().trim_start_matches('.');
    let output_name = format!("output.{output_ext}");
    let output_path = dir.file(&output_name);

    let mut argv: Vec<OsString> = vec![
        OsString::from("-hide_banner"),
        OsString::from("-loglevel"),
        OsString::from("error"),
        OsString::from("-nostdin"),
        OsString::from("-y"),
        OsString::from("-i"),
        dir.file(input_name).into_os_string(),
    ];

    // Video filters / frame-rate: -vf scale, -r fps.
    if let Some(scale) = options.scale.as_deref().filter(|s| !s.is_empty()) {
        argv.push(OsString::from("-vf"));
        argv.push(OsString::from(format!("scale={scale}")));
    }
    if let Some(fps) = options.fps {
        argv.push(OsString::from("-r"));
        argv.push(OsString::from(format_fps(fps)));
    }

    // Video codec options.
    if let Some(codec) = options.video_codec.as_deref() {
        argv.push(OsString::from("-c:v"));
        argv.push(OsString::from(codec));
    }
    if let Some(bitrate) = options.video_bitrate.as_deref() {
        argv.push(OsString::from("-b:v"));
        argv.push(OsString::from(bitrate));
    }
    if let Some(crf) = options.crf {
        argv.push(OsString::from("-crf"));
        argv.push(OsString::from(crf.to_string()));
    }
    if let Some(preset) = options.preset.as_deref() {
        argv.push(OsString::from("-preset"));
        argv.push(OsString::from(preset));
    }

    // Audio codec options.
    if let Some(codec) = options.audio_codec.as_deref() {
        argv.push(OsString::from("-c:a"));
        argv.push(OsString::from(codec));
    }
    if let Some(bitrate) = options.audio_bitrate.as_deref() {
        argv.push(OsString::from("-b:a"));
        argv.push(OsString::from(bitrate));
    }

    // Escape hatch: verbatim args immediately before the output path.
    for arg in &options.extra_args {
        argv.push(OsString::from(arg));
    }

    argv.push(output_path.clone().into_os_string());

    process::run_checked("ffmpeg", &rt.ffmpeg, &argv, None, limits)?;

    let bytes = dir.read_limited(&output_name, limits.max_output_bytes)?;
    if bytes.is_empty() {
        return Err(MediaError::ToolFailed {
            tool: "ffmpeg",
            status: "empty output".to_string(),
            detail: format!("ffmpeg produced an empty {output_name}"),
        });
    }

    let mut out = MediaOutput::single(output_name, bytes);
    if let Some(file) = out.primary.as_mut() {
        file.content_type = content_type_for(output_ext);
    }
    Ok(out)
}

/// Format an fps value compactly (`"30"` not `"30.0"`).
fn format_fps(fps: f64) -> String {
    if fps.fract() == 0.0 {
        format!("{}", fps as i64)
    } else {
        // Trim to millisecond precision — enough for any sane rate.
        let rounded = (fps * 1000.0).round() / 1000.0;
        format!("{rounded}")
    }
}

/// Best-effort MIME type for common output containers.
fn content_type_for(format: &str) -> Option<String> {
    let mime = match format.to_ascii_lowercase().as_str() {
        "mp4" | "m4v" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        "mp3" => "audio/mpeg",
        "m4a" | "aac" => "audio/mp4",
        "wav" => "audio/wav",
        "ogg" | "opus" => "audio/ogg",
        "flac" => "audio/flac",
        "gif" => "image/gif",
        _ => return None,
    };
    Some(mime.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::multimedia_engine::runtime::process::run;
    use std::path::PathBuf;

    fn runtime() -> Runtime {
        Runtime::discover().expect("ffmpeg/ffprobe must be on PATH")
    }

    fn limits() -> MediaLimits {
        MediaLimits::default()
    }

    /// Generate a 1-second testsrc+sine mp4 via lavfi.
    fn sample_input() -> Vec<u8> {
        let rt = runtime();
        let dir = TempDir::new("transcode-test-in").expect("tempdir");
        let out_path: PathBuf = dir.file("sample.mp4");
        let argv = process::args(&[
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=1:size=160x120:rate=10",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=1",
            "-c:v",
            "mpeg4",
            "-c:a",
            "aac",
            "-shortest",
            out_path.to_str().expect("utf8 path"),
        ]);
        run("ffmpeg", &rt.ffmpeg, &argv, None, &limits()).expect("generate sample");
        std::fs::read(&out_path).expect("read sample")
    }

    /// Run ffprobe on `bytes` (written to a temp file) and return the
    /// JSON document.
    fn probe_json(rt: &Runtime, name: &str, bytes: &[u8]) -> serde_json::Value {
        let dir = TempDir::new("transcode-test-probe").expect("tempdir");
        let path = dir.write(name, bytes).expect("write probe input");
        let argv = process::args(&[
            "-v",
            "error",
            "-show_format",
            "-show_streams",
            "-of",
            "json",
            path.to_str().expect("utf8 path"),
        ]);
        let out = run("ffprobe", &rt.ffprobe, &argv, None, &limits()).expect("ffprobe");
        assert!(out.success, "ffprobe failed: {}", out.stderr);
        serde_json::from_slice(&out.stdout).expect("ffprobe json")
    }

    #[test]
    fn transcode_mp4_to_mkv_h264_aac() {
        let rt = runtime();
        let input = sample_input();
        assert!(!input.is_empty());

        let options = TranscodeOptions {
            output_format: "mkv".to_string(),
            video_codec: Some("libopenh264".to_string()),
            audio_codec: Some("aac".to_string()),
            ..Default::default()
        };
        let out = transcode(&input, &options, &limits(), &rt).expect("transcode");
        let file = out.primary.expect("primary output");
        assert_eq!(file.name, "output.mkv");
        assert!(!file.bytes.is_empty());

        let report = probe_json(&rt, "out.mkv", &file.bytes);
        let format_name = report["format"]["format_name"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert!(
            format_name.contains("matroska"),
            "expected matroska container, got {format_name}"
        );
        let codecs: Vec<&str> = report["streams"]
            .as_array()
            .expect("streams array")
            .iter()
            .filter_map(|s| s["codec_name"].as_str())
            .collect();
        assert!(codecs.contains(&"h264"), "codecs: {codecs:?}");
        assert!(codecs.contains(&"aac"), "codecs: {codecs:?}");
    }

    #[test]
    fn transcode_rejects_unknown_encoder() {
        let rt = runtime();
        let input = sample_input();
        let options = TranscodeOptions {
            output_format: "mkv".to_string(),
            video_codec: Some("no_such_codec".to_string()),
            ..Default::default()
        };
        let err = transcode(&input, &options, &limits(), &rt).expect_err("must fail");
        match err {
            MediaError::Unsupported(msg) => {
                assert!(msg.contains("no_such_codec"), "msg: {msg}");
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn transcode_scale_fps_and_bit_rate() {
        let rt = runtime();
        let input = sample_input();
        let options = TranscodeOptions {
            output_format: "mp4".to_string(),
            video_codec: Some("mpeg4".to_string()),
            audio_codec: Some("aac".to_string()),
            video_bitrate: Some("400k".to_string()),
            audio_bitrate: Some("96k".to_string()),
            scale: Some("320:-2".to_string()),
            fps: Some(15.0),
            extra_args: vec!["-movflags".to_string(), "+faststart".to_string()],
            ..Default::default()
        };
        let out = transcode(&input, &options, &limits(), &rt).expect("transcode");
        let file = out.primary.expect("primary output");
        assert_eq!(file.name, "output.mp4");
        assert!(!file.bytes.is_empty());

        let report = probe_json(&rt, "out.mp4", &file.bytes);
        let video = report["streams"]
            .as_array()
            .expect("streams")
            .iter()
            .find(|s| s["codec_type"].as_str() == Some("video"))
            .expect("video stream");
        assert_eq!(video["width"].as_u64(), Some(320));
        let rate = video["r_frame_rate"].as_str().unwrap_or_default();
        assert!(
            rate.starts_with("15") || rate == "15/1",
            "unexpected frame rate {rate}"
        );
    }
}
