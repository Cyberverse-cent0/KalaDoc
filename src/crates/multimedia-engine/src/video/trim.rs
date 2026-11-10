//! Trim/cut a media file to a time range.
//!
//! [`trim`] writes the input buffer to a temp file, runs `ffmpeg`
//! with `-ss`/`-to`, and reads the produced file back under
//! [`MediaLimits`]. Two modes:
//!
//! - **Accurate** (`TrimOptions::accurate = true`, the default):
//!   `ffmpeg -ss <start> -to <end> -i <in> ...` re-encodes the
//!   streams (`libx264`/`aac` for MP4-like containers). Both `-ss`
//!   and `-to` are placed *before* `-i`, i.e. they are input options:
//!   ffmpeg seeks to the nearest keyframe at or before `start`,
//!   decodes forward, and discards frames until `start` — so the cut
//!   lands on the exact requested time at the cost of a full
//!   decode/re-encode of the range.
//! - **Fast** (`accurate = false`): same `-ss`/`-to` input options
//!   but with `-c copy`. No re-encode — the cut snaps to the nearest
//!   keyframe/packet boundary, which is much faster on long files but
//!   can include a few frames before `start` or drop frames near
//!   `end`. A warning is recorded in [`MediaOutput::warnings`] when
//!   this mode is used.
//!
//! `end = None` means "to the end of the input" (`-to` is omitted).
//! `audio_trim = false` adds `-an`, dropping all audio streams.

use std::ffi::OsString;

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaFile, MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Options for [`trim`]. Serde-friendly: all fields deserialize with
/// sensible defaults when omitted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrimOptions {
    /// Start of the kept range, in seconds. Must be `>= 0` and finite.
    pub start: f64,
    /// End of the kept range, in seconds. `None` keeps everything to
    /// the end of the input. When `Some`, must be `> start`.
    #[serde(default)]
    pub end: Option<f64>,
    /// Frame-accurate trim via re-encode (default `true`). `false`
    /// uses `-c copy` for a fast keyframe-snapped cut.
    #[serde(default = "default_true")]
    pub accurate: bool,
    /// Output container extension without a leading dot
    /// (`"mp4"`, `"mkv"`, `"webm"`, ...). Becomes `output.<format>`.
    /// `None` defaults to `"mp4"` for the accurate path and to the
    /// input's sniffed container for the copy path.
    #[serde(default)]
    pub output_format: Option<String>,
    /// Keep audio streams (default `true`). `false` adds `-an`.
    #[serde(default = "default_true")]
    pub audio_trim: bool,
}

impl Default for TrimOptions {
    fn default() -> Self {
        Self {
            start: 0.0,
            end: None,
            accurate: true,
            output_format: None,
            audio_trim: true,
        }
    }
}

fn default_true() -> bool {
    true
}

/// Trim `input` to `options.start..options.end` (end-exclusive-ish:
/// ffmpeg stops at the first packet/frame at or after `end`).
///
/// # Errors
///
/// - [`MediaError::InvalidInput`] — empty input, negative/NaN
///   `start`, non-finite `end`, or `end <= start`.
/// - [`MediaError::LimitExceeded`] — input exceeds
///   `max_input_bytes`, or the output exceeds `max_output_bytes`.
/// - [`MediaError::Unsupported`] — a codec this op needs (e.g.
///   `libx264` on the accurate path) is missing from this FFmpeg
///   build.
/// - [`MediaError::ToolFailed`] / [`MediaError::Timeout`] — ffmpeg
///   rejected the input or the requested cut is impossible (e.g.
///   stream copy into an incompatible container).
pub fn trim(
    input: &[u8],
    options: &TrimOptions,
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
    validate_options(options)?;

    let dir = TempDir::new("trim")?;
    // ffmpeg probes by content; a neutral name avoids misleading the
    // demuxer when callers pass raw elementary streams.
    dir.write("input.bin", input)?;

    let output_ext = resolve_output_ext(input, options);
    let output_name = format!("output.{output_ext}");
    let output_path = dir.file(&output_name);

    // Common argv head: quiet, no stdin interaction, overwrite.
    let mut argv: Vec<OsString> = vec![
        OsString::from("-hide_banner"),
        OsString::from("-loglevel"),
        OsString::from("error"),
        OsString::from("-nostdin"),
        OsString::from("-y"),
    ];

    // Input options: seek and stop before -i. With input seeking,
    // -ss snaps to the previous keyframe and ffmpeg decodes forward
    // to the exact time; on the copy path it simply starts emitting
    // from that keyframe.
    argv.push(OsString::from("-ss"));
    argv.push(OsString::from(fmt_secs(options.start)));
    if let Some(end) = options.end {
        argv.push(OsString::from("-to"));
        argv.push(OsString::from(fmt_secs(end)));
    }
    argv.push(OsString::from("-i"));
    argv.push(dir.file("input.bin").into_os_string());

    let mut warnings = Vec::new();
    if options.accurate {
        // Re-encode for frame accuracy. Pick sane per-container
        // defaults and verify the encoders exist in this build.
        let codecs = accurate_codecs(&output_ext, rt)?;
        for (flag, codec) in &codecs {
            argv.push(OsString::from(*flag));
            argv.push(OsString::from(*codec));
        }
        argv.extend(extra_codec_args(&output_ext));
    } else {
        argv.push(OsString::from("-c"));
        argv.push(OsString::from("copy"));
        warnings
            .push("fast copy trim: cut snaps to the nearest keyframe/packet boundary".to_string());
    }

    if !options.audio_trim {
        argv.push(OsString::from("-an"));
    }

    argv.push(output_path.into_os_string());

    process::run_checked("ffmpeg", &rt.ffmpeg, &argv, None, limits)?;

    let bytes = dir.read_limited(&output_name, limits.max_output_bytes)?;
    if bytes.is_empty() {
        return Err(MediaError::ToolFailed {
            tool: "ffmpeg",
            status: "empty output".to_string(),
            detail: format!("ffmpeg produced an empty {output_name}"),
        });
    }

    let out = MediaOutput {
        primary: Some(MediaFile {
            name: output_name.clone(),
            bytes,
            content_type: content_type_for(&output_ext),
        }),
        extra: Vec::new(),
        warnings,
    };
    debug_assert!(out.primary.is_some());
    Ok(out)
}

/// Validate the numeric fields of `options`.
fn validate_options(options: &TrimOptions) -> MediaResult<()> {
    if !options.start.is_finite() || options.start < 0.0 {
        return Err(MediaError::InvalidInput(format!(
            "start must be a finite number >= 0, got {}",
            options.start
        )));
    }
    if let Some(end) = options.end {
        if !end.is_finite() {
            return Err(MediaError::InvalidInput(format!(
                "end must be finite, got {end}"
            )));
        }
        if end <= options.start {
            return Err(MediaError::InvalidInput(format!(
                "end ({end}) must be greater than start ({})",
                options.start
            )));
        }
    }
    Ok(())
}

/// Format seconds for ffmpeg's `-ss`/`-to`. Three decimals is
/// accepted by ffmpeg's duration parser and is plenty of precision
/// for media time.
fn fmt_secs(secs: f64) -> String {
    format!("{secs:.3}")
}

/// Determine the output container extension: the explicit
/// `output_format` when given, otherwise `"mp4"` for the accurate
/// path and the input's sniffed container for the copy path (copy
/// cannot change containers freely).
fn resolve_output_ext(input: &[u8], options: &TrimOptions) -> String {
    if let Some(fmt) = options.output_format.as_deref() {
        let fmt = fmt.trim().trim_start_matches('.').to_ascii_lowercase();
        if !fmt.is_empty() {
            return fmt;
        }
    }
    if options.accurate {
        "mp4".to_string()
    } else {
        sniff_container(input).to_string()
    }
}

/// Best-effort container guess from magic bytes, used only to pick a
/// sane default output extension for the stream-copy path.
fn sniff_container(data: &[u8]) -> &'static str {
    if data.len() >= 12 && &data[4..8] == b"ftyp" {
        return if &data[8..12] == b"qt  " {
            "mov"
        } else {
            "mp4"
        };
    }
    if data.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return "mkv";
    }
    if data.starts_with(b"RIFF") && data.len() >= 12 {
        return match &data[8..12] {
            b"WAVE" => "wav",
            b"AVI " => "avi",
            _ => "mp4",
        };
    }
    if data.starts_with(b"OggS") {
        return "ogg";
    }
    if data.starts_with(b"fLaC") {
        return "flac";
    }
    if data.starts_with(b"ID3") || (data.len() >= 2 && data[0] == 0xFF && (data[1] & 0xE0) == 0xE0)
    {
        return "mp3";
    }
    "mp4"
}

/// Pick the re-encode codec set for `ext`, verifying each encoder
/// exists in this FFmpeg build. Returns `(flag, encoder)` pairs in
/// argv order.
///
/// Choices:
/// - MP4-like containers (`mp4`, `m4v`, `mov`, `3gp`, `mkv`, `m4a`):
///   `libx264` veryfast/crf 23 + `aac` 128k.
/// - `webm`: `libvpx-vp9` (fallback `libvpx`) + `libopus`
///   (fallback `libvorbis`).
/// - Audio-only targets: `mp3` → `libmp3lame`, `wav` → `pcm_s16le`,
///   `ogg`/`oga` → `libvorbis`, `opus` → `libopus`, `flac` → `flac`,
///   `aac`/`adts` → `aac`.
/// - Anything else falls back to the MP4 defaults — ffmpeg will fail
///   honestly if the muxer cannot take those codecs.
fn accurate_codecs(ext: &str, rt: &Runtime) -> MediaResult<Vec<(&'static str, &'static str)>> {
    let caps = rt.capabilities()?;
    let need = |kind: &str, name: &'static str| -> MediaResult<()> {
        if caps.has_encoder(name) {
            Ok(())
        } else {
            Err(MediaError::Unsupported(format!(
                "{kind} encoder '{name}' is not available in this FFmpeg build"
            )))
        }
    };

    match ext {
        "webm" => {
            let v = if caps.has_encoder("libvpx-vp9") {
                "libvpx-vp9"
            } else if caps.has_encoder("libvpx") {
                "libvpx"
            } else {
                return Err(MediaError::Unsupported(
                    "no WebM video encoder (libvpx-vp9/libvpx) in this FFmpeg build".to_string(),
                ));
            };
            let a = if caps.has_encoder("libopus") {
                "libopus"
            } else if caps.has_encoder("libvorbis") {
                "libvorbis"
            } else {
                return Err(MediaError::Unsupported(
                    "no WebM audio encoder (libopus/libvorbis) in this FFmpeg build".to_string(),
                ));
            };
            need("video", v)?;
            need("audio", a)?;
            Ok(vec![("-c:v", v), ("-c:a", a)])
        }
        "mp3" => {
            need("audio", "libmp3lame")?;
            Ok(vec![("-c:a", "libmp3lame")])
        }
        "wav" => Ok(vec![("-c:a", "pcm_s16le")]),
        "ogg" | "oga" => {
            need("audio", "libvorbis")?;
            Ok(vec![("-c:a", "libvorbis")])
        }
        "opus" => {
            need("audio", "libopus")?;
            Ok(vec![("-c:a", "libopus")])
        }
        "flac" => Ok(vec![("-c:a", "flac")]),
        "aac" | "adts" => {
            need("audio", "aac")?;
            Ok(vec![("-c:a", "aac")])
        }
        _ => {
            // mp4/mov/mkv/m4v/m4a and unknown extensions.
            need("video", "libx264")?;
            need("audio", "aac")?;
            Ok(vec![("-c:v", "libx264"), ("-c:a", "aac")])
        }
    }
}

/// Extra codec-quality arguments for the accurate path, keyed by
/// container. Kept separate from `accurate_codecs` so the encoder
/// checks stay simple.
fn extra_codec_args(ext: &str) -> Vec<OsString> {
    let pairs: &[(&str, &str)] = match ext {
        "webm" => &[("-crf", "33"), ("-b:v", "0"), ("-b:a", "128k")],
        "mp3" => &[("-b:a", "192k")],
        "ogg" | "oga" => &[("-b:a", "160k")],
        "opus" => &[("-b:a", "128k")],
        "aac" | "adts" => &[("-b:a", "128k")],
        _ => &[
            ("-preset", "veryfast"),
            ("-crf", "23"),
            ("-pix_fmt", "yuv420p"),
            ("-b:a", "128k"),
        ],
    };
    pairs
        .iter()
        .flat_map(|(k, v)| [OsString::from(*k), OsString::from(*v)])
        .collect()
}

/// Best-effort MIME type for the produced container.
fn content_type_for(ext: &str) -> Option<String> {
    let mime = match ext {
        "mp4" | "m4v" | "m4a" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        "mp3" => "audio/mpeg",
        "aac" | "adts" => "audio/aac",
        "wav" => "audio/wav",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "flac" => "audio/flac",
        _ => return None,
    };
    Some(mime.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::multimedia_engine::probe::inspect::inspect;
    use crate::crates::multimedia_engine::runtime::discovery;

    fn runtime() -> Runtime {
        Runtime::discover().expect("ffmpeg/ffprobe must be on PATH")
    }

    fn limits() -> MediaLimits {
        MediaLimits::default()
    }

    /// Generate a 3-second 64x64@15fps testsrc + 440Hz sine mp4 via
    /// lavfi with a small GOP (15 frames = 1s) so the stream-copy
    /// path has keyframes inside the trim range.
    fn sample_input() -> Option<Vec<u8>> {
        let ffmpeg = discovery::find_tool("ffmpeg")?;
        let dir = TempDir::new("trim-test-in").ok()?;
        let out_path = dir.file("sample.mp4");
        let argv = process::args(&[
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=64x64:rate=15:duration=3",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=3",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-g",
            "15",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-shortest",
            out_path.to_str()?,
        ]);
        let out = process::run("ffmpeg", &ffmpeg, &argv, None, &limits()).ok()?;
        if !out.success {
            return None;
        }
        std::fs::read(&out_path).ok()
    }

    #[test]
    fn trim_accurate_produces_expected_duration() {
        let Some(input) = sample_input() else {
            eprintln!("skipping: ffmpeg lavfi/libx264/aac unavailable");
            return;
        };
        let rt = runtime();
        let options = TrimOptions {
            start: 0.5,
            end: Some(2.0),
            accurate: true,
            ..Default::default()
        };
        let out = trim(&input, &options, &limits(), &rt).expect("trim accurate");
        let file = out.primary.expect("primary output");
        assert_eq!(file.name, "output.mp4");
        assert!(!file.bytes.is_empty());

        let info = inspect(&file.bytes, &limits()).expect("probe trimmed");
        let dur = info.duration_secs.expect("duration");
        assert!((dur - 1.5).abs() < 0.3, "expected ~1.5s, got {dur}s");
        assert!(info.video_streams >= 1);
        assert!(info.audio_streams >= 1);
    }

    #[test]
    fn trim_fast_copy_keeps_duration_near_range() {
        let Some(input) = sample_input() else {
            eprintln!("skipping: ffmpeg lavfi/libx264/aac unavailable");
            return;
        };
        let rt = runtime();
        let options = TrimOptions {
            start: 0.5,
            end: Some(2.0),
            accurate: false,
            ..Default::default()
        };
        let out = trim(&input, &options, &limits(), &rt).expect("trim copy");
        assert!(
            out.warnings.iter().any(|w| w.contains("keyframe")),
            "expected keyframe warning, got {:?}",
            out.warnings
        );
        let file = out.primary.expect("primary output");
        assert_eq!(file.name, "output.mp4");

        let info = inspect(&file.bytes, &limits()).expect("probe trimmed");
        let dur = info.duration_secs.expect("duration");
        // Keyframe-snapped: with 1s GOP the copy cut lands within a
        // second of the requested range but must still be much shorter
        // than the 3s source.
        assert!(dur > 0.4 && dur < 3.0, "unexpected copy duration {dur}s");
    }

    #[test]
    fn trim_rejects_invalid_ranges() {
        let rt = runtime();
        let input = vec![0u8; 64];
        for options in [
            TrimOptions {
                start: -1.0,
                ..Default::default()
            },
            TrimOptions {
                start: f64::NAN,
                ..Default::default()
            },
            TrimOptions {
                start: 2.0,
                end: Some(1.0),
                ..Default::default()
            },
            TrimOptions {
                start: 1.0,
                end: Some(1.0),
                ..Default::default()
            },
        ] {
            let err = trim(&input, &options, &limits(), &rt).expect_err("must fail");
            assert!(
                matches!(err, MediaError::InvalidInput(_)),
                "unexpected error: {err:?}"
            );
        }
    }

    #[test]
    fn trim_audio_trim_false_drops_audio() {
        let Some(input) = sample_input() else {
            eprintln!("skipping: ffmpeg lavfi/libx264/aac unavailable");
            return;
        };
        let rt = runtime();
        let options = TrimOptions {
            start: 0.0,
            end: Some(1.0),
            accurate: true,
            audio_trim: false,
            ..Default::default()
        };
        let out = trim(&input, &options, &limits(), &rt).expect("trim no audio");
        let file = out.primary.expect("primary output");
        let info = inspect(&file.bytes, &limits()).expect("probe trimmed");
        assert_eq!(info.audio_streams, 0);
        assert!(info.video_streams >= 1);
    }
}
