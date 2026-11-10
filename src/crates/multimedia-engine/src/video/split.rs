//! Video splitting into segments.
//!
//! [`split`] cuts one input video into multiple output files. Every
//! segment is produced by its own `ffmpeg` invocation against the
//! same temp-file input, so a failure mid-way fails the whole op —
//! there is no partial [`MediaOutput`].
//!
//! # Split strategies
//!
//! - [`SplitMode::Duration`] — fixed-length windows of `d` seconds
//!   covering the whole input (`ceil(duration / d)` segments). The
//!   input duration is probed with `ffprobe` first.
//! - [`SplitMode::Count`] — `n` equal-length segments covering the
//!   whole input.
//! - [`SplitMode::TimeRanges`] — explicit `(start_secs, end_secs)`
//!   windows; times are seconds from the start of the media.
//!
//! # Accuracy
//!
//! `options.accurate == false` uses `-c copy`: fast, but cuts land on
//! the nearest keyframe at or before the requested start, so segment
//! boundaries are approximate (a warning is reported). `accurate ==
//! true` re-encodes each segment (`libx264`/`aac`, crf 23) with
//! frame-accurate `-ss`/`-to` input seeking. Re-encode requires the
//! `libx264` encoder (and `aac` when the input has audio) in the
//! discovered FFmpeg build — checked against the capability registry;
//! a missing encoder yields [`MediaError::Unsupported`], never a
//! silent codec swap.
//!
//! # Output
//!
//! Segment files are named `{prefix}-{NNN}.{ext}` inside the temp
//! dir and read back under `max_output_bytes`. The first segment is
//! returned as [`MediaOutput::primary`]; the rest go to
//! [`MediaOutput::extra`]. The container is `output_format` when
//! given, else the input's own container (preferring `mp4` when the
//! probe lists it), verified against the muxer capability set.
//! `limits.max_output_files` caps the segment count *before* any
//! ffmpeg run.

use std::ffi::OsString;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::{MediaFile, MediaLimits, MediaOutput};
use crate::crates::multimedia_engine::probe::ProbeReport;
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// How the input timeline is divided into segments.
///
/// Serde representation is **adjacently tagged**:
/// `{"type": "duration", "value": 2.0}`,
/// `{"type": "count", "value": 4}` or
/// `{"type": "time_ranges", "value": [[0.0, 1.5], [1.5, 4.0]]}`.
/// (Internally-tagged enums cannot carry scalar newtype payloads;
/// untagged would make `2` and `2.0` ambiguous between `Duration`
/// and `Count`, so the explicit `type` tag is used.)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum SplitMode {
    /// Segments of `value` seconds each, covering `[0, duration)`.
    /// `value` must be finite and > 0.
    Duration(f64),
    /// `value` equal-length segments covering `[0, duration)`.
    /// `value` must be >= 1.
    Count(u32),
    /// Explicit `(start_secs, end_secs)` windows in seconds. Each
    /// pair must satisfy `0 <= start < end` (both finite).
    TimeRanges(Vec<(f64, f64)>),
}

/// Options for [`split`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SplitOptions {
    /// How to divide the input.
    pub mode: SplitMode,
    /// Output container/format passed to `ffmpeg -f`
    /// (`"mp4"`, `"matroska"`, `"webm"`, ...). `None` keeps the input
    /// container (see module docs).
    #[serde(default)]
    pub output_format: Option<String>,
    /// `true` re-encodes each segment for frame-accurate cuts;
    /// `false` stream-copies (`-c copy`, keyframe-aligned, fast).
    #[serde(default)]
    pub accurate: bool,
    /// File-name prefix for segments; sanitized to
    /// `[A-Za-z0-9._-]`. Defaults to `"part"`.
    #[serde(default = "default_prefix")]
    pub prefix: String,
}

fn default_prefix() -> String {
    "part".to_string()
}

/// Split `input` (encoded media bytes) into segments per `options`.
///
/// Returns a [`MediaOutput`] whose `primary` is the first segment and
/// whose `extra` holds the remaining segments, all named
/// `{prefix}-{NNN}.{ext}`.
pub fn split(
    input: &[u8],
    options: &SplitOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<MediaOutput> {
    if let Some(max) = limits.max_input_bytes
        && input.len() as u64 > max
    {
        return Err(MediaError::LimitExceeded(format!(
            "input is {} bytes, exceeding max_input_bytes {max}",
            input.len()
        )));
    }

    let tmp = TempDir::new("split")?;
    let input_name = format!("input{}", sniff_extension(input));
    let input_path = tmp.write(&input_name, input)?;

    let report = probe(&input_path, limits, rt)?;
    let has_video = report
        .streams
        .iter()
        .any(|s| s.codec_type.as_deref() == Some("video"));
    let has_audio = report
        .streams
        .iter()
        .any(|s| s.codec_type.as_deref() == Some("audio"));

    // Resolve the segment list. Duration/Count need the probed
    // duration; TimeRanges is explicit but still validated.
    let duration = match &options.mode {
        SplitMode::Duration(_) | SplitMode::Count(_) => Some(
            probed_duration(&report)
                .ok_or_else(|| MediaError::InvalidInput("input duration is unknown".to_string()))?,
        ),
        SplitMode::TimeRanges(_) => None,
    };
    let ranges = resolve_ranges(&options.mode, duration)?;

    if ranges.is_empty() {
        return Err(MediaError::InvalidInput(
            "split produced zero segments".to_string(),
        ));
    }
    if let Some(max) = limits.max_output_files
        && ranges.len() as u64 > max as u64
    {
        return Err(MediaError::LimitExceeded(format!(
            "split would produce {} segments, exceeding max_output_files {max}",
            ranges.len()
        )));
    }

    let format = resolve_format(&report, options, rt)?;
    let ext = extension_for(&format);
    let prefix = sanitize_prefix(&options.prefix);

    let video_encoder = if options.accurate {
        check_reencode_caps(rt, has_video, has_audio)?
    } else {
        None
    };

    let mut warnings = Vec::new();
    if !options.accurate {
        warnings.push(
            "stream-copy splitting is keyframe-aligned; segment start times may precede the \
             requested range"
                .to_string(),
        );
    }

    let mut files = Vec::with_capacity(ranges.len());
    for (i, &(start, end)) in ranges.iter().enumerate() {
        let name = format!("{prefix}-{i:03}.{ext}");
        let out_path = tmp.file(&name);
        run_segment(
            &input_path,
            &out_path,
            start,
            end,
            options.accurate,
            video_encoder,
            has_audio,
            &format,
            limits,
            rt,
        )?;
        let bytes = tmp.read_limited(&name, limits.max_output_bytes)?;
        if bytes.is_empty() {
            return Err(MediaError::ToolFailed {
                tool: "ffmpeg",
                status: "empty output".to_string(),
                detail: format!("segment {name} was produced but is empty"),
            });
        }
        files.push(MediaFile {
            name,
            bytes,
            content_type: mime_for(&format).map(str::to_string),
        });
    }

    let mut files = files.into_iter();
    let primary = files.next();
    Ok(MediaOutput {
        primary,
        extra: files.collect(),
        warnings,
    })
}

/// ffprobe the input; any probe failure means the bytes are not
/// recognizable media.
fn probe(input: &Path, limits: &MediaLimits, rt: &Runtime) -> MediaResult<ProbeReport> {
    let argv: Vec<OsString> = [
        OsString::from("-v"),
        OsString::from("error"),
        OsString::from("-print_format"),
        OsString::from("json"),
        OsString::from("-show_format"),
        OsString::from("-show_streams"),
        OsString::from("-show_chapters"),
        OsString::from(input.as_os_str()),
    ]
    .into_iter()
    .collect();
    let out =
        process::run_checked("ffprobe", &rt.ffprobe, &argv, None, limits).map_err(|e| match e {
            MediaError::ToolFailed { detail, .. } => {
                MediaError::InvalidInput(format!("ffprobe rejected input: {detail}"))
            }
            other => other,
        })?;
    serde_json::from_slice::<ProbeReport>(&out.stdout)
        .map_err(|e| MediaError::InvalidInput(format!("unparseable ffprobe output: {e}")))
}

/// Best-effort container duration in seconds: `format.duration`,
/// else the longest stream duration.
fn probed_duration(report: &ProbeReport) -> Option<f64> {
    if let Some(d) = report
        .format
        .as_ref()
        .and_then(|f| f.duration_secs())
        .filter(|d| d.is_finite() && *d > 0.0)
    {
        return Some(d);
    }
    report
        .streams
        .iter()
        .filter_map(|s| s.duration_secs())
        .filter(|d| d.is_finite() && *d > 0.0)
        .fold(None, |acc: Option<f64>, d| {
            Some(acc.map_or(d, |a| a.max(d)))
        })
}

/// Turn the requested [`SplitMode`] into concrete `(start, end)`
/// second ranges. `duration` is required for `Duration`/`Count` and
/// ignored for `TimeRanges`.
fn resolve_ranges(mode: &SplitMode, duration: Option<f64>) -> MediaResult<Vec<(f64, f64)>> {
    match mode {
        SplitMode::Duration(d) => {
            if !d.is_finite() || *d <= 0.0 {
                return Err(MediaError::InvalidInput(format!(
                    "split duration must be finite and > 0, got {d}"
                )));
            }
            let dur = duration.unwrap_or(0.0);
            let n = (dur / d).ceil() as u64;
            Ok((0..n.max(1))
                .map(|i| {
                    let s = i as f64 * d;
                    (s, ((i + 1) as f64 * d).min(dur))
                })
                .collect())
        }
        SplitMode::Count(n) => {
            if *n == 0 {
                return Err(MediaError::InvalidInput(
                    "split count must be >= 1".to_string(),
                ));
            }
            let dur = duration.unwrap_or(0.0);
            let step = dur / *n as f64;
            Ok((0..*n)
                .map(|i| (i as f64 * step, ((i + 1) as f64 * step).min(dur)))
                .collect())
        }
        SplitMode::TimeRanges(ranges) => {
            for &(s, e) in ranges {
                if !s.is_finite() || !e.is_finite() || s < 0.0 || e <= s {
                    return Err(MediaError::InvalidInput(format!(
                        "invalid time range ({s}, {e}): need 0 <= start < end"
                    )));
                }
            }
            Ok(ranges.clone())
        }
    }
}

/// Pick the output container: explicit `output_format`, else the
/// input's probed format (preferring `mp4`/`matroska`/`webm` when
/// listed), else `"mp4"`. Verified against the muxer capability set.
fn resolve_format(
    report: &ProbeReport,
    options: &SplitOptions,
    rt: &Runtime,
) -> MediaResult<String> {
    let format = match &options.output_format {
        Some(f) => f.trim().to_string(),
        None => {
            let names: Vec<&str> = report
                .format
                .as_ref()
                .and_then(|f| f.format_name.as_deref())
                .map(|n| n.split(',').map(str::trim).collect())
                .unwrap_or_default();
            for preferred in ["mp4", "matroska", "webm", "mov"] {
                if names.iter().any(|n| *n == preferred) {
                    return finish_format(preferred.to_string(), rt);
                }
            }
            names
                .first()
                .map(|s| s.to_string())
                .unwrap_or_else(|| "mp4".to_string())
        }
    };
    finish_format(format, rt)
}

fn finish_format(format: String, rt: &Runtime) -> MediaResult<String> {
    let caps = rt.capabilities()?;
    if caps.has_muxer(&format) {
        Ok(format)
    } else {
        Err(MediaError::Unsupported(format!(
            "ffmpeg build has no '{format}' muxer"
        )))
    }
}

/// H.264 encoders acceptable for frame-accurate re-encoding, in
/// preference order.
const ACCURATE_VIDEO_ENCODERS: &[&str] = &["libx264", "libopenh264", "h264", "mpeg4"];

/// Re-encode requires an H.264 encoder for video and `aac` for audio
/// when those streams exist. Returns the encoder to use.
fn check_reencode_caps<'a>(
    rt: &'a Runtime,
    has_video: bool,
    has_audio: bool,
) -> MediaResult<Option<&'a str>> {
    let caps = rt.capabilities()?;
    if has_video {
        let Some(enc) = ACCURATE_VIDEO_ENCODERS
            .iter()
            .copied()
            .find(|e| caps.has_encoder(e))
        else {
            return Err(MediaError::Unsupported(
                "accurate splitting needs an H.264 encoder (libx264/libopenh264/h264/mpeg4), \
                 none present in this FFmpeg build"
                    .to_string(),
            ));
        };
        if has_audio && !caps.has_encoder("aac") {
            return Err(MediaError::Unsupported(
                "accurate splitting needs the aac encoder for the audio stream, not present in \
                 this FFmpeg build"
                    .to_string(),
            ));
        }
        return Ok(Some(enc));
    }
    if has_audio && !caps.has_encoder("aac") {
        return Err(MediaError::Unsupported(
            "accurate splitting needs the aac encoder for the audio stream, not present in this \
             FFmpeg build"
                .to_string(),
        ));
    }
    Ok(None)
}

/// Run one `ffmpeg` segment extraction: `-ss/-to` input seeking for
/// both modes; codec args differ (`-c copy` vs re-encode).
#[allow(clippy::too_many_arguments)]
fn run_segment(
    input: &Path,
    output: &Path,
    start: f64,
    end: f64,
    accurate: bool,
    video_encoder: Option<&str>,
    has_audio: bool,
    format: &str,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<()> {
    let mut argv: Vec<OsString> = vec![
        OsString::from("-y"),
        OsString::from("-nostdin"),
        OsString::from("-ss"),
        OsString::from(format!("{start:.3}")),
        OsString::from("-to"),
        OsString::from(format!("{end:.3}")),
        OsString::from("-i"),
        OsString::from(input.as_os_str()),
    ];
    if accurate {
        if let Some(enc) = video_encoder {
            argv.extend([OsString::from("-c:v"), OsString::from(enc)]);
            if enc == "libx264" {
                argv.extend([
                    OsString::from("-preset"),
                    OsString::from("veryfast"),
                    OsString::from("-crf"),
                    OsString::from("23"),
                ]);
            }
        }
        if has_audio {
            argv.extend([OsString::from("-c:a"), OsString::from("aac")]);
        }
    } else {
        argv.extend([
            OsString::from("-c"),
            OsString::from("copy"),
            // Stream copy can emit negative dts when the seek lands
            // mid-GOP; shift to a clean zero-based timeline.
            OsString::from("-avoid_negative_ts"),
            OsString::from("make_zero"),
        ]);
    }
    argv.extend([
        OsString::from("-f"),
        OsString::from(format),
        OsString::from(output.as_os_str()),
    ]);

    let out = process::run_checked("ffmpeg", &rt.ffmpeg, &argv, None, limits)?;
    let _ = out;
    Ok(())
}

/// Guess an input extension from magic bytes so ffprobe gets a
/// hint; ffprobe probes by content regardless, so a wrong guess is
/// harmless.
fn sniff_extension(input: &[u8]) -> &'static str {
    if input.len() >= 12 && &input[4..8] == b"ftyp" {
        return match &input[8..12] {
            b"qt  " => ".mov",
            _ => ".mp4",
        };
    }
    if input.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return ".mkv";
    }
    if input.starts_with(b"RIFF") && input.len() >= 12 && &input[8..12] == b"AVI " {
        return ".avi";
    }
    if input.starts_with(b"OggS") {
        return ".ogg";
    }
    if input.starts_with(b"fLaC") {
        return ".flac";
    }
    if input.starts_with(b"ID3")
        || input.starts_with(&[0xFF, 0xFB])
        || input.starts_with(&[0xFF, 0xF3])
    {
        return ".mp3";
    }
    ".bin"
}

/// Map a container/format name to a file extension.
fn extension_for(format: &str) -> String {
    match format {
        "matroska" => "mkv".to_string(),
        "mpegts" | "mpegtsraw" => "ts".to_string(),
        "mjpeg" => "jpg".to_string(),
        "ipod" => "m4v".to_string(),
        other => other.to_string(),
    }
}

/// Map a container/format name to a MIME type, when known.
fn mime_for(format: &str) -> Option<&'static str> {
    Some(match format {
        "mp4" | "ipod" | "m4v" | "3gp" => "video/mp4",
        "matroska" => "video/x-matroska",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        "mpegts" | "mpegtsraw" => "video/mp2t",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "ogg" | "oga" => "audio/ogg",
        "opus" => "audio/ogg",
        "gif" => "image/gif",
        "png" => "image/png",
        "mjpeg" | "jpg" => "image/jpeg",
        _ => return None,
    })
}

/// Keep only filename-safe characters in the user-supplied prefix.
fn sanitize_prefix(prefix: &str) -> String {
    let cleaned: String = prefix
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches(|c| c == '.' || c == '_').to_string();
    if cleaned.is_empty() {
        default_prefix()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn rt() -> Runtime {
        Runtime::discover().expect("ffmpeg/ffprobe must be installed for tests")
    }

    /// Build a 4 s 320x240 test video (h264 + aac) with frequent
    /// keyframes (`-g 5` = every 0.5 s) so stream-copy cuts land
    /// close to the requested times.
    fn make_input(rt: &Runtime) -> Vec<u8> {
        let tmp = TempDir::new("split-test-src").unwrap();
        let out = tmp.file("src.mp4");
        let mut argv: Vec<OsString> = process::args(&[
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=4:size=320x240:rate=10",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=4",
            "-c:v",
            ACCURATE_VIDEO_ENCODERS
                .iter()
                .copied()
                .find(|e| rt.capabilities().unwrap().has_encoder(e))
                .expect("no usable video encoder in test ffmpeg build"),
            "-pix_fmt",
            "yuv420p",
            "-g",
            "5",
            "-c:a",
            "aac",
            "-shortest",
        ]);
        argv.push(OsString::from(out.as_os_str()));
        process::run_checked("ffmpeg", &rt.ffmpeg, &argv, None, &MediaLimits::default())
            .expect("failed to generate test input");
        std::fs::read(&out).expect("test input missing")
    }

    fn opts(mode: SplitMode, accurate: bool) -> SplitOptions {
        SplitOptions {
            mode,
            output_format: None,
            accurate,
            prefix: "part".to_string(),
        }
    }

    /// Probe `format=duration` of `bytes` written to a temp file.
    fn duration_secs(rt: &Runtime, bytes: &[u8]) -> f64 {
        let tmp = TempDir::new("split-test-probe").unwrap();
        let path = tmp.write("seg.mp4", bytes).unwrap();
        let argv: Vec<OsString> = [
            OsString::from("-v"),
            OsString::from("error"),
            OsString::from("-show_entries"),
            OsString::from("format=duration"),
            OsString::from("-of"),
            OsString::from("csv=p=0"),
            OsString::from(path.as_os_str()),
        ]
        .into_iter()
        .collect();
        let out =
            process::run_checked("ffprobe", &rt.ffprobe, &argv, None, &MediaLimits::default())
                .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .expect("ffprobe duration not a number")
    }

    #[test]
    fn split_duration_copy_produces_two_segments() {
        let rt = rt();
        let input = make_input(&rt);
        let out = split(
            &input,
            &opts(SplitMode::Duration(2.0), false),
            &MediaLimits::default(),
            &rt,
        )
        .expect("split failed");

        let files: Vec<&MediaFile> = out.files().collect();
        assert_eq!(files.len(), 2, "expected 2 segments, got {files:?}");
        assert!(out.primary.is_some(), "primary (first segment) must be set");
        for f in &files {
            assert!(!f.bytes.is_empty(), "{} is empty", f.name);
            assert!(f.name.starts_with("part-"), "unexpected name {}", f.name);
            assert!(f.name.ends_with(".mp4"), "unexpected ext in {}", f.name);
        }
        assert_eq!(files[0].name, "part-000.mp4");
        assert_eq!(files[1].name, "part-001.mp4");
        assert!(out.warnings.iter().any(|w| w.contains("keyframe")));
    }

    #[test]
    fn split_count_accurate_gives_equal_durations() {
        let rt = rt();
        let input = make_input(&rt);
        let out = split(
            &input,
            &opts(SplitMode::Count(2), true),
            &MediaLimits::default(),
            &rt,
        )
        .expect("accurate split failed");

        let files: Vec<&MediaFile> = out.files().collect();
        assert_eq!(files.len(), 2);
        for f in &files {
            let d = duration_secs(&rt, &f.bytes);
            assert!((1.6..=2.4).contains(&d), "{} duration {d}s not ~2s", f.name);
        }
    }

    #[test]
    fn split_time_ranges_produces_explicit_windows() {
        let rt = rt();
        let input = make_input(&rt);
        let out = split(
            &input,
            &opts(SplitMode::TimeRanges(vec![(0.0, 1.5), (1.5, 4.0)]), false),
            &MediaLimits::default(),
            &rt,
        )
        .expect("range split failed");

        let files: Vec<&MediaFile> = out.files().collect();
        assert_eq!(files.len(), 2);
        for f in &files {
            assert!(!f.bytes.is_empty());
        }
    }

    #[test]
    fn split_respects_max_output_files() {
        let rt = rt();
        let input = make_input(&rt);
        let limits = MediaLimits {
            max_output_files: Some(2),
            ..MediaLimits::default()
        };
        let err = split(
            &input,
            &opts(SplitMode::Duration(1.0), false), // 4 segments > cap 2
            &limits,
            &rt,
        )
        .expect_err("expected LimitExceeded");
        assert!(
            matches!(err, MediaError::LimitExceeded(_)),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn split_mode_serde_roundtrip() {
        for mode in [
            SplitMode::Duration(2.5),
            SplitMode::Count(3),
            SplitMode::TimeRanges(vec![(0.0, 1.0), (2.0, 3.5)]),
        ] {
            let json = serde_json::to_string(&mode).unwrap();
            let back: SplitMode = serde_json::from_str(&json).unwrap();
            assert_eq!(mode, back);
        }
        // Adjacently tagged shape: {"type": ..., "value": ...}
        let v: serde_json::Value =
            serde_json::from_str(r#"{"type":"duration","value":2.0}"#).unwrap();
        assert_eq!(v["type"], "duration");
    }
}
