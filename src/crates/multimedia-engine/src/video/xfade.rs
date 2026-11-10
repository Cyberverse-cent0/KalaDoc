//! Transition (crossfade) between exactly two videos via ffmpeg's
//! `xfade` filter for video and `acrossfade` for audio.
//!
//! [`xfade`] takes two raw byte buffers (`a` then `b`), materializes
//! them in a temp dir, probes both with ffprobe, and joins them in a
//! single `-filter_complex` graph:
//!
//! - **Video**: each input's first video stream is normalized to a
//!   common geometry/frame-rate/pixel format — the *first* input's
//!   probed width/height and frame-rate string (falling back to
//!   1280x720 @ 30 fps) plus canonical `yuv420p` pixels — via
//!   `scale`/`fps`/`setsar`/`format`, then joined with
//!   `xfade=transition=<name>:duration=<d>:offset=<o>`.
//! - **Audio**: included only when **both** inputs have an audio
//!   stream. Each branch is canonicalized to 48 kHz stereo float PCM
//!   (`aresample` + `aformat`) and joined with
//!   `acrossfade=d=<d>` — the same overlap duration as the video
//!   transition, so the audio crossfade lines up with the visual
//!   one. When only one (or neither) input has audio the output is
//!   video-only and a warning is recorded in
//!   [`MediaOutput::warnings`]; padding the silent side with
//!   `anullsrc`/`apad`/`adelay` is not implemented.
//!
//! `offset` semantics follow `xfade`: it is the time in seconds
//! (measured on the first input's timeline) at which the transition
//! starts. [`XfadeOptions::offset_secs`] defaults to
//! `duration_a - duration_secs`, producing the usual
//! `dur_a + dur_b - duration` output. When durations are probed, the
//! transition must fit inside the first input
//! (`offset + duration <= dur_a`) and inside the second
//! (`duration <= dur_b`) — violations are rejected up front instead
//! of failing inside ffmpeg.
//!
//! Transition names are built into the `xfade` filter (there is no
//! way to capability-probe individual names), so
//! [`XfadeOptions::transition`] is validated against [`TRANSITIONS`],
//! a curated list of the documented built-ins. Unknown names are
//! rejected with [`MediaError::InvalidInput`] before ffmpeg runs.
//!
//! Codecs are never assumed: the video encoder defaults to the first
//! available of `libx264`/`libopenh264`/`h264`/`mpeg4` and audio to
//! `aac`, each verified against this build's encoder list; explicit
//! [`XfadeOptions::video_codec`]/[`XfadeOptions::audio_codec`] values
//! must exist in the build or the op returns
//! [`MediaError::Unsupported`]. `xfade`/`acrossfade`/normalization
//! filters and the output muxer are capability-checked the same way.

use std::ffi::OsString;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaFile, MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::probe::{MediaStream, ProbeReport};
use crate::crates::multimedia_engine::runtime::discovery::{Runtime, RuntimeCapabilities};
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Canonical audio rate for both crossfade branches.
const CANONICAL_SAMPLE_RATE: u32 = 48_000;
/// Tolerance (seconds) when checking that the transition window fits
/// inside the probed input durations — absorbs container rounding.
const DURATION_EPS: f64 = 0.05;

/// Built-in `xfade` transition names accepted by
/// [`XfadeOptions::transition`].
///
/// These mirror the transitions compiled into ffmpeg's `xfade`
/// filter (the filter rejects unknown names itself, but only after
/// decoding has started — validating here gives a clean
/// [`MediaError::InvalidInput`]). Custom expression-based
/// transitions are not supported by this op.
pub const TRANSITIONS: &[&str] = &[
    "fade",
    "fadeblack",
    "fadewhite",
    "fadegrays",
    "fadefast",
    "fadeslow",
    "distance",
    "wipeleft",
    "wiperight",
    "wipeup",
    "wipedown",
    "wipetl",
    "wipetr",
    "wipebl",
    "wipebr",
    "slideleft",
    "slideright",
    "slideup",
    "slidedown",
    "smoothleft",
    "smoothright",
    "smoothup",
    "smoothdown",
    "circlecrop",
    "circleopen",
    "circleclose",
    "rectcrop",
    "radial",
    "pixelize",
    "dissolve",
    "diagtl",
    "diagtr",
    "diagbl",
    "diagbr",
    "hlslice",
    "hrslice",
    "vuslice",
    "vdslice",
    "hblur",
    "hlwind",
    "hrwind",
    "vuwind",
    "vdwind",
    "coverleft",
    "coverright",
    "coverup",
    "coverdown",
    "revealleft",
    "revealright",
    "revealup",
    "revealdown",
    "squeezeh",
    "squeezev",
    "zoomin",
    "vertopen",
    "vertclose",
    "horzopen",
    "horzclose",
];

/// Video encoders acceptable for the re-encode, in preference order.
const VIDEO_ENCODERS: &[&str] = &["libx264", "libopenh264", "h264", "mpeg4"];

/// Options for [`xfade`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XfadeOptions {
    /// `xfade` transition name (e.g. `"fade"`, `"wipeleft"`,
    /// `"dissolve"`). Must be one of [`TRANSITIONS`]; matched
    /// case-insensitively and lowercased. Default `"fade"`.
    #[serde(default = "default_transition")]
    pub transition: String,
    /// Transition duration in seconds (`xfade=duration=`,
    /// `acrossfade=d=`). Must be finite and `> 0`, and fit inside both
    /// inputs' probed durations.
    pub duration_secs: f64,
    /// Transition start offset in seconds on the first input's
    /// timeline (`xfade=offset=`). `None` (default) uses
    /// `duration_a - duration_secs`, i.e. the transition occupies the
    /// tail of `a` overlapping the head of `b`. Must be finite and
    /// `>= 0`; when `a`'s duration is known,
    /// `offset_secs + duration_secs` must not exceed it.
    #[serde(default)]
    pub offset_secs: Option<f64>,
    /// Output container extension without a dot (`"mp4"`, `"mkv"`,
    /// `"webm"`, ...). `None` or empty → `"mp4"`. Must be a
    /// video-capable container — audio-only targets fail inside
    /// ffmpeg.
    #[serde(default)]
    pub output_format: Option<String>,
    /// Video encoder. `None` picks the first available of
    /// `libx264`/`libopenh264`/`h264`/`mpeg4` in this build. An
    /// explicit value must exist in the build's encoder list or the
    /// op returns [`MediaError::Unsupported`].
    #[serde(default)]
    pub video_codec: Option<String>,
    /// Audio encoder, used only when both inputs have audio.
    /// `None` → `"aac"`. Explicit values are capability-checked.
    #[serde(default)]
    pub audio_codec: Option<String>,
}

fn default_transition() -> String {
    "fade".to_string()
}

impl Default for XfadeOptions {
    fn default() -> Self {
        Self {
            transition: default_transition(),
            duration_secs: 0.5,
            offset_secs: None,
            output_format: None,
            video_codec: None,
            audio_codec: None,
        }
    }
}

/// Join videos `a` and `b` with a transition.
///
/// `a` plays first; `b` is blended in starting at `offset_secs`
/// (default: the last `duration_secs` of `a`). Output duration is
/// `offset + duration_b` — with the default offset,
/// `dur_a + dur_b - duration_secs`.
///
/// Both inputs must contain a video stream; audio is crossfaded only
/// when both have audio (otherwise the output is video-only with a
/// warning). All video streams are re-encoded — this is never a
/// stream-copy operation.
///
/// # Errors
///
/// - [`MediaError::InvalidInput`] — empty/oversized input, bad
///   `duration_secs`/`offset_secs`, unknown transition name, missing
///   video stream, or a transition window that does not fit the
///   probed durations.
/// - [`MediaError::LimitExceeded`] — byte caps on inputs/output.
/// - [`MediaError::Unsupported`] — `xfade`/`acrossfade`/normalization
///   filters, chosen encoder or output muxer missing from this build.
/// - [`MediaError::ToolFailed`] / [`MediaError::Timeout`] — ffmpeg
///   rejected the inputs or the filter graph.
pub fn xfade(
    a: &[u8],
    b: &[u8],
    options: &XfadeOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    check_input(a, "a", limits)?;
    check_input(b, "b", limits)?;
    let transition = validate_options(options)?;

    // Capability checks that don't depend on the probed inputs.
    let caps = rt.capabilities()?;
    for name in ["xfade", "scale", "fps", "setsar", "format"] {
        if !caps.has_filter(name) {
            return Err(MediaError::Unsupported(format!(
                "ffmpeg build lacks the '{name}' filter required for xfade"
            )));
        }
    }
    let out_ext = options
        .output_format
        .as_deref()
        .map(|s| s.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "mp4".to_string());
    let muxer = muxer_name(&out_ext);
    if !caps.has_muxer(muxer) {
        return Err(MediaError::Unsupported(format!(
            "ffmpeg build lacks the '{muxer}' muxer for .{out_ext} output"
        )));
    }
    let video_encoder = pick_video_encoder(options.video_codec.as_deref(), caps)?;

    let tmp = TempDir::new("xfade")?;
    // ffmpeg/ffprobe probe by content; a sniffed extension only helps
    // the demuxer pick the right parser for ambiguous containers.
    let path_a = tmp.write(&format!("a{}", sniff_ext(a)), a)?;
    let path_b = tmp.write(&format!("b{}", sniff_ext(b)), b)?;

    let report_a = probe_file(&path_a, rt, limits)?;
    let report_b = probe_file(&path_b, rt, limits)?;

    if video_of(&report_a).is_none() {
        return Err(MediaError::InvalidInput(
            "input a has no video stream; xfade requires video on both inputs".to_string(),
        ));
    }
    if video_of(&report_b).is_none() {
        return Err(MediaError::InvalidInput(
            "input b has no video stream; xfade requires video on both inputs".to_string(),
        ));
    }
    let with_audio = audio_of(&report_a).is_some() && audio_of(&report_b).is_some();
    let mut warnings = Vec::new();
    if !with_audio {
        warnings.push(
            "xfade output is video-only: acrossfade requires an audio stream on BOTH \
             inputs (silent-side padding is not implemented)"
                .to_string(),
        );
    } else {
        for name in ["aresample", "aformat", "acrossfade"] {
            if !caps.has_filter(name) {
                return Err(MediaError::Unsupported(format!(
                    "ffmpeg build lacks the '{name}' filter required for xfade audio"
                )));
            }
        }
    }

    // Common video target: first input's probed geometry/fps with
    // canonical yuv420p pixels — the minimum unification xfade needs.
    let first_video = video_of(&report_a);
    let (width, height) = first_video
        .and_then(|v| v.width.zip(v.height))
        .unwrap_or((1280, 720));
    let fps = first_video
        .and_then(|v| v.r_frame_rate.as_deref().or(v.avg_frame_rate.as_deref()))
        .filter(|s| valid_rate(s))
        .unwrap_or("30")
        .to_string();

    let dur_a = probe_duration(&report_a);
    let dur_b = probe_duration(&report_b);
    let duration = options.duration_secs;
    if let Some(da) = dur_a
        && duration > da + DURATION_EPS
    {
        return Err(MediaError::InvalidInput(format!(
            "duration_secs ({duration}) exceeds input a's probed duration ({da:.3}s)"
        )));
    }
    if let Some(db) = dur_b
        && duration > db + DURATION_EPS
    {
        return Err(MediaError::InvalidInput(format!(
            "duration_secs ({duration}) exceeds input b's probed duration ({db:.3}s)"
        )));
    }
    let offset = match options.offset_secs {
        Some(o) => {
            if let Some(da) = dur_a
                && o + duration > da + DURATION_EPS
            {
                return Err(MediaError::InvalidInput(format!(
                    "offset_secs ({o}) + duration_secs ({duration}) exceeds input a's \
                     probed duration ({da:.3}s); the transition must end before a ends"
                )));
            }
            o
        }
        None => match dur_a {
            Some(da) => da - duration,
            None => {
                return Err(MediaError::InvalidInput(
                    "cannot determine default xfade offset: input a's duration could not \
                     be probed; pass offset_secs explicitly"
                        .to_string(),
                ));
            }
        },
    };
    if offset < 0.0 {
        return Err(MediaError::InvalidInput(format!(
            "computed xfade offset is negative ({offset}); duration_secs exceeds input a's duration"
        )));
    }

    // Audio encoder is only needed on the both-audio path.
    let audio_encoder = if with_audio {
        Some(pick_audio_encoder(options.audio_codec.as_deref(), caps)?)
    } else {
        None
    };

    let out_name = format!("output.{out_ext}");
    let out_path = tmp.file(&out_name);

    // Normalize both branches to identical parameters — xfade
    // requires matching geometry, fps, SAR and pixel format.
    let vchain = format!("scale={width}:{height},fps={fps},setsar=1,format=yuv420p");
    let mut graph = format!(
        "[0:v]{vchain}[v0];[1:v]{vchain}[v1];\
         [v0][v1]xfade=transition={transition}:duration={}:offset={}[vout]",
        fmt_secs(duration),
        fmt_secs(offset),
    );
    if with_audio {
        graph.push_str(&format!(
            ";[0:a]aresample={CANONICAL_SAMPLE_RATE},\
             aformat=sample_fmts=fltp:channel_layouts=stereo[a0];\
             [1:a]aresample={CANONICAL_SAMPLE_RATE},\
             aformat=sample_fmts=fltp:channel_layouts=stereo[a1];\
             [a0][a1]acrossfade=d={}[aout]",
            fmt_secs(duration),
        ));
    }

    let mut args: Vec<OsString> = vec![
        OsString::from("-hide_banner"),
        OsString::from("-loglevel"),
        OsString::from("error"),
        OsString::from("-nostdin"),
        OsString::from("-y"),
        OsString::from("-i"),
        path_a.into_os_string(),
        OsString::from("-i"),
        path_b.into_os_string(),
        OsString::from("-filter_complex"),
        OsString::from(graph),
        OsString::from("-map"),
        OsString::from("[vout]"),
        OsString::from("-c:v"),
        OsString::from(video_encoder),
        OsString::from("-pix_fmt"),
        OsString::from("yuv420p"),
    ];
    if video_encoder == "libx264" {
        args.extend([
            OsString::from("-preset"),
            OsString::from("veryfast"),
            OsString::from("-crf"),
            OsString::from("23"),
        ]);
    }
    if let Some(enc) = audio_encoder {
        args.extend([
            OsString::from("-map"),
            OsString::from("[aout]"),
            OsString::from("-c:a"),
            OsString::from(enc),
            OsString::from("-b:a"),
            OsString::from("128k"),
        ]);
    }
    args.push(out_path.into_os_string());

    process::run_checked("ffmpeg", &rt.ffmpeg, &args, None, limits)?;

    let bytes = tmp.read_limited(&out_name, limits.max_output_bytes)?;
    if bytes.is_empty() {
        return Err(MediaError::ToolFailed {
            tool: "ffmpeg",
            status: "empty output".to_string(),
            detail: "xfade produced a zero-byte file".to_string(),
        });
    }

    Ok(MediaOutput {
        primary: Some(MediaFile {
            name: out_name,
            bytes,
            content_type: content_type(&out_ext).map(str::to_string),
        }),
        extra: Vec::new(),
        warnings,
    })
}

/// Validate a raw input buffer against the byte cap.
fn check_input(data: &[u8], which: &str, limits: &MediaLimits) -> MediaResult<()> {
    if data.is_empty() {
        return Err(MediaError::InvalidInput(format!("input {which} is empty")));
    }
    if let Some(max) = limits.max_input_bytes
        && (data.len() as u64) > max
    {
        return Err(MediaError::LimitExceeded(format!(
            "input {which} is {} bytes, exceeding max_input_bytes {max}",
            data.len()
        )));
    }
    Ok(())
}

/// Validate `options`, returning the normalized (lowercased)
/// transition name. Runs before any capability or ffmpeg work so bad
/// options fail fast.
fn validate_options(options: &XfadeOptions) -> MediaResult<String> {
    if !options.duration_secs.is_finite() || options.duration_secs <= 0.0 {
        return Err(MediaError::InvalidInput(format!(
            "duration_secs must be finite and > 0, got {}",
            options.duration_secs
        )));
    }
    if let Some(o) = options.offset_secs
        && (!o.is_finite() || o < 0.0)
    {
        return Err(MediaError::InvalidInput(format!(
            "offset_secs must be finite and >= 0, got {o}"
        )));
    }
    let transition = options.transition.trim().to_ascii_lowercase();
    if transition.is_empty() || !TRANSITIONS.contains(&transition.as_str()) {
        return Err(MediaError::InvalidInput(format!(
            "unknown xfade transition {transition:?}; expected one of: {}",
            TRANSITIONS.join(", ")
        )));
    }
    Ok(transition)
}

fn video_of<'a>(r: &'a ProbeReport) -> Option<&'a MediaStream> {
    r.streams.iter().find(|s| s.kind() == Some("video"))
}

fn audio_of<'a>(r: &'a ProbeReport) -> Option<&'a MediaStream> {
    r.streams.iter().find(|s| s.kind() == Some("audio"))
}

/// First available video encoder: the explicit codec when given
/// (capability-checked), else the first of [`VIDEO_ENCODERS`] this
/// build has.
fn pick_video_encoder<'a>(
    requested: Option<&'a str>,
    caps: &'a RuntimeCapabilities,
) -> MediaResult<&'a str> {
    if let Some(name) = requested.map(str::trim).filter(|s| !s.is_empty()) {
        if caps.has_encoder(name) {
            return Ok(name);
        }
        return Err(MediaError::Unsupported(format!(
            "video encoder {name:?} is not available in this FFmpeg build"
        )));
    }
    VIDEO_ENCODERS
        .iter()
        .copied()
        .find(|e| caps.has_encoder(e))
        .ok_or_else(|| {
            MediaError::Unsupported(
                "ffmpeg build lacks a usable video encoder \
                 (libx264/libopenh264/h264/mpeg4) for xfade"
                    .to_string(),
            )
        })
}

/// Audio encoder for the both-audio path: explicit (checked) or `aac`.
fn pick_audio_encoder<'a>(
    requested: Option<&'a str>,
    caps: &'a RuntimeCapabilities,
) -> MediaResult<&'a str> {
    let name = requested
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("aac");
    if caps.has_encoder(name) {
        return Ok(name);
    }
    Err(MediaError::Unsupported(format!(
        "audio encoder {name:?} is not available in this FFmpeg build"
    )))
}

/// Container duration in seconds: prefer the format duration, fall
/// back to the first video stream's.
fn probe_duration(report: &ProbeReport) -> Option<f64> {
    report
        .format
        .as_ref()
        .and_then(|f| f.duration_secs())
        .or_else(|| {
            report
                .streams
                .iter()
                .find(|s| s.kind() == Some("video"))
                .and_then(|s| s.duration_secs())
        })
}

/// Probe a media file with ffprobe, returning the parsed report.
fn probe_file(path: &Path, rt: &Runtime, limits: &MediaLimits) -> MediaResult<ProbeReport> {
    let mut args = process::args(&[
        "-v",
        "error",
        "-show_format",
        "-show_streams",
        "-of",
        "json",
    ]);
    args.push(path.as_os_str().to_os_string());
    let out = process::run_checked("ffprobe", &rt.ffprobe, &args, None, limits)?;
    serde_json::from_slice(&out.stdout)
        .map_err(|e| MediaError::InvalidInput(format!("ffprobe returned unparseable JSON: {e}")))
}

/// Format seconds for ffmpeg filter arguments (millisecond
/// precision — ample for transition timing).
fn fmt_secs(secs: f64) -> String {
    format!("{secs:.3}")
}

/// Whether `s` looks like an ffprobe frame-rate string (`"30"`,
/// `"30000/1001"`) usable directly as an `fps=` filter value.
fn valid_rate(s: &str) -> bool {
    let (num, den) = s.split_once('/').unwrap_or((s, "1"));
    num.trim().parse::<f64>().is_ok() && den.trim().parse::<f64>().is_ok()
}

/// Sniff a container extension from magic bytes so ffmpeg gets a
/// demuxer hint; `.bin` (content probing) when unrecognized.
fn sniff_ext(data: &[u8]) -> &'static str {
    if data.len() >= 12 && &data[4..8] == b"ftyp" {
        return if &data[8..12] == b"qt  " {
            ".mov"
        } else {
            ".mp4"
        };
    }
    if data.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return ".mkv";
    }
    if data.starts_with(b"RIFF") && data.len() >= 12 {
        return match &data[8..12] {
            b"WAVE" => ".wav",
            b"AVI " => ".avi",
            _ => ".bin",
        };
    }
    if data.starts_with(b"OggS") {
        return ".ogg";
    }
    if data.starts_with(b"fLaC") {
        return ".flac";
    }
    if data.starts_with(b"ID3") || (data.len() >= 2 && data[0] == 0xFF && (data[1] & 0xE0) == 0xE0)
    {
        return ".mp3";
    }
    ".bin"
}

/// Map a file extension to the ffmpeg muxer name used for capability
/// checks.
fn muxer_name(ext: &str) -> &'static str {
    match ext {
        "mp4" | "m4v" => "mp4",
        "mov" => "mov",
        "mkv" => "matroska",
        "webm" => "webm",
        "avi" => "avi",
        "3gp" | "3g2" => "3gp",
        "wav" => "wav",
        "mp3" => "mp3",
        "flac" => "flac",
        "ogg" => "ogg",
        "opus" => "opus",
        "m4a" => "ipod",
        "aac" | "adts" => "adts",
        _ => "mp4",
    }
}

/// Best-effort MIME type for the output container.
fn content_type(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "mp4" | "m4v" | "m4a" => "video/mp4",
        "mov" => "video/quicktime",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "avi" => "video/x-msvideo",
        "3gp" | "3g2" => "video/3gpp",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "ogg" | "opus" => "audio/ogg",
        "aac" | "adts" => "audio/aac",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn runtime() -> Option<Runtime> {
        Runtime::discover().ok()
    }

    fn limits() -> MediaLimits {
        MediaLimits {
            timeout: Duration::from_secs(120),
            ..MediaLimits::default()
        }
    }

    /// Whether this build can run the full xfade path (filters,
    /// encoder, muxer). Tests skip gracefully when it cannot.
    fn xfade_supported(rt: &Runtime) -> bool {
        let Ok(caps) = rt.capabilities() else {
            return false;
        };
        [
            "xfade",
            "acrossfade",
            "scale",
            "fps",
            "setsar",
            "format",
            "aresample",
            "aformat",
        ]
        .iter()
        .all(|f| caps.has_filter(f))
            && VIDEO_ENCODERS.iter().any(|e| caps.has_encoder(e))
            && caps.has_encoder("aac")
            && caps.has_muxer("mp4")
    }

    /// Render a 1s 64x64@30 testsrc clip via lavfi, with a sine tone
    /// unless `with_audio` is false. Returns `None` (test skips) when
    /// ffmpeg or the needed encoders are unavailable.
    fn make_clip(rt: &Runtime, freq: u32, with_audio: bool) -> Option<Vec<u8>> {
        let caps = rt.capabilities().ok()?;
        let video_enc = VIDEO_ENCODERS
            .iter()
            .copied()
            .find(|e| caps.has_encoder(e))?;
        if with_audio && !caps.has_encoder("aac") {
            return None;
        }
        let dir = TempDir::new("xfade-test-in").ok()?;
        let out_path = dir.file("clip.mp4");
        let mut args: Vec<OsString> = vec![
            OsString::from("-hide_banner"),
            OsString::from("-loglevel"),
            OsString::from("error"),
            OsString::from("-nostdin"),
            OsString::from("-y"),
            OsString::from("-f"),
            OsString::from("lavfi"),
            OsString::from("-i"),
            OsString::from("testsrc=size=64x64:rate=30:duration=1"),
        ];
        if with_audio {
            args.extend([
                OsString::from("-f"),
                OsString::from("lavfi"),
                OsString::from("-i"),
                OsString::from(format!("sine=frequency={freq}:duration=1")),
            ]);
        }
        args.extend([
            OsString::from("-c:v"),
            OsString::from(video_enc),
            OsString::from("-pix_fmt"),
            OsString::from("yuv420p"),
        ]);
        if with_audio {
            args.extend([
                OsString::from("-c:a"),
                OsString::from("aac"),
                OsString::from("-shortest"),
            ]);
        } else {
            args.push(OsString::from("-an"));
        }
        args.push(out_path.as_os_str().to_os_string());
        let out = process::run("ffmpeg", &rt.ffmpeg, &args, None, &limits()).ok()?;
        if !out.success {
            return None;
        }
        std::fs::read(&out_path).ok()
    }

    /// Write output bytes to a temp file and return its probed
    /// container duration in seconds.
    fn output_duration(output: &MediaOutput, rt: &Runtime) -> Option<f64> {
        let dir = TempDir::new("xfade-test-out").ok()?;
        let name = "out.mp4";
        dir.write(name, &output.primary.as_ref()?.bytes).ok()?;
        let report = probe_file(&dir.file(name), rt, &limits()).ok()?;
        report.format.as_ref().and_then(|f| f.duration_secs())
    }

    #[test]
    fn xfade_fade_joins_two_clips() {
        let Some(rt) = runtime() else {
            eprintln!("skipping: ffmpeg/ffprobe unavailable");
            return;
        };
        if !xfade_supported(&rt) {
            eprintln!("skipping: xfade filters/encoders unavailable in this build");
            return;
        }
        let (Some(a), Some(b)) = (make_clip(&rt, 440, true), make_clip(&rt, 550, true)) else {
            eprintln!("skipping: lavfi clip generation unavailable");
            return;
        };

        let options = XfadeOptions {
            transition: "fade".to_string(),
            duration_secs: 0.3,
            ..XfadeOptions::default()
        };
        let out = xfade(&a, &b, &options, &limits(), &rt).expect("xfade failed");
        let primary = out.primary.as_ref().expect("primary output");
        assert_eq!(primary.name, "output.mp4");
        assert_eq!(primary.content_type.as_deref(), Some("video/mp4"));

        let dur = output_duration(&out, &rt).expect("output duration");
        // 1s + 1s - 0.3s overlap ≈ 1.7s.
        assert!(
            (dur - 1.7).abs() < 0.4,
            "expected ~1.7s xfade duration, got {dur}"
        );
    }

    #[test]
    fn xfade_video_only_warns_when_audio_missing() {
        let Some(rt) = runtime() else {
            eprintln!("skipping: ffmpeg/ffprobe unavailable");
            return;
        };
        let caps_ok = rt
            .capabilities()
            .map(|c| {
                ["xfade", "scale", "fps", "setsar", "format"]
                    .iter()
                    .all(|f| c.has_filter(f))
                    && VIDEO_ENCODERS.iter().any(|e| c.has_encoder(e))
                    && c.has_muxer("mp4")
            })
            .unwrap_or(false);
        if !caps_ok {
            eprintln!("skipping: xfade video path unavailable in this build");
            return;
        }
        let (Some(a), Some(b)) = (make_clip(&rt, 0, false), make_clip(&rt, 0, false)) else {
            eprintln!("skipping: lavfi clip generation unavailable");
            return;
        };

        let options = XfadeOptions {
            duration_secs: 0.3,
            ..XfadeOptions::default()
        };
        let out = xfade(&a, &b, &options, &limits(), &rt).expect("video-only xfade failed");
        assert!(
            out.warnings.iter().any(|w| w.contains("video-only")),
            "expected video-only warning, got {:?}",
            out.warnings
        );
    }

    #[test]
    fn xfade_rejects_invalid_options() {
        let Some(rt) = runtime() else {
            eprintln!("skipping: ffmpeg/ffprobe unavailable");
            return;
        };
        let a = vec![0u8; 64];
        let b = vec![0u8; 64];

        // Empty input.
        let err = xfade(&[], &b, &XfadeOptions::default(), &limits(), &rt)
            .expect_err("empty input must fail");
        assert!(matches!(err, MediaError::InvalidInput(_)));

        // Bad duration.
        for bad in [0.0, -1.0, f64::NAN] {
            let opts = XfadeOptions {
                duration_secs: bad,
                ..XfadeOptions::default()
            };
            let err = xfade(&a, &b, &opts, &limits(), &rt).expect_err("bad duration must fail");
            assert!(matches!(err, MediaError::InvalidInput(_)), "got {err:?}");
        }

        // Unknown transition name.
        let opts = XfadeOptions {
            transition: "not_a_transition".to_string(),
            ..XfadeOptions::default()
        };
        let err = xfade(&a, &b, &opts, &limits(), &rt).expect_err("bad transition must fail");
        assert!(matches!(err, MediaError::InvalidInput(_)));

        // Negative offset.
        let opts = XfadeOptions {
            offset_secs: Some(-0.5),
            ..XfadeOptions::default()
        };
        let err = xfade(&a, &b, &opts, &limits(), &rt).expect_err("bad offset must fail");
        assert!(matches!(err, MediaError::InvalidInput(_)));
    }
}
