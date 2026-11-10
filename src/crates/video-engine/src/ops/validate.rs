//! Media validation and corruption detection.
//!
//! [`validate_media`] answers "is this input usable media, and what is
//! wrong with it?" without producing any output artifact:
//!
//! 1. `ffprobe -show_format -show_streams -show_chapters` supplies the
//!    container format, duration, stream inventory and per-stream
//!    sanity fields (resolution, frame rate, sample rate).
//! 2. A full decode pass — `ffmpeg -v error -i in -map <all streams>
//!    -f null -` — exercises every decoder over every stream. stderr
//!    lines become [`ValidationIssue`]s; the pass deliberately does
//!    *not* use `-xerror` so the whole file is decoded and every
//!    corruption site is reported, not just the first.
//!
//! Packet-level probing (`ffprobe -show_packets`) was considered and
//! rejected: it is O(file size) metadata churn that duplicates what
//! the decode pass already proves, at a cost far beyond the value for
//! a validation report.
//!
//! [`validate_quick`] is the probe-only fast path for callers that
//! only need "can this be opened at all?".

use std::collections::BTreeSet;

use serde::Serialize;
use serde_json::Value;

use crate::crates::core::engine::EngineLimits;
use crate::crates::video_engine::error::VideoResult;
use crate::crates::video_engine::ops::{
    COMMAND_TIMEOUT, FFMPEG, FFPROBE, TempDir, check_input, find_tool, probe_json, run_tool,
    stderr_tail, write_input,
};

/// Severity of a [`ValidationIssue`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueSeverity {
    /// The stream/container cannot be decoded or is missing entirely.
    Error,
    /// Suspicious but likely playable (odd frame rate, zero duration,
    /// implausible-but-parseable dimensions, decode warnings).
    Warning,
    /// Informational notes that do not affect usability.
    Info,
}

/// A single finding from media validation.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ValidationIssue {
    /// How severe the finding is.
    pub severity: IssueSeverity,
    /// Stable machine-readable code (e.g. `"probe_failed"`,
    /// `"no_streams"`, `"decode_error"`).
    pub code: String,
    /// Human-readable detail, trimmed for display.
    pub message: String,
}

/// Result of [`validate_media`] / [`validate_quick`].
///
/// `valid` is `true` only when no [`IssueSeverity::Error`] issue was
/// recorded — warnings and info notes do not invalidate the report.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ValidationReport {
    /// `true` when the input probed successfully, has at least one
    /// A/V/subtitle stream, and produced no error-severity findings.
    pub valid: bool,
    /// ffprobe `format_name` (e.g. `"mov,mp4,m4a,3gp,3g2,mj2"`), or
    /// empty when probing failed.
    pub format_name: String,
    /// Container duration in seconds when ffprobe reported one.
    pub duration_secs: Option<f64>,
    /// Number of streams with `codec_type == "video"`.
    pub video_streams: u32,
    /// Number of streams with `codec_type == "audio"`.
    pub audio_streams: u32,
    /// Number of streams with `codec_type == "subtitle"`.
    pub subtitle_streams: u32,
    /// All findings, in discovery order.
    pub issues: Vec<ValidationIssue>,
}

impl ValidationReport {
    fn empty() -> Self {
        Self {
            valid: false,
            format_name: String::new(),
            duration_secs: None,
            video_streams: 0,
            audio_streams: 0,
            subtitle_streams: 0,
            issues: Vec::new(),
        }
    }

    fn push(&mut self, severity: IssueSeverity, code: &str, message: impl Into<String>) {
        self.issues.push(ValidationIssue {
            severity,
            code: code.to_string(),
            message: message.into(),
        });
    }

    /// Recompute `valid` from the collected issues: any error-severity
    /// finding invalidates the report.
    fn finalize(&mut self) {
        self.valid = !self
            .issues
            .iter()
            .any(|i| i.severity == IssueSeverity::Error);
    }
}

/// Probe + full decode validation of `input`.
///
/// `check_input` failures (empty buffer, hard size cap) and missing
/// tools are hard [`VideoError`]s; an unreadable *container* is not —
/// it yields a report with `valid == false` and a `probe_failed`
/// error issue so callers can present the finding instead of an
/// exception.
pub(crate) fn validate_media(input: &[u8], limits: &EngineLimits) -> VideoResult<ValidationReport> {
    check_input(input)?;
    let ffmpeg = find_tool(FFMPEG)?;
    find_tool(FFPROBE)?;

    let dir = TempDir::new("validate")?;
    let input_path = write_input(&dir, "input.bin", input)?;

    let json = match probe_json(&input_path) {
        Ok(json) => json,
        Err(err) => {
            let mut report = ValidationReport::empty();
            report.push(
                IssueSeverity::Error,
                "probe_failed",
                format!("ffprobe could not read the input: {err}"),
            );
            report.finalize();
            return Ok(report);
        }
    };

    let mut report = inspect_probe(&json);
    apply_limits(&mut report, input.len() as u64, limits);
    decode_pass(&ffmpeg, &input_path, &mut report)?;
    report.finalize();
    Ok(report)
}

/// Probe-only fast path: `true` when ffprobe can read the container
/// and at least one stream exists. No decode pass — much cheaper, but
/// it cannot detect mid-file corruption.
///
/// Returns `Ok(false)` for unreadable input; hard failures (missing
/// tools, I/O) still surface as [`VideoError`].
pub(crate) fn validate_quick(input: &[u8]) -> VideoResult<bool> {
    check_input(input)?;
    let dir = TempDir::new("validate-quick")?;
    let input_path = write_input(&dir, "input.bin", input)?;
    match probe_json(&input_path) {
        Ok(json) => {
            let streams = stream_list(&json);
            // ffprobe sniffs plain text/binary blobs as a `bintext`
            // "video" stream inside a `bin` container — those are not
            // real media, so the pseudo-codecs are excluded.
            const PSEUDO_CODECS: &[&str] = &["bintext", "bin", "unknown", "none"];
            Ok(streams.iter().any(|s| {
                let codec_ok = s
                    .get("codec_name")
                    .and_then(Value::as_str)
                    .is_some_and(|c| !PSEUDO_CODECS.contains(&c));
                codec_ok
                    && matches!(
                        s.get("codec_type").and_then(Value::as_str),
                        Some("video") | Some("audio") | Some("subtitle")
                    )
            }))
        }
        Err(_) => Ok(false),
    }
}

/// Build the report skeleton and run all probe-derived checks.
fn inspect_probe(json: &Value) -> ValidationReport {
    let mut report = ValidationReport::empty();
    report.valid = true; // provisional; finalize() recomputes

    let format = json.get("format");
    let format_name = format
        .and_then(|f| f.get("format_name"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    report.format_name = format_name.clone();
    report.duration_secs = format
        .and_then(|f| f.get("duration"))
        .and_then(parse_f64)
        .filter(|d| d.is_finite());

    if format_name.is_empty() {
        report.push(
            IssueSeverity::Warning,
            "unknown_format",
            "ffprobe reported no container format name",
        );
    }

    let streams = stream_list(json);
    for s in streams {
        match s.get("codec_type").and_then(Value::as_str) {
            Some("video") => report.video_streams += 1,
            Some("audio") => report.audio_streams += 1,
            Some("subtitle") => report.subtitle_streams += 1,
            _ => {}
        }
    }

    check_streams(&mut report, streams);
    check_duration(&mut report);
    report
}

fn stream_list<'a>(json: &'a Value) -> &'a [Value] {
    json.get("streams")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// Stream-inventory and per-stream sanity checks.
fn check_streams(report: &mut ValidationReport, streams: &[Value]) {
    if report.video_streams == 0 && report.audio_streams == 0 && report.subtitle_streams == 0 {
        report.push(
            IssueSeverity::Error,
            "no_streams",
            "container has no video, audio, or subtitle streams",
        );
        return;
    }

    for s in streams {
        match s.get("codec_type").and_then(Value::as_str) {
            Some("video") => check_video_stream(report, s),
            Some("audio") => check_audio_stream(report, s),
            _ => {}
        }
    }
}

fn check_video_stream(report: &mut ValidationReport, s: &Value) {
    let codec = stream_label(s);
    let width = s.get("width").and_then(num_u64);
    let height = s.get("height").and_then(num_u64);
    match (width, height) {
        (Some(w), Some(h)) if w > 0 && h > 0 && w <= 76800 && h <= 76800 => {}
        (Some(w), Some(h)) => report.push(
            IssueSeverity::Warning,
            "bad_resolution",
            format!("{codec}: implausible resolution {w}x{h}"),
        ),
        _ => report.push(
            IssueSeverity::Warning,
            "missing_resolution",
            format!("{codec}: video stream reports no width/height"),
        ),
    }

    match frame_rate(s) {
        Some(fps) if fps > 0.0 && fps <= 240.0 => {}
        _ => report.push(
            IssueSeverity::Warning,
            "bad_fps",
            format!(
                "{codec}: implausible frame rate {:?}",
                s.get("avg_frame_rate")
            ),
        ),
    }
}

fn check_audio_stream(report: &mut ValidationReport, s: &Value) {
    let codec = stream_label(s);
    match s.get("sample_rate").and_then(num_u64) {
        Some(rate) if rate > 0 => {}
        _ => report.push(
            IssueSeverity::Warning,
            "bad_sample_rate",
            format!("{codec}: audio stream reports no positive sample rate"),
        ),
    }
}

fn check_duration(report: &mut ValidationReport) {
    match report.duration_secs {
        Some(d) if d > 0.0 => {}
        Some(_) => report.push(
            IssueSeverity::Warning,
            "zero_duration",
            "container duration is zero or negative",
        ),
        None => report.push(
            IssueSeverity::Info,
            "duration_unknown",
            "ffprobe reported no container duration",
        ),
    }
}

/// Record [`EngineLimits`] breaches as warnings — validation reports,
/// it does not reject; the caller decides whether to proceed.
fn apply_limits(report: &mut ValidationReport, input_len: u64, limits: &EngineLimits) {
    if let Some(max) = limits.max_input_bytes {
        if input_len > max {
            report.push(
                IssueSeverity::Warning,
                "input_size_limit",
                format!("input is {input_len} bytes, exceeding max_input_bytes {max}"),
            );
        }
    }
    if let (Some(max), Some(d)) = (limits.max_duration_secs, report.duration_secs) {
        if d > max as f64 {
            report.push(
                IssueSeverity::Warning,
                "duration_limit",
                format!("duration {d:.3}s exceeds max_duration_secs {max}"),
            );
        }
    }
}

/// Full decode pass over every stream. Each stderr line becomes an
/// issue; a non-zero exit with no keyword-matched line still yields an
/// error-severity `decode_failed` so truncated/corrupt inputs cannot
/// slip through as "valid".
fn decode_pass(
    ffmpeg: &std::path::Path,
    input_path: &std::path::Path,
    report: &mut ValidationReport,
) -> VideoResult<()> {
    let args = vec![
        "-v".to_string(),
        "error".to_string(),
        "-i".to_string(),
        input_path.to_string_lossy().into_owned(),
        // Decode every stream type ffprobe found; `?` makes each
        // optional so stream-less types don't fail the mapping.
        "-map".to_string(),
        "0:v?".to_string(),
        "-map".to_string(),
        "0:a?".to_string(),
        "-map".to_string(),
        "0:s?".to_string(),
        "-f".to_string(),
        "null".to_string(),
        "-".to_string(),
    ];
    let output = run_tool(FFMPEG, ffmpeg, &args, COMMAND_TIMEOUT)?;

    let mut saw_error_line = false;
    let mut seen = BTreeSet::new();
    for line in String::from_utf8_lossy(&output.stderr).lines() {
        let line = line.trim();
        if line.is_empty() || !seen.insert(line.to_string()) {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        let severity = if ["error", "corrupt", "invalid", "missing"]
            .iter()
            .any(|kw| lower.contains(kw))
        {
            saw_error_line = true;
            IssueSeverity::Error
        } else {
            IssueSeverity::Warning
        };
        let code = if severity == IssueSeverity::Error {
            "decode_error"
        } else {
            "decode_warning"
        };
        let message: String = line.chars().take(300).collect();
        report.push(severity, code, message);
    }

    if !output.status.success() && !saw_error_line {
        report.push(
            IssueSeverity::Error,
            "decode_failed",
            format!(
                "ffmpeg decode exited with {}: {}",
                output.status,
                stderr_tail(&output.stderr)
            ),
        );
    }
    Ok(())
}

// ---- ffprobe JSON helpers (local mirrors of probe.rs parsing) ----

fn parse_f64(v: &Value) -> Option<f64> {
    v.as_str()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .or_else(|| v.as_f64())
        .filter(|f| f.is_finite())
}

fn num_u64(v: &Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<u64>().ok()))
        .or_else(|| v.as_f64().map(|f| f as u64))
}

/// `"h264"` / `"aac"` style label for messages; falls back to codec id.
fn stream_label(s: &Value) -> String {
    s.get("codec_name")
        .or_else(|| s.get("codec_type"))
        .and_then(Value::as_str)
        .unwrap_or("stream")
        .to_string()
}

/// Parse an ffprobe frame-rate field (`"30000/1001"`, `"25/1"`, or a
/// bare number) from `avg_frame_rate`, falling back to `r_frame_rate`.
/// `"0/0"` and malformed values yield `None`.
fn frame_rate(stream: &Value) -> Option<f64> {
    for key in ["avg_frame_rate", "r_frame_rate"] {
        let Some(raw) = stream.get(key).and_then(Value::as_str) else {
            continue;
        };
        if let Some((num, den)) = raw.split_once('/') {
            let (Ok(num), Ok(den)) = (num.parse::<f64>(), den.parse::<f64>()) else {
                continue;
            };
            if den != 0.0 && num.is_finite() && num > 0.0 {
                return Some(num / den);
            }
            continue;
        }
        if let Ok(v) = raw.parse::<f64>() {
            if v.is_finite() && v > 0.0 {
                return Some(v);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crates::core::engine::EngineLimits;
    use crate::crates::video_engine::ops::{
        COMMAND_TIMEOUT, FFMPEG, FFPROBE, TempDir, find_tool, h264_encoder, run_tool,
    };
    use std::fs;

    fn tools_available() -> bool {
        find_tool(FFMPEG).is_ok() && find_tool(FFPROBE).is_ok()
    }

    /// Run ffmpeg to produce `out` inside `dir`; `None` on any failure
    /// so tests skip cleanly when the toolchain is absent.
    fn encode(dir: &TempDir, name: &str, args: Vec<String>) -> Option<Vec<u8>> {
        let program = find_tool(FFMPEG).ok()?;
        let out = dir.file(name);
        let mut full = vec!["-y".to_string(), "-v".to_string(), "error".to_string()];
        full.extend(args);
        full.push(out.to_string_lossy().into_owned());
        let output = run_tool(FFMPEG, &program, &full, COMMAND_TIMEOUT).ok()?;
        if !output.status.success() {
            return None;
        }
        fs::read(out).ok()
    }

    fn lavfi_input(src: &str) -> Vec<String> {
        vec![
            "-f".to_string(),
            "lavfi".to_string(),
            "-i".to_string(),
            src.to_string(),
        ]
    }

    /// ~1s 160x120 testsrc + 440Hz sine, H.264+AAC mp4.
    fn good_fixture() -> Option<Vec<u8>> {
        if !tools_available() {
            return None;
        }
        let enc = h264_encoder()?;
        let dir = TempDir::new("validate-test-good").ok()?;
        let mut args = lavfi_input("testsrc=duration=1:size=160x120:rate=10");
        args.extend(lavfi_input("sine=frequency=440:duration=1"));
        args.extend([
            "-c:v".to_string(),
            enc.to_string(),
            "-pix_fmt".to_string(),
            "yuv420p".to_string(),
            "-c:a".to_string(),
            "aac".to_string(),
            "-shortest".to_string(),
        ]);
        encode(&dir, "good.mp4", args)
    }

    fn no_errors(report: &ValidationReport) -> bool {
        !report
            .issues
            .iter()
            .any(|i| i.severity == IssueSeverity::Error)
    }

    #[test]
    fn good_mp4_is_valid() {
        let Some(fx) = good_fixture() else { return };
        let limits = EngineLimits::default();
        let report = validate_media(&fx, &limits).expect("validate_media");
        assert!(report.valid, "issues: {:?}", report.issues);
        assert!(no_errors(&report));
        assert_eq!(report.video_streams, 1);
        assert_eq!(report.audio_streams, 1);
        assert!(report.duration_secs.unwrap_or(0.0) > 0.0);
        assert!(validate_quick(&fx).expect("validate_quick"));
    }

    #[test]
    fn truncated_mp4_reports_corruption() {
        let Some(fx) = good_fixture() else { return };
        let cut = fx.len() / 3; // first ~30%
        let truncated = &fx[..cut];
        let limits = EngineLimits::default();
        let report = validate_media(truncated, &limits).expect("validate_media");
        // Truncation may still probe (moov can survive) — accept either
        // an invalid report or at least one error-severity issue.
        assert!(
            !report.valid
                || report
                    .issues
                    .iter()
                    .any(|i| i.severity == IssueSeverity::Error),
            "expected corruption to be flagged, got: {:?}",
            report.issues
        );
        // validate_quick may still succeed on a truncated file (the
        // moov atom can survive) — it only asserts probe+streams.
        assert!(validate_quick(truncated).is_ok());
    }

    #[test]
    fn garbage_is_invalid() {
        if !tools_available() {
            return;
        }
        let garbage = b"this is definitely not a media container".repeat(64);
        let limits = EngineLimits::default();
        let report = validate_media(&garbage, &limits).expect("validate_media");
        assert!(!report.valid);
        // ffprobe may sniff text as `bin`/`bintext`; whatever the
        // path, garbage must surface at least one Error-severity issue.
        assert!(
            report
                .issues
                .iter()
                .any(|i| i.severity == IssueSeverity::Error)
        );
        assert!(!validate_quick(&garbage).expect("validate_quick"));
    }

    #[test]
    fn empty_input_is_hard_error() {
        let limits = EngineLimits::default();
        assert!(validate_media(&[], &limits).is_err());
        assert!(validate_quick(&[]).is_err());
    }
}
