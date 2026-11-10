//! Video segmentation: split a media input into segments by fixed
//! duration, by detected scene changes, or into equal parts; also
//! extract an arbitrary `[start_ms, end_ms)` range.
//!
//! Every segment is produced by a *re-encode* pass —
//! `-ss <start> -i <in> -t <dur>` followed by the format-appropriate
//! encoder args — so cut points are frame-accurate regardless of the
//! source keyframe layout. The output container is chosen by the
//! caller via `format`, restricted to [`SEGMENT_FORMATS`]
//! (`mp4`/`mkv`/`webm`/`avi`).
//!
//! Scene splitting is two-phase: a `select='gt(scene\,t)',showinfo`
//! pass over `null` collects `pts_time:` cut points from stderr, then
//! each consecutive `[boundary_i, boundary_{i+1})` range is encoded
//! like the other splits.

use std::path::{Path, PathBuf};

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::{VideoError, VideoResult};
use crate::crates::video_engine::ops::probe::probe;
use crate::crates::video_engine::ops::{
    COMMAND_TIMEOUT, FFMPEG, TempDir, check_input, check_limits, checked_ext, encoder_available,
    find_tool, h264_aac_args, read_output, run_tool, tool_failure, write_input,
};

/// Container targets `segment` accepts. All four mux the re-encode
/// codec sets this module emits (H.264/AAC, or VP9/Opus for `webm`).
pub(crate) const SEGMENT_FORMATS: &[&str] = &["mp4", "mkv", "webm", "avi"];

/// Hard cap on segments produced by one split call — bounds both the
/// ffmpeg fan-out and the returned `Vec<SegmentOutput>`.
const MAX_SEGMENTS: usize = 1000;
/// Tighter cap for scene splits: scene detection can legitimately find
/// far more boundaries than a duration split, and each one costs a
/// full re-encode.
const MAX_SCENE_SEGMENTS: usize = 500;
/// `split_equal` accepts 2..=100 parts.
const MAX_EQUAL_PARTS: u32 = 100;

/// One encoded segment of a [`SplitResult`].
#[derive(Debug, Clone)]
pub struct SegmentOutput {
    /// Zero-based position within the split.
    pub index: u32,
    /// Segment start in the source timeline, milliseconds.
    pub start_ms: u64,
    /// Segment end in the source timeline, milliseconds.
    pub end_ms: u64,
    /// Encoded container bytes for this segment.
    pub bytes: Vec<u8>,
}

/// Result of a split operation: the ordered segment list.
#[derive(Debug)]
pub struct SplitResult {
    pub segments: Vec<SegmentOutput>,
}

/// Scratch input plus the shared context every split needs: probed
/// duration, validated output extension, and the ffmpeg binary path.
struct Prepared {
    /// Scratch dir owning `input_path`; must outlive ffmpeg runs.
    _dir: TempDir,
    input_path: PathBuf,
    /// Probed duration in seconds; `None` when ffprobe could not
    /// determine one (callers that need it error honestly).
    duration_secs: Option<f64>,
    /// Validated, normalized output extension.
    ext: String,
    /// Resolved `ffmpeg` binary.
    program: PathBuf,
}

/// Validate `input`, probe it, enforce [`EngineLimits`], resolve the
/// tools, and write the input to a scratch file. `format` is checked
/// against [`SEGMENT_FORMATS`] once here so every split shares the
/// allowlist.
fn prepare(input: &[u8], format: &str, limits: &EngineLimits, tag: &str) -> VideoResult<Prepared> {
    let input_len = check_input(input)?;
    let ext = checked_ext(format, SEGMENT_FORMATS, tag)?;
    let info = probe(input)?;
    check_limits(&info, input_len, limits, false)?;
    let program = find_tool(FFMPEG)?;
    let dir = TempDir::new(tag)?;
    let input_path = write_input(&dir, "input.bin", input)?;
    Ok(Prepared {
        _dir: dir,
        input_path,
        duration_secs: info.duration_secs,
        ext,
        program,
    })
}

/// Probed duration or an honest error — every split is defined in
/// terms of the source duration.
fn require_duration(prepared: &Prepared) -> VideoResult<f64> {
    prepared.duration_secs.ok_or_else(|| {
        VideoError::InvalidInput(
            "cannot determine media duration; segmentation requires a probed duration".to_string(),
        )
    })
}

/// Encoder arguments for a re-encode into `ext`.
///
/// `webm` needs VP9/Opus (H.264 cannot be muxed there); the other
/// containers take the shared H.264/AAC args. Missing encoders are
/// reported before ffmpeg runs rather than as mid-run muxer errors.
fn encode_args(ext: &str) -> VideoResult<Vec<String>> {
    if ext == "webm" {
        if !encoder_available("libvpx-vp9") {
            return Err(VideoError::ExternalTool {
                tool: FFMPEG.to_string(),
                detail: "webm output requires the libvpx-vp9 encoder, which this \
                         ffmpeg build does not provide"
                    .to_string(),
            });
        }
        if !encoder_available("libopus") {
            return Err(VideoError::ExternalTool {
                tool: FFMPEG.to_string(),
                detail: "webm output requires the libopus audio encoder, which this \
                         ffmpeg build does not provide"
                    .to_string(),
            });
        }
        return Ok(vec![
            "-c:v".into(),
            "libvpx-vp9".into(),
            "-deadline".into(),
            "good".into(),
            "-cpu-used".into(),
            "8".into(),
            "-c:a".into(),
            "libopus".into(),
        ]);
    }
    h264_aac_args()
}

/// Re-encode `[start_secs, start_secs + dur_secs)` of the prepared
/// input into `out_path`. Input-side `-ss` seeks before demuxing;
/// `-t` bounds the output duration.
fn encode_range(
    program: &Path,
    input_path: &Path,
    ext: &str,
    start_secs: f64,
    dur_secs: f64,
    out_path: &Path,
) -> VideoResult<()> {
    let mut args = vec![
        "-y".to_string(),
        "-v".to_string(),
        "error".to_string(),
        "-ss".to_string(),
        format!("{start_secs:.3}"),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
        "-t".to_string(),
        format!("{dur_secs:.3}"),
    ];
    args.extend(encode_args(ext)?);
    args.push(out_path.to_string_lossy().into_owned());
    let output = run_tool(FFMPEG, program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(tool_failure(FFMPEG, &output));
    }
    Ok(())
}

/// Encode every `(start_secs, end_secs)` range into `seg-<i>.<ext>`
/// files and collect them as [`SegmentOutput`]s.
fn encode_ranges(
    prepared: &Prepared,
    ranges: &[(f64, f64)],
    cap: usize,
) -> VideoResult<Vec<SegmentOutput>> {
    if ranges.len() > cap {
        return Err(VideoError::LimitExceeded(format!(
            "split produced {} segments, exceeding the cap of {cap}",
            ranges.len()
        )));
    }
    let mut segments = Vec::with_capacity(ranges.len());
    for (i, &(start, end)) in ranges.iter().enumerate() {
        let dur = end - start;
        if !(dur > 0.0) {
            return Err(VideoError::InvalidInput(format!(
                "segment {i} has non-positive duration ({start:.3}s..{end:.3}s)"
            )));
        }
        let out_path = prepared._dir.file(&format!(
            "seg-{}-{}.{}",
            i,
            start_secs_key(start),
            prepared.ext
        ));
        encode_range(
            &prepared.program,
            &prepared.input_path,
            &prepared.ext,
            start,
            dur,
            &out_path,
        )?;
        segments.push(SegmentOutput {
            index: i as u32,
            start_ms: (start * 1000.0).round() as u64,
            end_ms: (end * 1000.0).round() as u64,
            bytes: read_output(&out_path)?,
        });
    }
    Ok(segments)
}

/// Filesystem-safe key for a segment filename: milliseconds rounded.
fn start_secs_key(secs: f64) -> u64 {
    (secs * 1000.0).round() as u64
}

/// Split `input` into consecutive chunks of `segment_secs` seconds.
///
/// `n = ceil(duration / segment_secs)` (capped at 1000); segment `i`
/// covers `[i*segment_secs, min((i+1)*segment_secs, duration))`. Each
/// chunk is re-encoded into `format`.
pub fn split_by_duration(
    input: &[u8],
    segment_secs: f64,
    format: &str,
    limits: &EngineLimits,
) -> VideoResult<SplitResult> {
    if !segment_secs.is_finite() || segment_secs <= 0.0 {
        return Err(VideoError::InvalidInput(format!(
            "segment_secs must be a finite positive number, got {segment_secs}"
        )));
    }
    let prepared = prepare(input, format, limits, "segment-duration")?;
    let duration = require_duration(&prepared)?;

    let n = (duration / segment_secs).ceil();
    if !n.is_finite() || n < 1.0 {
        return Err(VideoError::InvalidInput(format!(
            "cannot compute segment count for duration {duration:.3}s / {segment_secs}s"
        )));
    }
    let n = n as usize;
    if n > MAX_SEGMENTS {
        return Err(VideoError::LimitExceeded(format!(
            "duration {duration:.3}s at {segment_secs}s segments yields {n} segments, \
             exceeding the cap of {MAX_SEGMENTS}"
        )));
    }

    let ranges: Vec<(f64, f64)> = (0..n)
        .map(|i| {
            let start = i as f64 * segment_secs;
            let end = (start + segment_secs).min(duration);
            (start, end)
        })
        .collect();
    let segments = encode_ranges(&prepared, &ranges, MAX_SEGMENTS)?;
    Ok(SplitResult { segments })
}

/// Split `input` at detected scene changes.
///
/// Pass 1 runs `ffmpeg -i <in> -vf "select='gt(scene\,{threshold})',
/// showinfo" -f null -` and collects `pts_time:` values from the
/// showinfo stderr lines — those are the frames whose scene score
/// exceeded `threshold`. Cut points `0 < t < duration` become segment
/// boundaries; consecutive pairs are re-encoded into `format`. At
/// most 500 segments are produced.
pub fn split_by_scenes(
    input: &[u8],
    threshold: f64,
    format: &str,
    limits: &EngineLimits,
) -> VideoResult<SplitResult> {
    if !threshold.is_finite() || threshold < 0.0 {
        return Err(VideoError::InvalidInput(format!(
            "scene threshold must be a finite non-negative number, got {threshold}"
        )));
    }
    let prepared = prepare(input, format, limits, "segment-scenes")?;
    let duration = require_duration(&prepared)?;

    // Pass 1: scene detection to the null muxer.
    let filter = format!("select='gt(scene\\,{threshold})',showinfo");
    let args = vec![
        "-hide_banner".to_string(),
        "-v".to_string(),
        "info".to_string(),
        "-i".to_string(),
        prepared.input_path.to_string_lossy().into_owned(),
        "-an".to_string(),
        "-vf".to_string(),
        filter,
        "-f".to_string(),
        "null".to_string(),
        "-".to_string(),
    ];
    let output = run_tool(FFMPEG, &prepared.program, &args, COMMAND_TIMEOUT)?;
    if !output.status.success() {
        return Err(tool_failure(FFMPEG, &output));
    }
    let cuts = parse_scene_pts(&output.stderr, duration);

    // Boundaries: 0, each interior cut, duration — deduplicated and
    // sorted so jitter cannot produce empty ranges.
    let mut bounds: Vec<f64> = Vec::with_capacity(cuts.len() + 2);
    bounds.push(0.0);
    bounds.extend(cuts);
    bounds.push(duration);
    bounds.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    bounds.dedup_by(|a, b| (*a - *b).abs() < 1e-6);

    let ranges: Vec<(f64, f64)> = bounds
        .windows(2)
        .map(|w| (w[0], w[1]))
        .filter(|(s, e)| e - s > 1e-6)
        .collect();
    if ranges.is_empty() {
        // No interior scene changes: the whole input is one segment.
        return Ok(SplitResult {
            segments: encode_ranges(&prepared, &[(0.0, duration)], MAX_SCENE_SEGMENTS)?,
        });
    }
    let segments = encode_ranges(&prepared, &ranges, MAX_SCENE_SEGMENTS)?;
    Ok(SplitResult { segments })
}

/// Extract `pts_time:` values from `showinfo` stderr lines, keeping
/// only timestamps strictly inside `(0, duration)`.
fn parse_scene_pts(stderr: &[u8], duration: f64) -> Vec<f64> {
    let text = String::from_utf8_lossy(stderr);
    let mut times: Vec<f64> = text
        .lines()
        .filter(|line| line.contains("showinfo"))
        .filter_map(|line| {
            line.split_whitespace()
                .find_map(|tok| tok.strip_prefix("pts_time:"))
                .and_then(|v| v.parse::<f64>().ok())
        })
        .filter(|t| *t > 1e-6 && *t < duration - 1e-6)
        .collect();
    times.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    times.dedup_by(|a, b| (*a - *b).abs() < 1e-6);
    times
}

/// Split `input` into `parts` equal-duration segments.
///
/// `parts` must be in `2..=100`. Segment `i` covers
/// `[i*D/parts, (i+1)*D/parts)` of the probed duration `D`; each is
/// re-encoded into `format`.
pub fn split_equal(
    input: &[u8],
    parts: u32,
    format: &str,
    limits: &EngineLimits,
) -> VideoResult<SplitResult> {
    if !(2..=MAX_EQUAL_PARTS).contains(&parts) {
        return Err(VideoError::InvalidInput(format!(
            "parts must be in 2..={MAX_EQUAL_PARTS}, got {parts}"
        )));
    }
    let prepared = prepare(input, format, limits, "segment-equal")?;
    let duration = require_duration(&prepared)?;

    let p = parts as f64;
    let ranges: Vec<(f64, f64)> = (0..parts)
        .map(|i| {
            let start = duration * i as f64 / p;
            let end = duration * (i + 1) as f64 / p;
            (start, end)
        })
        .collect();
    let segments = encode_ranges(&prepared, &ranges, MAX_SEGMENTS)?;
    Ok(SplitResult { segments })
}

/// Extract `[start_ms, end_ms)` of `input`, re-encoded into `format`,
/// and return the container bytes.
///
/// Requires `0 <= start_ms < end_ms`. When the probed duration is
/// known, `end_ms` is clamped to it and a `start_ms` at or beyond the
/// duration is rejected.
pub fn extract_segment(
    input: &[u8],
    start_ms: u64,
    end_ms: u64,
    format: &str,
    limits: &EngineLimits,
) -> VideoResult<Vec<u8>> {
    if start_ms >= end_ms {
        return Err(VideoError::InvalidInput(format!(
            "start_ms must be less than end_ms, got {start_ms}..{end_ms}"
        )));
    }
    let prepared = prepare(input, format, limits, "segment-extract")?;
    let start_secs = start_ms as f64 / 1000.0;
    let mut dur_secs = (end_ms - start_ms) as f64 / 1000.0;
    if let Some(duration) = prepared.duration_secs {
        if start_secs >= duration {
            return Err(VideoError::InvalidInput(format!(
                "start_ms {start_ms} is at or beyond the media duration {duration:.3}s"
            )));
        }
        let end_secs = start_secs + dur_secs;
        if end_secs > duration {
            // Clamp the tail to the probed duration rather than
            // failing — ffmpeg would silently produce a shorter file
            // anyway, and the clamp keeps `end_ms` honest.
            dur_secs = duration - start_secs;
        }
    }
    if !(dur_secs > 0.0) {
        return Err(VideoError::InvalidInput(
            "extracted range has non-positive duration".to_string(),
        ));
    }
    let out_path = prepared._dir.file(&format!("extract.{}", prepared.ext));
    encode_range(
        &prepared.program,
        &prepared.input_path,
        &prepared.ext,
        start_secs,
        dur_secs,
        &out_path,
    )?;
    read_output(&out_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::video_engine::ops::{FFPROBE, find_tool};
    use std::fs;
    use std::time::Instant;

    fn tools_available() -> bool {
        find_tool(FFMPEG).is_ok() && find_tool(FFPROBE).is_ok()
    }

    /// Build a 3s 160x120 testsrc + 440Hz sine H.264/AAC mp4 via
    /// lavfi. Returns `None` when ffmpeg or an H.264 encoder is
    /// unavailable so tests skip cleanly.
    fn make_fixture() -> Option<Vec<u8>> {
        if !tools_available() || h264_aac_args().is_err() {
            return None;
        }
        let program = find_tool(FFMPEG).ok()?;
        let dir = TempDir::new("segment-test").ok()?;
        let out = dir.file("fixture.mp4");
        let mut args = vec![
            "-y".to_string(),
            "-v".to_string(),
            "error".to_string(),
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            "testsrc=duration=3:size=160x120:rate=10".to_string(),
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            "sine=frequency=440:duration=3".to_string(),
        ];
        args.extend(h264_aac_args().ok()?);
        args.extend(["-shortest".to_string()]);
        args.push(out.to_string_lossy().into_owned());
        let output = run_tool(FFMPEG, &program, &args, COMMAND_TIMEOUT).ok()?;
        if !output.status.success() {
            return None;
        }
        fs::read(out).ok()
    }

    /// Probed duration of an encoded segment, in seconds.
    fn segment_duration(bytes: &[u8]) -> Option<f64> {
        let info = probe(bytes).ok()?;
        info.duration_secs
    }

    #[test]
    fn split_by_duration_three_seconds_one_second_chunks() {
        if !tools_available() {
            return;
        }
        let Some(fixture) = make_fixture() else {
            return;
        };
        let start = Instant::now();
        let result = split_by_duration(&fixture, 1.0, "mp4", &EngineLimits::default()).unwrap();
        assert_eq!(result.segments.len(), 3, "expected 3 segments");
        for (i, seg) in result.segments.iter().enumerate() {
            assert_eq!(seg.index, i as u32);
            assert!(!seg.bytes.is_empty());
            let dur = segment_duration(&seg.bytes)
                .unwrap_or_else(|| panic!("segment {i} has no probed duration"));
            assert!(
                (dur - 1.0).abs() < 0.6,
                "segment {i} duration {dur:.3}s is not ~1s"
            );
        }
        // Timeline coverage: 0..~1, ~1..~2, ~2..3.
        assert_eq!(result.segments[0].start_ms, 0);
        assert_eq!(result.segments[2].end_ms, 3000);
        assert!(start.elapsed().as_secs() < 110, "test exceeded budget");
    }

    #[test]
    fn split_equal_three_parts() {
        if !tools_available() {
            return;
        }
        let Some(fixture) = make_fixture() else {
            return;
        };
        let result = split_equal(&fixture, 3, "mp4", &EngineLimits::default()).unwrap();
        assert_eq!(result.segments.len(), 3);
        for seg in &result.segments {
            let dur = segment_duration(&seg.bytes)
                .unwrap_or_else(|| panic!("segment {} has no probed duration", seg.index));
            assert!(
                (dur - 1.0).abs() < 0.6,
                "segment {} duration {dur:.3}s is not ~1s",
                seg.index
            );
        }
    }

    #[test]
    fn extract_segment_middle_second() {
        if !tools_available() {
            return;
        }
        let Some(fixture) = make_fixture() else {
            return;
        };
        let bytes = extract_segment(&fixture, 500, 1500, "mp4", &EngineLimits::default()).unwrap();
        assert!(!bytes.is_empty());
        let dur = segment_duration(&bytes).expect("extracted segment has no duration");
        assert!(
            (dur - 1.0).abs() < 0.6,
            "extracted duration {dur:.3}s is not ~1s"
        );
    }

    #[test]
    fn extract_segment_validates_range() {
        if !tools_available() {
            return;
        }
        let Some(fixture) = make_fixture() else {
            return;
        };
        assert!(matches!(
            extract_segment(&fixture, 1500, 500, "mp4", &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
        assert!(matches!(
            extract_segment(&fixture, 500, 500, "mp4", &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
    }

    #[test]
    fn split_by_scenes_returns_at_least_one_segment() {
        if !tools_available() {
            return;
        }
        let Some(fixture) = make_fixture() else {
            return;
        };
        // testsrc has a hard cut at ~1s cadence; a low threshold should
        // find at least the frame-0 boundary or none — either way we
        // get >= 1 valid segment.
        let result = split_by_scenes(&fixture, 0.3, "mp4", &EngineLimits::default()).unwrap();
        assert!(!result.segments.is_empty(), "expected at least one segment");
        for seg in &result.segments {
            assert!(!seg.bytes.is_empty());
            assert!(seg.end_ms > seg.start_ms);
        }
    }

    #[test]
    fn split_rejects_bad_args() {
        if !tools_available() {
            return;
        }
        let Some(fixture) = make_fixture() else {
            return;
        };
        assert!(matches!(
            split_by_duration(&fixture, 0.0, "mp4", &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
        assert!(matches!(
            split_by_duration(&fixture, f64::NAN, "mp4", &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
        assert!(matches!(
            split_equal(&fixture, 1, "mp4", &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
        assert!(matches!(
            split_equal(&fixture, 101, "mp4", &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
        assert!(matches!(
            split_by_scenes(&fixture, -1.0, "mp4", &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
        assert!(matches!(
            split_by_duration(&fixture, 1.0, "exe", &EngineLimits::default()),
            Err(VideoError::InvalidInput(_))
        ));
    }
}
