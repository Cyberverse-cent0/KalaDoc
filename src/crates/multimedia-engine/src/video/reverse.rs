//! Reverse playback of a media file (video and/or audio).
//!
//! [`reverse`] decodes the entire input and plays it backwards:
//!
//! - **Video**: `-vf reverse` — buffers every frame, then emits them
//!   in reverse order. Full decode/re-encode is mandatory; stream
//!   copy cannot reverse.
//! - **Audio**: `-af areverse` — same idea for samples.
//!
//! Inputs with only one stream kind get only the matching filter;
//! inputs with neither audio nor video are rejected. `keep_audio =
//! false` drops the audio stream entirely instead of reversing it.
//!
//! Re-encoding uses FFmpeg's per-muxer defaults unless
//! [`ReverseOptions::video_codec`] / [`ReverseOptions::audio_codec`]
//! name an encoder explicitly (capability-checked — an unavailable
//! codec is [`MediaError::Unsupported`], never a silent fallback).
//! When no codec is requested the video encoder is picked from a
//! preference chain: `libx264` → `libopenh264` → `h264` → `mpeg4`
//! for MP4-family/MKV/AVI output, and `libvpx-vp9` → `libvpx` for
//! WebM. This build of FFmpeg has no `libx264`, so `libopenh264`,
//! the native `h264` encoder, or `mpeg4` are the expected winners.

use std::ffi::OsString;

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::probe::ProbeReport;
use crate::crates::multimedia_engine::runtime::discovery::{Runtime, RuntimeCapabilities};
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Options for [`reverse`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ReverseOptions {
    /// Output container/format name (`"mp4"`, `"mkv"`, `"webm"`,
    /// `"wav"`, ...). `None` defaults to `"mp4"`.
    pub output_format: Option<String>,
    /// Explicit video encoder name (`"libopenh264"`, `"h264"`,
    /// `"mpeg4"`, `"libvpx-vp9"`, ...). `None` picks the first
    /// available encoder from the per-container preference chain.
    /// Must exist in this FFmpeg build or the call fails with
    /// [`MediaError::Unsupported`].
    pub video_codec: Option<String>,
    /// Explicit audio encoder name (`"aac"`, `"libopus"`, ...).
    /// `None` picks a per-container default (AAC for MP4-family,
    /// Opus/Vorbis for WebM, PCM for WAV, ...). Must exist in this
    /// FFmpeg build or the call fails with [`MediaError::Unsupported`].
    pub audio_codec: Option<String>,
    /// When `true` (default), audio streams are reversed with
    /// `areverse`. When `false`, audio is dropped from the output.
    pub keep_audio: bool,
}

impl Default for ReverseOptions {
    fn default() -> Self {
        Self {
            output_format: None,
            video_codec: None,
            audio_codec: None,
            keep_audio: true,
        }
    }
}

/// H.264-family encoders in preference order, then MPEG-4 part 2.
/// `libx264` is absent from this FFmpeg build; `libopenh264`, the
/// native `h264`, and `mpeg4` provide the fallback.
const VIDEO_ENCODERS: &[&str] = &["libx264", "libopenh264", "h264", "mpeg4"];
/// VP9/VP8 encoders for WebM output.
const WEBM_ENCODERS: &[&str] = &["libvpx-vp9", "libvpx"];
/// Encoders known to accept `-pix_fmt yuv420p` safely; anything else
/// (ProRes, rawvideo, ...) gets no forced pixel format.
const YUV420_ENCODERS: &[&str] = &[
    "mpeg4",
    "h264",
    "libx264",
    "libopenh264",
    "libvpx",
    "libvpx-vp9",
    "vp8",
    "vp9",
    "av1",
    "libaom-av1",
];

/// Check `input` against the input-size limit and non-emptiness.
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
    Ok(())
}

/// Probe the input bytes; the temp file lets ffprobe seek.
fn probe_input(input: &[u8], rt: &Runtime, limits: &MediaLimits) -> MediaResult<ProbeReport> {
    let tmp = TempDir::new("reverse-probe")?;
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

/// Resolve the output container extension: explicit `output_format`
/// wins, `None` defaults to `"mp4"`.
fn resolve_out_ext(options: &ReverseOptions) -> MediaResult<String> {
    let ext = match options.output_format.as_deref() {
        Some(f) => f
            .trim()
            .trim_start_matches('.')
            .to_ascii_lowercase()
            .replace(|c: char| !c.is_ascii_alphanumeric(), ""),
        None => "mp4".to_string(),
    };
    if ext.is_empty() {
        return Err(MediaError::InvalidInput(
            "output_format must contain a container name".to_string(),
        ));
    }
    Ok(ext)
}

/// Pick the video encoder for `ext`. An explicit `requested` codec is
/// honored verbatim after a capability check; otherwise the first
/// available encoder from the per-container preference chain wins.
/// Returns `None` when the input has no video stream.
fn video_encoder<'a>(
    ext: &str,
    requested: Option<&'a str>,
    caps: &'a RuntimeCapabilities,
) -> MediaResult<Option<&'a str>> {
    if let Some(name) = requested {
        let name = name.trim();
        if name.is_empty() {
            return Err(MediaError::InvalidInput(
                "video_codec must not be empty".to_string(),
            ));
        }
        return if caps.has_encoder(name) {
            Ok(Some(name))
        } else {
            Err(MediaError::Unsupported(format!(
                "video codec '{name}' is not available in this FFmpeg build"
            )))
        };
    }
    let candidates: &[&str] = if ext == "webm" {
        WEBM_ENCODERS
    } else {
        VIDEO_ENCODERS
    };
    Ok(candidates.iter().copied().find(|e| caps.has_encoder(e)))
}

/// Pick the audio encoder for `ext`. An explicit `requested` codec is
/// honored verbatim after a capability check; otherwise a per-
/// container default is chosen (AAC for MP4-family/MKV/AVI, Opus or
/// Vorbis for WebM, MP3 for `.mp3`, PCM for WAV, FLAC for FLAC,
/// Vorbis for OGG).
fn audio_encoder<'a>(
    ext: &str,
    requested: Option<&'a str>,
    caps: &'a RuntimeCapabilities,
) -> MediaResult<&'a str> {
    if let Some(name) = requested {
        let name = name.trim();
        if name.is_empty() {
            return Err(MediaError::InvalidInput(
                "audio_codec must not be empty".to_string(),
            ));
        }
        return if caps.has_encoder(name) {
            Ok(name)
        } else {
            Err(MediaError::Unsupported(format!(
                "audio codec '{name}' is not available in this FFmpeg build"
            )))
        };
    }
    let default = match ext {
        "webm" => {
            if caps.has_encoder("libopus") {
                Some("libopus")
            } else {
                caps.has_encoder("libvorbis").then_some("libvorbis")
            }
        }
        "mp3" => caps.has_encoder("libmp3lame").then_some("libmp3lame"),
        "wav" => Some("pcm_s16le"),
        "flac" => Some("flac"),
        "ogg" | "oga" => caps.has_encoder("libvorbis").then_some("libvorbis"),
        _ => caps.has_encoder("aac").then_some("aac"),
    };
    default.ok_or_else(|| {
        MediaError::Unsupported(format!(
            "no usable audio encoder for .{ext} output in this FFmpeg build"
        ))
    })
}

/// Reverse `input` media bytes: video frames via `reverse`, audio
/// samples via `areverse`, both fully decoded and re-encoded.
///
/// # Errors
///
/// - [`MediaError::InvalidInput`] — empty input, no audio/video
///   streams to reverse, or `keep_audio = false` on an audio-only
///   input.
/// - [`MediaError::LimitExceeded`] — input/output size limits.
/// - [`MediaError::Unsupported`] — missing `reverse`/`areverse`
///   filter, muxer, or encoder in this FFmpeg build.
/// - [`MediaError::ToolFailed`] / [`MediaError::Timeout`] — ffmpeg
///   could not decode the input.
pub fn reverse(
    input: &[u8],
    options: &ReverseOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    check_input(input, limits)?;

    let report = probe_input(input, rt, limits)?;
    let has_video = report.streams.iter().any(|s| s.kind() == Some("video"));
    let has_audio = options.keep_audio && report.streams.iter().any(|s| s.kind() == Some("audio"));
    if !has_video && !has_audio {
        return Err(MediaError::InvalidInput(
            "input has no audio or video streams to reverse".to_string(),
        ));
    }

    let caps = rt.capabilities()?;
    if has_video && !caps.has_filter("reverse") {
        return Err(MediaError::Unsupported(
            "ffmpeg build lacks the 'reverse' filter required for video reversal".to_string(),
        ));
    }
    if has_audio && !caps.has_filter("areverse") {
        return Err(MediaError::Unsupported(
            "ffmpeg build lacks the 'areverse' filter required for audio reversal".to_string(),
        ));
    }

    let out_ext = resolve_out_ext(options)?;
    if !caps.has_muxer(&out_ext) {
        return Err(MediaError::Unsupported(format!(
            "ffmpeg build has no {out_ext} muxer for the requested output format"
        )));
    }

    let video_enc = if has_video {
        match video_encoder(&out_ext, options.video_codec.as_deref(), caps)? {
            Some(e) => Some(e),
            None => {
                return Err(MediaError::Unsupported(format!(
                    "no usable video encoder for .{out_ext} output in this FFmpeg build"
                )));
            }
        }
    } else {
        None
    };
    let audio_enc = if has_audio {
        Some(audio_encoder(
            &out_ext,
            options.audio_codec.as_deref(),
            caps,
        )?)
    } else {
        None
    };

    let dir = TempDir::new("reverse")?;
    let in_ext = sniff_ext(input);
    let in_name = format!("input.{in_ext}");
    dir.write(&in_name, input)?;
    let out_name = format!("output.{out_ext}");

    // argv vector only — never a shell string. `?` makes each `-map`
    // optional so video-only or audio-only inputs still work.
    let mut argv: Vec<OsString> = vec![
        OsString::from("-hide_banner"),
        OsString::from("-loglevel"),
        OsString::from("error"),
        OsString::from("-nostdin"),
        OsString::from("-y"),
        OsString::from("-i"),
        dir.file(&in_name).into_os_string(),
        OsString::from("-map"),
        OsString::from("0:v?"),
    ];
    if has_audio {
        argv.extend([OsString::from("-map"), OsString::from("0:a?")]);
    }
    if has_video {
        argv.extend([OsString::from("-vf"), OsString::from("reverse")]);
    }
    if has_audio {
        argv.extend([OsString::from("-af"), OsString::from("areverse")]);
    }
    if let Some(enc) = video_enc {
        argv.extend([OsString::from("-c:v"), OsString::from(enc)]);
        if YUV420_ENCODERS.contains(&enc) {
            argv.extend([OsString::from("-pix_fmt"), OsString::from("yuv420p")]);
        }
    }
    if let Some(enc) = audio_enc {
        argv.extend([OsString::from("-c:a"), OsString::from(enc)]);
        if matches!(enc, "aac" | "libmp3lame" | "libopus" | "libvorbis") {
            argv.extend([OsString::from("-b:a"), OsString::from("128k")]);
        }
    }
    argv.push(dir.file(&out_name).into_os_string());

    let out = process::run_checked("ffmpeg", &rt.ffmpeg, &argv, None, limits)?;
    let bytes = dir.read_limited(&out_name, limits.max_output_bytes)?;
    if bytes.is_empty() {
        return Err(MediaError::ToolFailed {
            tool: "ffmpeg",
            status: "empty output".to_string(),
            detail: format!("ffmpeg produced an empty {out_name}"),
        });
    }

    let mut result = MediaOutput::single(out_name, bytes);
    if let Some(primary) = result.primary.as_mut() {
        primary.content_type = content_type(&out_ext).map(str::to_string);
    }
    if !out.stderr.is_empty() {
        result.warnings.push(out.stderr);
    }
    Ok(result)
}

/// Best-effort container sniff for naming the input temp file.
/// ffmpeg probes content anyway, so `bin` is a safe fallback.
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
    "bin"
}

/// Best-effort MIME type for the output container.
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

#[cfg(test)]
mod tests {
    use super::*;
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

    /// True when this FFmpeg build can actually run a default
    /// reverse: both filters plus at least one usable encoder for
    /// the default mp4 container.
    fn can_reverse(rt: &Runtime) -> bool {
        let Ok(caps) = rt.capabilities() else {
            return false;
        };
        caps.has_filter("reverse")
            && caps.has_filter("areverse")
            && caps.has_muxer("mp4")
            && video_encoder("mp4", None, caps).ok().flatten().is_some()
            && audio_encoder("mp4", None, caps).is_ok()
    }

    /// Generate a 1s clip with an asymmetric test pattern and a
    /// 440 Hz tone via lavfi. `testsrc` frames are time-varying, so
    /// a correct reverse cannot be confused with a pass-through.
    fn make_clip(rt: &Runtime) -> Option<Vec<u8>> {
        let dir = TempDir::new("reverse-test-in").ok()?;
        let out: PathBuf = dir.file("clip.mp4");
        let argv = process::args(&[
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
        let mut full = argv;
        full.push(out.to_string_lossy().into_owned().into());
        let res = process::run("ffmpeg", &rt.ffmpeg, &full, None, &limits()).ok()?;
        if !res.success {
            return None;
        }
        std::fs::read(&out).ok()
    }

    /// Probe the `format=duration` of produced output bytes.
    fn output_duration(bytes: &[u8]) -> Option<f64> {
        let dir = TempDir::new("reverse-dur").ok()?;
        let p = dir.file("out.mp4");
        std::fs::write(&p, bytes).ok()?;
        let out = Command::new("ffprobe")
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
    fn reverse_produces_same_duration_media() {
        let Some(rt) = rt() else {
            eprintln!("skipping: ffmpeg/ffprobe not on PATH");
            return;
        };
        if !can_reverse(&rt) {
            eprintln!("skipping: reverse/areverse filters or mp4 encoders unavailable");
            return;
        }
        let Some(clip) = make_clip(&rt) else {
            eprintln!("skipping: lavfi testsrc/sine or mpeg4/aac encoders unavailable");
            return;
        };
        let out =
            reverse(&clip, &ReverseOptions::default(), &limits(), &rt).expect("reverse failed");
        let file = out.primary.expect("primary output");
        assert!(!file.bytes.is_empty(), "output must be non-empty");
        let dur = output_duration(&file.bytes).expect("probe duration");
        assert!((dur - 1.0).abs() < 0.4, "expected ~1s output, got {dur}s");

        // Reversed output must differ from the source bytes.
        assert_ne!(
            file.bytes, clip,
            "output identical to input — not reversed?"
        );
    }

    #[test]
    fn reverse_without_audio_still_works() {
        let Some(rt) = rt() else {
            eprintln!("skipping: ffmpeg/ffprobe not on PATH");
            return;
        };
        let caps = match rt.capabilities() {
            Ok(c) => c,
            Err(_) => {
                eprintln!("skipping: capability probe failed");
                return;
            }
        };
        if !caps.has_filter("reverse")
            || !caps.has_muxer("mp4")
            || video_encoder("mp4", None, caps).ok().flatten().is_none()
        {
            eprintln!("skipping: reverse filter or mp4 video encoder unavailable");
            return;
        }
        // Video-only lavfi clip: no audio stream at all.
        let dir = match TempDir::new("reverse-test-vid") {
            Ok(d) => d,
            Err(_) => return,
        };
        let clip_path = dir.file("vid.mp4");
        let argv = process::args(&[
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=1:size=160x120:rate=15",
            "-c:v",
            "mpeg4",
            "-pix_fmt",
            "yuv420p",
        ]);
        let mut full = argv;
        full.push(clip_path.to_string_lossy().into_owned().into());
        let res = process::run("ffmpeg", &rt.ffmpeg, &full, None, &limits());
        let Ok(res) = res else {
            eprintln!("skipping: could not generate video-only clip");
            return;
        };
        if !res.success {
            eprintln!("skipping: video-only clip generation failed");
            return;
        }
        let Ok(clip) = std::fs::read(&clip_path) else {
            eprintln!("skipping: could not read video-only clip");
            return;
        };
        let opts = ReverseOptions {
            keep_audio: true,
            ..ReverseOptions::default()
        };
        let out = reverse(&clip, &opts, &limits(), &rt).expect("reverse video-only failed");
        assert!(!out.primary.unwrap().bytes.is_empty());
    }

    #[test]
    fn reverse_rejects_empty_and_streamless() {
        let Some(rt) = rt() else {
            eprintln!("skipping: ffmpeg/ffprobe not on PATH");
            return;
        };
        let err = reverse(&[], &ReverseOptions::default(), &limits(), &rt)
            .expect_err("empty input must fail");
        assert!(matches!(err, MediaError::InvalidInput(_)));

        // 64 bytes of zeros probe as nothing → InvalidInput or ToolFailed,
        // but never a silent success.
        let res = reverse(&vec![0u8; 64], &ReverseOptions::default(), &limits(), &rt);
        assert!(res.is_err());
    }
}
