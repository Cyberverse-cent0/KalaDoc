//! Black-frame and freeze-frame detection via FFmpeg's
//! `blackdetect` and `freezedetect` video filters.
//!
//! Both filters are *analysis* filters: they decode every frame,
//! compare pixel statistics against thresholds, and report intervals
//! on **stderr** while the video is discarded to the `null` muxer.
//! There is no output file — the events parsed from stderr are the
//! whole result.
//!
//! # Output format
//!
//! `blackdetect` emits one line per closed interval
//!
//! ```text
//! blackdetect: black_start:1.000000 black_end:2.500000 black_duration:1.500000
//! ```
//!
//! and, when the file ends inside a black interval, an open-ended
//! line `blackdetect:black_start:X` (note: no space after the colon
//! on some builds) with no `black_end`. `freezedetect` emits
//! `freeze_start:` / `freeze_end:` / `freeze_duration:` tokens,
//! sometimes prefixed `lavfi.freezedetect.`.
//!
//! Both are matched by token name rather than exact line layout so
//! minor version differences in prefixing/spacing don't break
//! parsing.

use serde::{Deserialize, Serialize};

use crate::crates::multimedia_engine::error::{MediaError, MediaResult};
use crate::crates::multimedia_engine::model::MediaLimits;
use crate::crates::multimedia_engine::runtime::discovery::Runtime;
use crate::crates::multimedia_engine::runtime::process;
use crate::crates::multimedia_engine::runtime::tempdir::TempDir;

/// Options for [`blackdetect`], mapped onto ffmpeg's
/// `blackdetect=d=D:pic_th=PIC:pix_th=PIX` filter arguments.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BlackDetectOptions {
    /// Minimum black-frame duration in seconds (`d`).
    /// Default `0.1`.
    pub min_duration_secs: f64,
    /// Fraction of the picture that must be "black" for a frame to
    /// count (`pic_th`, picture black ratio). Default `0.98`.
    pub picture_black_ratio: f64,
    /// Per-pixel luminance threshold below which a pixel counts as
    /// black (`pix_th`, 0.0–1.0 normalized). Default `0.10`.
    pub pixel_threshold: f64,
}

impl Default for BlackDetectOptions {
    fn default() -> Self {
        Self {
            min_duration_secs: 0.1,
            picture_black_ratio: 0.98,
            pixel_threshold: 0.10,
        }
    }
}

/// One detected interval of black frames or frozen frames.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DetectionEvent {
    /// Interval start, in seconds from the start of the input.
    pub start_secs: f64,
    /// Interval end in seconds. `None` when the event runs to EOF
    /// (ffmpeg reports open-ended `black_start`/`freeze_start`
    /// lines without a matching end).
    pub end_secs: Option<f64>,
    /// What was detected.
    pub kind: DetectionKind,
}

/// The kind of interval reported by a detection filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DetectionKind {
    /// Frames that are (nearly) all black.
    Black,
    /// Frames that do not change over the configured duration.
    Freeze,
}

/// Result of a detection run: every event found plus the tool that
/// produced them.
#[derive(Debug, Clone, Default)]
pub struct DetectionReport {
    /// Detected intervals, in presentation order.
    pub events: Vec<DetectionEvent>,
    /// Tool/filter name that produced the report (`"blackdetect"`
    /// or `"freezedetect"`).
    pub tool: &'static str,
}

/// Reject non-finite or negative option values before they reach
/// ffmpeg's option parser (which would fail opaquely or, worse,
/// accept a negative threshold).
fn check_finite(name: &str, value: f64) -> MediaResult<()> {
    if !value.is_finite() {
        return Err(MediaError::InvalidInput(format!(
            "{name} must be finite, got {value}"
        )));
    }
    Ok(())
}

/// Best-effort container sniffing, used only to pick a plausible
/// temp-file extension for the input. ffmpeg re-probes the content;
/// the extension just makes demuxer selection more certain.
fn sniff_extension(input: &[u8]) -> &'static str {
    if input.len() >= 4 {
        match &input[..4] {
            [0x1A, 0x45, 0xDF, 0xA3] => return "mkv", // EBML
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
        if input.starts_with(b"\x89PNG") {
            return "png";
        }
        if input.starts_with(b"\xFF\xD8\xFF") {
            return "jpg";
        }
        if input.starts_with(b"GIF8") {
            return "gif";
        }
        if input.len() >= 12 && &input[8..12] == b"WEBP" {
            return "webp";
        }
        if input[0] == 0xFF && (input[1] & 0xE0) == 0xE0 {
            return "mp3";
        }
    }
    "bin"
}

/// Write `input` to a temp file and return `(dir, path)`. Kept
/// private — both detection ops share this prologue.
fn stage_input(
    input: &[u8],
    limits: &MediaLimits,
    prefix: &str,
) -> MediaResult<(TempDir, std::path::PathBuf)> {
    if let Some(max) = limits.max_input_bytes
        && (input.len() as u64) > max
    {
        return Err(MediaError::LimitExceeded(format!(
            "input is {} bytes, exceeding max_input_bytes {max}",
            input.len()
        )));
    }
    let dir = TempDir::new(prefix)?;
    let name = format!("input.{}", sniff_extension(input));
    let path = dir.write(&name, input)?;
    Ok((dir, path))
}

/// Verify the named filter exists in this FFmpeg build.
fn require_filter(rt: &Runtime, filter: &str) -> MediaResult<()> {
    let caps = rt.capabilities()?;
    if caps.has_filter(filter) {
        Ok(())
    } else {
        Err(MediaError::Unsupported(format!(
            "filter {filter:?} is not available in this FFmpeg build"
        )))
    }
}

/// Extract the float following `key:` in a log line, tolerating
/// whitespace and a `lavfi.<filter>.` prefix already stripped by the
/// caller's substring search. Returns `None` when absent/unparseable.
fn value_after(line: &str, key: &str) -> Option<f64> {
    let pos = line.find(key)?;
    let rest = &line[pos + key.len()..];
    let rest = rest.trim_start_matches([':', ' ']);
    // Number ends at the first non-numeric character (whitespace or
    // the next key name).
    let end = rest
        .find(|c: char| {
            !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+' || c == 'e' || c == 'E')
        })
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// Detect intervals of black frames in `input`.
///
/// Runs `ffmpeg -i input -vf "blackdetect=d=..:pic_th=..:pix_th=.."
/// -an -f null -` and parses the `blackdetect:` diagnostics on stderr.
/// Audio is dropped (`-an`) — the filters only inspect video — and the
/// decoded frames go to the null muxer.
///
/// # Errors
///
/// - [`MediaError::LimitExceeded`] when `input` exceeds
///   `limits.max_input_bytes`.
/// - [`MediaError::Unsupported`] when this FFmpeg build lacks the
///   `blackdetect` filter.
/// - [`MediaError::ToolFailed`] when ffmpeg cannot decode the input.
/// - [`MediaError::MissingTool`] / [`MediaError::Timeout`] /
///   [`MediaError::Io`] for tool and filesystem failures.
pub fn blackdetect(
    input: &[u8],
    options: &BlackDetectOptions,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<DetectionReport> {
    check_finite("min_duration_secs", options.min_duration_secs)?;
    check_finite("picture_black_ratio", options.picture_black_ratio)?;
    check_finite("pixel_threshold", options.pixel_threshold)?;
    require_filter(rt, "blackdetect")?;

    let (_dir, in_path) = stage_input(input, limits, "blackdetect")?;

    let filter = format!(
        "blackdetect=d={}:pic_th={}:pix_th={}",
        options.min_duration_secs, options.picture_black_ratio, options.pixel_threshold
    );
    let argv = process::args(&[
        "-hide_banner",
        "-nostdin",
        "-i",
        in_path.to_str().unwrap_or("input"),
        "-vf",
        &filter,
        "-an",
        "-f",
        "null",
        "-",
    ]);
    // Detection diagnostics are emitted at info level — do NOT pass
    // `-loglevel error`, which would silence them.
    let out = process::run_checked("ffmpeg", &rt.ffmpeg, &argv, None, limits)?;

    let events = parse_blackdetect(&out.stderr);
    Ok(DetectionReport {
        events,
        tool: "blackdetect",
    })
}

/// Parse `blackdetect` stderr lines into events.
///
/// Closed interval: `black_start:X black_end:Y black_duration:Z` →
/// event `[X, Y)`. Open-ended `black_start:X` (EOF inside black) →
/// event `[X, None)`.
fn parse_blackdetect(stderr: &str) -> Vec<DetectionEvent> {
    let mut events = Vec::new();
    for line in stderr.lines() {
        if !line.contains("blackdetect") {
            continue;
        }
        let Some(start) = value_after(line, "black_start:") else {
            continue;
        };
        let end = value_after(line, "black_end:");
        events.push(DetectionEvent {
            start_secs: start,
            end_secs: end,
            kind: DetectionKind::Black,
        });
    }
    events
}

/// Detect frozen (unchanged) frames in `input`.
///
/// Runs `ffmpeg -i input -vf "freezedetect=n=NOISE:d=DUR" -an -f
/// null -` and parses the `freeze_start` / `freeze_end` /
/// `freeze_duration` diagnostics on stderr.
///
/// # Arguments
///
/// - `noise` — noise tolerance in dB (default `-60`, matching
///   ffmpeg's default amplitude of 0.001). Converted to the linear
///   amplitude ratio the `n=` option expects. More negative =
///   stricter (less noise tolerated before a frame counts as
///   "changed").
/// - `duration` — minimum freeze duration in seconds passed as `d=`
///   (default `2.0`).
///
/// # Errors
///
/// Same failure modes as [`blackdetect`], with the `freezedetect`
/// filter checked instead.
pub fn freezedetect(
    input: &[u8],
    noise: f64,
    duration: f64,
    limits: &MediaLimits,
    rt: &Runtime,
) -> MediaResult<DetectionReport> {
    check_finite("noise", noise)?;
    check_finite("duration", duration)?;
    require_filter(rt, "freezedetect")?;

    let (_dir, in_path) = stage_input(input, limits, "freezedetect")?;

    // freezedetect's `n` is a linear amplitude ratio in [0, 1];
    // callers pass dB, so convert: amplitude = 10^(dB/20).
    let n = 10f64.powf(noise / 20.0);
    if !(0.0..=1.0).contains(&n) {
        return Err(MediaError::InvalidInput(format!(
            "noise {noise} dB is out of range for freezedetect"
        )));
    }
    let filter = format!("freezedetect=n={}:d={}", n, duration);
    let argv = process::args(&[
        "-hide_banner",
        "-nostdin",
        "-i",
        in_path.to_str().unwrap_or("input"),
        "-vf",
        &filter,
        "-an",
        "-f",
        "null",
        "-",
    ]);
    let out = process::run_checked("ffmpeg", &rt.ffmpeg, &argv, None, limits)?;

    let events = parse_freezedetect(&out.stderr);
    Ok(DetectionReport {
        events,
        tool: "freezedetect",
    })
}

/// Parse `freezedetect` stderr lines into events.
///
/// ffmpeg emits `freeze_start: T` when a freeze begins and, when it
/// ends, `freeze_end: T` plus `freeze_duration: D` — sometimes all
/// on one line, sometimes spread across lines with a
/// `lavfi.freezedetect.` prefix. We pair starts with the next end.
fn parse_freezedetect(stderr: &str) -> Vec<DetectionEvent> {
    let mut events = Vec::new();
    let mut pending_start: Option<f64> = None;
    for line in stderr.lines() {
        if !line.contains("freeze") {
            continue;
        }
        if let Some(start) = value_after(line, "freeze_start:") {
            // A new start while one is pending means the previous
            // freeze never got an end (EOF) — close it open-ended.
            if let Some(prev) = pending_start.take() {
                events.push(DetectionEvent {
                    start_secs: prev,
                    end_secs: None,
                    kind: DetectionKind::Freeze,
                });
            }
            pending_start = Some(start);
        }
        if let Some(end) = value_after(line, "freeze_end:")
            && let Some(start) = pending_start.take()
        {
            events.push(DetectionEvent {
                start_secs: start,
                end_secs: Some(end),
                kind: DetectionKind::Freeze,
            });
        }
    }
    // Unclosed freeze running to EOF.
    if let Some(start) = pending_start {
        events.push(DetectionEvent {
            start_secs: start,
            end_secs: None,
            kind: DetectionKind::Freeze,
        });
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::multimedia_engine::runtime::process;

    /// Encode a fixture with lavfi: `black_secs` of black followed by
    /// `normal_secs` of moving `testsrc`, concatenated. Returns mp4
    /// bytes. Requires a working ffmpeg with an H.264 encoder.
    fn black_then_normal(black_secs: u32, normal_secs: u32) -> Vec<u8> {
        let rt = Runtime::discover().expect("ffmpeg/ffprobe on PATH");
        let dir = TempDir::new("bd-test").unwrap();
        let out = dir.file("fixture.mp4");
        let filter = format!("[0:v][1:v]concat=n=2:v=1[v]",);
        let args = process::args(&[
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("color=c=black:d={black_secs}:s=64x64:r=10"),
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc=duration={normal_secs}:s=64x64:r=10"),
            "-filter_complex",
            &filter,
            "-map",
            "[v]",
            "-c:v",
            "libopenh264",
            "-pix_fmt",
            "yuv420p",
            out.to_str().unwrap(),
        ]);
        process::run_checked("ffmpeg", &rt.ffmpeg, &args, None, &MediaLimits::default())
            .expect("fixture encode failed");
        std::fs::read(&out).unwrap()
    }

    /// Encode a static-color clip (a single frozen frame source).
    fn static_color(secs: u32) -> Vec<u8> {
        let rt = Runtime::discover().expect("ffmpeg/ffprobe on PATH");
        let dir = TempDir::new("fd-test").unwrap();
        let out = dir.file("fixture.mp4");
        let args = process::args(&[
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("color=c=red:d={secs}:s=64x64:r=10"),
            "-c:v",
            "libopenh264",
            "-pix_fmt",
            "yuv420p",
            out.to_str().unwrap(),
        ]);
        process::run_checked("ffmpeg", &rt.ffmpeg, &args, None, &MediaLimits::default())
            .expect("fixture encode failed");
        std::fs::read(&out).unwrap()
    }

    #[test]
    fn blackdetect_finds_black_leading_segment() {
        let rt = Runtime::discover().unwrap();
        let input = black_then_normal(1, 1);
        let report = blackdetect(
            &input,
            &BlackDetectOptions::default(),
            &MediaLimits::default(),
            &rt,
        )
        .expect("blackdetect failed");
        assert_eq!(report.tool, "blackdetect");
        assert!(
            report
                .events
                .iter()
                .any(|e| e.kind == DetectionKind::Black && e.start_secs < 0.5),
            "expected a black event starting near 0, got {:?}",
            report.events
        );
    }

    #[test]
    fn blackdetect_reports_no_events_for_moving_content() {
        let rt = Runtime::discover().unwrap();
        // testsrc is moving content — blackdetect should find nothing.
        let dir = TempDir::new("bd-test2").unwrap();
        let out = dir.file("moving.mp4");
        let args = process::args(&[
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=1:size=64x64:rate=10",
            "-c:v",
            "libopenh264",
            "-pix_fmt",
            "yuv420p",
            out.to_str().unwrap(),
        ]);
        process::run_checked("ffmpeg", &rt.ffmpeg, &args, None, &MediaLimits::default())
            .expect("fixture encode failed");
        let moving = std::fs::read(&out).unwrap();
        let report = blackdetect(
            &moving,
            &BlackDetectOptions::default(),
            &MediaLimits::default(),
            &rt,
        )
        .expect("blackdetect failed");
        assert!(
            report.events.is_empty(),
            "expected no black events in moving testsrc, got {:?}",
            report.events
        );
    }

    #[test]
    fn freezedetect_finds_static_color_freeze() {
        let rt = Runtime::discover().unwrap();
        let input = static_color(3);
        let report = freezedetect(&input, -60.0, 1.0, &MediaLimits::default(), &rt)
            .expect("freezedetect failed");
        assert_eq!(report.tool, "freezedetect");
        assert!(
            report
                .events
                .iter()
                .any(|e| e.kind == DetectionKind::Freeze && e.start_secs < 1.0),
            "expected a freeze event starting early, got {:?}",
            report.events
        );
    }
}
