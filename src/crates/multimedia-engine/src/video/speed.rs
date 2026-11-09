//! Playback speed change for video (and audio) with optional pitch
//! preservation.
//!
//! [`set_speed`] re-encodes the media at `factor`× its original
//! speed:
//!
//! - **Video**: `-vf setpts=PTS/<factor>` compresses or stretches the
//!   presentation timestamps, so frames play `factor`× faster (or
//!   slower for `factor < 1`).
//! - **Audio, `preserve_pitch = true` (default)**: the `atempo`
//!   filter chain. `atempo` accepts only `0.5..=2.0` per instance, so
//!   out-of-range factors are decomposed into a chain — e.g. `4.0` →
//!   `atempo=2.0,atempo=2.0`, `0.25` → `atempo=0.5,atempo=0.5`.
//!   `atempo` preserves pitch while changing tempo.
//! - **Audio, `preserve_pitch = false`**: varispeed-style pitch shift
//!   via `asetrate=<rate>*<factor>,aresample=<rate>` — the sample rate
//!   is multiplied by the factor (raising/lowering pitch) and then
//!   resampled back to the original rate, yielding `factor`× speed
//!   with pitch shifted by the same amount (tape-style). The `rubberband`
//!   filter would give higher quality but is not assumed present, so
//!   this op intentionally stays within the always-available
//!   `asetrate`/`aresample` pair; the probed sample rate is used and
//!   `48000` is assumed when unknown.
//!
//! Re-encoding is always required — stream copy cannot retime
//! timestamps. Codecs are left to FFmpeg's per-muxer defaults; only
//! the filters and the output muxer are capability-checked up front.
//!
//! Only audio and video streams are mapped (`-map 0:v? -map 0:a?`);
//! subtitle/data/attachment streams are dropped because they cannot
//! be meaningfully retimed here.

use std::ffi::OsString;

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::probe::ProbeReport;
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Minimum supported speed factor (4× slow motion).
pub const MIN_FACTOR: f64 = 0.25;
/// Maximum supported speed factor (4× fast motion).
pub const MAX_FACTOR: f64 = 4.0;

/// Options for [`set_speed`].
#[derive(Debug, Clone)]
pub struct SpeedOptions {
    /// Playback speed multiplier. `1.0` = unchanged, `< 1.0` = slow
    /// motion, `> 1.0` = fast motion. Must be finite and within
    /// [`MIN_FACTOR`]..=[`MAX_FACTOR`] or the call fails with
    /// [`MediaError::InvalidInput`].
    pub factor: f64,
    /// When `true` (default), audio tempo changes while pitch is
    /// preserved via chained `atempo` filters. When `false`, pitch
    /// shifts with speed (varispeed) via `asetrate`+`aresample`.
    pub preserve_pitch: bool,
    /// Output container/format name (`"mp4"`, `"mkv"`, `"webm"`,
    /// `"wav"`, ...). `None` picks a sensible default: `"mp4"` when
    /// the input has video, `"wav"` for audio-only input.
    pub output_format: Option<String>,
}

impl Default for SpeedOptions {
    fn default() -> Self {
        Self {
            factor: 1.0,
            preserve_pitch: true,
            output_format: None,
        }
    }
}

/// Change the playback speed of `input` media bytes.
///
/// Writes the input to a temp file, probes it with `ffprobe` to learn
/// which stream kinds exist, then runs `ffmpeg` with `setpts` for
/// video and an `atempo` chain (or `asetrate`/`aresample` when
/// `preserve_pitch` is false) for audio. The produced file is read
/// back under [`MediaLimits::max_output_bytes`].
pub fn set_speed(
    input: &[u8],
    options: &SpeedOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    let factor = options.factor;
    if !factor.is_finite() || !(MIN_FACTOR..=MAX_FACTOR).contains(&factor) {
        return Err(MediaError::InvalidInput(format!(
            "speed factor must be finite and within {MIN_FACTOR}..={MAX_FACTOR}, got {factor}"
        )));
    }
    if let Some(max) = limits.max_input_bytes
        && input.len() as u64 > max
    {
        return Err(MediaError::LimitExceeded(format!(
            "input is {} bytes, exceeding max_input_bytes {max}",
            input.len()
        )));
    }

    // Probe once: stream-kind detection drives filter selection, the
    // input temp-file extension, the audio sample rate (for
    // asetrate), and the default output container.
    let report = probe_input(input, rt, limits)?;
    let has_video = report.streams.iter().any(|s| s.kind() == Some("video"));
    let has_audio = report.streams.iter().any(|s| s.kind() == Some("audio"));
    if !has_video && !has_audio {
        return Err(MediaError::InvalidInput(
            "input has no audio or video streams to retime".to_string(),
        ));
    }

    // Capability checks before touching the filesystem output —
    // never silently fall back to different filters.
    let caps = rt.capabilities()?;
    if has_video && !caps.has_filter("setpts") {
        return Err(MediaError::Unsupported(
            "ffmpeg build lacks the setpts filter required for video retiming".to_string(),
        ));
    }
    if has_audio {
        if options.preserve_pitch {
            if !caps.has_filter("atempo") {
                return Err(MediaError::Unsupported(
                    "ffmpeg build lacks the atempo filter required for pitch-preserving audio retiming"
                        .to_string(),
                ));
            }
        } else {
            for f in ["asetrate", "aresample"] {
                if !caps.has_filter(f) {
                    return Err(MediaError::Unsupported(format!(
                        "ffmpeg build lacks the {f} filter required for varispeed audio retiming"
                    )));
                }
            }
        }
    }

    let input_ext = input_extension(&report);
    let out_ext = output_extension(options.output_format.as_deref(), has_video)?;
    if !caps.has_muxer(&out_ext) {
        return Err(MediaError::Unsupported(format!(
            "ffmpeg build has no {out_ext} muxer for the requested output format"
        )));
    }

    let tmp = TempDir::new("speed")?;
    let input_name = format!("input.{input_ext}");
    let input_path = tmp.write(&input_name, input)?;
    let output_name = format!("output.{out_ext}");
    let output_path = tmp.file(&output_name);

    let mut args: Vec<OsString> = process::args(&[
        "-y",
        "-hide_banner",
        "-loglevel",
        "warning",
        "-i",
        input_path.to_str().unwrap_or("input"),
        // Retimed streams only: subtitles/data cannot be speed-changed
        // meaningfully and would break re-encode in some muxers.
        "-map",
        "0:v?",
        "-map",
        "0:a?",
    ]);
    if has_video {
        args.extend(process::args(&["-vf", &setpts_arg(factor)]));
    }
    if has_audio {
        let af = if options.preserve_pitch {
            atempo_chain(factor)
        } else {
            let rate = audio_sample_rate(&report).unwrap_or(48_000);
            format!("asetrate={rate}*{},aresample={rate}", fmt_num(factor))
        };
        if !af.is_empty() {
            args.extend(process::args(&["-af", &af]));
        }
    }
    args.push(OsString::from(output_path.to_string_lossy().as_ref()));

    let out = process::run_checked("ffmpeg", &rt.ffmpeg, &args, None, limits)?;

    let bytes = tmp.read_limited(&output_name, limits.max_output_bytes)?;
    let mut warnings = Vec::new();
    if !out.stderr.is_empty() {
        warnings.push(out.stderr.clone());
    }

    Ok(MediaOutput {
        primary: Some(crate::crates::multimedia_engine::model::MediaFile {
            name: output_name,
            bytes,
            content_type: content_type(&out_ext).map(str::to_string),
        }),
        extra: Vec::new(),
        warnings,
    })
}

/// Run ffprobe over the input bytes and parse the JSON report.
fn probe_input(input: &[u8], rt: &Runtime, limits: &MediaLimits) -> MediaResult<ProbeReport> {
    // Bytes must live at a path for ffprobe; set_speed probes before
    // writing the working input, so use a short-lived dir.
    let tmp = TempDir::new("speed-probe")?;
    let path = tmp.write("probe-input.bin", input)?;
    let path_arg = path.to_string_lossy().into_owned();
    let args = process::args(&[
        "-v",
        "error",
        "-show_streams",
        "-show_format",
        "-of",
        "json",
        &path_arg,
    ]);
    let out = process::run_checked("ffprobe", &rt.ffprobe, &args, None, limits)?;
    serde_json::from_slice::<ProbeReport>(&out.stdout).map_err(|e| {
        MediaError::InvalidInput(format!("ffprobe output was not parseable JSON: {e}"))
    })
}

/// `setpts=PTS/<factor>` filter argument.
fn setpts_arg(factor: f64) -> String {
    format!("setpts=PTS/{}", fmt_num(factor))
}

/// Decompose `factor` into a comma-separated `atempo=…` filter chain
/// where every stage stays inside `atempo`'s `0.5..=2.0` range.
/// Returns an empty string when `factor == 1.0` (no filter needed).
fn atempo_chain(factor: f64) -> String {
    let mut stages = Vec::new();
    let mut f = factor;
    while f > 2.0 {
        stages.push("atempo=2.0".to_string());
        f /= 2.0;
    }
    while f < 0.5 {
        stages.push("atempo=0.5".to_string());
        f /= 0.5;
    }
    if (f - 1.0).abs() > f64::EPSILON {
        stages.push(format!("atempo={}", fmt_num(f)));
    }
    stages.join(",")
}

/// Format a factor for filter arguments: shortest round-trip decimal,
/// never scientific notation for our 0.25..=4.0 range.
fn fmt_num(x: f64) -> String {
    let s = format!("{x}");
    if s.contains('e') || s.contains('E') {
        format!("{x:.10}")
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_string()
    } else {
        s
    }
}

/// Extension for the input temp file, derived from the probed
/// container (`mov,mp4,...` → `mov`); falls back to `bin` — ffmpeg
/// probes content anyway.
fn input_extension(report: &ProbeReport) -> String {
    report
        .format
        .as_ref()
        .and_then(|f| f.primary_name())
        .map(sanitize_ext)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "bin".to_string())
}

/// Resolve the output extension from `output_format`, checking the
/// muxer exists. Default: `mp4` with video, `wav` audio-only (PCM is
/// always encodable; mp3 would depend on libmp3lame).
fn output_extension(output_format: Option<&str>, has_video: bool) -> MediaResult<String> {
    let ext = match output_format {
        Some(f) => sanitize_ext(f),
        None => {
            if has_video {
                "mp4".to_string()
            } else {
                "wav".to_string()
            }
        }
    };
    if ext.is_empty() {
        return Err(MediaError::InvalidInput(
            "output_format must contain a container name".to_string(),
        ));
    }
    Ok(ext)
}

/// Keep only `[a-z0-9]` so a format string can never smuggle path or
/// option characters into the output filename.
fn sanitize_ext(s: &str) -> String {
    s.chars()
        .map(|c| c.to_ascii_lowercase())
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

/// Sample rate (Hz) of the first audio stream, for `asetrate`.
fn audio_sample_rate(report: &ProbeReport) -> Option<u32> {
    report
        .streams
        .iter()
        .find(|s| s.kind() == Some("audio"))
        .and_then(|s| s.sample_rate.as_deref())
        .and_then(|r| r.parse::<u32>().ok())
        .filter(|r| *r > 0)
}

/// Best-effort MIME type for the output file.
fn content_type(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "mp4" | "m4v" | "mov" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "avi" => "video/x-msvideo",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "aac" => "audio/aac",
        "ogg" | "oga" | "opus" => "audio/ogg",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn runtime() -> Runtime {
        Runtime::discover().expect("ffmpeg/ffprobe must be on PATH for tests")
    }

    /// Generate a 2-second 320x240@25fps clip with a 440 Hz tone.
    fn make_clip() -> Vec<u8> {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("clip.mp4");
        let status = Command::new("ffmpeg")
            .args([
                "-y",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=2:size=320x240:rate=25",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=2",
                "-c:v",
                "mpeg4",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                "-shortest",
            ])
            .arg(&out)
            .status()
            .expect("ffmpeg must run to build the test clip");
        assert!(status.success(), "test clip generation failed");
        std::fs::read(&out).unwrap()
    }

    /// Duration in seconds of produced output bytes, via ffprobe.
    fn output_duration(bytes: &[u8]) -> f64 {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.mp4");
        std::fs::write(&path, bytes).unwrap();
        let out = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "format=duration",
                "-of",
                "default=noprint_wrappers=1:nokey=1",
            ])
            .arg(&path)
            .output()
            .expect("ffprobe must run");
        assert!(out.status.success(), "ffprobe failed: {:?}", out);
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse::<f64>()
            .expect("duration must parse")
    }

    #[test]
    fn speed_up_halves_duration() {
        let clip = make_clip();
        let opts = SpeedOptions {
            factor: 2.0,
            preserve_pitch: true,
            output_format: None,
        };
        let out = set_speed(&clip, &opts, &MediaLimits::default(), &runtime())
            .expect("set_speed 2.0 failed");
        let dur = output_duration(&out.primary.unwrap().bytes);
        assert!(
            (dur - 1.0).abs() < 0.25,
            "2s clip at 2x should be ~1s, got {dur}s"
        );
    }

    #[test]
    fn slow_down_doubles_duration() {
        let clip = make_clip();
        let opts = SpeedOptions {
            factor: 0.5,
            preserve_pitch: true,
            output_format: None,
        };
        let out = set_speed(&clip, &opts, &MediaLimits::default(), &runtime())
            .expect("set_speed 0.5 failed");
        let dur = output_duration(&out.primary.unwrap().bytes);
        assert!(
            (dur - 4.0).abs() < 0.4,
            "2s clip at 0.5x should be ~4s, got {dur}s"
        );
    }

    #[test]
    fn factor_out_of_range_is_invalid_input() {
        let rt = runtime();
        for bad in [0.1, 5.0, f64::NAN, f64::INFINITY] {
            let opts = SpeedOptions {
                factor: bad,
                ..SpeedOptions::default()
            };
            let err = set_speed(&[0u8; 16], &opts, &MediaLimits::default(), &rt)
                .expect_err("out-of-range factor must fail");
            assert!(
                matches!(err, MediaError::InvalidInput(_)),
                "factor {bad} should be InvalidInput, got {err:?}"
            );
        }
    }
}
