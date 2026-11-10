//! Frame-rate conversion for video via the FFmpeg `fps` filter.
//!
//! [`fps_convert`] re-times the video stream to a target frame rate:
//!
//! - **Video**: `-vf fps=fps=<rate>:round=<rounding>` resamples frame
//!   timestamps to the requested rate, duplicating or dropping frames
//!   as needed. The stream is always re-encoded — the `fps` filter
//!   operates on decoded frames, so stream copy cannot express it.
//!   Only the first video stream (`-map 0:v:0`) and any audio streams
//!   (`-map 0:a?`) are carried over; subtitle/data/attachment streams
//!   are dropped (`-sn -dn`).
//! - **Audio, `resample_audio = true` (default)**: audio is re-encoded
//!   (default AAC) and — when the input sample rate differs from the
//!   common 48 kHz rate — passed through `aresample=48000` first so
//!   the encoder sees a normalized rate.
//! - **Audio, `resample_audio = false`**: the stream is copied
//!   (`-c:a copy`) when the probed codec is known to be storable in the
//!   output container; otherwise it falls back to AAC re-encode.
//!   `audio_codec = Some("copy")` forces stream copy regardless.
//!
//! Encoder selection is capability-driven, never assumed: an explicit
//! `video_codec`/`audio_codec` must exist in the FFmpeg build's
//! encoder registry ([`MediaError::Unsupported`] otherwise), and the
//! built-in defaults are picked from ordered candidate lists. The
//! default video candidates deliberately exclude `libx264`; callers
//! that want x264 can pass it explicitly.

use std::ffi::OsString;

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaFile, MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::probe::{MediaStream, ProbeReport};
use crate::crates::multimedia_engine::runtime::discovery::{Runtime, RuntimeCapabilities};
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Highest frame rate accepted — well above any broadcast or capture
/// format while still bounding filter/encoder work.
pub const MAX_FPS: f64 = 240.0;

/// Sample rate audio is normalized to when `resample_audio` is set
/// and the input rate differs.
const TARGET_SAMPLE_RATE: u32 = 48_000;

/// Video encoder preference when `video_codec` is `None`, best-first.
/// `libx264` is intentionally absent from the defaults — it remains
/// available by passing it explicitly — so the fallback chain works
/// on GPL-free and minimal FFmpeg builds alike.
const DEFAULT_VIDEO_ENCODERS: &[&str] = &[
    "libopenh264",
    "h264_nvenc",
    "h264",
    "mpeg4",
    "libvpx-vp9",
    "vp9",
];

/// Audio encoder preference for re-encoded audio, best-first.
const DEFAULT_AUDIO_ENCODERS: &[&str] = &["aac", "libfdk_aac"];

/// Rounding method for the `fps` filter's `round` option.
///
/// Serde representation is snake_case (`"near"`, `"up"`, ...).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FpsRounding {
    /// Round to the nearest output timestamp (FFmpeg default).
    #[default]
    Near,
    /// Always round up — may duplicate a frame.
    Up,
    /// Always round down — may drop a frame.
    Down,
    /// Round toward zero (truncate).
    Zero,
}

impl FpsRounding {
    /// The value the `fps` filter expects for `round=`.
    fn as_str(self) -> &'static str {
        match self {
            Self::Near => "near",
            Self::Up => "up",
            Self::Down => "down",
            Self::Zero => "zero",
        }
    }
}

/// Options for [`fps_convert`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FpsOptions {
    /// Target frame rate in frames per second. Must be finite,
    /// `> 0` and `<= [`MAX_FPS`]` or the call fails with
    /// [`MediaError::InvalidInput`].
    #[serde(default = "default_fps")]
    pub fps: f64,
    /// Timestamp rounding method passed to the `fps` filter.
    #[serde(default)]
    pub rounding: FpsRounding,
    /// Output container/muxer name (`"mp4"`, `"mkv"`, `"webm"`, ...).
    /// Sanitized to `[a-z0-9-]`; must name a muxer in this FFmpeg
    /// build.
    #[serde(default = "default_output_format")]
    pub output_format: String,
    /// Video encoder name. `None` picks the first available candidate
    /// from [`DEFAULT_VIDEO_ENCODERS`]. `libx264` is not a default but
    /// is honored when passed explicitly (subject to capability check).
    #[serde(default)]
    pub video_codec: Option<String>,
    /// Audio encoder name, or `"copy"` for stream copy. `None`
    /// re-encodes to AAC (first available of
    /// [`DEFAULT_AUDIO_ENCODERS`]) — or copies the audio stream when
    /// `resample_audio` is `false` and the codec is container-safe.
    #[serde(default)]
    pub audio_codec: Option<String>,
    /// When `true` (default), re-encoded audio is first resampled to
    /// [`TARGET_SAMPLE_RATE`] via `aresample` whenever the input rate
    /// differs. When `false`, audio is stream-copied where possible
    /// and never resampled.
    #[serde(default = "default_true")]
    pub resample_audio: bool,
}

fn default_fps() -> f64 {
    30.0
}
fn default_output_format() -> String {
    "mp4".to_string()
}
fn default_true() -> bool {
    true
}

impl Default for FpsOptions {
    fn default() -> Self {
        Self {
            fps: default_fps(),
            rounding: FpsRounding::default(),
            output_format: default_output_format(),
            video_codec: None,
            audio_codec: None,
            resample_audio: true,
        }
    }
}

/// How the audio stream will be handled, decided after probing.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AudioPlan {
    /// No usable audio stream in the input.
    Absent,
    /// `-c:a copy`.
    Copy,
    /// `-c:a <codec>` re-encode.
    Encode,
}

/// Change the frame rate of `input` media bytes.
///
/// Writes the input to a temp file, probes it with `ffprobe` (the op
/// requires at least one video stream), verifies the `fps` filter,
/// muxer and encoders exist in this FFmpeg build, then runs `ffmpeg`
/// with `-vf fps=...`. The produced file is read back under
/// [`MediaLimits::max_output_bytes`].
///
/// # Errors
///
/// - [`MediaError::InvalidInput`] for a non-finite/out-of-range `fps`,
///   an empty `output_format`, or input with no video stream.
/// - [`MediaError::LimitExceeded`] when `input` exceeds
///   `limits.max_input_bytes` or the produced file exceeds
///   `limits.max_output_bytes`.
/// - [`MediaError::Unsupported`] when this FFmpeg build lacks the
///   `fps` (or `aresample`) filter, the requested muxer, or the
///   requested/chosen video or audio encoder.
pub fn fps_convert(
    input: &[u8],
    options: &FpsOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    let fps = options.fps;
    if !(fps.is_finite() && fps > 0.0 && fps <= MAX_FPS) {
        return Err(MediaError::InvalidInput(format!(
            "fps must be finite, > 0 and <= {MAX_FPS}, got {fps}"
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

    // Probe once: stream kinds drive the video requirement and the
    // audio plan; codec name, sample rate and container format drive
    // copy-safety and encoder defaults.
    let report = probe_input(input, rt, limits)?;
    if !report.streams.iter().any(|s| s.kind() == Some("video")) {
        return Err(MediaError::InvalidInput(
            "input has no video stream to resample".to_string(),
        ));
    }
    let audio_stream = report.streams.iter().find(|s| s.kind() == Some("audio"));

    let out_ext = sanitize_ext(&options.output_format);
    if out_ext.is_empty() {
        return Err(MediaError::InvalidInput(
            "output_format must contain a container name".to_string(),
        ));
    }

    let audio_plan = audio_plan(options, audio_stream, &out_ext);
    // Whether `aresample=48000` will actually be applied — decided now
    // so the filter capability check happens before any encoding.
    let needs_aresample = match &audio_plan {
        AudioPlan::Encode => {
            options.resample_audio && audio_sample_rate(&report) != Some(TARGET_SAMPLE_RATE)
        }
        _ => false,
    };

    // Capability checks before touching the output — never silently
    // fall back to different filters/codecs.
    let caps = rt.capabilities()?;
    if !caps.has_filter("fps") {
        return Err(MediaError::Unsupported(
            "ffmpeg build lacks the fps filter required for frame-rate conversion".to_string(),
        ));
    }
    if needs_aresample && !caps.has_filter("aresample") {
        return Err(MediaError::Unsupported(
            "ffmpeg build lacks the aresample filter required for audio resampling".to_string(),
        ));
    }
    if !caps.has_muxer(&out_ext) {
        return Err(MediaError::Unsupported(format!(
            "ffmpeg build has no {out_ext} muxer for the requested output format"
        )));
    }
    let video_encoder = pick_encoder(
        caps,
        options.video_codec.as_deref(),
        DEFAULT_VIDEO_ENCODERS,
        "video",
    )?;
    let audio_encoder = match &audio_plan {
        AudioPlan::Absent | AudioPlan::Copy => None,
        AudioPlan::Encode => Some(pick_encoder(
            caps,
            options.audio_codec.as_deref().filter(|c| *c != "copy"),
            DEFAULT_AUDIO_ENCODERS,
            "audio",
        )?),
    };

    let input_ext = input_extension(&report);
    let tmp = TempDir::new("fps")?;
    let input_name = format!("input.{input_ext}");
    let input_path = tmp.write(&input_name, input)?;
    let output_name = format!("output.{out_ext}");
    let output_path = tmp.file(&output_name);

    let vf = format!(
        "fps=fps={}:round={}",
        fmt_fps(fps),
        options.rounding.as_str()
    );
    let mut args: Vec<OsString> = process::args(&[
        "-y",
        "-hide_banner",
        "-loglevel",
        "warning",
        "-i",
        input_path.to_str().unwrap_or("input"),
        "-map",
        "0:v:0",
        "-map",
        "0:a?",
        "-sn",
        "-dn",
        "-vf",
        &vf,
    ]);
    if needs_aresample {
        args.extend(process::args(&[
            "-af",
            &format!("aresample={TARGET_SAMPLE_RATE}"),
        ]));
    }
    args.extend(process::args(&["-c:v", &video_encoder]));
    if video_encoder == "libx264" {
        args.extend(process::args(&["-crf", "23", "-preset", "veryfast"]));
    }
    // yuv420p is the safe chroma layout for the common delivery
    // containers; other formats keep the encoder's default.
    if matches!(out_ext.as_str(), "mp4" | "m4v" | "mov" | "mkv" | "webm") {
        args.extend(process::args(&["-pix_fmt", "yuv420p"]));
    }
    match &audio_plan {
        AudioPlan::Absent => {}
        AudioPlan::Copy => {
            args.extend(process::args(&["-c:a", "copy"]));
        }
        AudioPlan::Encode => {
            let enc = audio_encoder.as_deref().unwrap_or("aac");
            args.extend(process::args(&["-c:a", enc]));
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
        primary: Some(MediaFile {
            name: output_name,
            bytes,
            content_type: content_type(&out_ext).map(str::to_string),
        }),
        extra: Vec::new(),
        warnings,
    })
}

/// Decide the audio handling from options + probed stream info.
/// `audio_stream` is `None` when the input has no audio stream.
fn audio_plan(
    options: &FpsOptions,
    audio_stream: Option<&MediaStream>,
    out_ext: &str,
) -> AudioPlan {
    let Some(stream) = audio_stream else {
        return AudioPlan::Absent;
    };
    // Explicit "copy" always wins — the caller takes responsibility for
    // container compatibility.
    if options.audio_codec.as_deref() == Some("copy") {
        return AudioPlan::Copy;
    }
    if options.audio_codec.is_some() || options.resample_audio {
        // Explicit encoder, or default AAC re-encode when resampling is
        // requested. `needs_aresample` adds the filter when the input
        // rate differs from the target.
        return AudioPlan::Encode;
    }
    // resample_audio = false, no explicit codec: stream copy when the
    // probed codec is known to be storable in the target container.
    match stream.codec_name.as_deref() {
        Some(c) if audio_copy_safe(c, out_ext) => AudioPlan::Copy,
        _ => AudioPlan::Encode,
    }
}

/// Whether `codec` (ffprobe `codec_name`) is known to mux cleanly into
/// `ext`. Conservative allow-lists; anything unrecognized is re-encoded
/// rather than risking a muxer failure.
fn audio_copy_safe(codec: &str, ext: &str) -> bool {
    let safe: &[&str] = match ext {
        "mp4" | "m4v" | "mov" | "m4a" => &["aac", "mp3", "ac3", "eac3", "alac"],
        "mkv" => &[
            "aac", "mp3", "vorbis", "opus", "flac", "ac3", "eac3", "alac", "dts", "truehd",
        ],
        "webm" => &["vorbis", "opus"],
        "wav" => &[
            "pcm_s16le",
            "pcm_s24le",
            "pcm_s32le",
            "pcm_u8",
            "pcm_f32le",
            "pcm_f64le",
            "flac",
        ],
        _ => &[
            "aac", "mp3", "vorbis", "opus", "flac", "ac3", "eac3", "alac",
        ],
    };
    safe.contains(&codec)
}

/// Resolve the encoder to use: the explicit `requested` name (must
/// exist in this build) or the first available `candidates` entry.
fn pick_encoder(
    caps: &RuntimeCapabilities,
    requested: Option<&str>,
    candidates: &[&str],
    kind: &str,
) -> MediaResult<String> {
    if let Some(name) = requested {
        if caps.has_encoder(name) {
            return Ok(name.to_string());
        }
        return Err(MediaError::Unsupported(format!(
            "{kind} encoder '{name}' is not available in this FFmpeg build"
        )));
    }
    candidates
        .iter()
        .copied()
        .find(|e| caps.has_encoder(e))
        .map(str::to_string)
        .ok_or_else(|| {
            MediaError::Unsupported(format!(
                "ffmpeg build has no usable {kind} encoder ({})",
                candidates.join("/")
            ))
        })
}

/// Run ffprobe over the input bytes and parse the JSON report.
fn probe_input(input: &[u8], rt: &Runtime, limits: &MediaLimits) -> MediaResult<ProbeReport> {
    // Bytes must live at a path for ffprobe; use a short-lived dir
    // before the working input file is staged.
    let tmp = TempDir::new("fps-probe")?;
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

/// Format the target fps for the filter: shortest round-trip decimal,
/// never scientific notation for our 0 < fps <= 240 range.
fn fmt_fps(x: f64) -> String {
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

/// Keep only `[a-z0-9-]` so a format string can never smuggle path or
/// option characters into the muxer name or filename.
fn sanitize_ext(s: &str) -> String {
    s.chars()
        .map(|c| c.to_ascii_lowercase())
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

/// Sample rate (Hz) of the first audio stream.
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
    use crate::crates::multimedia_engine::runtime::discovery::find_tool;
    use std::process::Command;

    fn runtime() -> Option<Runtime> {
        if find_tool("ffmpeg").is_none() || find_tool("ffprobe").is_none() {
            eprintln!("skipping: ffmpeg/ffprobe not on PATH");
            return None;
        }
        match Runtime::discover() {
            Ok(rt) => Some(rt),
            Err(e) => {
                eprintln!("skipping: runtime discovery failed: {e}");
                None
            }
        }
    }

    /// Generate a 2-second 320x240@10fps video-only clip.
    fn make_clip() -> Option<Vec<u8>> {
        let dir = TempDir::new("fps-test-src").ok()?;
        let out = dir.file("clip.mp4");
        // `mpeg4` is a native FFmpeg encoder — no external library needed.
        let ok = Command::new("ffmpeg")
            .args([
                "-y",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=2:size=320x240:rate=10",
                "-c:v",
                "mpeg4",
                "-pix_fmt",
                "yuv420p",
                "-an",
                out.to_string_lossy().as_ref(),
            ])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return None;
        }
        std::fs::read(&out).ok()
    }

    /// Probed video facts for the first video stream: `(avg_fps,
    /// duration_secs, nb_frames)` — each `None` when ffprobe omits it.
    fn probe_video(bytes: &[u8]) -> Option<(Option<f64>, Option<f64>, Option<f64>)> {
        let dir = TempDir::new("fps-probe-out").ok()?;
        let input = dir.write("out.mp4", bytes).ok()?;
        let ffprobe = find_tool("ffprobe")?;
        let out = Command::new(ffprobe)
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream=avg_frame_rate,nb_frames,duration",
                "-of",
                "json",
            ])
            .arg(&input)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let report: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
        let stream = report.get("streams")?.get(0)?;
        // avg_frame_rate is "num/den" (e.g. "30000/1001") or a bare
        // number; take the quotient.
        let rate = stream.get("avg_frame_rate")?.as_str().and_then(|r| {
            let (n, d) = r.split_once('/').unwrap_or((r, "1"));
            let n: f64 = n.trim().parse().ok()?;
            let d: f64 = d.trim().parse().ok()?;
            (d != 0.0).then_some(n / d)
        });
        let frames = stream.get("nb_frames")?.as_str()?.parse::<f64>().ok();
        let dur = stream.get("duration")?.as_str()?.parse::<f64>().ok();
        Some((rate, dur, frames))
    }

    #[test]
    fn halving_fps_halves_frame_count_keeps_duration() {
        let Some(rt) = runtime() else { return };
        let Some(input) = make_clip() else {
            eprintln!("skipping: could not build test clip");
            return;
        };
        let (in_rate, in_dur, in_frames) = probe_video(&input).expect("probe input");
        assert!(
            (in_rate.unwrap_or(0.0) - 10.0).abs() < 1.0,
            "fixture should be ~10fps, got {in_rate:?}"
        );

        let opts = FpsOptions {
            fps: 5.0,
            ..FpsOptions::default()
        };
        let out = fps_convert(&input, &opts, &MediaLimits::default(), &rt)
            .expect("fps_convert 10→5 failed");
        let primary = out.primary.expect("primary output");
        assert_eq!(primary.content_type.as_deref(), Some("video/mp4"));

        let (out_rate, out_dur, out_frames) = probe_video(&primary.bytes).expect("probe output");

        // Duration must be preserved (~2s) while the rate halves.
        let dur = out_dur.or(in_dur).expect("duration available");
        assert!(
            (dur - 2.0).abs() < 0.5,
            "duration should stay ~2s, got {dur}"
        );
        assert!(
            (out_rate.unwrap_or(0.0) - 5.0).abs() < 1.0,
            "output rate should be ~5fps, got {out_rate:?}"
        );

        // Frame count roughly halves: input 2s@10fps ≈ 20 frames,
        // output ≈ 10. Tolerate encoder timestamp rounding.
        if let (Some(f0), Some(f1)) = (in_frames, out_frames) {
            assert!(
                (f1 - f0 / 2.0).abs() <= 3.0,
                "frames {f0}→{f1} should be ~half"
            );
        }
    }

    #[test]
    fn invalid_fps_is_rejected() {
        let Some(rt) = runtime() else { return };
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY, MAX_FPS + 1.0] {
            let opts = FpsOptions {
                fps: bad,
                ..FpsOptions::default()
            };
            let err = fps_convert(&[0u8; 16], &opts, &MediaLimits::default(), &rt)
                .expect_err("invalid fps must fail");
            assert!(
                matches!(err, MediaError::InvalidInput(_)),
                "fps {bad} should be InvalidInput, got {err:?}"
            );
        }
    }

    #[test]
    fn unknown_encoder_is_unsupported() {
        let Some(rt) = runtime() else { return };
        let Some(input) = make_clip() else {
            eprintln!("skipping: could not build test clip");
            return;
        };
        let opts = FpsOptions {
            video_codec: Some("definitely-not-an-encoder".to_string()),
            ..FpsOptions::default()
        };
        let err = fps_convert(&input, &opts, &MediaLimits::default(), &rt)
            .expect_err("unknown video codec must fail");
        assert!(
            matches!(err, MediaError::Unsupported(_)),
            "expected Unsupported, got {err:?}"
        );
    }
}
