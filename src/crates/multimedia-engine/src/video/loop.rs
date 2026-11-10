//! Loop a media file — repeat it `count` extra times or until a total
//! duration via ffmpeg's `-stream_loop` input option.
//!
//! [`loop_video`] writes the input buffer to a temp file and runs
//! `ffmpeg -stream_loop <n> -i <in> ...`. `-stream_loop` is an *input*
//! option (it must precede `-i`); `-stream_loop n` feeds the demuxer
//! output `n + 1` times, so `count` extra repeats maps directly to
//! `-stream_loop <count>`.
//!
//! Two output paths:
//!
//! - **Stream copy** (`LoopOptions::reencode = false`, the default):
//!   `-c copy` remuxes the looped streams without decoding. Fast and
//!   lossless, but loop boundaries can produce non-monotonic
//!   timestamps/A-V drift on some containers — a warning is recorded
//!   in [`MediaOutput::warnings`].
//! - **Re-encode** (`reencode = true`): audio and video are decoded
//!   and re-encoded with capability-checked encoders (this build has
//!   no `libx264`; `libopenh264`/`h264`/`mpeg4` are tried in order).
//!   Frame-accurate looping at the cost of a full transcode.
//!
//! For `duration_secs` looping the op adds `-t <duration>` as an
//! output option and picks `-stream_loop` from the probed input
//! duration when it is known (falling back to `-stream_loop -1`,
//! i.e. loop until `-t` stops it, when it is not).

use std::ffi::OsString;

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaFile, MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::probe::ProbeReport;
use crate::crates::multimedia_engine::runtime::discovery::{Runtime, RuntimeCapabilities};
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Options for [`loop_video`]. Serde-friendly: all fields default
/// when omitted.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LoopOptions {
    /// Repeat the input this many *extra* times (`Some(2)` on a 1s
    /// clip yields ~3s of output). Mutually exclusive with
    /// [`LoopOptions::duration_secs`] — exactly one must be set.
    #[serde(default)]
    pub count: Option<u32>,
    /// Loop until the output's total duration reaches this many
    /// seconds. Mutually exclusive with [`LoopOptions::count`] —
    /// exactly one must be set. Must be finite and `> 0`.
    #[serde(default)]
    pub duration_secs: Option<f64>,
    /// Output container extension or muxer name without a leading
    /// dot (`"mp4"`, `"mkv"`, `"webm"`, ...). `None` keeps the input's
    /// sniffed container, falling back to `"mp4"`.
    #[serde(default)]
    pub output_format: Option<String>,
    /// Re-encode audio/video instead of stream-copying (default
    /// `false`). Only meaningful when the op actually changes stream
    /// timing; copy is faster but can drift at loop boundaries.
    #[serde(default)]
    pub reencode: bool,
}

/// Video encoder candidates for the re-encode path, in preference
/// order. `libx264` is absent from this FFmpeg build; `libopenh264`
/// (used by the test fixtures) leads, with native `h264`/`mpeg4`
/// fallbacks. WebM output uses the VP8/VP9 family instead.
const VIDEO_ENCODERS: &[&str] = &["libopenh264", "libx264", "h264", "mpeg4"];
const WEBM_ENCODERS: &[&str] = &["libvpx-vp9", "libvpx"];

fn pick_encoder(caps: &RuntimeCapabilities, candidates: &[&'static str]) -> Option<&'static str> {
    candidates.iter().copied().find(|e| caps.has_encoder(e))
}

/// Best-effort container guess from magic bytes, used to pick a temp
/// input extension and the default output container. ffmpeg re-probes
/// content itself; a wrong guess is harmless.
fn sniff_ext(input: &[u8]) -> &'static str {
    if input.len() >= 4 {
        match &input[..4] {
            [0x1A, 0x45, 0xDF, 0xA3] => return "mkv",
            [b'O', b'g', b'g', b'S'] => return "ogg",
            [b'f', b'L', b'a', b'C'] => return "flac",
            [b'R', b'I', b'F', b'F'] => {
                if input.len() >= 12 && &input[8..12] == b"WAVE" {
                    return "wav";
                }
                if input.len() >= 12 && &input[8..12] == b"AVI " {
                    return "avi";
                }
                return "bin";
            }
            [b'I', b'D', b'3', _] => return "mp3",
            _ => {}
        }
        if input.len() >= 8 && &input[4..8] == b"ftyp" {
            return "mp4";
        }
    }
    "mp4"
}

fn check_input(input: &[u8], limits: &MediaLimits) -> MediaResult<()> {
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
    // This op always produces exactly one file.
    if let Some(max) = limits.max_output_files
        && max == 0
    {
        return Err(MediaError::LimitExceeded(
            "max_output_files is 0 but loop always produces one output".to_string(),
        ));
    }
    Ok(())
}

/// Validate that exactly one of `count`/`duration_secs` is set and
/// that the set value is sane.
fn validate_options(options: &LoopOptions) -> MediaResult<()> {
    match (options.count, options.duration_secs) {
        (Some(_), Some(_)) | (None, None) => Err(MediaError::InvalidInput(
            "exactly one of count or duration_secs must be set".to_string(),
        )),
        (Some(_), None) => Ok(()),
        (None, Some(d)) => {
            if !d.is_finite() || d <= 0.0 {
                Err(MediaError::InvalidInput(format!(
                    "duration_secs must be finite and > 0, got {d}"
                )))
            } else {
                Ok(())
            }
        }
    }
}

fn probe_input(input: &[u8], rt: &Runtime, limits: &MediaLimits) -> MediaResult<ProbeReport> {
    let tmp = TempDir::new("loop-probe")?;
    let path = tmp.write("probe-input.bin", input)?;
    let args = process::args(&[
        "-v",
        "error",
        "-show_streams",
        "-show_format",
        "-of",
        "json",
        path.to_str().unwrap_or("probe-input.bin"),
    ]);
    let out = process::run_checked("ffprobe", &rt.ffprobe, &args, None, limits)?;
    serde_json::from_slice::<ProbeReport>(&out.stdout).map_err(|e| {
        MediaError::InvalidInput(format!("ffprobe output was not parseable JSON: {e}"))
    })
}

/// Best-known input duration in seconds: container duration first,
/// then the longest stream duration as a fallback.
fn input_duration_secs(report: &ProbeReport) -> Option<f64> {
    if let Some(d) = report.format.as_ref().and_then(|f| f.duration_secs())
        && d > 0.0
    {
        return Some(d);
    }
    report
        .streams
        .iter()
        .filter_map(|s| s.duration_secs())
        .fold(None, |acc: Option<f64>, d| {
            if d > 0.0 {
                Some(acc.map_or(d, |a| a.max(d)))
            } else {
                acc
            }
        })
}

/// Resolve the output container extension/muxer name: the explicit
/// `output_format` when given (sanitized to muxer-name characters),
/// otherwise the input's sniffed container, falling back to `"mp4"`.
fn resolve_out_ext(input: &[u8], options: &LoopOptions) -> String {
    options
        .output_format
        .as_deref()
        .map(|s| {
            s.trim()
                .trim_start_matches('.')
                .to_ascii_lowercase()
                .replace(
                    |c: char| !c.is_ascii_lowercase() && !c.is_ascii_digit() && c != '_',
                    "",
                )
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| sniff_ext(input).to_string())
}

fn video_encoder(ext: &str, caps: &RuntimeCapabilities) -> Option<&'static str> {
    let candidates: &[&str] = if ext == "webm" {
        WEBM_ENCODERS
    } else {
        VIDEO_ENCODERS
    };
    pick_encoder(caps, candidates)
}

fn audio_encoder(ext: &str, caps: &RuntimeCapabilities) -> MediaResult<&'static str> {
    match ext {
        "webm" => {
            if caps.has_encoder("libopus") {
                Ok("libopus")
            } else if caps.has_encoder("libvorbis") {
                Ok("libvorbis")
            } else {
                Err(MediaError::Unsupported(
                    "no WebM audio encoder (libopus/libvorbis) in this FFmpeg build".to_string(),
                ))
            }
        }
        "mp3" => {
            if caps.has_encoder("libmp3lame") {
                Ok("libmp3lame")
            } else {
                Err(MediaError::Unsupported(
                    "no mp3 audio encoder (libmp3lame) in this FFmpeg build".to_string(),
                ))
            }
        }
        "wav" => Ok("pcm_s16le"),
        "flac" => Ok("flac"),
        "ogg" | "oga" => {
            if caps.has_encoder("libvorbis") {
                Ok("libvorbis")
            } else {
                Err(MediaError::Unsupported(
                    "no ogg audio encoder (libvorbis) in this FFmpeg build".to_string(),
                ))
            }
        }
        "opus" => {
            if caps.has_encoder("libopus") {
                Ok("libopus")
            } else {
                Err(MediaError::Unsupported(
                    "no opus audio encoder in this FFmpeg build".to_string(),
                ))
            }
        }
        _ => {
            if caps.has_encoder("aac") {
                Ok("aac")
            } else {
                Err(MediaError::Unsupported(
                    "no AAC audio encoder in this FFmpeg build".to_string(),
                ))
            }
        }
    }
}

/// Best-effort MIME type for the produced container.
fn content_type(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "mp4" | "m4v" | "mov" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "avi" => "video/x-msvideo",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "aac" | "adts" => "audio/aac",
        "ogg" | "oga" | "opus" => "audio/ogg",
        _ => return None,
    })
}

/// Loop `input` according to `options` and return the single produced
/// media file.
///
/// # Arguments
///
/// - `input` — complete media file bytes.
/// - `options` — see [`LoopOptions`]; exactly one of `count` /
///   `duration_secs` must be set.
/// - `limits` — per-op byte/time limits (`max_input_bytes`,
///   `max_output_bytes`, `max_output_files`, `timeout`).
/// - `rt` — the discovered FFmpeg [`Runtime`]; its capability
///   registry validates the muxer and re-encode encoders.
///
/// # Errors
///
/// - [`MediaError::InvalidInput`] — empty input, both/neither of
///   `count`/`duration_secs` set, non-positive/non-finite
///   `duration_secs`, or no audio/video streams to loop.
/// - [`MediaError::LimitExceeded`] — input/output size limits, or
///   `max_output_files == 0`.
/// - [`MediaError::Unsupported`] — the target muxer or a required
///   re-encode encoder is missing from this FFmpeg build.
/// - [`MediaError::ToolFailed`] / [`MediaError::Timeout`] /
///   [`MediaError::MissingTool`] / [`MediaError::Io`] — tool and
///   filesystem failures.
pub fn loop_video(
    input: &[u8],
    options: &LoopOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    check_input(input, limits)?;
    validate_options(options)?;

    let caps = rt.capabilities()?;
    let out_ext = resolve_out_ext(input, options);
    if !caps.has_muxer(&out_ext) {
        return Err(MediaError::Unsupported(format!(
            "container {out_ext:?} is not available as a muxer in this FFmpeg build"
        )));
    }

    let report = probe_input(input, rt, limits)?;
    let has_video = report.streams.iter().any(|s| s.kind() == Some("video"));
    let has_audio = report.streams.iter().any(|s| s.kind() == Some("audio"));
    if !has_video && !has_audio {
        return Err(MediaError::InvalidInput(
            "input has no audio or video streams to loop".to_string(),
        ));
    }

    // Resolve the -stream_loop repeat count and optional -t bound.
    let mut warnings = Vec::new();
    let (stream_loop, output_duration) = match (options.count, options.duration_secs) {
        (Some(count), None) => {
            // count = extra repeats; -stream_loop n plays n+1 times.
            (count.to_string(), None)
        }
        (None, Some(dur)) => {
            match input_duration_secs(&report) {
                Some(in_dur) => {
                    // Enough plays to cover `dur`, then -t trims the
                    // overshoot. plays = ceil(dur / in_dur), at least 1.
                    let plays = (dur / in_dur).ceil().max(1.0);
                    let extra = (plays as u64).saturating_sub(1);
                    (extra.to_string(), Some(dur))
                }
                None => {
                    warnings.push(
                        "input duration unknown; using -stream_loop -1 bounded by -t".to_string(),
                    );
                    ("-1".to_string(), Some(dur))
                }
            }
        }
        _ => unreachable!("validate_options guarantees exactly one mode"),
    };

    // Codec plan: copy everything, or pick capability-checked
    // encoders for the re-encode path.
    let mut video_enc = None;
    let mut audio_enc = None;
    if options.reencode {
        if has_video {
            video_enc = Some(video_encoder(&out_ext, caps).ok_or_else(|| {
                MediaError::Unsupported(format!(
                    "no usable video encoder for .{out_ext} output in this FFmpeg build"
                ))
            })?);
        }
        if has_audio {
            audio_enc = Some(audio_encoder(&out_ext, caps)?);
        }
    } else {
        warnings.push(
            "stream copy: loop boundaries can produce timestamp drift on some containers"
                .to_string(),
        );
    }

    let dir = TempDir::new("loop")?;
    let in_ext = sniff_ext(input);
    let in_name = format!("input.{in_ext}");
    dir.write(&in_name, input)?;
    let out_name = format!("output.{out_ext}");
    let out_path = dir.file(&out_name);

    // Common argv head: quiet, no stdin interaction, overwrite.
    let mut argv: Vec<OsString> = vec![
        OsString::from("-hide_banner"),
        OsString::from("-loglevel"),
        OsString::from("error"),
        OsString::from("-nostdin"),
        OsString::from("-y"),
        // -stream_loop is an input option: it must precede -i.
        OsString::from("-stream_loop"),
        OsString::from(stream_loop),
        OsString::from("-i"),
        dir.file(&in_name).into_os_string(),
    ];

    if options.reencode {
        // Map only audio/video — other stream types (subtitles, data,
        // attachments) have no loop semantics worth re-encoding and
        // would need per-codec copy rules.
        argv.extend([
            OsString::from("-map"),
            OsString::from("0:v?"),
            OsString::from("-map"),
            OsString::from("0:a?"),
        ]);
        if let Some(enc) = video_enc {
            argv.extend([
                OsString::from("-c:v"),
                OsString::from(enc),
                OsString::from("-pix_fmt"),
                OsString::from("yuv420p"),
            ]);
        }
        if let Some(enc) = audio_enc {
            argv.extend([OsString::from("-c:a"), OsString::from(enc)]);
            if matches!(enc, "aac" | "libmp3lame" | "libopus" | "libvorbis") {
                argv.extend([OsString::from("-b:a"), OsString::from("128k")]);
            }
        }
    } else {
        argv.extend([OsString::from("-c"), OsString::from("copy")]);
    }

    if let Some(dur) = output_duration {
        argv.extend([OsString::from("-t"), OsString::from(format!("{dur:.3}"))]);
    }

    argv.push(out_path.into_os_string());

    let out = process::run_checked("ffmpeg", &rt.ffmpeg, &argv, None, limits)?;
    let bytes = dir.read_limited(&out_name, limits.max_output_bytes)?;
    if bytes.is_empty() {
        return Err(MediaError::ToolFailed {
            tool: "ffmpeg",
            status: "empty output".to_string(),
            detail: format!("ffmpeg produced an empty {out_name}"),
        });
    }

    let mut result = MediaOutput {
        primary: Some(MediaFile {
            name: out_name,
            bytes,
            content_type: content_type(&out_ext).map(str::to_string),
        }),
        extra: Vec::new(),
        warnings,
    };
    // ffmpeg stderr at -loglevel error is already terse; keep it as
    // a warning rather than failing.
    if !out.stderr.is_empty() {
        result.warnings.push(out.stderr);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::multimedia_engine::runtime::discovery;
    use std::path::PathBuf;
    use std::process::Command;
    use std::time::Duration;

    fn rt() -> Option<Runtime> {
        Runtime::discover().ok()
    }

    fn limits() -> MediaLimits {
        MediaLimits {
            timeout: Duration::from_secs(120),
            ..MediaLimits::default()
        }
    }

    /// 1s lavfi clip (mpeg4/aac — no libx264 on this build).
    fn make_clip() -> Option<Vec<u8>> {
        let rt = rt()?;
        let dir = TempDir::new("loop-test-in").ok()?;
        let out: PathBuf = dir.file("clip.mp4");
        let mut argv = process::args(&[
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=1:size=160x120:rate=15",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=1",
            "-c:v",
            "mpeg4",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-shortest",
        ]);
        argv.push(out.to_string_lossy().into_owned().into());
        let res = process::run("ffmpeg", &rt.ffmpeg, &argv, None, &limits()).ok()?;
        if !res.success {
            return None;
        }
        std::fs::read(&out).ok()
    }

    /// Probe `format=duration` via ffprobe on a temp copy.
    fn output_duration(bytes: &[u8]) -> Option<f64> {
        let dir = TempDir::new("loop-dur").ok()?;
        let p = dir.file("out.bin");
        std::fs::write(&p, bytes).ok()?;
        let ffprobe = discovery::find_tool("ffprobe")?;
        let out = Command::new(ffprobe)
            .args([
                "-v",
                "error",
                "-show_entries",
                "format=duration",
                "-of",
                "default=noprint_wrappers=1:nokey=1",
            ])
            .arg(&p)
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout).trim().parse().ok()
    }

    #[test]
    fn loop_count_two_stream_copy_triples_duration() {
        let (Some(rt), Some(clip)) = (rt(), make_clip()) else {
            eprintln!("skipping: ffmpeg/ffprobe or lavfi encoders unavailable");
            return;
        };
        let opts = LoopOptions {
            count: Some(2),
            ..Default::default()
        };
        let out = loop_video(&clip, &opts, &limits(), &rt).expect("loop_video failed");
        let file = out.primary.expect("primary output");
        assert_eq!(file.name, "output.mp4");
        assert!(file.content_type.is_some());
        let dur = output_duration(&file.bytes).expect("probe duration");
        // count=2 extra repeats on a 1s clip -> ~3s total.
        assert!(
            (2.0..=3.5).contains(&dur),
            "expected ~2-3.5s for count=2 on 1s clip, got {dur}s"
        );
    }

    #[test]
    fn loop_duration_secs_bounds_total() {
        let (Some(rt), Some(clip)) = (rt(), make_clip()) else {
            eprintln!("skipping: ffmpeg/ffprobe or lavfi encoders unavailable");
            return;
        };
        let opts = LoopOptions {
            duration_secs: Some(2.0),
            ..Default::default()
        };
        let out = loop_video(&clip, &opts, &limits(), &rt).expect("loop_video duration");
        let dur = output_duration(&out.primary.unwrap().bytes).expect("probe duration");
        assert!(
            (1.5..=2.6).contains(&dur),
            "expected ~2s for duration_secs=2, got {dur}s"
        );
    }

    #[test]
    fn loop_reencode_produces_longer_output() {
        let (Some(rt), Some(clip)) = (rt(), make_clip()) else {
            eprintln!("skipping: ffmpeg/ffprobe or lavfi encoders unavailable");
            return;
        };
        let opts = LoopOptions {
            count: Some(1),
            duration_secs: None,
            reencode: true,
            output_format: Some("mp4".to_string()),
        };
        let out = loop_video(&clip, &opts, &limits(), &rt).expect("loop_video reencode");
        let dur = output_duration(&out.primary.unwrap().bytes).expect("probe duration");
        assert!(
            (1.5..=2.6).contains(&dur),
            "expected ~2s for count=1 reencode, got {dur}s"
        );
    }

    #[test]
    fn loop_rejects_bad_options() {
        let Some(rt) = rt() else {
            eprintln!("skipping: ffmpeg/ffprobe unavailable");
            return;
        };
        let clip = vec![0u8; 64];
        // Neither count nor duration_secs.
        let err = loop_video(&clip, &LoopOptions::default(), &limits(), &rt)
            .expect_err("no mode must fail");
        assert!(matches!(err, MediaError::InvalidInput(_)));
        // Both set.
        let err = loop_video(
            &clip,
            &LoopOptions {
                count: Some(2),
                duration_secs: Some(3.0),
                ..Default::default()
            },
            &limits(),
            &rt,
        )
        .expect_err("both modes must fail");
        assert!(matches!(err, MediaError::InvalidInput(_)));
        // Non-positive duration.
        let err = loop_video(
            &clip,
            &LoopOptions {
                duration_secs: Some(0.0),
                ..Default::default()
            },
            &limits(),
            &rt,
        )
        .expect_err("zero duration must fail");
        assert!(matches!(err, MediaError::InvalidInput(_)));
        // Empty input.
        let err = loop_video(
            &[],
            &LoopOptions {
                count: Some(1),
                ..Default::default()
            },
            &limits(),
            &rt,
        )
        .expect_err("empty input must fail");
        assert!(matches!(err, MediaError::InvalidInput(_)));
    }
}
